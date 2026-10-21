//! The per-cycle view handed to instruments, and the open-node tracking behind it.
//!
//! The source metadata of a cycle is selected once per step by the shared source resolution
//! (`SourceContext`). This module adds what instruments need on top of it: the forest, the
//! exec node, and, for a cycle that starts a source node, the source node that started it.
//! [`DebugExecutor`] and [`Stepper`] both produce it, so there is one reading of the
//! continuation stream.
//!
//! [`DebugExecutor`]: crate::DebugExecutor
//! [`Stepper`]: crate::Stepper

use alloc::{sync::Arc, vec::Vec};

use miden_core::{
    mast::{MastForest, MastNode, MastNodeId},
    operations::Operation,
};
use miden_mast_package::debug_info::{DebugSourceNodeId, PackageDebugInfo};
use miden_processor::{Continuation, ResumeContext};

use super::source::SourceContext;
use crate::debug::{ControlFlowOp, InlineCallFrame};

/// What the next step will execute, resolved to source.
///
/// Produced once per VM cycle and handed to instruments before the cycle runs, so a cycle that
/// fails is described like any other. The finalization step that follows the last cycle
/// produces no record. The references borrow the resume context the step is about to consume
/// and the source metadata selected for it.
///
/// A step whose node is a MAST external node resolves the callee's forest and runs the callee
/// root's first cycle without stopping in between, so the callee root's own start is never
/// described: the first record in the callee's forest is already inside the root.
#[derive(Debug)]
pub struct CycleInfo<'a> {
    /// The forest the step executes in.
    pub forest: &'a Arc<MastForest>,
    /// The debug info of that forest's package, when it carries any.
    pub debug_info: Option<&'a Arc<PackageDebugInfo>>,
    /// The MAST node the step works on. In step mode every described cycle has one.
    pub exec_node: Option<MastNodeId>,
    /// The source occurrence of `exec_node`, when the package has debug info.
    pub source_node: Option<DebugSourceNodeId>,
    /// The operation the step executes; `None` on a control-flow cycle.
    pub op: Option<Operation>,
    /// The padded index of `op` within its basic block, the index the debug-info rows are
    /// keyed by. On a `RESPAN` cycle it is the index of the first operation of the next batch.
    pub op_idx: Option<u32>,
    /// The control-flow operation of the cycle, when it is one the call stack tracks.
    pub control: Option<ControlFlowOp>,
    /// The source node this step starts, if it starts one.
    pub node_start: Option<NodeStart>,
    /// The inline call frames active for the step, innermost first.
    pub inline_frames: &'a [InlineCallFrame],
}

/// A start of a source node together with the node that started it.
///
/// A procedure entered by `call`, dynamically, or from another package has one source tree
/// however many places call it, so a node's own start count cannot tell the call sites apart;
/// the parent can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStart {
    /// The source node that starts.
    pub node: DebugSourceNodeId,
    /// The innermost open source node of the same forest that lists `node` as a child, or the
    /// innermost open node of that forest when none does. A root entered dynamically from the
    /// same forest gets the dynamic node. `None` when nothing is open since the forest was
    /// entered: a root entered dynamically from another package, and the children of a root
    /// entered through an external node, whose own start is never described.
    pub parent: Option<DebugSourceNodeId>,
}

/// Keeps the open source nodes between steps so a node start can name its parent.
///
/// A start pushes, a finish pops through the node, and a loop's per-iteration finish pops the
/// body while keeping the loop. A start whose callee may run in another forest, an external
/// or a dynamic node, also pushes a boundary: whatever the callee's forest leaves open, such
/// as the children of a root whose start was never described, is popped with the boundary on
/// the first cycle back in this forest, and a parent is only ever looked for above the
/// innermost boundary. One tracker follows one execution from start to end; it must see every
/// cycle, in order.
#[derive(Debug, Default)]
pub struct SourceTracker {
    /// Open nodes and forest boundaries, innermost last.
    open: Vec<Entry>,
    /// Positions of the boundaries in `open`, innermost last.
    boundaries: Vec<usize>,
}

/// One entry of the open stack.
#[derive(Debug)]
enum Entry {
    /// A source node that started and has not finished.
    Node {
        /// The forest the node belongs to. Forests are told apart by the address of their
        /// shared allocation; the clone keeps the address from being reused while the entry
        /// is open.
        forest: Arc<MastForest>,
        node: DebugSourceNodeId,
    },
    /// The boundary left by a start whose callee may run in another forest.
    Boundary {
        /// The forest that left through the boundary.
        forest: Arc<MastForest>,
        /// The dynamic node that left, when the boundary is a dynamic node's; an external
        /// node leaves no entry of its own.
        dynamic: Option<DebugSourceNodeId>,
    },
}

impl SourceTracker {
    /// Creates a tracker with no open node.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of source nodes currently open; boundaries are not counted.
    pub fn open_nodes(&self) -> usize {
        self.open.iter().filter(|entry| matches!(entry, Entry::Node { .. })).count()
    }

    /// Describes the cycle that will consume `ctx`, whose source metadata is `source`, and
    /// records its effect on the open nodes. `None` for the finalization step after the last
    /// cycle, whose context has no continuation left.
    ///
    /// Must be called exactly once before each step.
    pub(crate) fn observe<'a>(
        &mut self,
        ctx: &'a ResumeContext,
        source: &'a SourceContext,
    ) -> Option<CycleInfo<'a>> {
        // After the last cycle the processor hands back a context with nothing left to run;
        // the step that consumes it only finalizes the execution.
        let next = ctx.continuation_stack().peek_continuation()?;
        let forest = ctx.current_forest();
        let cycle = source.cycle;

        // The processor pops forest switches when it builds a resume context, so the next
        // clock has exactly one continuation and it is the one `source` described.
        debug_assert!(
            !matches!(next, Continuation::EnterForest { .. }),
            "a resume context never starts with a forest switch"
        );
        let returned_from = self.return_to(forest);
        let node_start = match next {
            Continuation::StartNode(exec_node) => {
                // The boundary does not depend on the node having a source node: a forest
                // without debug info still calls into forests that have some.
                let kind = forest.get_node_by_id(*exec_node);
                let node = cycle.source_node_id;
                let start = node.map(|node| {
                    // A same-forest dynamic callee pops its boundary on its first cycle,
                    // which is its root's start: the dynamic node is the parent, whatever
                    // else the frame holds.
                    let parent = returned_from.or_else(|| {
                        source.debug_info().and_then(|info| self.parent_of(forest, info, node))
                    });
                    NodeStart { node, parent }
                });
                match kind {
                    // An external node has no finish in this forest: its callee runs
                    // elsewhere and execution comes back to the next continuation.
                    Some(MastNode::External(_)) => self.push_boundary(forest, None),
                    // A dynamic node has a finish, but its callee may run elsewhere too.
                    Some(MastNode::Dyn(_)) => {
                        if let Some(node) = node {
                            self.push_node(forest, node);
                        }
                        self.push_boundary(forest, node);
                    }
                    _ => {
                        if let Some(node) = node {
                            self.push_node(forest, node);
                        }
                    }
                }
                start
            }
            Continuation::FinishJoin(_)
            | Continuation::FinishSplit(_)
            | Continuation::FinishCall(_)
            | Continuation::FinishDyn(_)
            | Continuation::FinishBasicBlock(_) => {
                if let Some(node) = cycle.source_node_id {
                    self.close(forest, node, false);
                }
                None
            }
            // A loop's finish runs once per iteration and rechecks the condition, so the loop
            // stays open; its body, done with this iteration, is popped. The loop itself is
            // popped by the finish of the node that holds it, or by a boundary.
            Continuation::FinishLoop(_) => {
                if let Some(node) = cycle.source_node_id {
                    self.close(forest, node, true);
                }
                None
            }
            _ => None,
        };

        Some(CycleInfo {
            forest,
            debug_info: source.debug_info(),
            exec_node: cycle.exec_node,
            source_node: cycle.source_node_id,
            op: cycle.op,
            op_idx: cycle.op_idx.map(|idx| idx as u32),
            control: cycle.control_flow_kind,
            node_start,
            inline_frames: source.inline_frames(),
        })
    }

    /// Opens `node` of `forest`.
    fn push_node(&mut self, forest: &Arc<MastForest>, node: DebugSourceNodeId) {
        self.open.push(Entry::Node {
            forest: forest.clone(),
            node,
        });
    }

    /// Marks that `forest` leaves through `dynamic`, or through an external node.
    fn push_boundary(&mut self, forest: &Arc<MastForest>, dynamic: Option<DebugSourceNodeId>) {
        self.boundaries.push(self.open.len());
        self.open.push(Entry::Boundary {
            forest: forest.clone(),
            dynamic,
        });
    }

    /// Position of the first entry above the innermost boundary: the start of the current
    /// frame, what has been opened since the current forest was entered, or since the start
    /// when it never left.
    fn frame_start(&self) -> usize {
        self.boundaries.last().map_or(0, |position| position + 1)
    }

    /// Pops everything from `position` on, boundaries included.
    fn truncate(&mut self, position: usize) {
        self.open.truncate(position);
        while self.boundaries.last().is_some_and(|boundary| *boundary >= position) {
            self.boundaries.pop();
        }
    }

    /// Pops the innermost boundary and everything above it once execution is back in the
    /// forest that left through it. Returns the dynamic node that left, if it was one.
    ///
    /// A callee that is in the same forest as its caller pops the boundary on its first
    /// cycle, so it opens its nodes in the caller's frame; that only loses the boundary's
    /// cleanup for a self-referencing external node, which the assembler does not emit.
    fn return_to(&mut self, forest: &Arc<MastForest>) -> Option<DebugSourceNodeId> {
        let position = *self.boundaries.last()?;
        let Entry::Boundary {
            forest: left,
            dynamic,
        } = &self.open[position]
        else {
            unreachable!("boundary positions point at boundaries")
        };
        if !Arc::ptr_eq(left, forest) {
            return None;
        }
        let dynamic = *dynamic;
        self.truncate(position);
        dynamic
    }

    /// The innermost open node of `forest` that lists `child` among its children, or the
    /// innermost open node of that forest when none does.
    ///
    /// Only the current frame is searched, so a node of a root entered from another package,
    /// whose start was never described, gets no parent rather than a node of the caller. The
    /// second rule covers a node whose parent is open but does not list it, which happens when
    /// the debug info and the forest disagree; a loop that exited but is still open never
    /// lists its sibling as a child, so the first rule skips it.
    fn parent_of(
        &self,
        forest: &Arc<MastForest>,
        debug_info: &PackageDebugInfo,
        child: DebugSourceNodeId,
    ) -> Option<DebugSourceNodeId> {
        let same_forest =
            self.open[self.frame_start()..].iter().rev().filter_map(|entry| match entry {
                Entry::Node { forest: open, node } if Arc::ptr_eq(open, forest) => Some(*node),
                _ => None,
            });
        let lists_child = |node: &DebugSourceNodeId| {
            debug_info
                .source_node(*node)
                .is_some_and(|source| source.children.contains(&child))
        };
        same_forest.clone().find(lists_child).or_else(|| same_forest.clone().next())
    }

    /// Pops the open nodes above `node` of `forest`, and `node` itself unless `keep` is set.
    /// Leaves the stack alone when the node is not open in the current frame.
    fn close(&mut self, forest: &Arc<MastForest>, node: DebugSourceNodeId, keep: bool) {
        let frame_start = self.frame_start();
        let position = self.open[frame_start..].iter().rposition(|entry| {
            matches!(entry, Entry::Node { forest: open, node: open_node }
                if *open_node == node && Arc::ptr_eq(open, forest))
        });
        if let Some(position) = position {
            self.truncate(frame_start + position + usize::from(keep));
        }
    }
}

#[cfg(test)]
mod tests;
