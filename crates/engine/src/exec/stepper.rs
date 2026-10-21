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
/// not. The only thing it adds to the processor's own step is the per-cycle record, which
/// makes it the cheapest way to observe an execution through the engine.
#[derive(Debug)]
pub struct Stepper {
    processor: FastProcessor,
    resume_ctx: Option<ResumeContext>,
    /// Debug info to attribute the first step to when the context was built without it.
    pending_debug_info: Option<Arc<PackageDebugInfo>>,
    tracker: SourceTracker,
    /// Number of VM cycles executed so far.
    cycle: usize,
}

impl Stepper {
    /// Creates a stepper that will run `processor` from `resume_ctx`.
    pub fn new(processor: FastProcessor, resume_ctx: ResumeContext) -> Self {
        Self {
            processor,
            resume_ctx: Some(resume_ctx),
            pending_debug_info: None,
            tracker: SourceTracker::new(),
            cycle: 0,
        }
    }

    /// Creates a stepper over the program of `package`, with its debug info when it has any.
    ///
    /// `package` must be executable.
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
    /// step; a context that already has debug info keeps its own.
    pub fn with_debug_info(mut self, debug_info: Arc<PackageDebugInfo>) -> Self {
        self.pending_debug_info = Some(debug_info);
        self
    }

    /// Number of VM cycles executed so far.
    pub fn cycle(&self) -> usize {
        self.cycle
    }

    /// Whether the program has terminated, successfully or not.
    pub fn stopped(&self) -> bool {
        self.resume_ctx.is_none()
    }

    /// The processor being stepped.
    pub fn processor(&self) -> &FastProcessor {
        &self.processor
    }

    /// Number of source nodes currently open, innermost last.
    pub fn open_source_nodes(&self) -> usize {
        self.tracker.open_nodes()
    }

    /// Executes one step with a synchronous host, describing its cycle to `on_cycle` first.
    ///
    /// Returns whether the program is still running. Returns the same `Ok(false)` once the
    /// program has terminated; an error terminates it. The step that finalizes a finished
    /// program runs no cycle and is not described.
    pub fn step_sync(
        &mut self,
        host: &mut impl SyncHost,
        on_cycle: impl FnOnce(&CycleInfo<'_>),
    ) -> Result<bool, ExecutionError> {
        let Some((ctx, debug_info)) = self.before_step(on_cycle) else {
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
        let Some((ctx, debug_info)) = self.before_step(on_cycle) else {
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

    /// The outputs of the terminated program, as a full execution would have returned them.
    ///
    /// Fails if the program is still running.
    pub fn into_output(self) -> Result<ExecutionOutput, ExecutionError> {
        if self.resume_ctx.is_some() {
            return Err(ExecutionError::Internal("the program has not terminated"));
        }
        // The last step already rejected a final stack deeper than the output width.
        let stack: Vec<_> = self.processor.stack().iter().rev().copied().collect();
        let stack = StackOutputs::new(&stack)
            .map_err(|_| ExecutionError::Internal("the final stack is not a valid output"))?;
        let precompile_witness =
            self.processor.deferred_state().clone().into_witness().map_err(|_| {
                ExecutionError::Internal("failed to export deferred execution witness")
            })?;
        let (advice, memory) = self.processor.into_parts();
        Ok(ExecutionOutput {
            stack,
            advice,
            memory,
            precompile_witness,
        })
    }

    /// Takes the resume context, describes its cycle to `on_cycle` if it has one, and returns
    /// the context with the debug info the step should run with. `None` once the program has
    /// terminated.
    fn before_step(
        &mut self,
        on_cycle: impl FnOnce(&CycleInfo<'_>),
    ) -> Option<(ResumeContext, Option<Arc<PackageDebugInfo>>)> {
        let ctx = self.resume_ctx.take()?;
        // The pending debug info is consumed by the first step whatever the context carries:
        // a later context without debug info belongs to a library and must not inherit it.
        let pending = self.pending_debug_info.take();
        let debug_info = ctx.debug_info().or(pending);
        // After the last cycle the processor hands back a context with nothing left to run;
        // the step that consumes it only finalizes the execution.
        if ctx.continuation_stack().peek_continuation().is_some() {
            let source = SourceContext::with_debug_info(&ctx, debug_info.clone());
            on_cycle(&self.tracker.observe(&ctx, &source));
            self.cycle += 1;
        }
        Some((ctx, debug_info))
    }

    /// Records the result of a step.
    fn after_step(
        &mut self,
        next: Result<Option<ResumeContext>, ExecutionError>,
    ) -> Result<bool, ExecutionError> {
        self.resume_ctx = next?;
        Ok(self.resume_ctx.is_some())
    }
}
