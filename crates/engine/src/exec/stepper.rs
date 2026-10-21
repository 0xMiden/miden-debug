//! Stepping a program to its end over any host, one described cycle at a time.

use alloc::{sync::Arc, vec::Vec};

use miden_core::program::StackOutputs;
use miden_mast_package::{Package, debug_info::PackageDebugInfo};
use miden_processor::{
    ExecutionError, ExecutionOutput, FastProcessor, Host, ResumeContext, SyncHost,
};

use super::{
    cycle::{CycleInfo, SourceTracker},
    source::SourceContext,
};

/// Steps a processor one cycle at a time and describes each cycle before it runs.
///
/// Unlike [`DebugExecutor`](crate::DebugExecutor) it owns no host and keeps no state for
/// inspection: the host is passed to every step, which lets a caller that only borrows one,
/// such as a [`ProgramExecutor`](miden_processor::ProgramExecutor), drive it, synchronously or
/// not. Besides the processor's own step it only resolves the cycle to source and tracks the
/// open source nodes, which makes it the cheap way to observe an execution through the engine.
#[derive(Debug)]
pub struct Stepper {
    /// The processor being stepped.
    processor: FastProcessor,
    /// Where the program is in its life.
    state: State,
    /// Debug info to attribute the first step to when the context was built without it.
    pending_debug_info: Option<Arc<PackageDebugInfo>>,
    /// The open source nodes, so a start can name its parent.
    tracker: SourceTracker,
}

/// Where the stepper is in the program's life.
///
/// Completion is tracked explicitly rather than inferred from a missing context, so a step
/// that was cancelled while in flight cannot pass for a finished program.
#[derive(Debug)]
enum State {
    /// The context the next step consumes.
    Ready(ResumeContext),
    /// A step took the context and has not returned; stays if that step's future was dropped
    /// or if it unwound through the callback, the source resolution, or the host.
    Stepping,
    /// The program terminated successfully.
    Finished,
    /// A step failed; the program is stopped and has no output.
    Failed,
}

impl Stepper {
    /// Creates a stepper that will run `processor` from `resume_ctx`.
    ///
    /// `resume_ctx` must be the initial context of the execution, as the processor's
    /// `get_initial_resume_context*` methods return it: the open-node tracking that gives a
    /// node start its parent assumes it sees the execution from its first cycle.
    pub fn new(processor: FastProcessor, resume_ctx: ResumeContext) -> Self {
        Self {
            processor,
            state: State::Ready(resume_ctx),
            pending_debug_info: None,
            tracker: SourceTracker::new(),
        }
    }

    /// Creates a stepper over the program of `package`, with its debug info when it has any,
    /// starting at the entry-point source node the package names.
    ///
    /// Fails when the package's advice map conflicts with the processor's or its debug info
    /// does not match its program. Panics when `package` is not executable.
    pub fn for_package(
        mut processor: FastProcessor,
        package: Arc<Package>,
    ) -> Result<Self, ExecutionError> {
        let resume_ctx = processor.get_initial_resume_context_for_package(package)?;
        Ok(Self::new(processor, resume_ctx))
    }

    /// Attributes the execution to `debug_info` when the resume context carries none, which
    /// is the case for a context built from a bare program.
    ///
    /// The first cycle is resolved against it and the processor installs it on the first
    /// step; a context that already has debug info keeps its own, and so does every later
    /// context, such as one of a library without debug info. Source attribution then starts
    /// from the unique source root of the entry point, and the first step fails when that
    /// root is ambiguous: the step API has no way to start from a given entry-point source
    /// node, only [`Self::for_package`] honors the one a package names.
    ///
    /// Panics when a step has already run.
    pub fn with_debug_info(mut self, debug_info: Arc<PackageDebugInfo>) -> Self {
        assert!(
            matches!(self.state, State::Ready(_)) && self.cycle() == 0,
            "debug info is attributed from the first step"
        );
        self.pending_debug_info = Some(debug_info);
        self
    }

    /// Number of VM cycles completed so far, the processor's clock. A cycle whose operation
    /// fails is not counted; one that completes and then trips the cycle or continuation-stack
    /// limit is.
    pub fn cycle(&self) -> usize {
        u32::from(self.processor.state().clock()) as usize
    }

    /// Whether the program has terminated, successfully or not.
    pub fn stopped(&self) -> bool {
        matches!(self.state, State::Finished | State::Failed)
    }

    /// Whether a step failed. A failed program is stopped and has no output.
    pub fn failed(&self) -> bool {
        matches!(self.state, State::Failed)
    }

    /// The processor being stepped.
    pub fn processor(&self) -> &FastProcessor {
        &self.processor
    }

    /// Number of source nodes currently open.
    pub fn open_source_nodes(&self) -> usize {
        self.tracker.open_nodes()
    }

    /// Executes one step with a synchronous host, describing its cycle to `on_cycle` first.
    ///
    /// Returns whether the program is still running. Returns the same `Ok(false)` once the
    /// program has finished. A failing step returns its error and stops the program; stepping
    /// again after that, or after a step that did not complete (its future dropped, or a
    /// panic unwound through it), is an error. The step that finalizes a finished program
    /// runs no cycle and is not described.
    pub fn step_sync(
        &mut self,
        host: &mut impl SyncHost,
        on_cycle: impl FnOnce(&CycleInfo<'_>),
    ) -> Result<bool, ExecutionError> {
        let Some(Step { ctx, debug_info }) = self.before_step(on_cycle)? else {
            return Ok(false);
        };
        let next = match debug_info {
            Some(debug_info) => {
                self.processor.step_with_package_debug_info_sync(host, ctx, &debug_info)
            }
            None => self.processor.step_sync(host, ctx),
        };
        self.after_step(next)
    }

    /// Executes one step with an asynchronous host, describing its cycle to `on_cycle` first.
    ///
    /// Returns whether the program is still running, like [`Self::step_sync`].
    pub async fn step(
        &mut self,
        host: &mut impl Host,
        on_cycle: impl FnOnce(&CycleInfo<'_>),
    ) -> Result<bool, ExecutionError> {
        let Some(Step { ctx, debug_info }) = self.before_step(on_cycle)? else {
            return Ok(false);
        };
        let next = match debug_info {
            Some(debug_info) => {
                self.processor.step_with_package_debug_info(host, ctx, &debug_info).await
            }
            None => self.processor.step(host, ctx).await,
        };
        self.after_step(next)
    }

    /// Steps with a synchronous host until the program terminates.
    pub fn run_sync(
        &mut self,
        host: &mut impl SyncHost,
        mut on_cycle: impl FnMut(&CycleInfo<'_>),
    ) -> Result<(), ExecutionError> {
        while self.step_sync(host, &mut on_cycle)? {}
        Ok(())
    }

    /// Steps with an asynchronous host until the program terminates.
    pub async fn run(
        &mut self,
        host: &mut impl Host,
        mut on_cycle: impl FnMut(&CycleInfo<'_>),
    ) -> Result<(), ExecutionError> {
        while self.step(host, &mut on_cycle).await? {}
        Ok(())
    }

    /// The outputs of the finished program, as a full execution would have returned them.
    ///
    /// Fails unless the program ran to its end: while it is running, after a failed step, and
    /// after a step that did not complete.
    pub fn into_output(self) -> Result<ExecutionOutput, ExecutionError> {
        match self.state {
            State::Finished => {}
            State::Ready(_) | State::Stepping => {
                return Err(ExecutionError::Internal("the program has not terminated"));
            }
            State::Failed => {
                return Err(ExecutionError::Internal("the program failed and has no output"));
            }
        }
        // The processor keeps the stack bottom first and the outputs want the top first. The
        // last step already rejected a final stack deeper than the output width.
        let stack: Vec<_> = self.processor.stack().iter().rev().copied().collect();
        let stack = StackOutputs::new(&stack)
            .map_err(|_| ExecutionError::Internal("the final stack is not a valid output"))?;
        execution_output(self.processor, stack)
    }

    /// Takes the resume context, describes its cycle to `on_cycle` if it has one, and returns
    /// what the step needs. `None` once the program has finished.
    fn before_step(
        &mut self,
        on_cycle: impl FnOnce(&CycleInfo<'_>),
    ) -> Result<Option<Step>, ExecutionError> {
        let ctx = match core::mem::replace(&mut self.state, State::Stepping) {
            State::Ready(ctx) => ctx,
            State::Finished => {
                self.state = State::Finished;
                return Ok(None);
            }
            State::Failed => {
                self.state = State::Failed;
                return Err(ExecutionError::Internal("the program failed; it cannot be stepped"));
            }
            State::Stepping => {
                return Err(ExecutionError::Internal(
                    "a step did not complete; the execution is lost",
                ));
            }
        };
        // The pending debug info is consumed by the first step whatever the context carries:
        // a later context without debug info belongs to a library and must not inherit it.
        let pending = self.pending_debug_info.take();
        let debug_info = ctx.debug_info().or(pending);
        let source = SourceContext::with_debug_info(&ctx, debug_info.clone());
        if let Some(cycle) = self.tracker.observe(&ctx, &source) {
            on_cycle(&cycle);
        }
        Ok(Some(Step { ctx, debug_info }))
    }

    /// Records the result of a step.
    fn after_step(
        &mut self,
        next: Result<Option<ResumeContext>, ExecutionError>,
    ) -> Result<bool, ExecutionError> {
        match next {
            Ok(next) => {
                let running = next.is_some();
                self.state = next.map_or(State::Finished, State::Ready);
                Ok(running)
            }
            Err(error) => {
                self.state = State::Failed;
                Err(error)
            }
        }
    }
}

/// What one step runs: the context it consumes and the debug info to run it with.
struct Step {
    ctx: ResumeContext,
    debug_info: Option<Arc<PackageDebugInfo>>,
}

/// Packages a processor that ran to its end into the output a full execution returns, with
/// `stack` as the outputs.
///
/// `into_parts` gives the advice provider and the memory but drops the deferred state, so the
/// precompile witness is exported from a clone of it first.
pub(crate) fn execution_output(
    processor: FastProcessor,
    stack: StackOutputs,
) -> Result<ExecutionOutput, ExecutionError> {
    let precompile_witness = processor
        .deferred_state()
        .clone()
        .into_witness()
        .map_err(|_| ExecutionError::Internal("failed to export deferred execution witness"))?;
    let (advice, memory) = processor.into_parts();
    Ok(ExecutionOutput {
        stack,
        advice,
        memory,
        precompile_witness,
    })
}
