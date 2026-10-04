use alloc::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    vec::Vec,
};

use miden_assembly_syntax::{ast::DebugVarLocation, debuginfo::SourceManager};
use miden_core::operations::AssemblyOp;
use miden_processor::{
    ContextId, ExecutionError, FastProcessor, Felt, ResumeContext, StackOutputs,
    operation::Operation, trace::RowIndex,
};

pub(crate) use super::source::{CurrentCycleInfo, extract_current_op};
use super::{DebuggerHost, ExecutionTrace, source::SourceContext};
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
    pub host: DebuggerHost<dyn miden_assembly_syntax::debuginfo::SourceManager>,
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
    /// Felts `current_proc` returns, from debug info; `None` means unknown.
    pub current_return_felts: Option<usize>,
    /// Control-flow kind of this step; ex. `Call`/`Dyn` enter a new procedure.
    pub current_control: Option<ControlFlowOp>,
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

/// Wait for entry declarations in the current straight-line source occurrence, but never run
/// through a branch or a callee just because a later location exists somewhere in the function.
/// Only declarations observed this cycle count; kills and stale caller variables do not.
pub(crate) fn should_wait_for_entry_variables(
    resume_ctx: &ResumeContext,
    procedure: &str,
    variables: &DebugVarTracker,
    cycle: usize,
) -> bool {
    if variables
        .current_variables()
        .any(|variable| variable.clk == RowIndex::from(cycle as u32))
    {
        return false;
    }
    let Some(debug_info) = resume_ctx.debug_info() else {
        return false;
    };
    let next = extract_current_op(resume_ctx);
    let Some(source_node) = next.source_node_id.map(|node| &debug_info[node]) else {
        return false;
    };
    let Some(op_idx) = next.op_idx else {
        return false;
    };
    source_node.debug_vars.iter().any(|variable| {
        variable.op_idx as usize >= op_idx
            && !matches!(variable.value_location, DebugVarLocation::Unavailable)
            && source_node.asm_op_for_operation(variable.op_idx).is_some_and(|operation| {
                debug_info[operation.context_name_idx].as_ref() == procedure
            })
    })
}

impl DebugExecutor {
    /// Returns whether a function breakpoint should wait for pending entry-block declarations.
    pub fn should_wait_for_entry_variables(&self, procedure: &str) -> bool {
        self.resume_ctx.as_ref().is_some_and(|resume_ctx| {
            should_wait_for_entry_variables(resume_ctx, procedure, &self.debug_vars, self.cycle)
        })
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

        let source = SourceContext::new(&resume_ctx);
        let CurrentCycleInfo {
            op,
            control_flow_kind,
            ..
        } = source.cycle;
        let debug_var_infos = source.debug_vars();
        let source_asmop = source.asmop();
        self.callstack.observe_source_context(
            &resume_ctx,
            source.source_operation(),
            source_asmop.as_ref(),
        );

        let debug_var_count = debug_var_infos.len();
        self.debug_vars.observe_execution(
            self.callstack.invocation_ids(),
            self.callstack.current_invocation(),
            &self.processor,
            RowIndex::from((self.cycle + 1) as u32),
            debug_var_infos,
        );

        // Execute one step
        let step_result = if let Some(debug_info) = source.debug_info() {
            self.processor
                .step_with_package_debug_info_sync(&mut self.host, resume_ctx, debug_info)
        } else {
            self.processor.step_sync(&mut self.host, resume_ctx)
        };
        match step_result {
            Ok(Some(new_ctx)) => {
                self.resume_ctx = Some(new_ctx);
                self.cycle += 1;

                self.current_control = control_flow_kind;

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
                self.current_asmop = source_asmop;
                if let Some(asmop) = self.current_asmop.as_ref() {
                    let name = asmop.context_name().clone();
                    // Look up the return width only when the procedure changes, not on every cycle.
                    if self.current_proc.as_deref() != Some(name.as_ref()) {
                        self.current_return_felts = source.debug_info()
                            .map(Arc::as_ref)
                            .and_then(|di| crate::debug::return_felt_count(di, name.as_ref()));
                    }
                    self.current_proc = Some(name);
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
                    inline_frames: source.inline_frames(),
                };
                let exited = self.callstack.next(&step_info);

                self.last_debug_var_count = debug_var_count;

                Ok(exited)
            }
            Ok(None) => {
                // Program completed: no invocation remains available for inspection.
                self.debug_vars.reset();
                self.stopped = true;
                self.last_debug_var_count = 0;
                let state = self.processor.state();
                self.current_stack = state.get_stack_state();

                // Capture the final stack as StackOutputs (truncate to 16 elements)
                let len = self.current_stack.len().min(16);
                self.stack_outputs =
                    StackOutputs::new(&self.current_stack[..len]).expect("invalid stack outputs");

                // Write profiling reports in case its enabled
                #[cfg(feature = "std")]
                {
                    self.profiler.write_reports();
                }
                Ok(None)
            }
            Err(err) => {
                self.stopped = true;
                self.last_debug_var_count = 0;
                Err(err)
            }
        }
    }

    /// Whether a captured invocation has returned and execution has reached a caller operation.
    /// Event-only targets may stop without source information.
    pub fn has_returned(&self, target: &crate::debug::ReturnTarget) -> bool {
        self.callstack.has_returned(target)
            && (!target.is_recovered()
                || !self.callstack.has_recovered_current_frame()
                || (self.current_op.is_some() && self.current_asmop.is_some()))
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
        let return_target =
            (breakpoint.ty == BreakpointType::Finish).then(|| self.callstack.return_target());
        while !self.stopped {
            self.step()?;
            if return_target.as_ref().is_some_and(|target| self.has_returned(target)) {
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
mod tests;
