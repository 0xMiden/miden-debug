use alloc::string::ToString;

use super::*;

/// One entry per `on_event` invocation, in execution order, and the recorded log replays to
/// an identical result without the original event handlers.
#[test]
fn records_event_mutations_and_replays_them() {
    use std::sync::atomic::{AtomicU64, Ordering};

    use miden_assembly::DefaultSourceManager;
    use miden_core::events::EventId;
    use miden_processor::{ProcessorState, advice::AdviceMutation, event::EventError};

    struct CountingHandler {
        calls: AtomicU64,
    }

    impl EventHandler for CountingHandler {
        fn on_event(
            &self,
            _process: &ProcessorState<'_>,
        ) -> Result<Vec<AdviceMutation>, EventError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![AdviceMutation::extend_advice_stack(
                [Felt::from(100u32 + call as u32)].into_iter().collect(),
            )])
        }
    }

    let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
    let event_name = "miden-debug::test::record-replay";
    let event_id = EventId::from_name(event_name).as_u64();
    // Each emit invokes the handler, which pushes one value onto the advice stack;
    // adv_push moves it to the operand stack, and the sum of both values is the result.
    let source = format!(
        "begin push.{event_id} emit drop adv_push push.{event_id} emit drop adv_push add swap \
         drop end"
    );
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", source)
        .map(Arc::from)
        .expect("failed to assemble test program");

    let mut executor = Executor::new(Vec::new());
    executor
        .register_event_handler(
            EventName::from_string(event_name.to_string()),
            Arc::new(CountingHandler {
                calls: AtomicU64::new(0),
            }),
        )
        .expect("failed to register event handler");
    executor.with_event_advice_mutations_recording();

    // Run to completion through the debug executor: recording is an internal detail of its
    // host, and the log is taken from the host once execution finishes.
    let mut debug_executor = executor.into_debug(Arc::clone(&program), source_manager.clone());
    while !debug_executor.stopped {
        debug_executor.step().expect("recording step failed");
    }
    let recorded = debug_executor.host.take_recorded_event_mutations();
    let recorded_result: u32 =
        debug_executor.into_execution_trace().parse_result().expect("invalid result");
    assert_eq!(recorded_result, 201);

    assert_eq!(recorded.len(), 2, "expected one recorded entry per emit");
    for (index, batch) in recorded.iter().enumerate() {
        match batch.as_slice() {
            [AdviceMutation::ExtendStack { stack }] => {
                assert_eq!(
                    stack.iter().copied().collect::<Vec<_>>(),
                    [Felt::from(100u32 + index as u32)],
                );
            }
            _ => panic!("unexpected mutations recorded for event {index}"),
        }
    }

    // Replay the recorded mutations without any event handlers registered: execution must
    // reach the same result, proving the log is sufficient for event replay.
    let replay_executor = Executor::new(Vec::new());
    let mut debug_executor = replay_executor.into_debug_with_replay(
        program,
        source_manager,
        Vec::new(),
        recorded.into(),
    );
    while !debug_executor.stopped {
        debug_executor.step().expect("replay step failed");
    }
    let replayed_result: u32 = debug_executor
        .into_execution_trace()
        .parse_result()
        .expect("invalid replay result");
    assert_eq!(replayed_result, recorded_result);
}

/// A recorded execution serialized into a [ReplaySnapshot](crate::exec::ReplaySnapshot) and
/// read back from bytes replays to the same result — the offline record→replay path, end to
/// end, exactly what `miden-debug --replay <snapshot>` drives.
#[test]
fn replays_from_a_serialized_snapshot() {
    use std::sync::atomic::{AtomicU64, Ordering};

    use miden_assembly::DefaultSourceManager;
    use miden_core::events::EventId;
    use miden_processor::{ProcessorState, advice::AdviceMutation, event::EventError};

    use crate::exec::ReplaySnapshot;

    struct CountingHandler {
        calls: AtomicU64,
    }

    impl EventHandler for CountingHandler {
        fn on_event(
            &self,
            _process: &ProcessorState<'_>,
        ) -> Result<Vec<AdviceMutation>, EventError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![AdviceMutation::extend_advice_stack(
                [Felt::from(100u32 + call as u32)].into_iter().collect(),
            )])
        }
    }

    let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
    let event_name = "miden-debug::test::snapshot-replay";
    let event_id = EventId::from_name(event_name).as_u64();
    let source = format!(
        "begin push.{event_id} emit drop adv_push push.{event_id} emit drop adv_push add add end"
    );
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", source)
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let stack_inputs = StackInputs::new(&[Felt::from(7u32)]).unwrap();
    let advice_inputs = AdviceInputs::default();
    let options = ExecutionOptions::default();

    // Record the event mutations by running to completion with a live handler.
    let mut executor = Executor::from_config(ExecutionConfig {
        inputs: stack_inputs,
        advice_inputs: advice_inputs.clone(),
        options,
    });
    executor
        .register_event_handler(
            EventName::from_string(event_name.to_string()),
            Arc::new(CountingHandler {
                calls: AtomicU64::new(0),
            }),
        )
        .expect("failed to register event handler");
    executor.with_event_advice_mutations_recording();
    let mut debug_executor = executor.into_debug(program.clone(), source_manager.clone());
    while !debug_executor.stopped {
        debug_executor.step().expect("recording step failed");
    }
    let event_log = debug_executor.host.take_recorded_event_mutations();
    let recorded_result: u32 =
        debug_executor.into_execution_trace().parse_result().expect("invalid result");

    // Persist the recording as a snapshot and read it back from its serialized bytes.
    let snapshot = ReplaySnapshot {
        package: program.clone(),
        stack_inputs,
        advice_inputs,
        options,
        mast_forests: vec![LoadedMastForest::with_package_debug_info(
            program.mast_forest().clone(),
            program.debug_info(),
        )],
        event_log,
    };
    let restored = ReplaySnapshot::read_from_bytes(&snapshot.to_bytes())
        .expect("snapshot failed to deserialize");

    // Replay from the deserialized snapshot, with no event handlers registered.
    let replay_executor = Executor::from_config(ExecutionConfig {
        inputs: restored.stack_inputs,
        advice_inputs: restored.advice_inputs,
        options: restored.options,
    });
    let mut debug_executor = replay_executor.into_debug_with_replay(
        restored.package.clone(),
        source_manager,
        restored.mast_forests.clone(),
        restored.event_log.into(),
    );
    while !debug_executor.stopped {
        debug_executor.step().expect("replay step failed");
    }
    let replayed_result: u32 = debug_executor
        .into_execution_trace()
        .parse_result()
        .expect("invalid replay result");
    assert_eq!(replayed_result, recorded_result);
    assert_eq!(replayed_result, 208);
}

#[test]
fn replay_ignores_legacy_frame_events_for_callstack_tracking() {
    use miden_assembly::DefaultSourceManager;

    let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
begin
emit.event("{FRAME_START_EVENT}")
emit.event("{FRAME_START_EVENT}")
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");

    let event_replay = VecDeque::from([Vec::new(), Vec::new()]);
    let mut debug_executor = Executor::new(Vec::new()).into_debug_with_replay(
        program,
        source_manager,
        Vec::new(),
        event_replay,
    );
    while !debug_executor.stopped {
        debug_executor.step().expect("replay step failed");
    }

    assert!(
        debug_executor.callstack.frames().len() <= 1,
        "legacy frame events must not create debugger call frames"
    );
}
