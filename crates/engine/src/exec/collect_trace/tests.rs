use alloc::{sync::Arc, vec::Vec};

use miden_debug_types::DefaultSourceManager;
use miden_mast_package::Package;
use miden_processor::Felt;

use super::collect_call_trace;
use crate::{
    debug::CallFrameRecord,
    event::{FRAME_END_EVENT, FRAME_START_EVENT},
    exec::Executor,
};

/// A real run with the markers the compiler writes around each `exec`. `function_1` calls
/// `function_2` and `function_3`; names, nesting and results all come from the run.
#[test]
fn frames_follow_the_calls_the_program_makes() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc function_2
push.42
end

proc function_3
push.7
end

proc function_1() -> [felt; 2]
emit.event("{FRAME_START_EVENT}")
exec.function_2
emit.event("{FRAME_END_EVENT}")
emit.event("{FRAME_START_EVENT}")
exec.function_3
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
drop
drop
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);

    // function_1 declares two results; its nested MASM callees have no signatures.
    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    assert_eq!(trace.roots.len(), 1, "expected one root, got {:?}", trace.roots);

    let function_1 = &trace.roots[0];
    assert_eq!(function_1.callee.as_deref(), Some("::$exec::function_1"));
    assert_eq!(function_1.results.as_deref(), Some([7, 42].as_slice()));
    assert_eq!(function_1.children.len(), 2);

    let function_2 = &function_1.children[0];
    assert_eq!(function_2.callee.as_deref(), Some("::$exec::function_2"));
    assert_eq!(
        function_2.results, None,
        "a nested call has no signature, so results are unknown"
    );
    assert!(function_2.children.is_empty());

    let function_3 = &function_1.children[1];
    assert_eq!(function_3.callee.as_deref(), Some("::$exec::function_3"));
    assert_eq!(
        function_3.results, None,
        "a nested call has no signature, so results are unknown"
    );
    assert!(function_3.children.is_empty());

    let exit = |frame: &CallFrameRecord| frame.exit_clk.expect("every call returned");
    assert!(function_1.enter_clk < function_2.enter_clk);
    assert!(exit(function_2) < function_3.enter_clk);
    assert!(exit(function_3) < exit(function_1));
}

/// `function_2` returns, then `function_3` fails. The trace keeps what ran: `function_2` has
/// its exit cycle and its result, `function_3` and `function_1` have none.
#[test]
fn a_failing_call_keeps_the_trace_up_to_the_failure() {
    // function_3 fails its assertion after function_2 has returned.
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc function_2
push.42
end

proc function_3
push.0
assert
end

proc function_1
emit.event("{FRAME_START_EVENT}")
exec.function_2
emit.event("{FRAME_END_EVENT}")
emit.event("{FRAME_START_EVENT}")
exec.function_3
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
drop
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);

    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_some(), "execution should fail in function_3");
    assert_eq!(trace.roots.len(), 1, "expected one root, got {:?}", trace.roots);

    let function_1 = &trace.roots[0];
    assert_eq!(function_1.callee.as_deref(), Some("::$exec::function_1"));
    assert_eq!(function_1.exit_clk, None);
    assert_eq!(function_1.children.len(), 2);

    let function_2 = &function_1.children[0];
    assert_eq!(function_2.callee.as_deref(), Some("::$exec::function_2"));
    assert!(function_2.exit_clk.is_some());
    assert_eq!(
        function_2.results, None,
        "a nested call has no signature, so results are unknown"
    );

    let function_3 = &function_1.children[1];
    assert_eq!(function_3.callee.as_deref(), Some("::$exec::function_3"));
    assert_eq!(function_3.exit_clk, None);
}

#[test]
fn a_call_failing_on_its_first_operation_keeps_its_identity() {
    let replay = replay_source(
        r#"
proc fail
assert
end
begin
emit.event("readonly::miden_debug::frame_start")
exec.fail
emit.event("readonly::miden_debug::frame_end")
end
"#,
        false,
    );

    assert!(replay.error.is_some(), "asserting the zero input should fail");
    assert_eq!(replay.trace.roots.len(), 1);
    let frame = &replay.trace.roots[0];
    assert_eq!(frame.callee.as_deref(), Some("::$exec::fail"));
    assert_eq!(frame.exit_clk, None);
    assert_eq!(frame.results, None);
}

/// A call with no debug information: it takes a value off the stack, and nothing says how
/// many it took, so what it returned cannot be worked out.
///
/// ```text
/// proc function_2   // drops one value, pushes nothing
///
/// stack 17 -> 16, no arguments reported   -> results are not known
/// ```
#[test]
fn results_are_unknown_without_debug_information() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc function_2
drop
end

proc function_1
push.42
emit.event("{FRAME_START_EVENT}")
exec.function_2
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);

    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    let function_2 = &trace.roots[0].children[0];
    assert_eq!(function_2.callee.as_deref(), Some("::$exec::function_2"));
    assert_eq!(function_2.results, None);
}

/// Runs `function_1(x)` where `x` is one argument of `type_info`, and returns the call
/// `function_1` makes.
///
/// ```text
/// fn function_2(x: T) { .. }
///
/// stack 18 -> 16   two elements off, nothing on
/// ```
///
/// What those two elements hold depends on `T`.
fn trace_one_typed_argument(
    type_info: miden_mast_package::debug_info::DebugTypeInfo,
) -> CallFrameRecord {
    use miden_assembly_syntax::ast::DebugVarLocation;
    use miden_core::serde::Serializable;
    use miden_mast_package::{
        SectionId,
        debug_info::{DebugSourceNodeId, DebugSourceVar, PackageDebugInfoBuilder},
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc function_2
push.99
drop
drop
drop
end

proc function_1
push.1
push.2
emit.event("{FRAME_START_EVENT}")
exec.function_2
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .expect("failed to assemble test program");

    // `x` is function_2's only parameter, sitting on top of the stack.
    let debug_info = package.debug_info().unwrap().expect("assembled package has debug info");
    let mut builder = PackageDebugInfoBuilder::from(alloc::boxed::Box::new(debug_info));
    let x_idx = builder.add_string(Arc::from("x"));
    let type_id = builder.add_type(type_info);
    let mut added = 0;
    let node_count = builder.debug_info().nodes().len() as u32;
    for source_node in (0..node_count).map(DebugSourceNodeId::from) {
        let node = &builder.debug_info()[source_node];
        let ops: Vec<u32> = (node.op_start..node.op_end)
            .filter(|op_idx| {
                node.asm_op_for_operation(*op_idx)
                    .is_some_and(|asm_op| &*builder.debug_info()[asm_op.op_name_idx] == "push.99")
            })
            .collect();
        for op_idx in ops {
            builder[source_node].debug_vars.push(DebugSourceVar {
                op_idx,
                name_idx: x_idx,
                type_id: Some(type_id),
                arg_idx: core::num::NonZeroU32::new(1),
                location_idx: None,
                value_location: DebugVarLocation::Stack(0),
            });
            added += 1;
        }
    }
    assert!(added >= 1, "fixture did not attach the variable");
    package.sections.retain(|section| section.id != SectionId::DEBUG_INFO);
    package.sections.push(miden_mast_package::Section::new(
        SectionId::DEBUG_INFO,
        builder.build().to_bytes(),
    ));

    let mut executor =
        Executor::new(Vec::<Felt>::new()).into_debug(Arc::from(package), source_manager);
    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    let mut function_1 = trace.roots.into_iter().next().expect("the program made a call");
    let function_2 = function_1.children.remove(0);
    assert_eq!(function_2.callee.as_deref(), Some("::$exec::function_2"));
    function_2
}

/// A `u64` argument takes two stack elements, and both limbs are read from its type. Results
/// are unknown here because the callee is a nested call with no signature.
#[test]
fn a_wide_argument_is_read_and_counted_by_its_type() {
    use miden_mast_package::debug_info::{DebugPrimitiveType, DebugTypeInfo};

    let function_2 = trace_one_typed_argument(DebugTypeInfo::Primitive(DebugPrimitiveType::U64));

    let x = &function_2.args[0];
    assert_eq!(x.name, "x");
    assert_eq!(x.felt_count, Some(2), "a u64 takes two stack elements");
    assert_eq!(x.values.as_deref(), Some([2, 1].as_slice()), "both limbs, not just the low one");
    // The callee has no return signature, so its results remain unknown.
    assert_eq!(function_2.results, None);
}

/// An argument type the ABI cannot measure leaves its value unknown. This callee also
/// has no return signature, so its results are unknown independently of the argument.
///
/// This is the same run as the `u64` case above: only the declared type differs.
#[test]
fn an_argument_the_abi_cannot_measure_reports_no_results() {
    use miden_mast_package::debug_info::{DebugPrimitiveType, DebugTypeInfo};

    let function_2 = trace_one_typed_argument(DebugTypeInfo::Primitive(DebugPrimitiveType::U256));

    let x = &function_2.args[0];
    assert_eq!(x.name, "x", "the argument is still reported, by name");
    assert_eq!(x.felt_count, None, "the ABI gives no felt count for a u256");
    assert_eq!(x.values, None, "and nothing is claimed about its value");
    assert_eq!(function_2.results, None);
}

/// ```text
/// fn function_1(a: Felt) { function_2(7) }   // `a` before and on the frame_start marker
/// fn function_2(b: Felt) { .. }
/// ```
///
/// Caller variables recorded up to and including `enter_clk` are not the callee's arguments.
#[test]
fn the_callers_arguments_are_not_reported_as_the_callees() {
    use miden_assembly_syntax::ast::DebugVarLocation;
    use miden_core::serde::Serializable;
    use miden_mast_package::{
        SectionId,
        debug_info::{DebugSourceNodeId, DebugSourceVar, PackageDebugInfoBuilder},
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc function_2
push.7
drop
end

proc function_1
push.111
drop
emit.event("{FRAME_START_EVENT}")
exec.function_2
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .expect("failed to assemble test program");

    // `a` goes on `push.111` (before `enter_clk`) and on the marker's `emit` (at `enter_clk`).
    let debug_info = package.debug_info().unwrap().expect("assembled package has debug info");
    let mut builder = PackageDebugInfoBuilder::from(alloc::boxed::Box::new(debug_info));
    let a_idx = builder.add_string(Arc::from("a"));
    let b_idx = builder.add_string(Arc::from("b"));
    let mut added = 0;
    let node_count = builder.debug_info().nodes().len() as u32;
    for source_node in (0..node_count).map(DebugSourceNodeId::from) {
        let node = &builder.debug_info()[source_node];
        let op_names: Vec<(u32, Arc<str>)> = (node.op_start..node.op_end)
            .filter_map(|op_idx| {
                let asm_op = node.asm_op_for_operation(op_idx)?;
                Some((op_idx, builder.debug_info()[asm_op.op_name_idx].clone()))
            })
            .collect();
        let mut vars: Vec<(u32, _)> = op_names
            .iter()
            .filter_map(|(op_idx, name)| match &**name {
                "push.111" => Some((*op_idx, a_idx)),
                "push.7" => Some((*op_idx, b_idx)),
                _ => None,
            })
            .collect();
        if let Some(push_111) = op_names.iter().position(|(_, name)| &**name == "push.111") {
            // `emit.event` is push, `emit`, drop; the frame opens on the `emit`. The first
            // marker after `push.111` is function_2's frame_start.
            let group_start = push_111
                + op_names[push_111..]
                    .iter()
                    .position(|(_, name)| name.contains(" emit "))
                    .expect("function_1 holds the frame markers");
            vars.push((op_names[group_start + 1].0, a_idx));
        }
        for (op_idx, name_idx) in vars {
            builder[source_node].debug_vars.push(DebugSourceVar {
                op_idx,
                name_idx,
                type_id: None,
                arg_idx: core::num::NonZeroU32::new(1),
                location_idx: None,
                value_location: DebugVarLocation::Stack(0),
            });
            added += 1;
        }
    }
    assert!(added >= 3, "fixture did not attach all variables");
    package.sections.retain(|section| section.id != SectionId::DEBUG_INFO);
    package.sections.push(miden_mast_package::Section::new(
        SectionId::DEBUG_INFO,
        builder.build().to_bytes(),
    ));

    let mut executor =
        Executor::new(Vec::<Felt>::new()).into_debug(Arc::from(package), source_manager);
    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    let function_1 = &trace.roots[0];
    let names: Vec<_> = function_1.args.iter().map(|arg| arg.name.as_str()).collect();
    assert_eq!(names, ["a"]);
    let function_2 = &function_1.children[0];
    let names: Vec<_> = function_2.args.iter().map(|arg| arg.name.as_str()).collect();
    assert_eq!(names, ["b"]);

    // Neither variable is given a type, so neither has a width, and nothing is claimed about
    // the values or about what the calls returned.
    let b = &function_2.args[0];
    assert_eq!((b.felt_count, b.values.as_deref()), (None, None));
    assert_eq!(function_2.results, None);
}

#[test]
fn calls_without_frame_markers_are_not_traced() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            r#"
proc function_2
push.42
end

proc function_1
exec.function_2
end

begin
exec.function_1
drop
end
"#,
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);

    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    assert!(trace.roots.is_empty(), "expected no frames, got {:?}", trace.roots);
}

/// ```text
/// fn consume_u64(n: u64)   // stack 16 -> 16
/// ```
///
/// Padding at the minimum stack depth is not reported as results.
#[test]
fn an_argument_consumed_at_the_minimum_stack_depth_is_not_a_result() {
    use miden_assembly_syntax::ast::DebugVarLocation;
    use miden_core::serde::Serializable;
    use miden_mast_package::{
        SectionId,
        debug_info::{
            DebugPrimitiveType, DebugSourceNodeId, DebugSourceVar, DebugTypeInfo,
            PackageDebugInfoBuilder,
        },
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc consume_u64(n: u64)
push.99
drop
drop
drop
end

proc function_1
emit.event("{FRAME_START_EVENT}")
exec.consume_u64
emit.event("{FRAME_END_EVENT}")
end

begin
emit.event("{FRAME_START_EVENT}")
exec.function_1
emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .expect("failed to assemble test program");

    // `n: u64` and a signature that returns nothing.
    let debug_info = package.debug_info().unwrap().expect("assembled package has debug info");
    let mut builder = PackageDebugInfoBuilder::from(alloc::boxed::Box::new(debug_info));
    let n_idx = builder.add_string(Arc::from("n"));
    let u64_type = builder.add_type(DebugTypeInfo::Primitive(DebugPrimitiveType::U64));
    let mut added = 0;
    let node_count = builder.debug_info().nodes().len() as u32;
    for source_node in (0..node_count).map(DebugSourceNodeId::from) {
        let node = &builder.debug_info()[source_node];
        let ops: Vec<u32> = (node.op_start..node.op_end)
            .filter(|op_idx| {
                node.asm_op_for_operation(*op_idx)
                    .is_some_and(|asm_op| &*builder.debug_info()[asm_op.op_name_idx] == "push.99")
            })
            .collect();
        for op_idx in ops {
            builder[source_node].debug_vars.push(DebugSourceVar {
                op_idx,
                name_idx: n_idx,
                type_id: Some(u64_type),
                arg_idx: core::num::NonZeroU32::new(1),
                location_idx: None,
                value_location: DebugVarLocation::Stack(0),
            });
            added += 1;
        }
    }
    assert!(added >= 1, "fixture did not attach the variable");
    package.sections.retain(|section| section.id != SectionId::DEBUG_INFO);
    package.sections.push(miden_mast_package::Section::new(
        SectionId::DEBUG_INFO,
        builder.build().to_bytes(),
    ));

    // The `u64` comes from the stack inputs, so the depth stays 16.
    let mut executor = Executor::new(Vec::from([Felt::from(1u32), Felt::from(2u32)]))
        .into_debug(Arc::from(package), source_manager);
    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    // The selected canonical function signature declares no results.
    let consume = &trace.roots[0].children[0];
    assert_eq!(consume.callee.as_deref(), Some("::$exec::consume_u64"));
    assert_eq!(consume.args[0].felt_count, Some(2));
    assert_eq!(consume.results.as_deref(), Some([].as_slice()), "padding reported as results");
}

/// ```text
/// fn recurse(n: Felt) { if n != 0 { recurse(n - 1) } }   // dynexec to itself
/// ```
///
/// A recursive call through `dynexec` is named after its callee.
#[test]
fn a_recursive_call_through_dynexec_is_named() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            format!(
                r#"
proc recurse
dup.0
neq.0
if.true
    sub.1
    emit.event("{FRAME_START_EVENT}")
    push.100
    dynexec
    emit.event("{FRAME_END_EVENT}")
else
    drop
end
end

begin
procref.recurse
mem_storew_le.100
dropw
push.2
emit.event("{FRAME_START_EVENT}")
exec.recurse
emit.event("{FRAME_END_EVENT}")
end
"#
            ),
        )
        .map(Arc::<Package>::from)
        .expect("failed to assemble test program");
    let mut executor = Executor::new(Vec::<Felt>::new()).into_debug(package, source_manager);

    let (trace, error) = collect_call_trace(&mut executor);

    assert!(error.is_none(), "execution failed: {error:?}");
    assert_eq!(trace.roots.len(), 1, "expected one root, got {:?}", trace.roots);
    let mut frame = &trace.roots[0];
    for depth in 0..3 {
        assert_eq!(
            frame.callee.as_deref(),
            Some("::$exec::recurse"),
            "frame at depth {depth} is not named"
        );
        assert!(frame.exit_clk.is_some(), "frame at depth {depth} did not return");
        if depth < 2 {
            assert_eq!(frame.children.len(), 1, "frame at depth {depth}");
            frame = &frame.children[0];
        }
    }
    assert!(frame.children.is_empty());
}

fn replay_source(source: &str, entrypoint_returns_void: bool) -> super::CallTraceReplay {
    use miden_assembly_syntax::ast::types::{CallConv, FunctionType};
    use miden_mast_package::{PackageExport, PackageManifest};
    let manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(manager)
        .assemble_program("program", source)
        .unwrap();
    if entrypoint_returns_void {
        let exports = package
            .manifest
            .exports()
            .cloned()
            .map(|mut export| {
                if let PackageExport::Procedure(procedure) = &mut export {
                    procedure.signature = Some(FunctionType::new(CallConv::ComponentModel, [], []));
                }
                export
            })
            .collect::<Vec<_>>();
        package.manifest = PackageManifest::new(exports).unwrap();
    }
    super::replay_call_trace(super::ReplaySnapshot {
        package: Arc::from(package),
        stack_inputs: Default::default(),
        advice_inputs: Default::default(),
        options: Default::default(),
        event_log: Vec::new(),
        mast_forests: Vec::new(),
    })
}

#[test]
fn marked_exec_only_wrapper_uses_its_own_identity() {
    let replay = replay_source(
        r#"
proc leaf() -> felt
push.42
end
proc wrapper() -> felt
exec.leaf
end
begin
emit.event("readonly::miden_debug::frame_start")
exec.wrapper
emit.event("readonly::miden_debug::frame_end")
drop
end
"#,
        false,
    );
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 1);
    let wrapper = &replay.trace.roots[0];
    assert_eq!(wrapper.callee.as_deref(), Some("::$exec::wrapper"));
    assert_eq!(wrapper.results.as_deref(), Some([42].as_slice()));
    assert!(wrapper.children.is_empty(), "unmarked leaf must not add a traced call");
}

const TYPED_HELPER: &str = r#"
proc helper() -> felt
push.42
end
begin
emit.event("readonly::miden_debug::frame_start")
exec.helper
emit.event("readonly::miden_debug::frame_end")
drop
end
"#;

#[test]
fn first_marked_helper_does_not_inherit_entrypoint_result_width() {
    let replay = replay_source(TYPED_HELPER, true);
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 1);
    let helper = &replay.trace.roots[0];
    assert_eq!(helper.callee.as_deref(), Some("::$exec::helper"));
    assert_eq!(helper.results.as_deref(), Some([42].as_slice()));
}

#[test]
fn first_marked_helper_keeps_its_signature_without_entrypoint_signature() {
    let replay = replay_source(TYPED_HELPER, false);
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 1);
    assert_eq!(replay.trace.roots[0].results.as_deref(), Some([42].as_slice()));
}

#[test]
fn same_display_name_uses_each_callees_signature() {
    let replay = replay_source(
        r#"
@source_name("same")
proc one() -> felt
push.42
end
@source_name("same")
proc two() -> [felt; 2]
push.7
push.8
end
begin
emit.event("readonly::miden_debug::frame_start")
exec.one
emit.event("readonly::miden_debug::frame_end")
drop
emit.event("readonly::miden_debug::frame_start")
exec.two
emit.event("readonly::miden_debug::frame_end")
drop
drop
end
"#,
        false,
    );
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 2);
    assert_eq!(replay.trace.roots[0].callee.as_deref(), Some("::$exec::same"));
    assert_eq!(replay.trace.roots[1].callee.as_deref(), Some("::$exec::same"));
    assert_eq!(replay.trace.roots[0].results.as_deref(), Some([42].as_slice()));
    assert_eq!(replay.trace.roots[1].results.as_deref(), Some([8, 7].as_slice()));
}

#[test]
fn nested_exec_only_wrappers_select_the_marked_outer_callee() {
    let replay = replay_source(
        r#"
proc leaf() -> felt
push.42
end
proc inner() -> felt
exec.leaf
end
proc outer() -> felt
exec.inner
end
begin
emit.event("readonly::miden_debug::frame_start")
exec.outer
emit.event("readonly::miden_debug::frame_end")
drop
end
"#,
        false,
    );
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 1);
    assert_eq!(replay.trace.roots[0].callee.as_deref(), Some("::$exec::outer"));
    assert_eq!(replay.trace.roots[0].results.as_deref(), Some([42].as_slice()));
}

#[test]
fn marked_wrapper_does_not_inherit_unmarked_leaf_arguments() {
    use miden_assembly_syntax::ast::DebugVarLocation;
    use miden_core::serde::Serializable;
    use miden_mast_package::{
        SectionId,
        debug_info::{DebugSourceNodeId, DebugSourceVar, PackageDebugInfoBuilder},
    };

    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut package = miden_assembly::Assembler::new(source_manager)
        .assemble_program(
            "program",
            format!(
                r#"
proc leaf(b: felt) -> felt
push.99
drop
drop
push.42
end
proc wrapper() -> felt
exec.leaf
end
begin
emit.event("{FRAME_START_EVENT}")
exec.wrapper
emit.event("{FRAME_END_EVENT}")
drop
end
"#
            ),
        )
        .expect("failed to assemble test program");

    // Attach the leaf's parameter to its first operation, including inlined copies.
    let debug_info = package.debug_info().unwrap().expect("assembled package has debug info");
    let mut builder = PackageDebugInfoBuilder::from(alloc::boxed::Box::new(debug_info));
    let b_idx = builder.add_string(Arc::from("b"));
    let mut added = 0;
    let node_count = builder.debug_info().nodes().len() as u32;
    for source_node in (0..node_count).map(DebugSourceNodeId::from) {
        let node = &builder.debug_info()[source_node];
        let ops: Vec<u32> = (node.op_start..node.op_end)
            .filter(|op_idx| {
                node.asm_op_for_operation(*op_idx)
                    .is_some_and(|asm_op| &*builder.debug_info()[asm_op.op_name_idx] == "push.99")
            })
            .collect();
        for op_idx in ops {
            builder[source_node].debug_vars.push(DebugSourceVar {
                op_idx,
                name_idx: b_idx,
                type_id: None,
                arg_idx: core::num::NonZeroU32::new(1),
                location_idx: None,
                value_location: DebugVarLocation::Stack(0),
            });
            added += 1;
        }
    }
    assert!(added >= 1, "fixture did not attach the leaf's parameter");
    package.sections.retain(|section| section.id != SectionId::DEBUG_INFO);
    package.sections.push(miden_mast_package::Section::new(
        SectionId::DEBUG_INFO,
        builder.build().to_bytes(),
    ));

    let replay = super::replay_call_trace(super::ReplaySnapshot {
        package: Arc::from(package),
        stack_inputs: Default::default(),
        advice_inputs: Default::default(),
        options: Default::default(),
        event_log: Vec::new(),
        mast_forests: Vec::new(),
    });
    assert!(replay.error.is_none(), "{:?}", replay.error);
    assert_eq!(replay.trace.roots.len(), 1);
    let wrapper = &replay.trace.roots[0];
    assert_eq!(wrapper.callee.as_deref(), Some("::$exec::wrapper"));
    assert!(wrapper.args.is_empty(), "wrapper inherited leaf arguments: {:?}", wrapper.args);
}
