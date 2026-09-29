use std::{fs, path::PathBuf};

use miden_assembly::DefaultSourceManager;
use miden_core::{
    Felt,
    events::{EventId, EventName},
};
use miden_debug_types::{ByteIndex, Location, SourceManagerExt, Uri};
use miden_processor::event::EventHandler;

use super::*;
use crate::exec::{DebuggerHost, EventMutationRecorder};

struct PushSeven;

#[test]
fn function_breakpoint_waits_for_entry_variables() {
    use miden_assembly_syntax::{
        Parse,
        ast::{Instruction, Op},
        debuginfo::{SourceSpan, Span},
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut module = Parse::parse(
        "proc entrypoint push.2 push.3 add nop drop push.1 if.true push.1 drop end end begin \
         exec.entrypoint end",
        false,
        source_manager.clone(),
    )
    .unwrap();
    for procedure in module.procedures_mut() {
        for operation in procedure.body_mut().iter_mut() {
            if let Op::Inst(instruction) = operation
                && matches!(instruction.inner(), Instruction::Nop)
            {
                *instruction = Span::new(
                    SourceSpan::default(),
                    Instruction::DebugVar(DebugVarInfo::new("n", DebugVarLocation::Stack(0))),
                );
            }
        }
    }
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", module)
        .unwrap();
    let mut host = DebuggerHost::new(source_manager);
    let mut wrapper = DapHostWrapper::new(&mut host, None, None);
    let mut processor = FastProcessor::new_with_options(
        StackInputs::new(&[]).unwrap(),
        AdviceInputs::default(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut resume_ctx =
        Some(processor.get_initial_resume_context_for_package(Arc::from(package)).unwrap());
    let mut cycle = 0;
    let mut current_asmop = None;
    let mut debug_state = DapDebugVarState::new();
    let function = [StoredFunctionBreakpoint {
        procedure_pattern: procedure_pattern("entrypoint").unwrap(),
        source_pattern: crate::glob::Glob::new("entrypoint").unwrap().compile_matcher(),
    }];
    let result = step_until_breakpoint(
        &mut processor,
        &mut wrapper,
        &mut resume_ctx,
        &mut cycle,
        &mut current_asmop,
        &ContinueBreakpoints {
            source: &[],
            function: &function,
            source_path_prefixes: &[],
        },
        &mut debug_state,
    );
    assert!(matches!(result, StepResult::Breakpoint(_)));
    let variable = debug_state.debug_vars.get_variable("n").expect("entry variable");
    assert_eq!(variable.info.value_location(), &DebugVarLocation::Const(Felt::from(5u32)));
}

impl EventHandler for PushSeven {
    fn on_event(&self, _process: &ProcessorState<'_>) -> Result<Vec<AdviceMutation>, EventError> {
        Ok(vec![AdviceMutation::extend_advice_stack(
            [Felt::from(7u32)].into_iter().collect(),
        )])
    }
}

/// The DAP host wrapper is what a client-provided host (e.g. a transaction executor host)
/// is driven through; recording must capture the mutations its event handlers produce.
#[test]
fn dap_host_wrapper_records_event_mutations() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let event_name = "miden-debug::test::dap-record";
    let event_id = EventId::from_name(event_name).as_u64();
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", format!("begin push.{event_id} emit drop adv_push drop end"))
        .map(Arc::from)
        .expect("failed to assemble test program");

    let mut host = DebuggerHost::new(source_manager);
    host.register_event_handler(
        EventName::from_string(event_name.to_string()),
        Arc::new(PushSeven),
    )
    .expect("failed to register event handler");

    let recorder = EventMutationRecorder::new();
    let mut wrapper = DapHostWrapper::new(&mut host, Some(recorder.clone()), None);

    let mut processor = FastProcessor::new_with_options(
        StackInputs::new(&[]).unwrap(),
        AdviceInputs::default(),
        ExecutionOptions::default(),
    )
    .expect("invalid inputs");
    let mut resume_ctx = Some(
        processor
            .get_initial_resume_context_for_package(program)
            .expect("invalid program"),
    );
    while let Some(ctx) = resume_ctx.take() {
        let debug_info = ctx.debug_info();
        let step_result = if let Some(debug_info) = debug_info.as_deref() {
            poll_immediately(processor.step_with_package_debug_info(&mut wrapper, ctx, debug_info))
        } else {
            poll_immediately(processor.step(&mut wrapper, ctx))
        };
        match step_result.expect("execution failed") {
            Some(next) => resume_ctx = Some(next),
            None => break,
        }
    }

    assert_eq!(recorder.len(), 1, "expected one recorded entry for the emitted event");
    let batches = recorder.take();
    match batches[0].as_slice() {
        [AdviceMutation::ExtendStack { stack }] => {
            assert_eq!(stack.iter().copied().collect::<Vec<_>>(), [Felt::from(7u32)]);
        }
        _ => panic!("unexpected mutations recorded"),
    }
    assert!(recorder.is_empty(), "take() should leave the recorder empty");
}

/// The executor is constructed internally (e.g. by a transaction executor) and consumed by
/// execution, so embedders read the recording through the handle obtained from the global
/// [DapConfig] before execution: both must share one log.
#[test]
fn dap_config_recorder_is_shared_with_the_executor() {
    let mut config = DapConfig::new("127.0.0.1:0");
    let config_handle = config.record_event_mutations();
    DapConfig::set_global(config);

    let mut executor = DapExecutor::new(
        StackInputs::new(&[]).unwrap(),
        AdviceInputs::default(),
        ExecutionOptions::default(),
    );
    executor.record_event_mutations().record(vec![]);

    assert_eq!(
        config_handle.len(),
        1,
        "recording through the executor must be visible through the config handle"
    );
}

#[test]
fn source_paths_match_only_uses_declared_trim_prefixes() {
    let trim_prefixes = vec!["/workspace/compiler/examples/fibonacci".into()];

    assert!(source_paths_match(
        "/workspace/compiler/examples/fibonacci/src/lib.rs",
        "src/lib.rs",
        &trim_prefixes,
    ));
    assert!(source_paths_match(
        "file:///workspace/compiler/examples/fibonacci/src/lib.rs",
        "/workspace/compiler/examples/fibonacci/src/lib.rs",
        &[],
    ));
    assert!(source_paths_match(
        "file:///C:/workspace/compiler/examples/fibonacci/src/lib.rs",
        "C:/workspace/compiler/examples/fibonacci/src/lib.rs",
        &[],
    ));
    assert!(source_paths_match(
        "file:///C:/workspace/compiler/examples/fibonacci/src/lib.rs",
        "c:/workspace/compiler/examples/fibonacci/src/lib.rs",
        &[],
    ));
    assert!(source_paths_match(
        "file://localhost/C:/workspace/compiler/examples/fibonacci/src/lib.rs",
        "c:/workspace/compiler/examples/fibonacci/src/lib.rs",
        &[],
    ));
    assert!(source_paths_match(
        "file:///C:/workspace/compiler/examples/fibonacci/src/lib.rs",
        "src/lib.rs",
        &["c:/workspace/compiler/examples/fibonacci".into()],
    ));
    assert!(!source_paths_match(
        "/workspace/compiler/examples/fibonacci/src/lib.rs",
        "src/lib.rs",
        &[],
    ));
    assert!(!source_paths_match(
        "/workspace/compiler/examples/fibonacci/src/lib.rs",
        "other/src/lib.rs",
        &trim_prefixes,
    ));
}

#[test]
fn resolve_breakpoint_line_moves_to_next_executable_line() {
    let lines = BTreeSet::from([39, 40, 41]);

    assert_eq!(resolve_breakpoint_line(&lines, 38), Some(39));
    assert_eq!(resolve_breakpoint_line(&lines, 40), Some(40));
    assert_eq!(resolve_breakpoint_line(&lines, 99), Some(41));
    assert_eq!(resolve_breakpoint_line(&BTreeSet::new(), 38), None);
}

#[test]
fn source_var_is_visible_after_its_declaration_line() {
    let prefixes = Vec::new();
    let path = "/tmp/src/lib.rs";

    assert!(!source_var_location_is_visible(path, 40, path, 39, &prefixes));
    assert!(!source_var_location_is_visible(path, 40, path, 40, &prefixes));
    assert!(source_var_location_is_visible(path, 40, path, 41, &prefixes));
}

#[test]
fn next_source_line_ignores_pre_body_mappings() {
    let prefixes = Vec::new();
    let start = ("/tmp/src/lib.rs".to_string(), 39);
    let pre_body = ("/tmp/src/lib.rs".to_string(), 1);
    let body = ("/tmp/src/lib.rs".to_string(), 40);

    assert!(!is_next_source_line(
        Some("entrypoint"),
        Some(&start),
        Some("entrypoint"),
        Some(&pre_body),
        &prefixes,
        Some(39),
    ));
    assert!(is_next_source_line(
        Some("entrypoint"),
        Some(&start),
        Some("entrypoint"),
        Some(&body),
        &prefixes,
        Some(39),
    ));
}

#[test]
fn dap_presents_inline_frames_in_innermost_first_order() {
    let frames = vec![DapCallFrame {
        name: "crate::physical".into(),
        source_path: Some("src/lib.rs".into()),
        line: 30,
        column: 5,
        inline_frames: vec![
            DapInlineFrame {
                name: "crate::inner".into(),
                source_path: Some("src/lib.rs".into()),
                line: 20,
                column: 3,
            },
            DapInlineFrame {
                name: "crate::outer".into(),
                source_path: Some("src/lib.rs".into()),
                line: 10,
                column: 1,
            },
        ],
    }];

    let presented = present_recorded_frames(&frames);

    assert_eq!(presented.len(), 3);
    assert_eq!(presented[0].name.as_ref(), "[inlined] crate::inner");
    assert_eq!((presented[0].line, presented[0].column), (30, 5));
    assert_eq!(presented[1].name.as_ref(), "[inlined] crate::outer");
    assert_eq!((presented[1].line, presented[1].column), (20, 3));
    assert_eq!(presented[2].name.as_ref(), "crate::physical");
    assert_eq!((presented[2].line, presented[2].column), (10, 1));
}

#[test]
fn dap_retains_unresolved_inline_call_sites_without_shifting_frames() {
    let path = PathBuf::from("target")
        .join("dap-source-tests")
        .join(format!("unresolved-inline-{}", std::process::id()))
        .join("source.masm");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "physical\nouter call\n").unwrap();

    let source_manager = Arc::new(DefaultSourceManager::default());
    let source_file = source_manager.load_file(&path).expect("source should load");
    let uri = source_file.uri().clone();
    let mut host = DebuggerHost::new(source_manager);
    let mut wrapper = DapHostWrapper::new(&mut host, None, None);
    let physical = AssemblyOp::new(
        Some(Location::new(uri.clone(), ByteIndex::new(0), ByteIndex::new(8))),
        "crate::physical".to_string(),
        1,
        "add".to_string(),
    );
    let inline_frames = vec![
        crate::debug::InlineCallFrame::new_for_test(
            "crate::inner",
            Location::new(Uri::new("memory://missing"), ByteIndex::new(0), ByteIndex::new(1)),
        ),
        crate::debug::InlineCallFrame::new_for_test(
            "crate::outer",
            Location::new(uri, ByteIndex::new(9), ByteIndex::new(19)),
        ),
    ];

    update_top_frame_with_debug(&mut wrapper, Some(&physical), &inline_frames);

    assert_eq!(wrapper.frames[0].inline_frames.len(), 2);
    let presented = present_recorded_frames(&wrapper.frames);
    assert_eq!(presented.len(), 3);
    assert_eq!(presented[0].name.as_ref(), "[inlined] crate::inner");
    assert!(presented[0].source_path.is_some());
    assert_eq!(presented[1].name.as_ref(), "[inlined] crate::outer");
    assert_eq!(presented[1].source_path, None);
    assert_eq!((presented[1].line, presented[1].column), (0, 0));
    assert_eq!(presented[2].name.as_ref(), "crate::physical");
    assert!(presented[2].source_path.is_some());

    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn dap_propagates_asmop_columns_through_frame_presentation() {
    let path = PathBuf::from("target")
        .join("dap-source-tests")
        .join(format!("asmop-column-{}", std::process::id()))
        .join("source.masm");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "zero\n  add\n    mul\n").unwrap();

    let source_manager = Arc::new(DefaultSourceManager::default());
    let source_file = source_manager.load_file(&path).expect("source should load");
    let uri = source_file.uri().clone();
    let mut host = DebuggerHost::new(source_manager);
    let mut wrapper = DapHostWrapper::new(&mut host, None, None);
    let add = AssemblyOp::new(
        Some(Location::new(uri.clone(), ByteIndex::new(7), ByteIndex::new(10))),
        "crate::physical".to_string(),
        1,
        "add".to_string(),
    );
    let mul = AssemblyOp::new(
        Some(Location::new(uri, ByteIndex::new(15), ByteIndex::new(18))),
        "crate::physical".to_string(),
        1,
        "mul".to_string(),
    );

    let fallback = presented_frames(&wrapper, Some(&add), 0);
    assert_eq!((fallback[0].line, fallback[0].column), (2, 3));

    update_top_frame(&mut wrapper, Some(&add));
    assert_eq!((wrapper.frames[0].line, wrapper.frames[0].column), (2, 3));

    update_top_frame(&mut wrapper, Some(&mul));
    assert_eq!((wrapper.frames[0].line, wrapper.frames[0].column), (3, 5));

    wrapper.frames[0].inline_frames.push(DapInlineFrame {
        name: "crate::inline".into(),
        source_path: None,
        line: 0,
        column: 0,
    });
    let presented = present_recorded_frames(&wrapper.frames);
    assert_eq!(presented[0].name.as_ref(), "[inlined] crate::inline");
    assert_eq!((presented[0].line, presented[0].column), (3, 5));

    fs::remove_dir_all(path.parent().unwrap()).ok();
}
