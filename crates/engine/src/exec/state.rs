use std::{
    collections::{BTreeSet, VecDeque},
    rc::Rc,
};

use miden_assembly::SourceManager;
use miden_core::{
    mast::{MastNode, MastNodeId},
    operations::AssemblyOp,
};
use miden_processor::{
    ContextId, Continuation, ExecutionError, FastProcessor, Felt, ResumeContext, StackOutputs,
    operation::Operation, trace::RowIndex,
};

use super::{DebuggerHost, ExecutionTrace, TraceMonitor};
use crate::{
    Breakpoint, BreakpointType,
    debug::{CallFrame, CallStack, ControlFlowOp, DebugVarTracker, StepInfo},
};

/// Resolve a future that is expected to complete immediately (synchronous host methods).
///
/// We use a noop waker because our Host methods all return `std::future::ready(...)`.
/// This avoids calling `step_sync()` which would create its own tokio runtime and
/// panic inside the TUI's existing tokio current-thread runtime.
/// TODO: Revisit this (djole).
fn poll_immediately<T>(fut: impl std::future::Future<Output = T>) -> T {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(val) => val,
        std::task::Poll::Pending => panic!("future was expected to complete immediately"),
    }
}

/// A special version of [crate::Executor] which provides finer-grained control over execution,
/// and captures a ton of information about the program being executed, so as to make it possible
/// to introspect everything about the program and the state of the VM at a given cycle.
///
/// This is used by the debugger to execute programs, and provide all of the functionality made
/// available by the TUI.
pub struct DebugExecutor {
    /// The underlying [FastProcessor] being driven
    pub processor: FastProcessor,
    /// The host providing debugging callbacks
    pub host: DebuggerHost<dyn miden_assembly::SourceManager>,
    /// The resume context for the next step (None if program has finished)
    pub resume_ctx: Option<ResumeContext>,

    // State from last step (replaces VmState fields)
    /// The current operand stack state
    pub current_stack: Vec<Felt>,
    /// The operation that was just executed
    pub current_op: Option<Operation>,
    /// The assembly-level operation info for the current op
    pub current_asmop: Option<AssemblyOp>,

    /// The final outcome of the program being executed
    pub stack_outputs: StackOutputs,
    /// The set of contexts allocated during execution so far
    pub contexts: BTreeSet<ContextId>,
    /// The root context
    pub root_context: ContextId,
    /// The current context at `cycle`
    pub current_context: ContextId,
    /// The current call stack
    pub callstack: CallStack,
    /// The most recent live procedure name observed from assembly operation metadata.
    pub current_proc: Option<Rc<str>>,
    /// Debug variable tracker for source-level variable inspection
    pub debug_vars: DebugVarTracker,
    /// Number of debug variable location records observed during the most recent step.
    pub last_debug_var_count: usize,
    /// A sliding window of the last 5 operations successfully executed by the VM
    pub recent: VecDeque<Operation>,
    /// The current clock cycle
    pub cycle: usize,
    /// Whether or not execution has terminated
    pub stopped: bool,
}

/// Extract the current operation and assembly info from the continuation stack
/// before a step is executed. This lets us know what operation will run next.
pub(crate) fn extract_current_op(
    ctx: &ResumeContext,
) -> (Option<Operation>, Option<MastNodeId>, Option<usize>, Option<ControlFlowOp>) {
    let forest = ctx.current_forest();
    for cont in ctx.continuation_stack().iter_continuations_for_next_clock() {
        match cont {
            Continuation::ResumeBasicBlock {
                node_id,
                batch_index,
                op_idx_in_batch,
            } => {
                let node = &forest[*node_id];
                if let MastNode::Block(block) = node {
                    // Compute global op index within the basic block
                    let mut global_idx = 0;
                    for batch in &block.op_batches()[..*batch_index] {
                        global_idx += batch.ops().len();
                    }
                    global_idx += op_idx_in_batch;
                    let op = block.op_batches()[*batch_index].ops().get(*op_idx_in_batch).copied();
                    return (op, Some(*node_id), Some(global_idx), None);
                }
            }
            Continuation::Respan {
                node_id,
                batch_index,
            } => {
                let node = &forest[*node_id];
                if let MastNode::Block(block) = node {
                    let mut global_idx = 0;
                    for batch in &block.op_batches()[..*batch_index] {
                        global_idx += batch.ops().len();
                    }
                    return (None, Some(*node_id), Some(global_idx), Some(ControlFlowOp::Respan));
                }
            }
            Continuation::StartNode(node_id) => {
                let control = match &forest[*node_id] {
                    MastNode::Block(_) => Some(ControlFlowOp::Span),
                    MastNode::Join(_) => Some(ControlFlowOp::Join),
                    MastNode::Split(_) => Some(ControlFlowOp::Split),
                    _ => None,
                };
                return (None, Some(*node_id), None, control);
            }
            Continuation::FinishBasicBlock(_)
            | Continuation::FinishJoin(_)
            | Continuation::FinishSplit(_)
            | Continuation::FinishLoop { .. }
            | Continuation::FinishCall(_)
            | Continuation::FinishDyn(_)
            | Continuation::FinishExternal(_) => {
                return (None, None, None, Some(ControlFlowOp::End));
            }
            other if other.increments_clk() => {
                return (None, None, None, None);
            }
            _ => continue,
        }
    }
    (None, None, None, None)
}

impl DebugExecutor {
    /// Returns true if the current program forest has debug-variable locations associated with
    /// `procedure`.
    pub fn procedure_has_debug_vars(&self, procedure: &str) -> bool {
        let Some(resume_ctx) = self.resume_ctx.as_ref() else {
            return false;
        };

        let forest = resume_ctx.current_forest();
        for (node_idx, node) in forest.nodes().iter().enumerate() {
            let MastNode::Block(block) = node else {
                continue;
            };
            let node_id = MastNodeId::new_unchecked(node_idx as u32);
            for op_idx in 0..block.num_operations() as usize {
                if forest.debug_vars_for_operation(node_id, op_idx).is_empty() {
                    continue;
                }
                if forest
                    .get_assembly_op(node_id, Some(op_idx))
                    .is_some_and(|op| op.context_name() == procedure)
                {
                    return true;
                }
            }
        }

        false
    }

    pub fn register_trace_monitor_for(&mut self, monitor: TraceMonitor, event: super::TraceEvent) {
        self.host.register_trace_handler(event, move |state, event| {
            monitor.handle_event(state.clock(), event)
        });
    }

    /// Advance the program state by one cycle.
    ///
    /// If the program has already reached its termination state, it returns the same result
    /// as the previous time it was called.
    ///
    /// Returns the call frame exited this cycle, if any
    pub fn step(&mut self) -> Result<Option<CallFrame>, ExecutionError> {
        if self.stopped {
            self.last_debug_var_count = 0;
            return Ok(None);
        }

        let resume_ctx = match self.resume_ctx.take() {
            Some(ctx) => ctx,
            None => {
                self.stopped = true;
                self.last_debug_var_count = 0;
                return Ok(None);
            }
        };

        // Before step: peek continuation to determine what will execute
        let (op, node_id, op_idx, control) = extract_current_op(&resume_ctx);
        let asmop = node_id
            .and_then(|nid| resume_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());

        // Look up debug vars from MAST forest for the current operation
        let debug_var_infos: Vec<_> = if let (Some(nid), Some(idx)) = (node_id, op_idx) {
            let forest = resume_ctx.current_forest();
            forest
                .debug_vars_for_operation(nid, idx)
                .iter()
                .filter_map(|vid| forest.debug_var(*vid).cloned())
                .collect()
        } else {
            vec![]
        };

        // Execute one step
        match poll_immediately(self.processor.step(&mut self.host, resume_ctx)) {
            Ok(Some(new_ctx)) => {
                self.resume_ctx = Some(new_ctx);
                self.cycle += 1;

                // Query processor state
                let state = self.processor.state();
                let ctx = state.ctx();
                self.current_stack = state.get_stack_state();

                if self.current_context != ctx {
                    self.contexts.insert(ctx);
                    self.current_context = ctx;
                }

                // Track operation
                self.current_op = op;
                self.current_asmop = asmop.clone();
                if let Some(asmop) = asmop.as_ref() {
                    self.current_proc = Some(Rc::from(asmop.context_name()));
                }

                if let Some(op) = op {
                    if self.recent.len() == 5 {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(op);
                }

                // Update call stack
                let step_info = StepInfo {
                    op,
                    control,
                    asmop: self.current_asmop.as_ref(),
                    clk: RowIndex::from(self.cycle as u32),
                    ctx: self.current_context,
                };
                let exited = self.callstack.next(&step_info);

                // Record and process debug variable events
                let debug_var_count = debug_var_infos.len();
                self.debug_vars
                    .record_events(RowIndex::from(self.cycle as u32), debug_var_infos);
                self.debug_vars.update_to_cycle(RowIndex::from(self.cycle as u32));
                self.last_debug_var_count = debug_var_count;

                Ok(exited)
            }
            Ok(None) => {
                // Program completed
                self.stopped = true;
                self.last_debug_var_count = 0;
                let state = self.processor.state();
                self.current_stack = state.get_stack_state();

                // Capture the final stack as StackOutputs (truncate to 16 elements)
                let len = self.current_stack.len().min(16);
                self.stack_outputs =
                    StackOutputs::new(&self.current_stack[..len]).expect("invalid stack outputs");
                Ok(None)
            }
            Err(err) => {
                self.stopped = true;
                self.last_debug_var_count = 0;
                Err(err)
            }
        }
    }

    /// Advance the program state until `breakpoint` is hit.
    ///
    /// If the program has already reached its termination state, it returns the same result
    /// as the previous time it was called.
    pub fn step_until(
        &mut self,
        breakpoint: BreakpointType,
        trace_monitor: Option<TraceMonitor>,
        source_manager: &dyn SourceManager,
    ) -> Result<(), ExecutionError> {
        let start_cycle = self.cycle;
        let start_clock = self.processor.state().clock();
        let breakpoint = Breakpoint {
            id: 0,
            creation_cycle: start_cycle,
            ty: breakpoint,
        };
        let start_asmop = self.current_asmop.clone();
        while !self.stopped {
            match self.step()? {
                Some(exited)
                    if exited.should_break_on_exit() && breakpoint.ty == BreakpointType::Finish =>
                {
                    return Ok(());
                }
                _ => (),
            }

            // Break on trace events, if monitored
            if let BreakpointType::Trace(event_id) = breakpoint.ty
                && let Some(trace_monitor) = trace_monitor.as_ref()
                && trace_monitor.has_event_occurred_since(start_clock, |event| event == event_id)
            {
                return Ok(());
            }

            let (op, is_op_boundary, proc, loc) = {
                let op = self.current_op;
                let is_boundary = self.current_asmop.as_ref().map(|_info| true).unwrap_or(false);
                let (proc, loc) = match self.callstack.current_frame() {
                    Some(frame) => {
                        let loc = frame
                            .recent()
                            .back()
                            .and_then(|detail| detail.resolve(source_manager))
                            .cloned();
                        (frame.procedure(""), loc)
                    }
                    None => (None, None),
                };
                (op, is_boundary, proc, loc)
            };

            if let Some(op) = op
                && breakpoint.should_break_for(&op)
            {
                return Ok(());
            }

            if is_op_boundary
                && let Some(asmop) = self.current_asmop.as_ref()
                && matches!(breakpoint.ty, BreakpointType::AsmOpcode(asm_opcode) if asmop.op() == asm_opcode)
            {
                return Ok(());
            }

            // Check if `breakpoint` was triggered at this cycle
            let current_cycle = self.cycle;
            let cycles_stepped = current_cycle - start_cycle;
            if let Some(n) = breakpoint.cycles_to_skip(current_cycle)
                && cycles_stepped >= n
            {
                return Ok(());
            }

            if cycles_stepped > 0
                && is_op_boundary
                && matches!(&breakpoint.ty, BreakpointType::Next)
                && self.current_asmop != start_asmop
            {
                return Ok(());
            }

            if let Some(loc) = loc.as_ref()
                && breakpoint.should_break_at(loc)
            {
                return Ok(());
            }

            if let Some(proc) = proc.as_deref()
                && breakpoint.should_break_in(proc)
            {
                return Ok(());
            }
        }

        Ok(())
    }

    /// Consume the [DebugExecutor], converting it into an [ExecutionTrace] at the current cycle.
    pub fn into_execution_trace(self) -> ExecutionTrace {
        ExecutionTrace {
            root_context: self.root_context,
            last_cycle: RowIndex::from(self.cycle as u32),
            processor: self.processor,
            outputs: self.stack_outputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly::DefaultSourceManager;

    use super::*;
    use crate::exec::Executor;

    #[test]
    fn callstack_tracks_nested_frame_trace_events() {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(
                r#"
proc inner
    nop
end

proc outer
    trace.240
    nop
    exec.inner
    trace.252
    nop
end

begin
    trace.240
    nop
    exec.outer
    trace.252
    nop
end
"#,
            )
            .unwrap();

        let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(&program, source_manager);
        let mut max_depth = 0;
        let mut saw_inner = false;
        let mut snapshots = Vec::new();

        for _ in 0..64 {
            executor.step().unwrap();
            let frames = executor.callstack.frames();
            max_depth = max_depth.max(frames.len());
            snapshots.push(
                frames
                    .iter()
                    .map(|frame| {
                        frame
                            .procedure("")
                            .map(|name| name.to_string())
                            .unwrap_or_else(|| "<unknown>".to_string())
                    })
                    .collect::<Vec<_>>(),
            );
            saw_inner |= frames.len() >= 3
                && frames
                    .last()
                    .and_then(|frame| frame.procedure(""))
                    .is_some_and(|name| name.contains("inner"));

            if saw_inner || executor.stopped {
                break;
            }
        }

        assert!(
            max_depth >= 3,
            "expected nested main -> outer -> inner frames, max depth was {max_depth}"
        );
        assert!(
            saw_inner,
            "expected innermost frame to resolve to inner; snapshots: {snapshots:?}"
        );
    }
}
