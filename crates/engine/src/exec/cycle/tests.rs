//! Tests of the per-cycle records that [`Stepper`] and [`DebugExecutor`] produce.
//!
//! [`DebugExecutor`]: crate::DebugExecutor

use std::{
    collections::BTreeMap,
    pin::pin,
    string::String,
    task::{Context, Poll, Waker},
};

use miden_assembly::{DefaultSourceManager, Linkage};
use miden_core::{Felt, mast::MastNode};
use miden_mast_package::Package;
use miden_processor::{DefaultHost, FastProcessor, StackInputs};

use super::*;
use crate::exec::{Executor, Stepper};

/// Two procedures, an if/else, and a loop guarded by the split the assembler wraps around every
/// `while.true`.
const SRC: &str = r#"
proc double
    mul.2
end

proc count_down
    dup neq.0
    while.true
        sub.1
        dup neq.0
    end
    drop
end

begin
    if.true
        exec.double
    else
        push.7 drop
    end
    exec.count_down
end
"#;

/// Assembles [`SRC`] with the source manager the debug executor needs.
fn fixture() -> (Arc<Package>, Arc<DefaultSourceManager>) {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("fixture", SRC)
        .map(Arc::<Package>::from)
        .unwrap();
    (package, source_manager)
}

/// The stack inputs of a run, `flag` on top of `n`.
fn inputs(flag: u64, n: u64) -> Vec<Felt> {
    vec![Felt::new(flag).unwrap(), Felt::new(n).unwrap()]
}

/// A stepper over the fixture with `flag` on top of `n`.
fn stepper(package: &Arc<Package>, flag: u64, n: u64) -> Stepper {
    let processor = FastProcessor::new(StackInputs::new(&inputs(flag, n)).unwrap());
    Stepper::for_package(processor, package.clone()).unwrap()
}

/// Counts derived from the records, as the coverage collector derives them.
#[derive(Debug, Default)]
struct Hits {
    node_starts: BTreeMap<u32, u64>,
    edge_starts: BTreeMap<(u32, u32), u64>,
    op_hits: BTreeMap<(u32, u32), u64>,
    records: usize,
}

impl Hits {
    /// Counts the starts and the operation hit of one record.
    fn record(&mut self, cycle: &CycleInfo<'_>) {
        self.records += 1;
        if let Some(NodeStart { node, parent }) = cycle.node_start {
            let node = u32::from(node);
            *self.node_starts.entry(node).or_default() += 1;
            if let Some(parent) = parent {
                *self.edge_starts.entry((u32::from(parent), node)).or_default() += 1;
            }
        }
        if let (Some(_), Some(node), Some(idx)) = (cycle.op, cycle.source_node, cycle.op_idx) {
            *self.op_hits.entry((u32::from(node), idx)).or_default() += 1;
        }
    }
}

/// Runs the fixture to its end through a [`Stepper`] and returns the counts.
fn run(package: &Arc<Package>, flag: u64, n: u64) -> Hits {
    let mut stepper = stepper(package, flag, n);
    let mut hits = Hits::default();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| hits.record(cycle))
        .unwrap();
    assert!(stepper.stopped());
    assert_eq!(hits.records, stepper.cycle());
    hits
}

/// The source nodes of the entry point's own tree whose exec node satisfies `pred`.
///
/// The assembler emits a source tree per procedure and one per call site, so a procedure's
/// nodes appear several times in the debug info; only the entry point's tree executes.
fn source_node(package: &Package, pred: impl Fn(&MastNode) -> bool) -> Vec<u32> {
    let debug_info = package.debug_info().unwrap().unwrap();
    let forest = package.mast_forest();
    let root = debug_info
        .unique_source_root_for_exec_node(package.unwrap_program().entrypoint())
        .unwrap()
        .unwrap();
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        let node = &debug_info[id];
        pending.extend(node.children.iter().copied());
        if pred(&forest[node.exec_node]) {
            found.push(u32::from(id));
        }
    }
    found.sort_unstable();
    found
}

/// An owned copy of the comparable fields of a [`CycleInfo`].
#[derive(Debug, PartialEq, Eq)]
struct Record {
    exec_node: Option<MastNodeId>,
    has_debug_info: bool,
    source_node: Option<u32>,
    op: Option<Operation>,
    op_idx: Option<u32>,
    control: Option<ControlFlowOp>,
    node_start: Option<NodeStart>,
    inline_frames: Vec<String>,
}

impl From<&CycleInfo<'_>> for Record {
    fn from(cycle: &CycleInfo<'_>) -> Self {
        Self {
            exec_node: cycle.exec_node,
            has_debug_info: cycle.debug_info.is_some(),
            source_node: cycle.source_node.map(u32::from),
            op: cycle.op,
            op_idx: cycle.op_idx,
            control: cycle.control,
            node_start: cycle.node_start,
            inline_frames: cycle.inline_frames.iter().map(|frame| frame.name().into()).collect(),
        }
    }
}

/// Drives `future` to completion on the current thread.
///
/// Only for futures that make progress on every poll, as those of a synchronous host do.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

#[test]
fn stepper_records_node_starts_with_parents_and_ops() {
    let (package, _) = fixture();
    let debug_info = package.debug_info().unwrap().unwrap();
    let splits = source_node(&package, |n| matches!(n, MastNode::Split(_)));
    let loops = source_node(&package, |n| matches!(n, MastNode::Loop(_)));
    assert_eq!(loops.len(), 1);
    assert_eq!(splits.len(), 2);
    let loop_src = loops[0];
    let body_src: u32 = debug_info.nodes()[loop_src.into()].children[0].into();
    // The split guarding the loop has the loop as its true child.
    let loop_guard = *splits
        .iter()
        .find(|s| u32::from(debug_info.nodes()[(**s).into()].children[0]) == loop_src)
        .unwrap();
    let if_split = *splits.iter().find(|s| **s != loop_guard).unwrap();
    let if_node = &debug_info.nodes()[if_split.into()];
    let (then_src, else_src): (u32, u32) = (if_node.children[0].into(), if_node.children[1].into());
    let guard_node = &debug_info.nodes()[loop_guard.into()];
    let skip_src: u32 = guard_node.children[1].into();

    // flag = 1 doubles n = 3, so the loop body runs six times.
    let hits = run(&package, 1, 3);
    assert!(hits.records > 0);
    // Each start is also recorded as an edge from the node that started it.
    assert_eq!(hits.edge_starts.get(&(if_split, then_src)), Some(&1));
    assert_eq!(hits.edge_starts.get(&(if_split, else_src)), None);
    assert_eq!(hits.edge_starts.get(&(loop_guard, loop_src)), Some(&1));
    assert_eq!(hits.edge_starts.get(&(loop_src, body_src)), Some(&6));
    assert_eq!(hits.node_starts.get(&then_src), Some(&1));
    assert_eq!(hits.node_starts.get(&else_src), None);
    assert_eq!(hits.node_starts.get(&loop_src), Some(&1));
    assert_eq!(hits.node_starts.get(&body_src), Some(&6));
    assert_eq!(hits.node_starts.get(&skip_src), None);
    // `mul.2` is the first row of the `double` block: push(2), mul.
    assert_eq!(hits.op_hits.get(&(then_src, 0)), Some(&1));
    assert_eq!(hits.op_hits.get(&(then_src, 1)), Some(&1));
    // The body block has five operations, each executed once per iteration.
    for op in 0..5 {
        assert_eq!(hits.op_hits.get(&(body_src, op)), Some(&6), "op {op}");
    }

    // flag = 0 takes the else arm and n = 0 skips the loop entirely.
    let hits = run(&package, 0, 0);
    assert_eq!(hits.edge_starts.get(&(if_split, else_src)), Some(&1));
    assert_eq!(hits.edge_starts.get(&(loop_guard, skip_src)), Some(&1));
    assert_eq!(hits.edge_starts.get(&(loop_src, body_src)), None);
    assert_eq!(hits.node_starts.get(&then_src), None);
    assert_eq!(hits.node_starts.get(&else_src), Some(&1));
    assert_eq!(hits.node_starts.get(&loop_src), None);
    assert_eq!(hits.node_starts.get(&body_src), None);
    assert_eq!(hits.node_starts.get(&skip_src), Some(&1));
}

#[test]
fn debug_executor_and_stepper_produce_the_same_records() {
    let (package, source_manager) = fixture();

    let mut stepper = stepper(&package, 1, 3);
    let mut from_stepper = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| from_stepper.push(Record::from(cycle)))
        .unwrap();

    // `Executor::new` hands its arguments to `StackInputs::new` like `stepper` does.
    let mut exec = Executor::new(inputs(1, 3)).into_debug(package, source_manager);
    let mut from_executor = Vec::new();
    while !exec.stopped {
        exec.step_with(|cycle| from_executor.push(Record::from(cycle))).unwrap();
    }

    // Both describe every cycle once and the finalization step not at all.
    assert_eq!(from_stepper.len(), from_executor.len());
    assert_eq!(from_stepper.len(), stepper.cycle());
    assert_eq!(from_executor.len(), exec.cycle);
    for (i, (stepper, executor)) in from_stepper.iter().zip(&from_executor).enumerate() {
        assert_eq!(stepper, executor, "record {i}");
    }
}

/// A loop whose body is a procedure of another package starts a source node per iteration
/// that no finish of this forest closes; the loop's own finish must pop it.
#[test]
fn external_loop_body_does_not_pile_up_open_nodes() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let library: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library(
                "dep",
                "namespace dep::counter pub proc tick sub.1 dup neq.0 end",
                None::<String>,
            )
            .unwrap(),
    );
    let package = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .with_package(library.clone(), Linkage::Dynamic)
            .unwrap()
            .assemble_program(
                "program",
                "use dep::counter begin dup neq.0 while.true exec.counter::tick end drop end",
            )
            .unwrap(),
    );

    let iterations = 12;
    let mut executor = Executor::new(vec![Felt::new(iterations).unwrap()]);
    executor.with_package(library).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package, source_manager);
    let mut deepest = 0;
    let mut body_starts = 0;
    while stepper
        .step_sync(&mut host, |cycle| {
            body_starts += usize::from(cycle.node_start.is_some_and(|start| {
                cycle.forest.get_node_by_id(cycle.exec_node.unwrap()).is_some_and(|node| {
                    matches!(node, MastNode::External(_)) && start.parent.is_some()
                })
            }));
        })
        .unwrap()
    {
        deepest = deepest.max(stepper.open_source_nodes());
    }
    assert_eq!(body_starts, iterations as usize);
    // Root join, loop guard, loop, body: nothing accumulates across iterations.
    assert!(deepest <= 6, "{deepest} nodes open at the deepest point");
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// `exec` of a MASM procedure is recorded as an inline call: the frames name the procedure
/// while its body runs and are empty at the program's own level.
/// The step that consumes the empty context left after the last cycle describes nothing: the
/// records match the VM clock, not the number of steps.
#[test]
fn finalization_step_produces_no_record() {
    let (package, _) = fixture();
    let mut stepper = stepper(&package, 1, 3);
    let mut host = DefaultHost::default();
    let mut records = 0;
    let mut steps = 0;
    while stepper.step_sync(&mut host, |_| records += 1).unwrap() {
        steps += 1;
    }
    steps += 1;
    let clock = u32::from(stepper.processor().state().clock()) as usize;
    assert_eq!(records, clock);
    assert_eq!(stepper.cycle(), clock);
    assert_eq!(steps, clock + 1);
}

/// A stepper over a bare program resolves its first cycle against the supplied debug info, so
/// the root's start is not lost, and the records equal those of the package entry.
#[test]
fn bare_program_first_record_has_the_supplied_debug_info() {
    let (package, _) = fixture();
    let debug_info = Arc::new(package.debug_info().unwrap().unwrap());

    let mut from_package = Vec::new();
    stepper(&package, 1, 3)
        .run_sync(&mut DefaultHost::default(), |cycle| from_package.push(Record::from(cycle)))
        .unwrap();

    let mut processor = FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap());
    let ctx = processor.get_initial_resume_context(&package.unwrap_program()).unwrap();
    let mut stepper = Stepper::new(processor, ctx).with_debug_info(debug_info);
    let mut from_program = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| from_program.push(Record::from(cycle)))
        .unwrap();

    let first = &from_program[0];
    assert!(first.has_debug_info, "{first:?}");
    assert!(first.source_node.is_some(), "{first:?}");
    assert!(first.node_start.is_some_and(|start| start.parent.is_none()), "{first:?}");
    assert_eq!(from_program.len(), from_package.len());
    for (i, (program, package)) in from_program.iter().zip(&from_package).enumerate() {
        assert_eq!(program, package, "record {i}");
    }
}

/// A failed program is stopped but has no output to export.
#[test]
fn into_output_is_refused_after_a_failure() {
    let package = miden_assembly::Assembler::default()
        .assemble_program("failing", "begin assert end")
        .map(Arc::<Package>::from)
        .unwrap();
    let mut stepper =
        Stepper::for_package(FastProcessor::new(StackInputs::default()), package).unwrap();
    let mut records = 0;
    stepper.run_sync(&mut DefaultHost::default(), |_| records += 1).unwrap_err();
    assert!(records > 0, "the failing cycle is described");
    assert!(stepper.stopped());
    assert!(stepper.failed());
    assert!(stepper.into_output().is_err());
}

#[test]
fn inline_frames_name_the_inlined_procedure() {
    let (package, _) = fixture();
    let mut stepper = stepper(&package, 1, 3);
    let mut inside_double = Vec::new();
    let mut at_top_level = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| {
            let frames: Vec<String> =
                cycle.inline_frames.iter().map(|frame| frame.name().into()).collect();
            match cycle.op {
                Some(Operation::Mul) => inside_double.push(frames),
                Some(Operation::Push(_)) if frames.is_empty() => at_top_level.push(frames),
                _ => {}
            }
        })
        .unwrap();
    assert_eq!(inside_double.len(), 1, "`mul.2` runs once");
    assert_eq!(inside_double[0].len(), 1);
    assert!(inside_double[0][0].ends_with("double"), "{:?}", inside_double[0]);
    assert!(
        !at_top_level.is_empty(),
        "the entry block's own operations carry no inline frame"
    );
}

#[test]
fn stepper_async_step_and_output() {
    let (package, _) = fixture();

    let unstepped = stepper(&package, 1, 3);
    assert!(unstepped.into_output().is_err());

    let mut stepper = stepper(&package, 1, 3);
    let mut records = 0;
    block_on(stepper.run(&mut DefaultHost::default(), |_| records += 1)).unwrap();
    assert!(stepper.stopped());
    assert_eq!(records, stepper.cycle());
    let output = stepper.into_output().unwrap();

    let expected = FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap())
        .execute_sync(&package.unwrap_program(), &mut DefaultHost::default())
        .unwrap();
    assert_eq!(output.stack, expected.stack);
}
