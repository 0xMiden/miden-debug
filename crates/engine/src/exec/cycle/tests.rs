//! Tests of the per-cycle records that [`Stepper`] and [`DebugExecutor`] produce.
//!
//! [`DebugExecutor`]: crate::DebugExecutor

use std::{
    boxed::Box,
    cell::RefCell,
    collections::BTreeMap,
    pin::pin,
    rc::Rc,
    string::String,
    task::{Context, Poll, Waker},
};

use miden_assembly::{DefaultSourceManager, Linkage};
use miden_core::{Felt, Word, mast::MastNode};
use miden_mast_package::Package;
use miden_processor::{
    BaseHost, DefaultHost, FastProcessor, FutureMaybeSend, Host, LoadedMastForest, ProcessorState,
    StackInputs, advice::AdviceMutation, event::EventError,
};

use super::*;
use crate::{
    exec::{Executor, Stepper},
    profiling::{Instrument, OutputResult, OutputWriter, ProfilerConfig},
};

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

/// The node starts, their parents, and the operation hits of the fixture, for a run that takes
/// the `if` and loops, and for one that takes the `else` and skips the loop.
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

/// Both drivers go through the same resolution and produce the same records, in the same
/// number.
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
    executor.with_package(library.clone()).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package, source_manager);
    let mut deepest = 0;
    let mut body_starts = 0;
    let mut first_in_library = None;
    while stepper
        .step_sync(&mut host, |cycle| {
            body_starts += usize::from(cycle.node_start.is_some_and(|start| {
                cycle.forest.get_node_by_id(cycle.exec_node.unwrap()).is_some_and(|node| {
                    matches!(node, MastNode::External(_)) && start.parent.is_some()
                })
            }));
            if Arc::ptr_eq(cycle.forest, library.mast_forest()) && first_in_library.is_none() {
                first_in_library = Some(Record::from(cycle));
            }
        })
        .unwrap()
    {
        deepest = deepest.max(stepper.open_source_nodes());
    }
    assert_eq!(body_starts, iterations as usize);
    // The entry join, its prologue or body block, the loop guard, the loop, the body: nothing
    // accumulates across iterations.
    assert!(deepest <= 5, "{deepest} nodes open at the deepest point");
    assert_eq!(stepper.open_source_nodes(), 0);
    // The callee root's start runs in the same step as the external node's start and is
    // never described: the first record in the library is already inside the root.
    let first = first_in_library.expect("the library ran");
    assert!(first.node_start.is_none(), "{first:?}");
    assert!(first.op.is_some(), "{first:?}");
}

/// Two procedures of a second package called from a procedure of a first one, itself called
/// from the program: the callee root's start is never described, so its children have no
/// parent, and nothing from the libraries stays open once the program is back.
#[test]
fn external_siblings_under_a_foreign_root_have_no_parent() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let leaf: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library(
                "leaf",
                "namespace leaf::ops pub proc a push.1 drop end pub proc b push.2 drop end",
                None::<String>,
            )
            .unwrap(),
    );
    let mid: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .with_package(leaf.clone(), Linkage::Dynamic)
            .unwrap()
            .assemble_library(
                "mid",
                "namespace mid::glue use leaf::ops pub proc f exec.ops::a exec.ops::b end",
                None::<String>,
            )
            .unwrap(),
    );
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .with_package(mid.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program("program", "use mid::glue begin exec.glue::f end")
        .map(Arc::<Package>::from)
        .unwrap();

    let mut executor = Executor::new(Vec::new());
    executor.with_package(leaf.clone()).unwrap();
    executor.with_package(mid.clone()).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package.clone(), source_manager);
    let mut external_starts = Vec::new();
    let mut open_after_return = None;
    let mut was_in_library = false;
    while stepper
        .step_sync(&mut host, |cycle| {
            let in_program = Arc::ptr_eq(cycle.forest, package.mast_forest());
            if in_program && was_in_library && open_after_return.is_none() {
                open_after_return = Some(());
            }
            was_in_library = !in_program;
            let is_external = cycle
                .forest
                .get_node_by_id(cycle.exec_node.unwrap())
                .is_some_and(|node| matches!(node, MastNode::External(_)));
            if let Some(start) = cycle.node_start
                && is_external
            {
                external_starts.push((Arc::ptr_eq(cycle.forest, mid.mast_forest()), start));
            }
        })
        .unwrap()
    {
        if open_after_return.take().is_some() {
            // Back in the program after the libraries: only the program's own nodes are open.
            assert!(stepper.open_source_nodes() <= 2, "{}", stepper.open_source_nodes());
        }
    }
    // `f`'s own root is never described, so `a` and `b` get no parent rather than each other.
    let in_mid: Vec<_> = external_starts.iter().filter(|(in_mid, _)| *in_mid).collect();
    assert_eq!(in_mid.len(), 2, "{external_starts:?}");
    assert!(in_mid.iter().all(|(_, start)| start.parent.is_none()), "{external_starts:?}");
    // The program's own external start has the program's root as its parent.
    let in_program: Vec<_> = external_starts.iter().filter(|(in_mid, _)| !*in_mid).collect();
    assert_eq!(in_program.len(), 1);
    assert!(in_program[0].1.parent.is_some());
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// Adjacent calls of a library procedure whose body is one loop: the loop stays open in the
/// library's forest between calls, but it is popped with the forest boundary, so a later
/// call's loop neither chains to it nor names itself.
#[test]
fn adjacent_calls_of_a_loop_only_library_procedure_do_not_chain() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let library: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library(
                "lib",
                "namespace lib::loops pub proc drain while.true push.0 end end",
                None::<String>,
            )
            .unwrap(),
    );
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .with_package(library.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program(
            "program",
            "use lib::loops begin exec.loops::drain exec.loops::drain exec.loops::drain end",
        )
        .map(Arc::<Package>::from)
        .unwrap();
    // Each call pops a 1, loops once, pushes a 0 and pops it as the exit condition.
    let mut executor = Executor::new(vec![Felt::ONE, Felt::ONE, Felt::ONE]);
    executor.with_package(library.clone()).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package, source_manager);
    let mut starts = Vec::new();
    stepper
        .run_sync(&mut host, |cycle| {
            if let Some(start) = cycle.node_start
                && Arc::ptr_eq(cycle.forest, library.mast_forest())
            {
                let kind = match cycle.forest.get_node_by_id(cycle.exec_node.unwrap()) {
                    Some(MastNode::Loop(_)) => "loop",
                    Some(MastNode::Block(_)) => "block",
                    Some(MastNode::Split(_)) => "split",
                    _ => "other",
                };
                starts.push((kind, start));
            }
        })
        .unwrap();
    assert!(starts.iter().all(|(_, start)| start.parent != Some(start.node)), "{starts:?}");
    let loops: Vec<_> = starts.iter().filter(|(kind, _)| *kind == "loop").collect();
    assert_eq!(loops.len(), 3, "{starts:?}");
    // The procedure's root split is never described, so the loop has no parent.
    assert!(loops.iter().all(|(_, start)| start.parent.is_none()), "{starts:?}");
    // The body's parent is its loop, on every call.
    let bodies: Vec<_> = starts.iter().filter(|(kind, _)| *kind == "block").collect();
    assert!(!bodies.is_empty());
    assert!(bodies.iter().all(|(_, start)| start.parent.is_some()), "{starts:?}");
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// A host whose event handling yields once before answering, so that a step's future is
/// observed pending.
struct YieldingHost {
    inner: DefaultHost,
    yielded: bool,
}

impl BaseHost for YieldingHost {
    fn get_label_and_source_file(
        &self,
        location: &miden_debug_types::Location,
    ) -> (miden_debug_types::SourceSpan, Option<Arc<miden_debug_types::SourceFile>>) {
        self.inner.get_label_and_source_file(location)
    }
}

impl Host for YieldingHost {
    fn get_mast_forest(
        &self,
        node_digest: &Word,
    ) -> impl FutureMaybeSend<Option<LoadedMastForest>> {
        let forest = miden_processor::SyncHost::get_mast_forest(&self.inner, node_digest);
        async move { forest }
    }

    fn on_event(
        &mut self,
        _process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        let first = !self.yielded;
        self.yielded = true;
        let mut pending = first;
        core::future::poll_fn(move |_| {
            if pending {
                pending = false;
                Poll::Pending
            } else {
                Poll::Ready(Ok(Vec::new()))
            }
        })
    }
}

/// A step whose future is dropped while the host is pending leaves no context behind, but
/// that is not a finished program: the stepper refuses to go on and has no output.
#[test]
fn a_dropped_step_is_not_a_finished_program() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let event_name = "miden-debug::test::yield";
    let event_id = miden_core::events::EventId::from_name(event_name).as_u64();
    let package = miden_assembly::Assembler::new(source_manager)
        .assemble_program("program", format!("begin push.{event_id} emit drop end"))
        .map(Arc::<Package>::from)
        .unwrap();
    // The host answers the event itself, after yielding once.
    let mut host = YieldingHost {
        inner: DefaultHost::default(),
        yielded: false,
    };
    let mut stepper =
        Stepper::for_package(FastProcessor::new(StackInputs::default()), package).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        let mut step = pin!(stepper.step(&mut host, |_| {}));
        match step.as_mut().poll(&mut cx) {
            Poll::Pending => break,
            Poll::Ready(Ok(true)) => continue,
            other => panic!("the program ended before yielding: {other:?}"),
        }
    }
    assert!(!stepper.stopped());
    assert!(!stepper.failed());
    assert!(stepper.step_sync(&mut host.inner, |_| {}).is_err());
    assert!(stepper.into_output().is_err());
}

/// An instrument that keeps the operation of every record it receives.
struct RecordingInstrument(Rc<RefCell<Vec<Option<Operation>>>>);

impl Instrument for RecordingInstrument {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn on_operation_execution_cycle(&mut self, _op: Operation, _proc: Option<&str>) {}

    fn on_cycle(&mut self, cycle: &CycleInfo<'_>) {
        self.0.borrow_mut().push(cycle.op);
    }

    fn write_report_to(&self, _writer: &mut dyn OutputWriter) -> OutputResult<()> {
        Ok(())
    }
}

/// The profiler's instruments receive every record before its cycle runs, the failing one
/// included, and after the internal observer has seen the source context.
#[test]
fn instruments_receive_the_record_before_the_cycle_runs() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program("failing", "begin push.1 drop assert end")
        .map(Arc::<Package>::from)
        .unwrap();
    let records = Rc::new(RefCell::new(Vec::new()));
    let order = Rc::new(RefCell::new(Vec::new()));
    let mut executor = Executor::new(Vec::new());
    executor.with_profiler_config(ProfilerConfig {
        instruments: vec![Box::new(RecordingInstrument(records.clone()))],
        reports_dir: None,
    });
    let mut exec = executor.into_debug(package, source_manager);
    let mut outcome = Ok(None);
    while !exec.stopped {
        let order_observer = order.clone();
        let order_cycle = order.clone();
        outcome = exec.step_impl(
            move |_, _| order_observer.borrow_mut().push("observer"),
            move |_| order_cycle.borrow_mut().push("on_cycle"),
        );
        if outcome.is_err() {
            break;
        }
    }
    assert!(outcome.is_err(), "the program fails on `assert`");
    let records = records.borrow();
    assert!(matches!(records.last(), Some(Some(Operation::Assert(_)))), "{records:?}");
    assert_eq!(records.len(), exec.cycle + 1, "every cycle plus the failing one");
    let order = order.borrow();
    assert_eq!(order.len(), 2 * records.len());
    assert!(order.chunks(2).all(|pair| pair == ["observer", "on_cycle"]), "{order:?}");
}

/// The pending debug info of a bare-program stepper is spent on the first step: a library
/// without debug info, entered later, does not inherit it.
#[test]
fn a_library_without_debug_info_does_not_inherit_the_pending_one() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let library: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .with_profile(&miden_project::Profile::release())
            .assemble_library("lib", "namespace lib::m pub proc f push.3 drop end", None::<String>)
            .unwrap(),
    );
    assert!(library.debug_info().unwrap().is_none(), "the release profile strips debug info");
    let package: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .with_package(library.clone(), Linkage::Dynamic)
            .unwrap()
            .assemble_program("program", "use lib::m begin exec.m::f end")
            .unwrap(),
    );
    let debug_info = Arc::new(package.debug_info().unwrap().unwrap());
    let mut executor = Executor::new(Vec::new());
    executor.with_package(library.clone()).unwrap();
    let (_, mut host) = executor.into_stepper(package.clone(), source_manager);
    let mut processor = FastProcessor::new(StackInputs::default());
    let ctx = processor.get_initial_resume_context(&package.unwrap_program()).unwrap();
    let mut stepper = Stepper::new(processor, ctx).with_debug_info(debug_info);
    let mut in_library = Vec::new();
    let mut in_program = Vec::new();
    stepper
        .run_sync(&mut host, |cycle| {
            if Arc::ptr_eq(cycle.forest, library.mast_forest()) {
                in_library.push(Record::from(cycle));
            } else {
                in_program.push(Record::from(cycle));
            }
        })
        .unwrap();
    assert!(!in_library.is_empty() && !in_program.is_empty());
    assert!(in_program.iter().all(|record| record.has_debug_info), "{in_program:?}");
    assert!(in_library.iter().all(|record| !record.has_debug_info), "{in_library:?}");
}

/// The stepper can be held across an await inside a `Send` future.
#[test]
fn stepper_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Stepper>();
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
    let mut last = None;
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| last = cycle.op)
        .unwrap_err();
    assert!(
        matches!(last, Some(Operation::Assert(_))),
        "the failing cycle is described: {last:?}"
    );
    assert!(stepper.stopped());
    assert!(stepper.failed());
    // A failed program is not stepped again, and the cycle that failed is not counted.
    assert!(stepper.step_sync(&mut DefaultHost::default(), |_| {}).is_err());
    assert!(stepper.run_sync(&mut DefaultHost::default(), |_| {}).is_err());
    let clock = u32::from(stepper.processor().state().clock()) as usize;
    assert_eq!(stepper.cycle(), clock);
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
    let mut running = stepper(&package, 1, 3);
    running.step_sync(&mut DefaultHost::default(), |_| {}).unwrap();
    assert!(running.into_output().is_err(), "no output while running");

    let mut from_sync = Vec::new();
    stepper(&package, 1, 3)
        .run_sync(&mut DefaultHost::default(), |cycle| from_sync.push(Record::from(cycle)))
        .unwrap();

    let mut stepper = stepper(&package, 1, 3);
    let mut from_async = Vec::new();
    block_on(
        stepper.run(&mut DefaultHost::default(), |cycle| from_async.push(Record::from(cycle))),
    )
    .unwrap();
    assert!(stepper.stopped());
    assert_eq!(from_async.len(), stepper.cycle());
    assert_eq!(from_async, from_sync, "the async path resolves cycles like the sync one");
    // Stepping a finished program is a no-op.
    assert!(!block_on(stepper.step(&mut DefaultHost::default(), |_| {})).unwrap());
    let output = stepper.into_output().unwrap();

    let expected = FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap())
        .execute_sync(&package.unwrap_program(), &mut DefaultHost::default())
        .unwrap();
    assert_eq!(output.stack, expected.stack);
}
