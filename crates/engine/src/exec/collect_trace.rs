use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use miden_core::FMP_ADDR;
use miden_debug_types::DefaultSourceManager;
use miden_processor::{ExecutionError, Felt, event::EventId};

use super::{DebugQuery, Event, ExecutionConfig, Executor, ReplaySnapshot, state::DebugExecutor};
use crate::debug::{
    CallTrace, CallTraceRecorder, DebugVarSnapshot, FrameTransition, resolve_variable_values,
    value_felt_count,
};

/// The result of replaying a recorded execution with [`replay_call_trace`].
pub struct CallTraceReplay {
    pub trace: CallTrace,
    pub cycles: usize,
    pub error: Option<ExecutionError>,
}

/// Replay `recording` and collect its call trace.
pub fn replay_call_trace(recording: ReplaySnapshot) -> CallTraceReplay {
    let ReplaySnapshot {
        package,
        stack_inputs,
        advice_inputs,
        options,
        event_log,
        mast_forests,
    } = recording;

    let executor = Executor::from_config(ExecutionConfig {
        inputs: stack_inputs,
        advice_inputs,
        options,
    });
    let mut executor = executor.into_debug_with_replay(
        package,
        Arc::new(DefaultSourceManager::default()),
        mast_forests,
        VecDeque::from(event_log),
    );

    let (trace, error) = collect_call_trace(&mut executor);

    CallTraceReplay {
        trace,
        cycles: executor.cycle,
        error,
    }
}

/// Run `executor` to completion and collect its call trace.
///
/// If execution fails, the trace collected up to the failure is returned alongside the error.
fn collect_call_trace(executor: &mut DebugExecutor) -> (CallTrace, Option<ExecutionError>) {
    let mut recorder = CallTraceRecorder::default();
    let mut error = None;

    while !executor.stopped {
        if let Err(err) = executor.step() {
            error = Some(err);
            break;
        }

        let clk = executor.cycle;
        // Frames are opened and closed by the frame events, not by the call stack: a frame-end
        // event ends a call even when the call stack has nothing left to pop.
        let transition = executor.callstack.take_frame_transition();

        let stack = match transition {
            Some(_) => stack_without_frame_marker(&executor.current_stack),
            None => Vec::new(),
        };

        match transition {
            Some(FrameTransition::Exited) => recorder.exit(clk, &stack),
            // On the entry cycle execution is still in the caller, so the callee is named later.
            Some(FrameTransition::Entered) => {
                recorder.enter(clk, executor.current_proc.clone(), stack.len())
            }
            None => {
                if let Some(name) = executor.current_proc.as_deref() {
                    recorder.observe_name(name);
                }
            }
        }

        if let Some(enter_clk) = recorder.accepting_args() {
            for snapshot in executor.debug_vars.current_variables() {
                let Some(index) = snapshot.info.arg_index() else {
                    continue;
                };
                // Variables recorded before the frame was entered belong to the caller.
                if usize::from(snapshot.clk) < enter_clk {
                    continue;
                }
                // Resolving a value reads VM memory, and the frame keeps the value the call was
                // made with, so skip an argument that is already recorded.
                if recorder.has_arg(index.get()) {
                    continue;
                }
                // The argument's stack width comes from its type - 1 element for `Felt`, 2 for
                // `u64`, 4 for `Word` - and results are counted back from it. An untyped argument
                // has no width, so record it by name with no value.
                let felt_count = snapshot.info.ty().and_then(value_felt_count);
                let values =
                    felt_count.and_then(|count| resolve_arg_values(executor, snapshot, count));
                recorder.observe_arg(index.get(), snapshot.info.name(), felt_count, values);
            }
        }
    }

    (recorder.finish(), error)
}

/// Read `count` consecutive felts of an argument out of the VM state.
fn resolve_arg_values(
    executor: &DebugExecutor,
    snapshot: &DebugVarSnapshot,
    count: usize,
) -> Option<Vec<u64>> {
    let to_u64 = |values: Vec<Felt>| values.iter().map(Felt::as_canonical_u64).collect();

    // `Stack` locations are resolved at the debug decorator and rewritten to a `Const`, which
    // holds a single felt (see `snapshot_transient_debug_values`). Anything wider than one felt
    // survives only in the capture.
    if let Some(captured) = executor.debug_vars.captured_values(snapshot.info.name())
        && captured.len() == count
    {
        return Some(to_u64(captured.to_vec()));
    }

    let read_memory = |addr: u32| executor.read_memory_element(addr);
    let read_local = |offset: i16| {
        let fmp = read_memory(FMP_ADDR.as_canonical_u64() as u32)?;
        let addr = u32::try_from(fmp.as_canonical_u64() as i64 + i64::from(offset)).ok()?;
        read_memory(addr)
    };

    resolve_variable_values(
        snapshot.info.value_location(),
        count,
        &executor.current_stack,
        read_memory,
        read_local,
    )
    .map(to_u64)
}

fn stack_without_frame_marker(stack: &[Felt]) -> Vec<u64> {
    // `emit.event(...)` pushes the event id, so drop the top element when it is a frame event id.
    let is_marker = |value: &Felt| {
        let id = EventId::from_felt(*value);
        id == Event::FrameStart.as_event_id() || id == Event::FrameEnd.as_event_id()
    };

    let rest = match stack.split_first() {
        Some((top, rest)) if is_marker(top) => rest,
        _ => stack,
    };

    rest.iter().map(Felt::as_canonical_u64).collect()
}

#[cfg(test)]
mod tests {
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
    drop
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

        let function_1 = &trace.roots[0];
        assert_eq!(function_1.callee.as_deref(), Some("::$exec::function_1"));
        assert_eq!(function_1.results.as_deref(), Some([7, 42].as_slice()));
        assert_eq!(function_1.children.len(), 2);

        let function_2 = &function_1.children[0];
        assert_eq!(function_2.callee.as_deref(), Some("::$exec::function_2"));
        assert_eq!(function_2.results.as_deref(), Some([42].as_slice()));
        assert!(function_2.children.is_empty());

        let function_3 = &function_1.children[1];
        assert_eq!(function_3.callee.as_deref(), Some("::$exec::function_3"));
        assert_eq!(function_3.results.as_deref(), Some([7].as_slice()));
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
        assert_eq!(function_2.results.as_deref(), Some([42].as_slice()));

        let function_3 = &function_1.children[1];
        assert_eq!(function_3.callee.as_deref(), Some("::$exec::function_3"));
        assert_eq!(function_3.exit_clk, None);
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
                    node.asm_op_for_operation(*op_idx).is_some_and(|asm_op| {
                        &*builder.debug_info()[asm_op.op_name_idx] == "push.99"
                    })
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

    /// A `u64` argument takes two stack elements, and both the value and the result count follow
    /// from its type rather than from the number of arguments.
    ///
    /// The frame is entered at depth 18 and left at 16. Counting the argument as two elements
    /// gives 0 results; counting it as one gives -1, which reads as unknown.
    #[test]
    fn a_wide_argument_is_read_and_counted_by_its_type() {
        use miden_mast_package::debug_info::{DebugPrimitiveType, DebugTypeInfo};

        let function_2 =
            trace_one_typed_argument(DebugTypeInfo::Primitive(DebugPrimitiveType::U64));

        let x = &function_2.args[0];
        assert_eq!(x.name, "x");
        assert_eq!(x.felt_count, Some(2), "a u64 takes two stack elements");
        assert_eq!(
            x.values.as_deref(),
            Some([2, 1].as_slice()),
            "both limbs, not just the low one"
        );
        assert_eq!(function_2.results.as_deref(), Some([].as_slice()));
    }

    /// A type the ABI cannot measure - `u256` is one, so are `Unknown`, `Never` and a dynamically
    /// sized list - leaves the argument without a width, and the call without results.
    ///
    /// This is the same run as the `u64` case above: only the declared type differs.
    #[test]
    fn an_argument_the_abi_cannot_measure_reports_no_results() {
        use miden_mast_package::debug_info::{DebugPrimitiveType, DebugTypeInfo};

        let function_2 =
            trace_one_typed_argument(DebugTypeInfo::Primitive(DebugPrimitiveType::U256));

        let x = &function_2.args[0];
        assert_eq!(x.name, "x", "the argument is still reported, by name");
        assert_eq!(x.felt_count, None, "the ABI gives no felt count for a u256");
        assert_eq!(x.values, None, "and nothing is claimed about its value");
        assert_eq!(function_2.results, None);
    }

    /// The caller's arguments are not reported as the callee's.
    ///
    /// ```text
    /// fn function_1(a: Felt) { function_2(7) }
    /// fn function_2(b: Felt) { .. }
    ///
    /// a   recorded before function_2 was entered   -> skipped
    /// b   recorded inside function_2              -> kept
    /// ```
    ///
    /// The variable tracker holds both while `function_2` runs, and reports them by name, so `a`
    /// comes first.
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

        // `a` is function_1's parameter, `b` is function_2's; both are the first parameter of
        // their procedure, and each is attached to an operation of its own body.
        let debug_info = package.debug_info().unwrap().expect("assembled package has debug info");
        let mut builder = PackageDebugInfoBuilder::from(alloc::boxed::Box::new(debug_info));
        let a_idx = builder.add_string(Arc::from("a"));
        let b_idx = builder.add_string(Arc::from("b"));
        let mut added = 0;
        let node_count = builder.debug_info().nodes().len() as u32;
        for source_node in (0..node_count).map(DebugSourceNodeId::from) {
            let node = &builder.debug_info()[source_node];
            let vars: Vec<(u32, _)> = (node.op_start..node.op_end)
                .filter_map(|op_idx| {
                    let asm_op = node.asm_op_for_operation(op_idx)?;
                    let name = match &*builder.debug_info()[asm_op.op_name_idx] {
                        "push.111" => a_idx,
                        "push.7" => b_idx,
                        _ => return None,
                    };
                    Some((op_idx, name))
                })
                .collect();
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
        assert!(added >= 2, "fixture did not attach both variables");
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
}
