//! Tests of the per-cycle records that [`Stepper`] and [`DebugExecutor`] produce.
//!
//! [`DebugExecutor`]: crate::DebugExecutor

use std::{
    boxed::Box,
    cell::{Cell, RefCell},
    collections::BTreeMap,
    pin::pin,
    rc::Rc,
    string::String,
    task::{Context, Poll, Waker},
};

use miden_assembly::{DefaultSourceManager, Linkage};
use miden_core::{Felt, Word, mast::MastNode};
use miden_debug_types::SourceManager;
use miden_mast_package::Package;
use miden_processor::{
    BaseHost, DefaultHost, ExecutionError, FastProcessor, FutureMaybeSend, Host, LoadedMastForest,
    ProcessorState, StackInputs, advice::AdviceMutation, event::EventError,
};

use super::*;
use crate::{
    exec::{DebuggerHost, Executor, Stepper, poll_immediately},
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

/// The source nodes of the entry point's own tree, each once per place it occupies in it.
///
/// The assembler emits a source tree per procedure and one per call site, so a procedure's
/// nodes appear several times in the debug info; only the entry point's tree executes.
fn entry_tree(package: &Package) -> Vec<DebugSourceNodeId> {
    let debug_info = package.debug_info().unwrap().unwrap();
    let root = debug_info
        .unique_source_root_for_exec_node(package.unwrap_program().entrypoint())
        .unwrap()
        .unwrap();
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        pending.extend(debug_info[id].children.iter().copied());
        found.push(id);
    }
    found
}

/// The source nodes of the entry point's own tree whose exec node satisfies `pred`.
fn source_node(package: &Package, pred: impl Fn(&MastNode) -> bool) -> Vec<u32> {
    let debug_info = package.debug_info().unwrap().unwrap();
    let forest = package.mast_forest();
    let mut found: Vec<u32> = entry_tree(package)
        .into_iter()
        .filter(|id| pred(&forest[debug_info[*id].exec_node]))
        .map(u32::from)
        .collect();
    found.sort_unstable();
    found
}

/// The node of the entry point's own tree that lists `child` among its children, `None` for
/// the tree's root.
///
/// Panics when several nodes list it: the debug info shares one source node between identical
/// subtrees, such as the repetitions of a `repeat`, and such a child has no single parent.
fn listed_by(package: &Package, child: DebugSourceNodeId) -> Option<DebugSourceNodeId> {
    let debug_info = package.debug_info().unwrap().unwrap();
    let mut listing: Vec<u32> = entry_tree(package)
        .into_iter()
        .filter(|id| debug_info[*id].children.contains(&child))
        .map(u32::from)
        .collect();
    listing.sort_unstable();
    listing.dedup();
    assert!(listing.len() <= 1, "{child:?} is listed by {listing:?}");
    listing.pop().map(DebugSourceNodeId::from)
}

/// The root source node of the procedure of `package` whose name ends with `suffix`.
fn procedure_root(package: &Package, suffix: &str) -> DebugSourceNodeId {
    let debug_info = package.debug_info().unwrap().unwrap();
    debug_info
        .functions()
        .iter()
        .find(|function| {
            debug_info.get_string(function.name_idx).is_some_and(|n| n.ends_with(suffix))
        })
        .and_then(|function| function.source_node.into_option())
        .unwrap_or_else(|| panic!("no procedure `{suffix}` with a root"))
}

/// An owned copy of the comparable fields of a [`CycleInfo`].
#[derive(PartialEq, Eq)]
struct Record {
    exec_node: Option<MastNodeId>,
    /// The debug info the cycle was resolved against, compared by value.
    debug_info: Option<Arc<PackageDebugInfo>>,
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
            debug_info: cycle.debug_info.cloned(),
            source_node: cycle.source_node.map(u32::from),
            op: cycle.op,
            op_idx: cycle.op_idx,
            control: cycle.control,
            node_start: cycle.node_start,
            inline_frames: cycle.inline_frames.iter().map(|frame| frame.name().into()).collect(),
        }
    }
}

/// Prints the debug info only by its number of source nodes, `None` when there is none: every
/// record of a forest shares it, and a failing comparison of records would print it whole.
impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("exec_node", &self.exec_node)
            .field("debug_info_nodes", &self.debug_info.as_ref().map(|info| info.nodes().len()))
            .field("source_node", &self.source_node)
            .field("op", &self.op)
            .field("op_idx", &self.op_idx)
            .field("control", &self.control)
            .field("node_start", &self.node_start)
            .field("inline_frames", &self.inline_frames)
            .finish()
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
    // The body is an external node, which leaves a boundary, not a node; the deepest point
    // is reached on every iteration and nothing accumulates across them.
    assert_eq!(deepest, 4, "nodes open at the deepest point");
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
            // Back in the program after the libraries, on the finish of the program's root:
            // nothing of the libraries is open any more.
            assert_eq!(stepper.open_source_nodes(), 0);
        }
    }
    // `f`'s own root is never described, so `a` and `b` get no parent rather than each other.
    let in_mid: Vec<_> = external_starts.iter().filter(|(in_mid, _)| *in_mid).collect();
    assert_eq!(in_mid.len(), 2, "{external_starts:?}");
    assert!(in_mid.iter().all(|(_, start)| start.parent.is_none()), "{external_starts:?}");
    // The program's own external start has the program's root as its parent.
    let root = package
        .debug_info()
        .unwrap()
        .unwrap()
        .unique_source_root_for_exec_node(package.unwrap_program().entrypoint())
        .unwrap()
        .unwrap();
    let in_program: Vec<_> = external_starts.iter().filter(|(in_mid, _)| !*in_mid).collect();
    assert_eq!(in_program.len(), 1);
    assert_eq!(in_program[0].1.parent, Some(root));
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

/// A host that yields once before answering the first forest request or event it gets,
/// whichever comes first, so that a step's future is observed pending.
struct YieldingHost {
    inner: DefaultHost,
    /// Whether the host has yielded; a cell since forests are requested through `&self`.
    yielded: Cell<bool>,
}

impl YieldingHost {
    /// A host over `inner` that has not yielded yet.
    fn new(inner: DefaultHost) -> Self {
        Self {
            inner,
            yielded: Cell::new(false),
        }
    }

    /// A future of `value` that is pending on its first poll if the host has not yielded yet.
    fn answer<T: Send>(&self, value: T) -> impl FutureMaybeSend<T> {
        let mut pending = !self.yielded.replace(true);
        let mut value = Some(value);
        core::future::poll_fn(move |_| {
            if core::mem::take(&mut pending) {
                Poll::Pending
            } else {
                Poll::Ready(value.take().expect("polled after completion"))
            }
        })
    }
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
        self.answer(miden_processor::SyncHost::get_mast_forest(&self.inner, node_digest))
    }

    fn on_event(
        &mut self,
        _process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.answer(Ok(Vec::new()))
    }
}

/// Steps `stepper` until a step's future is pending and drops that future.
fn drop_a_pending_step(stepper: &mut Stepper, host: &mut YieldingHost) {
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        let mut step = pin!(stepper.step(host, |_| {}));
        match step.as_mut().poll(&mut cx) {
            Poll::Pending => break,
            Poll::Ready(Ok(true)) => continue,
            other => panic!("the program ended before yielding: {other:?}"),
        }
    }
}

/// A step whose future is dropped while the host is pending leaves no context behind, but
/// that is not a finished program: the stepper refuses to go on and has no output.
#[test]
fn a_dropped_step_is_not_a_finished_program() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let event_name = "miden-debug::test::yield";
    let event_id = miden_core::events::EventId::from_name(event_name).as_u64();
    // The event id comes from the inputs, so the stack is a valid output while the step is
    // pending: only the state can refuse the output.
    let package = miden_assembly::Assembler::new(source_manager)
        .assemble_program("program", "begin emit end")
        .map(Arc::<Package>::from)
        .unwrap();
    // The host answers the event itself, after yielding once.
    let mut host = YieldingHost::new(DefaultHost::default());
    let inputs = StackInputs::new(&[Felt::new(event_id).unwrap()]).unwrap();
    let mut stepper = Stepper::for_package(FastProcessor::new(inputs), package).unwrap();
    drop_a_pending_step(&mut stepper, &mut host);
    assert!(!stepper.stopped());
    assert!(!stepper.failed());
    assert!(stepper.step_sync(&mut host.inner, |_| {}).is_err());
    assert!(stepper.into_output().is_err());
}

/// An instrument that keeps the operation of every record it receives and notes its turn.
struct RecordingInstrument {
    ops: Rc<RefCell<Vec<Option<Operation>>>>,
    order: Rc<RefCell<Vec<&'static str>>>,
}

impl Instrument for RecordingInstrument {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn on_operation_execution_cycle(&mut self, _op: Operation, _proc: Option<&str>) {}

    fn on_cycle(&mut self, cycle: &CycleInfo<'_>) {
        self.ops.borrow_mut().push(cycle.op);
        self.order.borrow_mut().push("instrument");
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
        instruments: vec![Box::new(RecordingInstrument {
            ops: records.clone(),
            order: order.clone(),
        })],
        ..Default::default()
    });
    let mut exec = executor.into_debug(package, source_manager);
    let mut outcome = Ok(None);
    while !exec.stopped {
        let order_observer = order.clone();
        let order_cycle = order.clone();
        outcome = exec.step_impl(
            move |_| order_observer.borrow_mut().push("observer"),
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
    assert_eq!(order.len(), 3 * records.len());
    assert!(
        order.chunks(3).all(|turn| turn == ["observer", "instrument", "on_cycle"]),
        "{order:?}"
    );
}

/// The pending debug info of a bare-program stepper is spent on the first step: a library
/// without debug info, entered later, does not inherit it.
#[test]
fn a_library_without_debug_info_does_not_inherit_the_pending_one() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let mut library = *miden_assembly::Assembler::new(source_manager.clone())
        .assemble_library("lib", "namespace lib::m pub proc f push.3 drop end", None::<String>)
        .unwrap();
    library.strip_debug_info().unwrap();
    let library = Arc::new(library);
    assert!(library.debug_info().unwrap().is_none());
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
    assert!(in_program.iter().all(|record| record.debug_info.is_some()), "{in_program:?}");
    assert!(in_library.iter().all(|record| record.debug_info.is_none()), "{in_library:?}");
}

/// The stepper can be held across an await inside a `Send` future.
#[test]
fn stepper_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Stepper>();
}

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
    let mut stepper = Stepper::new(processor, ctx).with_debug_info(debug_info.clone());
    let mut from_program = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| from_program.push(Record::from(cycle)))
        .unwrap();

    let first = &from_program[0];
    assert_eq!(first.debug_info.as_deref(), Some(&*debug_info), "{first:?}");
    assert!(first.source_node.is_some(), "{first:?}");
    assert!(first.node_start.is_some_and(|start| start.parent.is_none()), "{first:?}");
    assert_eq!(from_program, from_package);

    // The async path attributes the first cycle the same way.
    let mut processor = FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap());
    let ctx = processor.get_initial_resume_context(&package.unwrap_program()).unwrap();
    let mut stepper = Stepper::new(processor, ctx).with_debug_info(debug_info.clone());
    let mut from_async = Vec::new();
    poll_immediately(
        stepper.run(&mut DefaultHost::default(), |cycle| from_async.push(Record::from(cycle))),
    )
    .unwrap();
    assert_eq!(from_async, from_package);

    // A context that carries debug info keeps it: a different package's debug info, supplied
    // as pending, changes nothing.
    let other = miden_assembly::Assembler::default()
        .assemble_program("other", "begin push.1 drop end")
        .map(Arc::<Package>::from)
        .unwrap();
    let other_info = Arc::new(other.debug_info().unwrap().unwrap());
    let mut stepper = Stepper::for_package(
        FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap()),
        package.clone(),
    )
    .unwrap()
    .with_debug_info(other_info);
    let mut with_other = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| with_other.push(Record::from(cycle)))
        .unwrap();
    assert_eq!(with_other, from_package);
}

/// A forest without debug info between two with some: the boundary of the stripped forest's
/// external start is pushed although the start has no source node, so the leaf's loop is
/// popped on return and the next call's loop neither chains nor names itself.
#[test]
fn a_boundary_is_kept_for_a_forest_without_debug_info() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let leaf: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library(
                "lib",
                "namespace lib::loops pub proc drain while.true push.0 end end",
                None::<String>,
            )
            .unwrap(),
    );
    let mut mid = *miden_assembly::Assembler::new(source_manager.clone())
        .with_package(leaf.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_library(
            "mid",
            "namespace mid::glue use lib::loops pub proc f exec.loops::drain exec.loops::drain end",
            None::<String>,
        )
        .unwrap();
    mid.strip_debug_info().unwrap();
    let mid = Arc::new(mid);
    assert!(mid.debug_info().unwrap().is_none());
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .with_package(mid.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program("program", "use mid::glue begin exec.glue::f end")
        .map(Arc::<Package>::from)
        .unwrap();

    let mut executor = Executor::new(vec![Felt::ONE, Felt::ONE]);
    executor.with_package(leaf.clone()).unwrap();
    executor.with_package(mid.clone()).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package, source_manager);
    let mut loop_starts = Vec::new();
    let mut open_counts = Vec::new();
    while stepper
        .step_sync(&mut host, |cycle| {
            if let Some(start) = cycle.node_start
                && Arc::ptr_eq(cycle.forest, leaf.mast_forest())
                && matches!(
                    cycle.forest.get_node_by_id(cycle.exec_node.unwrap()),
                    Some(MastNode::Loop(_))
                )
            {
                loop_starts.push(start);
            }
        })
        .unwrap()
    {
        open_counts.push(stepper.open_source_nodes());
    }
    assert_eq!(loop_starts.len(), 2, "{loop_starts:?}");
    assert!(loop_starts.iter().all(|start| start.parent.is_none()), "{loop_starts:?}");
    // Never the first call's loop under the second call's: the deepest point is the same on
    // both calls.
    assert_eq!(open_counts.iter().max(), Some(&3), "{open_counts:?}");
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// A procedure entered by `call` that re-enters itself through `dynexec` in the same forest:
/// the open `call` node lists the procedure's root, so only the hand-over on the dynamic
/// boundary names the dynamic node as the parent of the dynamic entries.
#[test]
fn dynamic_entries_in_the_same_forest_name_the_dynamic_node() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    // `f` gets the counter on top of its own hash, stores the hash in its context, and
    // re-enters itself dynamically while the counter is not zero: one entry through `call`,
    // two through `dynexec`.
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(
            "program",
            "proc f movdn.4 mem_storew_le.100 movup.4 dup neq.0 if.true sub.1 push.100 dynexec \
             else nop end end begin procref.f push.2 call.f drop dropw end",
        )
        .map(Arc::<Package>::from)
        .unwrap();
    let forest = package.mast_forest().clone();
    let debug_info = package.debug_info().unwrap().unwrap();
    let root = procedure_root(&package, "::f");
    let mut stepper = stepper(&package, 0, 0);
    let mut entries = Vec::new();
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| {
            if let Some(start) = cycle.node_start
                && start.node == root
            {
                let kind = start
                    .parent
                    .and_then(|parent| debug_info.source_node(parent))
                    .and_then(|parent| forest.get_node_by_id(parent.exec_node))
                    .map(|node| match node {
                        MastNode::Call(_) => "call",
                        MastNode::Dyn(_) => "dyn",
                        _ => "other",
                    });
                entries.push(kind);
            }
        })
        .unwrap();
    assert_eq!(entries, [Some("call"), Some("dyn"), Some("dyn")]);
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// A library whose one procedure, `lib::t::trampoline`, has `trampoline` as its body, without
/// debug info when `strip` is set, and a program that links it, has a procedure `target` of its
/// own, stores `target`'s hash at address 100, and then runs `body`.
fn trampoline_packages(
    source_manager: &Arc<DefaultSourceManager>,
    trampoline: &str,
    strip: bool,
    body: &str,
) -> (Arc<Package>, Arc<Package>) {
    let mut library = *miden_assembly::Assembler::new(source_manager.clone())
        .assemble_library(
            "lib",
            format!("namespace lib::t pub proc trampoline {trampoline} end"),
            None::<String>,
        )
        .unwrap();
    if strip {
        library.strip_debug_info().unwrap();
    }
    let library = Arc::new(library);
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .with_package(library.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program(
            "program",
            format!(
                "use lib::t proc target push.1 drop end begin procref.target mem_storew_le.100 \
                 dropw {body} end"
            ),
        )
        .map(Arc::<Package>::from)
        .unwrap();
    (library, package)
}

/// A stepper over `package` and its host, which also serves `package`'s own forest so that a
/// dynamic call from `library` back into the program finds it.
fn callback_stepper(
    library: &Arc<Package>,
    package: &Arc<Package>,
    source_manager: Arc<DefaultSourceManager>,
) -> (Stepper, DebuggerHost<dyn SourceManager>) {
    let mut executor = Executor::new(Vec::new());
    executor.with_package(library.clone()).unwrap();
    executor.with_package(package.clone()).unwrap();
    executor.into_stepper(package.clone(), source_manager)
}

/// A library procedure whose root is itself a dynamic node, calling back into the program:
/// the program's external boundary is popped on the return, not on the callback, so the
/// target's root has no parent and nothing of the callback stays open. The second shape, where
/// the dynamic node is not the root, pushes a boundary of its own and is the control.
#[test]
fn a_callback_through_a_trampoline_does_not_pop_the_boundary_early() {
    for trampoline in ["dynexec", "push.100 dynexec"] {
        let source_manager = Arc::new(DefaultSourceManager::default());
        // The first variant takes the address from the caller's stack.
        let address = if trampoline == "dynexec" {
            "push.100 "
        } else {
            ""
        };
        let (library, package) = trampoline_packages(
            &source_manager,
            trampoline,
            false,
            &format!("{address}exec.t::trampoline push.9 drop"),
        );
        let (mut stepper, mut host) = callback_stepper(&library, &package, source_manager);
        let mut starts = Vec::new();
        let mut open_at_external = None;
        let mut open_on_return = None;
        let mut returned = false;
        while stepper
            .step_sync(&mut host, |cycle| {
                let in_program = Arc::ptr_eq(cycle.forest, package.mast_forest());
                if let Some(start) = cycle.node_start {
                    let external = cycle
                        .forest
                        .get_node_by_id(cycle.exec_node.unwrap())
                        .is_some_and(|node| matches!(node, MastNode::External(_)));
                    starts.push((in_program, external, start));
                }
                if cycle.op == Some(Operation::Push(Felt::new(9).unwrap())) {
                    returned = true;
                }
            })
            .unwrap()
        {
            let open = stepper.open_source_nodes();
            // The counts are read after the step: right after the external start, and right
            // after the `push.9` cycle, the third cycle back in the program.
            if open_at_external.is_none()
                && starts.last().is_some_and(|(in_program, external, _)| *in_program && *external)
            {
                open_at_external = Some(open);
            } else if returned && open_on_return.is_none() {
                open_on_return = Some(open);
            }
        }
        let external = starts.iter().position(|(in_program, external, _)| *in_program && *external);
        let external = external.unwrap_or_else(|| panic!("{trampoline}: no external start"));
        // The first start back in the program after the external one is the target's root.
        let target = starts[external + 1..]
            .iter()
            .find(|(in_program, ..)| *in_program)
            .unwrap_or_else(|| panic!("{trampoline}: no callback start in {starts:?}"));
        assert_eq!(target.2.parent, None, "{trampoline}: {starts:?}");
        assert!(returned, "{trampoline}");
        // Between the two reads the join holding the external node closed and the block holding
        // `push.9` opened, so the counts are equal when nothing of the trampoline or the
        // callback is left open.
        assert_eq!(open_on_return, open_at_external, "{trampoline}");
        assert_eq!(stepper.open_source_nodes(), 0, "{trampoline}");
    }
}

/// A dynamic call into another package: the callee's root start is described, with no
/// parent, and the dynamic node's finish pops what the callee left open.
#[test]
fn dynamic_calls_into_another_forest_are_bounded() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let library: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library("lib", "namespace lib::m pub proc f push.7 drop end", None::<String>)
            .unwrap(),
    );
    let package = miden_assembly::Assembler::new(source_manager.clone())
        .with_package(library.clone(), Linkage::Dynamic)
        .unwrap()
        .assemble_program(
            "program",
            "use lib::m begin procref.m::f mem_storew_le.100 dropw push.100 dyncall drop push.100 \
             dyncall drop end",
        )
        .map(Arc::<Package>::from)
        .unwrap();
    let mut executor = Executor::new(Vec::new());
    executor.with_package(library.clone()).unwrap();
    let (mut stepper, mut host) = executor.into_stepper(package.clone(), source_manager);
    let mut roots = Vec::new();
    let mut dynamic_nodes = Vec::new();
    let mut in_program = Vec::new();
    let mut deepest = 0;
    while stepper
        .step_sync(&mut host, |cycle| {
            if let Some(start) = cycle.node_start {
                if Arc::ptr_eq(cycle.forest, library.mast_forest()) {
                    roots.push(start);
                    return;
                }
                in_program.push(start);
                if matches!(
                    cycle.forest.get_node_by_id(cycle.exec_node.unwrap()),
                    Some(MastNode::Dyn(_))
                ) {
                    dynamic_nodes.push(start);
                }
            }
        })
        .unwrap()
    {
        deepest = deepest.max(stepper.open_source_nodes());
    }
    assert_eq!(roots.len(), 2, "{roots:?}");
    assert!(roots.iter().all(|start| start.parent.is_none()), "{roots:?}");
    // The second dynamic node's parent is the program's node that lists it.
    assert_eq!(dynamic_nodes.len(), 2, "{dynamic_nodes:?}");
    let second = dynamic_nodes[1];
    assert_eq!(second.parent, listed_by(&package, second.node), "{dynamic_nodes:?}");
    // Every start in the program names the node that lists it: a boundary of the first call
    // left open would hide the nodes opened before it, so the join holding the second call
    // would get no parent.
    for start in &in_program {
        assert_eq!(start.parent, listed_by(&package, start.node), "{in_program:?}");
    }
    // The deepest point is reached on both calls: the first leaves nothing behind.
    assert_eq!(deepest, 5);
    assert_eq!(stepper.open_source_nodes(), 0);
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
    let mut records = 0;
    stepper
        .run_sync(&mut DefaultHost::default(), |cycle| {
            last = cycle.op;
            records += 1;
        })
        .unwrap_err();
    assert!(
        matches!(last, Some(Operation::Assert(_))),
        "the failing cycle is described: {last:?}"
    );
    assert_eq!(records, stepper.cycle() + 1, "the failing cycle is described, not counted");
    assert!(stepper.stopped());
    assert!(stepper.failed());
    // A failed program is not stepped again, stays failed, and the cycle that failed is not
    // counted: the clock stays at the five cycles that completed before it.
    assert!(stepper.step_sync(&mut DefaultHost::default(), |_| {}).is_err());
    assert!(stepper.failed() && stepper.stopped());
    assert_eq!(stepper.cycle(), 5);
    assert!(stepper.run_sync(&mut DefaultHost::default(), |_| {}).is_err());
    assert!(stepper.into_output().is_err());
}

/// `exec` of a MASM procedure is recorded as an inline call: the frames name the procedure
/// while its body runs and are empty at the program's own level.
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

/// The async path produces the sync path's records, refuses the output while running, and
/// exports the processor's own outputs once finished.
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
    poll_immediately(
        stepper.run(&mut DefaultHost::default(), |cycle| from_async.push(Record::from(cycle))),
    )
    .unwrap();
    assert!(stepper.stopped());
    assert_eq!(from_async.len(), stepper.cycle());
    assert_eq!(from_async, from_sync, "the async path resolves cycles like the sync one");
    // Stepping a finished program is a no-op.
    assert!(!poll_immediately(stepper.step(&mut DefaultHost::default(), |_| {})).unwrap());
    let output = stepper.into_output().unwrap();

    let expected = FastProcessor::new(StackInputs::new(&inputs(1, 3)).unwrap())
        .execute_sync(&package.unwrap_program(), &mut DefaultHost::default())
        .unwrap();
    assert_eq!(output.stack, expected.stack);
}

/// A library without debug info whose trampoline is a bare `dynexec`, and a program that also
/// stores the trampoline's hash at address 104 before it runs `body`.
fn stripped_trampoline(
    source_manager: &Arc<DefaultSourceManager>,
    body: &str,
) -> (Arc<Package>, Arc<Package>) {
    trampoline_packages(
        source_manager,
        "dynexec",
        true,
        &format!("procref.t::trampoline mem_storew_le.104 dropw {body}"),
    )
}

/// A dynamic node without a source node, the root of a library without debug info entered
/// dynamically, calling back into the program. The callback target's root gets no parent.
/// Once the callback has returned, only the program's own three nodes are open, every start
/// after the return names the program's node that lists it, and nothing is open at the end.
///
/// Whether the library's boundary is classified as dynamic or external is not observable here:
/// an external one would be popped one cycle later, by the finish of the program's dynamic
/// node, with nothing observable between. What the test pins is that the boundary is popped at
/// all, although its dynamic node has no source node and no forest is compared.
#[test]
fn a_dynamic_root_without_a_source_node_pops_its_boundary() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    // After the return a split starts, under the join that holds it and the block before it.
    let (library, package) = stripped_trampoline(
        &source_manager,
        "push.100 push.104 dynexec push.7 drop push.1 if.true push.9 drop end",
    );
    let target = procedure_root(&package, "::target");
    let (mut stepper, mut host) = callback_stepper(&library, &package, source_manager);
    let mut target_starts = Vec::new();
    let mut returned = false;
    let mut starts_after_return = Vec::new();
    let mut open_after_return = None;
    while stepper
        .step_sync(&mut host, |cycle| {
            if cycle.op == Some(Operation::Push(Felt::new(7).unwrap())) {
                returned = true;
            }
            if let Some(start) = cycle.node_start
                && Arc::ptr_eq(cycle.forest, package.mast_forest())
            {
                if start.node == target {
                    target_starts.push(start);
                }
                if returned {
                    starts_after_return.push(start);
                }
            }
        })
        .unwrap()
    {
        if returned && open_after_return.is_none() {
            open_after_return = Some(stepper.open_source_nodes());
        }
    }
    assert!(returned);
    assert_eq!(target_starts.len(), 1, "{target_starts:?}");
    assert_eq!(target_starts[0].parent, None, "{target_starts:?}");
    // Read after the `push.7` cycle: the root join, the join holding the rest of the body, and
    // the block running `push.7`.
    assert_eq!(open_after_return, Some(3));
    // The split and its `then` block name the program's nodes that list them.
    assert_eq!(starts_after_return.len(), 2, "{starts_after_return:?}");
    for start in &starts_after_return {
        assert_eq!(start.parent, listed_by(&package, start.node), "{starts_after_return:?}");
    }
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// The same trampoline in a loop of 50 iterations. Every callback target's root gets no parent,
/// the library starts no source node since it has no debug info, the open count after each
/// return from the trampoline is the one before its call, so it never grows across the
/// iterations, and nothing is open at the end.
#[test]
fn a_dynamic_root_without_a_source_node_in_a_loop_stays_bounded() {
    let iterations = 50;
    let source_manager = Arc::new(DefaultSourceManager::default());
    let (library, package) = stripped_trampoline(
        &source_manager,
        &format!("repeat.{iterations} push.100 push.104 dynexec end"),
    );
    let target = procedure_root(&package, "::target");
    let (mut stepper, mut host) = callback_stepper(&library, &package, source_manager);
    let mut target_starts = Vec::new();
    let mut library_starts = Vec::new();
    // The open counts before each start of the program's dynamic node and after its finish.
    let mut before_calls = Vec::new();
    let mut after_returns = Vec::new();
    loop {
        let before = stepper.open_source_nodes();
        // Whether the cycle starts (`true`) or finishes the program's dynamic node.
        let mut dynamic = None;
        let running = stepper
            .step_sync(&mut host, |cycle| {
                if !Arc::ptr_eq(cycle.forest, package.mast_forest()) {
                    library_starts.extend(cycle.node_start);
                    return;
                }
                if let Some(start) = cycle.node_start
                    && start.node == target
                {
                    target_starts.push(start);
                }
                if matches!(
                    cycle.forest.get_node_by_id(cycle.exec_node.unwrap()),
                    Some(MastNode::Dyn(_))
                ) {
                    dynamic = Some(cycle.node_start.is_some());
                }
            })
            .unwrap();
        match dynamic {
            Some(true) => before_calls.push(before),
            Some(false) => after_returns.push(stepper.open_source_nodes()),
            None => {}
        }
        if !running {
            break;
        }
    }
    assert!(library_starts.is_empty(), "{library_starts:?}");
    assert_eq!(target_starts.len(), iterations, "{target_starts:?}");
    assert!(target_starts.iter().all(|start| start.parent.is_none()), "{target_starts:?}");
    assert_eq!(before_calls.len(), iterations);
    assert_eq!(after_returns, before_calls);
    assert_eq!(stepper.open_source_nodes(), 0);
}

/// Debug info can only be attributed from the first step: after a step that completed, the
/// pending info would land on whatever context comes next.
#[test]
#[should_panic(expected = "attributed from the first step")]
fn debug_info_after_a_completed_step_panics() {
    let (package, _) = fixture();
    let debug_info = Arc::new(package.debug_info().unwrap().unwrap());
    let mut stepper = stepper(&package, 1, 3);
    stepper.step_sync(&mut DefaultHost::default(), |_| {}).unwrap();
    let _ = stepper.with_debug_info(debug_info);
}

/// Nor after a step whose future was dropped before its cycle completed: the clock is still
/// zero, but the context the first step consumes is gone with the dropped future.
#[test]
#[should_panic(expected = "debug info is attributed from the first step")]
fn debug_info_after_a_step_dropped_at_clock_zero_panics() {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let leaf: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager.clone())
            .assemble_library("lib", "namespace lib::m pub proc f push.3 drop end", None::<String>)
            .unwrap(),
    );
    let mid: Arc<Package> = Arc::from(
        miden_assembly::Assembler::new(source_manager)
            .with_package(leaf, Linkage::Dynamic)
            .unwrap()
            .assemble_library(
                "mid",
                "namespace mid::m use lib::m pub proc g exec.m::f end",
                None::<String>,
            )
            .unwrap(),
    );
    // An assembled program starts with a block of its own, so the bare program runs `g`, a
    // library procedure whose root is the external node: the first step asks the host for the
    // leaf's forest before its cycle completes. The host never gets to answer, so it needs no
    // forest.
    let forest = mid.mast_forest().clone();
    let root = *forest
        .procedure_roots()
        .iter()
        .find(|root| matches!(forest[**root], MastNode::External(_)))
        .expect("`g`'s root is the external node");
    let program = miden_core::program::Program::new(forest, root);
    let debug_info = Arc::new(mid.debug_info().unwrap().unwrap());
    let mut host = YieldingHost::new(DefaultHost::default());
    let mut processor = FastProcessor::new(StackInputs::default());
    let ctx = processor.get_initial_resume_context(&program).unwrap();
    let mut stepper = Stepper::new(processor, ctx);
    drop_a_pending_step(&mut stepper, &mut host);
    assert_eq!(stepper.cycle(), 0);
    let _ = stepper.with_debug_info(debug_info);
}

/// A cycle that completes and then trips the cycle limit is counted and described like any
/// other; only a cycle whose operation fails is left out of the count.
#[test]
fn a_cycle_that_trips_the_limit_is_counted() {
    let (package, _) = fixture();
    let default = miden_processor::ExecutionOptions::default();
    let options =
        miden_processor::ExecutionOptions::new(Some(64), 64, default.core_trace_fragment_size())
            .unwrap();
    let processor = FastProcessor::new_with_options(
        StackInputs::new(&inputs(1, 3)).unwrap(),
        miden_processor::advice::AdviceInputs::default(),
        options,
    )
    .unwrap();
    let mut stepper = Stepper::for_package(processor, package).unwrap();
    let mut records = 0;
    let error = stepper.run_sync(&mut DefaultHost::default(), |_| records += 1).unwrap_err();
    assert!(matches!(error, ExecutionError::CycleLimitExceeded(64)), "{error:?}");
    assert_eq!(records, stepper.cycle());
    assert!(stepper.failed());
}
