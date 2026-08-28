use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};

use miden_assembly::SourceManager;
use miden_core::{
    mast::{MastNode, MastNodeId},
    operations::AssemblyOp,
};
use miden_mast_package::debug_info::{DebugSourceNodeId, PackageDebugInfo};
use miden_processor::{
    ContextId, Continuation, ExecutionError, FastProcessor, Felt, ResumeContext, StackOutputs,
    operation::Operation, trace::RowIndex,
};

use super::{DebuggerHost, ExecutionTrace};
use crate::{
    Breakpoint, BreakpointType, OperationMatcher,
    debug::{CallFrame, CallStack, ControlFlowOp, DebugVarTracker, StepInfo},
    profiling::Profiler,
};

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
    pub current_proc: Option<Arc<str>>,
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
    /// The profiler used by this executor
    pub profiler: Profiler,
}

impl super::query::DebugQuery for DebugExecutor {
    #[inline]
    fn state(&self) -> miden_processor::ProcessorState<'_> {
        self.processor.state()
    }

    fn current_context(&self) -> ContextId {
        self.current_context
    }

    fn current_clock(&self) -> RowIndex {
        self.processor.state().clock()
    }
}

impl DebugExecutor {
    /// Get the current operand stack as a slice - the top of the stack is the last item in the slice
    ///
    /// This returns the entire stack, not just the top 16 elements
    pub fn stack(&self) -> &[Felt] {
        self.processor.stack()
    }
}

pub(crate) struct CurrentCycleInfo {
    pub op: Option<Operation>,
    pub node_id: Option<MastNodeId>,
    pub source_node_id: Option<DebugSourceNodeId>,
    pub op_idx: Option<usize>,
    pub control_flow_kind: Option<ControlFlowOp>,
}

/// Extract the current operation and assembly info from the continuation stack
/// before a step is executed. This lets us know what operation will run next.
pub(crate) fn extract_current_op(ctx: &ResumeContext) -> CurrentCycleInfo {
    let forest = ctx.current_forest();
    let debug_info = ctx.debug_info();
    let continuation_stack = ctx.continuation_stack();
    for (cont, source_node_id) in
        continuation_stack.iter_continuations_for_next_clock_with_source_node_ids()
    {
        let exec_node = cont.exec_node();
        let source_node_id = source_node_id.or_else(|| {
            exec_node.zip(debug_info.as_deref()).and_then(|(exec_node, di)| {
                di.unique_source_root_for_exec_node(exec_node).ok().flatten()
            })
        });
        match cont {
            Continuation::ResumeBasicBlock {
                node_id,
                batch_index,
                op_idx_in_batch,
            } => {
                let MastNode::Block(block) = &forest[*node_id] else {
                    unreachable!()
                };
                // Compute global op index within the basic block
                let mut global_idx = 0;
                for batch in &block.op_batches()[..*batch_index] {
                    global_idx += batch.ops().len();
                }
                global_idx += op_idx_in_batch;
                let op = block.op_batches()[*batch_index].ops().get(*op_idx_in_batch).copied();
                return CurrentCycleInfo {
                    op,
                    node_id: Some(*node_id),
                    source_node_id,
                    op_idx: Some(global_idx),
                    control_flow_kind: None,
                };
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
                    return CurrentCycleInfo {
                        op: None,
                        node_id: Some(*node_id),
                        source_node_id,
                        op_idx: Some(global_idx),
                        control_flow_kind: Some(ControlFlowOp::Respan),
                    };
                }
            }
            Continuation::StartNode(node_id) => {
                let control_flow_kind = match &forest[*node_id] {
                    MastNode::Block(_) => Some(ControlFlowOp::Span),
                    MastNode::Join(_) => Some(ControlFlowOp::Join),
                    MastNode::Split(_) => Some(ControlFlowOp::Split),
                    _ => None,
                };
                return CurrentCycleInfo {
                    op: None,
                    node_id: Some(*node_id),
                    source_node_id,
                    op_idx: None,
                    control_flow_kind,
                };
            }
            Continuation::FinishBasicBlock(_)
            | Continuation::FinishJoin(_)
            | Continuation::FinishSplit(_)
            | Continuation::FinishLoop { .. }
            | Continuation::FinishCall(_)
            | Continuation::FinishDyn(_) => {
                return CurrentCycleInfo {
                    op: None,
                    node_id: None,
                    source_node_id,
                    op_idx: None,
                    control_flow_kind: Some(ControlFlowOp::End),
                };
            }
            Continuation::EnterForest { .. } => {
                return CurrentCycleInfo {
                    op: None,
                    node_id: None,
                    source_node_id,
                    op_idx: None,
                    control_flow_kind: None,
                };
            }
        }
    }
    CurrentCycleInfo {
        op: None,
        node_id: None,
        source_node_id: None,
        op_idx: None,
        control_flow_kind: None,
    }
}

impl DebugExecutor {
    /// Returns true if the current program forest has debug-variable locations associated with
    /// `procedure`.
    #[allow(unused)]
    pub fn procedure_has_debug_vars(&self, procedure: &str) -> bool {
        let Some(resume_ctx) = self.resume_ctx.as_ref() else {
            return false;
        };
        let Some(debug_info) = resume_ctx.debug_info() else {
            return false;
        };

        for function_info in debug_info.functions() {
            if debug_info[function_info.name_idx].as_ref() != procedure {
                continue;
            }
            if let Some(source_node) = function_info.source_node.into_option() {
                return !debug_info[source_node].debug_vars.is_empty();
            } else if let Some(exec_node) =
                resume_ctx.current_forest().find_procedure_root(function_info.mast_root)
                && let Ok(Some(source_node)) =
                    debug_info.unique_source_root_for_exec_node(exec_node)
            {
                return !debug_info[source_node].debug_vars.is_empty();
            } else {
                return false;
            }
        }

        false
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

        let debug_info: Option<Arc<PackageDebugInfo>> = resume_ctx.debug_info();

        // Before step: peek continuation to determine what will execute
        let CurrentCycleInfo {
            op,
            node_id,
            source_node_id,
            op_idx,
            control_flow_kind,
        } = extract_current_op(&resume_ctx);
        let debug_node_id = source_node_id.or_else(|| {
            node_id.zip(debug_info.as_deref()).and_then(|(exec_node, di)| {
                di.unique_source_root_for_exec_node(exec_node).ok().flatten()
            })
        });
        let source_node = debug_node_id.zip(debug_info.as_deref()).map(|(dnid, di)| &di[dnid]);
        let asmop = source_node.and_then(|source_node| match op_idx {
            Some(op_idx) => source_node.asm_op_for_operation(op_idx as u32),
            None => source_node.asm_op_for_operation(0),
        });

        // Look up debug vars from MAST forest for the current operation
        let debug_var_infos: Vec<_> = source_node
            .zip(op_idx)
            .zip(debug_info.as_deref())
            .map(|((source_node, op_idx), di)| {
                source_node.debug_infos_for_operation(op_idx as u32, di).collect()
            })
            .unwrap_or_default();
        let pre_step_stack = self.processor.state().get_stack_state();

        // Execute one step
        let step_result = if let Some(debug_info) = debug_info.as_deref() {
            self.processor
                .step_with_package_debug_info_sync(&mut self.host, resume_ctx, debug_info)
        } else {
            self.processor.step_sync(&mut self.host, resume_ctx)
        };
        match step_result {
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
                self.current_asmop = asmop.zip(debug_info.as_deref()).map(|(asmop, di)| {
                    AssemblyOp::new(
                        asmop.location_idx.into_option().and_then(|loc| di.get_location(loc)),
                        di[asmop.context_name_idx].clone(),
                        asmop.num_cycles,
                        di[asmop.op_name_idx].clone(),
                    )
                });
                if let Some(asmop) = self.current_asmop.as_ref() {
                    self.current_proc = Some(asmop.context_name().clone());
                }

                if let Some(op) = op {
                    if self.recent.len() == 5 {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(op);
                    self.profiler.on_operation_execution_cycle(op, self.current_proc.as_deref());
                }

                // Update call stack
                let step_info = StepInfo {
                    op,
                    control: control_flow_kind,
                    asmop: self.current_asmop.as_ref(),
                    clk: RowIndex::from(self.cycle as u32),
                    ctx: self.current_context,
                };
                let exited = self.callstack.next(&step_info);

                // Record and process debug variable events
                let debug_var_count = debug_var_infos.len();
                self.debug_vars.record_events_with_stack(
                    RowIndex::from(self.cycle as u32),
                    debug_var_infos,
                    &pre_step_stack,
                );
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

                // Write profiling reports in case its enabled
                self.profiler.write_reports();
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
        source_manager: &dyn SourceManager,
    ) -> Result<(), ExecutionError> {
        let start_cycle = self.cycle;
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
                && breakpoint.should_break_for(&op, &self.processor.state())
            {
                return Ok(());
            }

            if is_op_boundary
                && let Some(asmop) = self.current_asmop.as_ref()
                && matches!(&breakpoint.ty, BreakpointType::Opcode(OperationMatcher::Asm(expected)) if expected.as_str() == asmop.op().as_ref())
            {
                return Ok(());
            }

            // Check if `breakpoint` was triggered at this cycle
            let current_cycle = self.cycle;
            let cycles_stepped = current_cycle - start_cycle;
            if let Some(n) = breakpoint.cycles_to_skip(current_cycle)
                && cycles_stepped > 0
                && n == 0
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
            processor: self.processor,
            outputs: self.stack_outputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly::DefaultSourceManager;
    use miden_mast_package::Package;

    use super::*;
    use crate::exec::Executor;

    #[test]
    fn callstack_tracks_nested_frame_events() {
        use crate::event::{FRAME_END_EVENT, FRAME_START_EVENT};
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(
                "program",
                format!(
                    r#"
proc inner
    emit.event("{FRAME_START_EVENT}")
    nop
    emit.event("{FRAME_END_EVENT}")
end

proc outer
    emit.event("{FRAME_START_EVENT}")
    exec.inner
    emit.event("{FRAME_END_EVENT}")
end

begin
    emit.event("{FRAME_START_EVENT}")
    exec.outer
    emit.event("{FRAME_END_EVENT}")
end
"#
                ),
            )
            .map(Arc::<Package>::from)
            .unwrap();

        let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(program, source_manager);
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
