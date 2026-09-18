use alloc::{sync::Arc, vec::Vec};

use miden_core::{Word, events::EventId, program::Program};
use miden_processor::{
    BaseHost, ExecutionError, ExecutionOptions, ExecutionOutput, FastProcessor, Felt,
    FutureMaybeSend, Host, LoadedMastForest, ProcessorState, StackInputs,
    advice::{AdviceInputs, AdviceMutation},
    event::EventError,
    trace::RowIndex,
};

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
    #[cfg(feature = "std")]
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

    #[cfg(not(feature = "std"))]
    fn report_diagnostics(&self, _err: &ExecutionError) {}

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

    fn resolve_event(
        &self,
        event_id: miden_core::events::EventId,
    ) -> Option<&miden_core::events::EventName> {
        self.inner.resolve_event(event_id)
    }
}

impl<H: Host> Host for DiagnosticHostWrapper<'_, H> {
    fn get_mast_forest(
        &self,
        node_digest: &Word,
    ) -> impl FutureMaybeSend<Option<LoadedMastForest>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.capture_state(process);
        let event_id = EventId::from_felt(process.get_stack_item(0));
        match crate::Event::from(event_id) {
            crate::Event::FrameStart => self.call_depth += 1,
            crate::Event::FrameEnd => self.call_depth = self.call_depth.saturating_sub(1),
            _ => (),
        }
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
            let processor = FastProcessor::new_with_options(
                self.stack_inputs,
                self.advice_inputs,
                self.options,
            )
            .expect("advice inputs should fit advice map limits");

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

#[cfg(all(test, feature = "std"))]
mod tests {
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use miden_assembly::{Assembler, DefaultSourceManager};

    use super::*;
    use crate::exec::DebuggerHost;

    fn ready<F: Future>(future: F) -> F::Output {
        let mut future = core::pin::pin!(future);
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("synchronous test host unexpectedly yielded"),
        }
    }

    #[test]
    fn diagnostics_delegate_events_capture_state_and_preserve_execution_results() {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let source = format!(
            "begin emit.event(\"{}\") emit.event(\"{}\") emit.event(\"{}\") push.7 add end",
            crate::exec::event::FRAME_START_EVENT,
            crate::exec::event::FRAME_END_EVENT,
            crate::exec::event::FRAME_END_EVENT
        );
        let package = Assembler::new(source_manager.clone())
            .assemble_program("diagnostic-test", source.as_str())
            .unwrap();
        let mut host = DebuggerHost::new(source_manager).with_event_advice_mutations_recording();
        for event in [crate::exec::event::FRAME_START_EVENT, crate::exec::event::FRAME_END_EVENT] {
            host.register_event_handler(
                event,
                Arc::new(|_: &ProcessorState<'_>| -> Result<Vec<AdviceMutation>, EventError> {
                    Ok(Vec::new())
                }),
            )
            .unwrap();
        }
        let mut wrapper = DiagnosticHostWrapper::new(&mut host);
        assert!(
            wrapper
                .resolve_event(crate::exec::event::FRAME_START_EVENT.to_event_id())
                .is_some()
        );
        assert!(ready(wrapper.get_mast_forest(&Word::default())).is_none());
        let processor = FastProcessor::new_with_options(
            StackInputs::default(),
            AdviceInputs::default(),
            ExecutionOptions::default(),
        )
        .unwrap();
        let output = ready(processor.execute(&package.unwrap_program(), &mut wrapper)).unwrap();
        assert_eq!(output.stack[0], Felt::from(7u32));
        assert_eq!(wrapper.call_depth, 0);
        assert!(wrapper.last_cycle > RowIndex::from(0u32));
        assert!(!wrapper.last_stack_state.is_empty());
        wrapper.report_diagnostics(&ExecutionError::Internal("test failure"));
        assert_eq!(host.take_recorded_event_mutations().len(), 3);
    }

    #[test]
    fn diagnostic_executor_returns_success_and_reports_failures() {
        for (source, success) in
            [("begin push.9 add end", true), ("begin push.0 assert end", false)]
        {
            let source_manager = Arc::new(DefaultSourceManager::default());
            let package = Assembler::new(source_manager.clone())
                .assemble_program("diagnostic-test", source)
                .unwrap();
            let mut host = miden_processor::DefaultHost::default();
            let result = ready(
                DiagnosticExecutor::new(
                    StackInputs::default(),
                    AdviceInputs::default(),
                    ExecutionOptions::default(),
                )
                .execute_async(&package.unwrap_program(), &mut host),
            );
            assert_eq!(result.is_ok(), success);
            if let Ok(output) = result {
                assert_eq!(output.stack[0], Felt::from(9u32));
            }
        }
    }
}
