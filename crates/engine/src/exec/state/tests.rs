use alloc::string::{String, ToString};
use std::sync::Arc;

use miden_assembly::DefaultSourceManager;
use miden_mast_package::Package;

use super::*;
use crate::exec::Executor;

#[test]
fn callstack_tracks_nested_frame_events() {
    use crate::event::{FRAME_END_EVENT, FRAME_START_EVENT};
    let source_manager = Arc::new(DefaultSourceManager::default());
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc inner
    emit.event("{FRAME_START_EVENT}")
    nop
    emit.event("{FRAME_END_EVENT}")
end

proc outer
    emit.event("{FRAME_START_EVENT}")
    exec.inner
    emit.event("{FRAME_END_EVENT}")
end

begin
    emit.event("{FRAME_START_EVENT}")
    exec.outer
    emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .unwrap();

    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(program, source_manager);
    let mut max_depth = 0;
    let mut saw_inner = false;
    let mut snapshots = Vec::new();

    for _ in 0..64 {
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

        if saw_inner || executor.stopped {
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
}

#[test]
fn recovered_backtrace_preserves_exec_callers_without_duplicate_inline_frames() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            "proc inner push.7 drop end proc outer exec.inner push.8 drop end begin exec.outer \
             push.9 drop end",
        )
        .map(Arc::<Package>::from)
        .unwrap();
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);
    let mut inner_cycles = 0;
    while !executor.stopped {
        executor.step().unwrap();
        if executor
            .current_asmop
            .as_ref()
            .is_some_and(|op| op.context_name().ends_with("::inner"))
        {
            let frames = executor.callstack.logical_frames("");
            let names = frames.iter().map(|frame| frame.name()).collect::<Vec<_>>();
            assert_eq!(names, ["$exec::$main", "$exec::outer", "$exec::inner"]);
            assert!(frames[1].display_name().starts_with("[inferred] "));
            assert!(frames[2].display_name().starts_with("[inferred] "));
            inner_cycles += 1;
        }
    }
    assert!(inner_cycles >= 2);
}

#[test]
fn recovered_backtrace_handles_calls_dynamic_recursion_and_returns() {
    for source in [
        "proc inner push.7 drop end begin call.inner call.inner push.9 drop end",
        "proc inner dup neq.0 if.true sub.1 push.100 dynexec else nop end end begin procref.inner \
         mem_storew_le.100 dropw push.3 push.100 dynexec drop push.9 drop end",
    ] {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let package = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program("program", source)
            .map(Arc::<Package>::from)
            .unwrap();
        let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);
        let mut max_depth = 0;
        while !executor.stopped {
            executor.step().unwrap();
            let frames = executor.callstack.logical_frames("");
            max_depth = max_depth.max(frames.len());
            if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.9") {
                assert_eq!(frames.len(), 1, "returned callers must not remain live");
                assert_eq!(frames[0].name(), "$exec::$main");
            }
        }
        assert_eq!(max_depth, if source.contains("dynexec") { 5 } else { 2 });
    }
}

#[test]
fn recovered_backtrace_does_not_duplicate_explicit_event_frames() {
    use crate::event::{FRAME_END_EVENT, FRAME_START_EVENT};
    let source_manager = Arc::new(DefaultSourceManager::default());
    let source = format!(
        "proc inner emit.event(\"{FRAME_START_EVENT}\") push.7 drop \
         emit.event(\"{FRAME_END_EVENT}\") end begin emit.event(\"{FRAME_START_EVENT}\") \
         exec.inner emit.event(\"{FRAME_END_EVENT}\") end"
    );
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", source)
        .map(Arc::<Package>::from)
        .unwrap();
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);
    let mut seen = false;
    while !executor.stopped {
        executor.step().unwrap();
        if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.7") {
            let frames = executor.callstack.logical_frames("");
            assert_eq!(
                frames.iter().map(|frame| frame.name()).collect::<Vec<_>>(),
                ["$exec::$main", "$exec::inner"]
            );
            assert!(
                executor.callstack.frames().len() >= 2,
                "physical event stack remains available"
            );
            seen = true;
        }
    }
    assert!(seen);
}

#[test]
fn stripped_package_loads_and_executes_without_recovered_frames() {
    use miden_core::serde::Serializable;
    use miden_debug_types::Uri;
    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", "begin push.7 add end")
        .unwrap();
    package.strip_debug_info().unwrap();
    let package =
        crate::package::read_package_from_bytes(&package.to_bytes(), &Uri::new("stripped.masp"))
            .unwrap();
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);
    while !executor.stopped {
        executor.step().unwrap();
    }
    assert_eq!(executor.stack_outputs.as_ref()[0], Felt::from_u32(7));
}

#[test]
fn finish_returns_from_uninstrumented_invocations() {
    for invocation in ["exec.inner", "call.inner", "push.100 dynexec"] {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let package = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(
                "program",
                format!(
                    "proc inner push.7 drop push.6 drop end proc outer {invocation} push.8 drop \
                     end begin procref.inner mem_storew_le.100 dropw exec.outer push.9 drop end"
                ),
            )
            .unwrap();
        let mut executor = Executor::new(vec![]).into_debug(package.into(), source_manager.clone());
        while !executor.stopped {
            executor.step().unwrap();
            if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.7") {
                break;
            }
        }
        assert!(!executor.stopped);
        let before = executor.cycle;
        executor.step_until(BreakpointType::Finish, source_manager.as_ref()).unwrap();
        assert!(!executor.stopped, "finish must stop in outer after {invocation}");
        assert!(executor.cycle > before + 1);
        assert!(executor.current_asmop.as_ref().unwrap().context_name().ends_with("::outer"));
        assert_eq!(executor.callstack.logical_frames("").len(), 2);
    }
}

#[test]
fn finish_unwinds_one_recursive_activation_at_a_time() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            "proc inner dup neq.0 if.true sub.1 push.100 dynexec else push.7 drop end push.8 drop \
             end begin procref.inner mem_storew_le.100 dropw push.3 push.100 dynexec drop push.9 \
             drop end",
        )
        .unwrap();
    let mut executor = Executor::new(vec![]).into_debug(package.into(), source_manager.clone());
    while !executor.stopped {
        executor.step().unwrap();
        if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.7") {
            break;
        }
    }
    assert_eq!(executor.callstack.logical_frames("").len(), 5);
    for depth in (1..5).rev() {
        executor.step_until(BreakpointType::Finish, source_manager.as_ref()).unwrap();
        assert!(!executor.stopped);
        assert_eq!(executor.callstack.logical_frames("").len(), depth);
    }
    executor.step_until(BreakpointType::Finish, source_manager.as_ref()).unwrap();
    assert!(executor.stopped, "finishing the root must run to completion");
}

#[test]
fn finish_uses_frame_events_when_metadata_is_stripped() {
    use crate::event::{FRAME_END_EVENT, FRAME_START_EVENT};
    let source_manager = Arc::new(DefaultSourceManager::default());
    let source = format!(
        "proc inner emit.event(\"{FRAME_START_EVENT}\") push.7 drop push.6 drop \
         emit.event(\"{FRAME_END_EVENT}\") end begin emit.event(\"{FRAME_START_EVENT}\") \
         exec.inner push.9 drop emit.event(\"{FRAME_END_EVENT}\") end"
    );
    let mut package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("program", source)
        .unwrap();
    package.strip_debug_info().unwrap();
    let mut executor = Executor::new(vec![]).into_debug(package.into(), source_manager.clone());
    // The fallback includes a synthetic root, followed by the explicit main and inner frames.
    while executor.callstack.frames().len() < 3 && !executor.stopped {
        executor.step().unwrap();
    }
    assert!(!executor.stopped);
    assert!(executor.current_asmop.is_none());
    let before = executor.cycle;
    executor.step_until(BreakpointType::Finish, source_manager.as_ref()).unwrap();
    assert!(!executor.stopped);
    assert!(executor.cycle > before + 1);
    assert_eq!(executor.callstack.frames().len(), 2);
}

#[test]
fn finish_waits_through_nested_calls_and_control_flow() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            "proc leaf push.4 drop end proc inner push.7 drop push.1 if.true exec.leaf push.6 \
             drop else nop end repeat.2 push.5 drop end end proc outer exec.inner push.8 drop end \
             begin exec.outer push.9 drop end",
        )
        .unwrap();
    let mut executor = Executor::new(vec![]).into_debug(package.into(), source_manager.clone());
    while !executor.stopped {
        executor.step().unwrap();
        if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.7") {
            break;
        }
    }
    executor.step_until(BreakpointType::Finish, source_manager.as_ref()).unwrap();
    assert!(!executor.stopped);
    assert!(executor.current_asmop.as_ref().unwrap().context_name().ends_with("::outer"));
    assert_eq!(executor.current_asmop.as_ref().unwrap().op().as_ref(), "push.8");
}

#[test]
fn finish_returns_across_a_dynamic_package_boundary() {
    use miden_assembly::Linkage;
    let manager = Arc::new(DefaultSourceManager::default());
    let library: Arc<Package> = miden_assembly::Assembler::new(manager.clone())
        .assemble_library(
            "dep",
            "namespace dep::math pub proc target push.7 drop push.6 drop end",
            None::<String>,
        )
        .unwrap()
        .into();
    let package = miden_assembly::Assembler::new(manager.clone())
        .with_package(library.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program("program", "use dep::math begin exec.math::target push.9 drop end")
        .unwrap();
    let mut executor = Executor::new(vec![]);
    executor.with_package(library).unwrap();
    let mut executor = executor.into_debug(package.into(), manager.clone());
    while !executor.stopped {
        executor.step().unwrap();
        if executor.current_asmop.as_ref().is_some_and(|op| op.op().as_ref() == "push.7") {
            break;
        }
    }
    assert!(!executor.stopped, "must observe the library invocation");
    executor.step_until(BreakpointType::Finish, manager.as_ref()).unwrap();
    assert!(!executor.stopped);
    assert_eq!(executor.callstack.logical_frames("").len(), 1);
    assert_eq!(executor.current_asmop.as_ref().unwrap().op().as_ref(), "push.9");
}

#[test]
fn finish_uses_callee_events_when_only_the_caller_has_metadata() {
    use miden_assembly::Linkage;

    use crate::event::{FRAME_END_EVENT, FRAME_START_EVENT};
    let manager = Arc::new(DefaultSourceManager::default());
    let mut library = miden_assembly::Assembler::new(manager.clone())
        .assemble_library(
            "dep",
            format!(
                "namespace dep::math pub proc target emit.event(\"{FRAME_START_EVENT}\") push.7 \
                 drop push.6 drop emit.event(\"{FRAME_END_EVENT}\") end"
            ),
            None::<String>,
        )
        .unwrap();
    library.strip_debug_info().unwrap();
    let library: Arc<Package> = library.into();
    let package = miden_assembly::Assembler::new(manager.clone())
        .with_package(library.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program("program", "use dep::math begin exec.math::target push.9 drop end")
        .unwrap();
    let mut executor = Executor::new(vec![]);
    executor.with_package(library).unwrap();
    let mut executor = executor.into_debug(package.into(), manager.clone());
    while executor.callstack.frames().len() < 2 && !executor.stopped {
        executor.step().unwrap();
    }
    assert!(!executor.stopped);
    assert!(executor.current_asmop.is_none());
    assert!(!executor.callstack.has_recovered_current_frame());
    let frames = executor.callstack.logical_frames("");
    assert_eq!(frames.len(), 2, "the stripped callee must remain visible");
    assert_eq!(frames[1].physical_index(), Some(1));
    executor.step_until(BreakpointType::Finish, manager.as_ref()).unwrap();
    assert!(!executor.stopped, "must finish the stripped callee, not its recovered caller");
    assert_eq!(executor.callstack.frames().len(), 1);
}
