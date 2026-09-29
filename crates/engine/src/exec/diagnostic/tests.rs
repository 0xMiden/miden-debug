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
    for (source, success) in [("begin push.9 add end", true), ("begin push.0 assert end", false)] {
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
