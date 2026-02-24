use std::collections::{BTreeSet, VecDeque};

use miden_core::{
    mast::{MastNode, MastNodeId},
    operations::AssemblyOp,
};
use miden_processor::{
    ContextId, ExecutionError, FastProcessor, Felt, ResumeContext, StackOutputs,
    continuation_stack::Continuation, operation::Operation, trace::RowIndex,
};

use super::{DebuggerHost, ExecutionTrace};
use crate::debug::{CallFrame, CallStack, StepInfo};

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
    /// A sliding window of the last 5 operations successfully executed by the VM
    pub recent: VecDeque<Operation>,
    /// The current clock cycle
    pub cycle: usize,
    /// Whether or not execution has terminated
    pub stopped: bool,
}

/// Extract the current operation and assembly info from the continuation stack
/// before a step is executed. This lets us know what operation will run next.
fn extract_current_op(
    ctx: &ResumeContext,
) -> (Option<Operation>, Option<MastNodeId>, Option<usize>) {
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
                    return (op, Some(*node_id), Some(global_idx));
                }
            }
            Continuation::StartNode(node_id) => {
                return (None, Some(*node_id), None);
            }
            Continuation::FinishBasicBlock(_) => {
                return (Some(Operation::End), None, None);
            }
            other if other.increments_clk() => {
                return (None, None, None);
            }
            _ => continue,
        }
    }
    (None, None, None)
}

impl DebugExecutor {
    /// Advance the program state by one cycle.
    ///
    /// If the program has already reached its termination state, it returns the same result
    /// as the previous time it was called.
    ///
    /// Returns the call frame exited this cycle, if any
    pub fn step(&mut self) -> Result<Option<CallFrame>, ExecutionError> {
        if self.stopped {
            return Ok(None);
        }

        let resume_ctx = match self.resume_ctx.take() {
            Some(ctx) => ctx,
            None => {
                self.stopped = true;
                return Ok(None);
            }
        };

        // Before step: peek continuation to determine what will execute
        let (op, node_id, op_idx) = extract_current_op(&resume_ctx);
        let asmop = node_id
            .and_then(|nid| resume_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());

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

                if let Some(op) = op {
                    if self.recent.len() == 5 {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(op);
                }

                // Update call stack
                let step_info = StepInfo {
                    op,
                    asmop: self.current_asmop.as_ref(),
                    clk: RowIndex::from(self.cycle as u32),
                    ctx: self.current_context,
                };
                let exited = self.callstack.next(&step_info);

                Ok(exited)
            }
            Ok(None) => {
                // Program completed
                self.stopped = true;
                let state = self.processor.state();
                self.current_stack = state.get_stack_state();
                Ok(None)
            }
            Err(err) => {
                self.stopped = true;
                Err(err)
            }
        }
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
