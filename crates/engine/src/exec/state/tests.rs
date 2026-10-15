use alloc::string::ToString;
use std::sync::Arc;

use miden_assembly::DefaultSourceManager;
use miden_mast_package::Package;

use super::*;
use crate::exec::Executor;

#[test]
fn callstack_tracks_nested_frames_without_events() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            r#"
proc inner
nop
end

proc outer
exec.inner
push.1 drop
end

begin
exec.outer
push.2 drop
end
"#,
        )
        .map(Arc::<Package>::from)
        .unwrap();

    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(program, source_manager);
    let mut max_depth = 0;
    let mut saw_inner = false;
    let mut saw_outer_after_inner = false;
    let mut saw_main_after_outer = false;
    let mut snapshots = Vec::new();

    for _ in 0..128 {
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
        saw_outer_after_inner |= saw_inner
            && frames.len() == 2
            && frames
                .last()
                .and_then(|frame| frame.procedure(""))
                .is_some_and(|name| name.contains("outer"));
        saw_main_after_outer |= saw_outer_after_inner && frames.len() == 1;

        if saw_main_after_outer || executor.stopped {
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
    assert!(
        saw_outer_after_inner,
        "expected inner frame to expire back to outer; snapshots: {snapshots:?}"
    );
    assert!(
        saw_main_after_outer,
        "expected outer frame to expire back to main; snapshots: {snapshots:?}"
    );
}

#[test]
fn inline_context_can_enter_and_clear_inside_an_exec_callee() {
    use miden_assembly_syntax::{
        Parse,
        ast::{DebugInlineCallInfo, Instruction, Op},
        debuginfo::Span,
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut module = Parse::parse(
        "proc inner push.7 drop nop push.6 drop nop push.5 drop end proc outer exec.inner push.8 \
         drop end begin exec.outer push.9 drop end",
        false,
        source_manager.clone(),
    )
    .unwrap();
    let mut markers = [Some("source::inline"), None].into_iter();
    for procedure in module.procedures_mut() {
        for operation in procedure.body_mut().iter_mut() {
            let Op::Inst(instruction) = operation else {
                continue;
            };
            if !matches!(instruction.inner(), Instruction::Nop) {
                continue;
            }
            let span = instruction.span();
            let instruction = match markers.next().unwrap() {
                Some(name) => {
                    let location = source_manager.file_line_col(span).unwrap();
                    Instruction::DebugInlineCall(DebugInlineCallInfo::new(
                        name,
                        location.clone(),
                        location,
                    ))
                }
                None => Instruction::DebugInlineCallClear,
            };
            *operation = Op::Inst(Span::new(span, instruction));
        }
    }
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("inline-phases", module)
        .map(Arc::from)
        .unwrap();
    let mut executor = Executor::new(Vec::new()).into_debug(package, source_manager);
    let mut phases = Vec::new();
    while !executor.stopped {
        executor.step().unwrap();
        let Some(operation) = executor.current_asmop.as_ref() else {
            continue;
        };
        let operation = operation.op();
        let expected = match operation.as_ref() {
            "push.7" | "push.5" => false,
            "push.6" => true,
            _ => continue,
        };
        let frames = executor.callstack.logical_frames("");
        assert_eq!(frames.iter().any(|frame| frame.name() == "source::inline"), expected);
        for name in ["::inner", "::outer"] {
            assert_eq!(frames.iter().filter(|frame| frame.name().ends_with(name)).count(), 1);
        }
        phases.push(operation.to_string());
    }
    for phase in ["push.7", "push.6", "push.5"] {
        assert!(phases.iter().any(|operation| operation == phase), "{phases:?}");
    }
}

#[test]
fn source_inlining_with_the_physical_procedure_name_is_preserved() {
    use miden_assembly_syntax::{
        Parse,
        ast::{DebugInlineCallInfo, Instruction, Op},
        debuginfo::{Span, Spanned},
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut module = Parse::parse(
        "proc worker nop push.2 mul end begin push.5 exec.worker drop end",
        false,
        source_manager.clone(),
    )
    .unwrap();
    let procedure = module
        .procedures_mut()
        .find(|procedure| procedure.name().as_str() == "worker")
        .unwrap();
    let marker = procedure.body_mut().iter_mut().next().unwrap();
    let span = marker.span();
    let location = source_manager.file_line_col(span).unwrap();
    *marker = Op::Inst(Span::new(
        span,
        Instruction::DebugInlineCall(DebugInlineCallInfo::new(
            "::$exec::worker",
            location.clone(),
            location,
        )),
    ));
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("same-named-inline", module)
        .map(Arc::from)
        .unwrap();
    let mut executor = Executor::new(Vec::new()).into_debug(package, source_manager);
    let mut checked = false;
    while !executor.stopped {
        executor.step().unwrap();
        if executor.current_op != Some(miden_processor::operation::Operation::Mul) {
            continue;
        }
        let logical = executor.callstack.logical_frames("");
        let worker = logical
            .iter()
            .filter(|frame| frame.name().ends_with("::worker"))
            .collect::<Vec<_>>();
        assert_eq!(worker.len(), 2, "{logical:?}");
        assert_eq!(
            worker
                .iter()
                .filter(|frame| frame.kind() == crate::debug::LogicalFrameKind::Inline)
                .count(),
            1
        );
        checked = true;
    }
    assert!(checked);
}

#[test]
fn inherited_inline_frames_belong_to_the_physical_caller() {
    use miden_assembly_syntax::{
        Parse,
        ast::{DebugInlineCallInfo, Instruction, Op},
        debuginfo::Span,
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    for invoke in [
        "exec.inner",
        "call.inner",
        "procref.inner mem_storew_le.100 dropw push.100 dynexec",
        "procref.inner mem_storew_le.100 dropw push.100 dyncall",
    ] {
        let source = format!(
            "proc inner nop push.3 mul end\nproc outer nop push.2 add {invoke} push.4 add \
             end\nbegin nop push.5 exec.outer push.1 add drop end"
        );
        let mut module = Parse::parse(source.as_str(), false, source_manager.clone()).unwrap();
        for procedure in module.procedures_mut() {
            let name = if procedure.is_entrypoint() {
                "inline_main"
            } else if procedure.name().as_str() == "outer" {
                "inline_outer"
            } else {
                "inline_inner"
            };
            let op = procedure.body_mut().iter_mut().next().unwrap();
            let Op::Inst(instruction) = op else {
                panic!("expected marker");
            };
            let span = instruction.span();
            let location = source_manager.file_line_col(span).unwrap();
            *op = Op::Inst(Span::new(
                span,
                Instruction::DebugInlineCall(DebugInlineCallInfo::new(
                    name,
                    location.clone(),
                    location,
                )),
            ));
        }
        let package = Arc::from(
            miden_assembly::Assembler::new(source_manager.clone())
                .assemble_program("program", module)
                .unwrap(),
        );
        let mut executor =
            Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager.clone());
        let mut checked = false;
        while !executor.stopped {
            executor.step().unwrap();
            let logical = executor.callstack.logical_frames("");
            for name in ["inline_main", "inline_outer", "inline_inner"] {
                assert!(
                    logical.iter().filter(|frame| frame.name() == name).count() <= 1,
                    "duplicate {name} during {invoke}: {logical:?}"
                );
            }
            if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "mul") {
                let frames = executor.callstack.logical_frames("");
                let names = frames.iter().map(|frame| frame.name()).collect::<Vec<_>>();
                assert_eq!(
                    names,
                    [
                        "$exec::$main",
                        "inline_main",
                        "$exec::outer",
                        "inline_outer",
                        "$exec::inner",
                        "inline_inner",
                    ],
                    "{invoke}"
                );
                checked = true;
            }
        }
        assert!(checked, "{invoke}");
    }
}
