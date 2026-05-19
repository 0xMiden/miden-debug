use std::{sync::Arc, vec::Vec};

use miden_core::{Word, operations::DebugOptions, program::Program};
use miden_processor::{
    BaseHost, ExecutionError, ExecutionOptions, ExecutionOutput, FastProcessor, Felt,
    FutureMaybeSend, Host, ProcessorState, StackInputs, TraceError,
    advice::{AdviceInputs, AdviceMutation},
    event::EventError,
    mast::MastForest,
    trace::RowIndex,
};

use super::TraceEvent;

// DIAGNOSTIC HOST WRAPPER
// ================================================================================================

/// A host wrapper that intercepts trace events to track call frames and processor state,
/// while delegating all other operations to the inner host.
///
/// This enables capturing diagnostic information during transaction execution (or any program
/// execution) without modifying the inner host.
struct DiagnosticHostWrapper<'a, H: Host> {
    inner: &'a mut H,
    /// Call depth tracked from FrameStart/FrameEnd trace events.
    call_depth: usize,
    /// Stack state captured at the last trace or event callback.
    last_stack_state: Vec<Felt>,
    /// Clock cycle at the last trace or event callback.
    last_cycle: RowIndex,
}

impl<'a, H: Host> DiagnosticHostWrapper<'a, H> {
    fn new(inner: &'a mut H) -> Self {
        Self {
            inner,
            call_depth: 0,
            last_stack_state: Vec::new(),
            last_cycle: RowIndex::from(0u32),
        }
    }

    /// Report diagnostic information when an execution error occurs.
    fn report_diagnostics(&self, err: &ExecutionError) {
        eprintln!("\n=== Transaction Execution Failed ===");
        eprintln!("Error: {err}");
        eprintln!("Last known cycle: {}", self.last_cycle);
        eprintln!("Call depth at failure: {}", self.call_depth);

        if !self.last_stack_state.is_empty() {
            let stack_display: Vec<_> =
                self.last_stack_state.iter().take(16).map(|f| f.as_canonical_u64()).collect();
            eprintln!("Last known stack state (top 16): {stack_display:?}");
        }

        eprintln!("====================================\n");
    }

    fn capture_state(&mut self, process: &ProcessorState<'_>) {
        self.last_stack_state = process.get_stack_state();
        self.last_cycle = process.clock();
    }
}

impl<H: Host> BaseHost for DiagnosticHostWrapper<'_, H> {
    fn get_label_and_source_file(
        &self,
        location: &miden_debug_types::Location,
    ) -> (miden_debug_types::SourceSpan, Option<Arc<miden_debug_types::SourceFile>>) {
        self.inner.get_label_and_source_file(location)
    }

    fn on_debug(
        &mut self,
        process: &ProcessorState<'_>,
        options: &DebugOptions,
    ) -> Result<(), miden_processor::DebugError> {
        self.inner.on_debug(process, options)
    }

    fn on_trace(&mut self, process: &ProcessorState<'_>, trace_id: u32) -> Result<(), TraceError> {
        self.capture_state(process);

        let event = TraceEvent::from(trace_id);
        match event {
            TraceEvent::FrameStart => self.call_depth += 1,
            TraceEvent::FrameEnd => self.call_depth = self.call_depth.saturating_sub(1),
            _ => {}
        }

        self.inner.on_trace(process, trace_id)
    }

    fn resolve_event(
        &self,
        event_id: miden_core::events::EventId,
    ) -> Option<&miden_core::events::EventName> {
        self.inner.resolve_event(event_id)
    }
}

impl<H: Host> Host for DiagnosticHostWrapper<'_, H> {
    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.capture_state(process);
        self.inner.on_event(process)
    }
}

// DIAGNOSTIC EXECUTOR
// ================================================================================================

/// A [`ProgramExecutor`] that wraps [`FastProcessor`] with diagnostic capabilities.
///
/// When execution fails, it captures and reports rich diagnostic information including:
/// - The clock cycle at failure
/// - The call depth (from trace events)
/// - The last known operand stack state
///
/// This executor is intended for use with [`TransactionExecutor`] to provide better error
/// diagnostics when transactions fail during testing or development.
///
/// # Usage
///
/// ```ignore
/// use miden_tx::TransactionExecutor;
/// use miden_debug::DiagnosticExecutor;
///
/// let executor = TransactionExecutor::new(&store)
///     .with_program_executor::<DiagnosticExecutor>()
///     .execute_transaction(account_id, block_num, notes, tx_args)
///     .await;
/// ```
pub struct DiagnosticExecutor {
    stack_inputs: StackInputs,
    advice_inputs: AdviceInputs,
    options: ExecutionOptions,
}

impl DiagnosticExecutor {
    pub fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        DiagnosticExecutor {
            stack_inputs,
            advice_inputs,
            options,
        }
    }

    pub fn execute_async<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            // Enable debugging and tracing for richer diagnostics.
            let options = self.options.with_debugging(true).with_tracing(true);
            let processor = FastProcessor::new(self.stack_inputs)
                .with_advice(self.advice_inputs)
                .expect("advice inputs should fit advice map limits")
                .with_options(options)
                .expect("execution options should be compatible with advice inputs");

            let mut wrapper = DiagnosticHostWrapper::new(host);

            match processor.execute(program, &mut wrapper).await {
                Ok(output) => Ok(output),
                Err(err) => {
                    wrapper.report_diagnostics(&err);
                    Err(err)
                }
            }
        }
    }
}
