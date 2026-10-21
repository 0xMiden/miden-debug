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
    mast::{MastForest, MastNodeId},
    operations::Operation,
};
use miden_mast_package::debug_info::{DebugSourceNodeId, PackageDebugInfo};
use miden_processor::{Continuation, ResumeContext};

use super::source::SourceContext;
use crate::debug::{ControlFlowOp, InlineCallFrame};

/// What the next step will execute, resolved to source.
///
/// Produced once per step and handed to instruments before the step runs, so a cycle that
/// fails is described like any other. Every reference borrows the resume context the step is
/// about to consume.
#[derive(Debug)]
pub struct CycleInfo<'a> {
    /// The forest the step executes in.
    pub forest: &'a Arc<MastForest>,
    /// The debug info of that forest's package, when it carries any.
    pub debug_info: Option<&'a Arc<PackageDebugInfo>>,
    /// The MAST node the step works on; `None` when the step only switches forests.
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
/// A procedure has one source tree however many places call it, so a node's own start count
/// cannot tell the call sites apart; the parent can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStart {
    /// The source node that starts.
    pub node: DebugSourceNodeId,
    /// The innermost open source node of the same forest that lists `node` as a child, or the
    /// innermost open node of that forest when none does. `None` when nothing of the forest is
    /// open, which happens for the children of a root entered from another package.
    pub parent: Option<DebugSourceNodeId>,
}

/// Keeps the open source nodes between steps so a node start can name its parent.
///
/// A start pushes, a finish pops through the node, and a loop's per-iteration finish leaves the
/// loop open. One tracker follows one execution from start to end; it must see every cycle, in
/// order.
#[derive(Debug, Default)]
pub struct SourceTracker {
    /// Nodes that have started and not finished, innermost last, each with its forest.
    open: Vec<(Arc<MastForest>, DebugSourceNodeId)>,
}

impl SourceTracker {
    /// Creates a tracker with no open node.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of source nodes currently open.
    pub fn open_nodes(&self) -> usize {
        self.open.len()
    }

    /// Describes the cycle that will consume `ctx`, whose source metadata is `source`, and
    /// records its effect on the open nodes.
    ///
    /// Must be called exactly once before each step.
    pub(crate) fn observe<'a>(
        &mut self,
        ctx: &'a ResumeContext,
        source: &'a SourceContext,
    ) -> CycleInfo<'a> {
        let forest = ctx.current_forest();
        let cycle = source.cycle;

        // The processor pops forest switches when it builds a resume context, so the next
        // clock has exactly one continuation and it is the one `source` described.
        let next = ctx.continuation_stack().peek_continuation();
        debug_assert!(
            !matches!(next, Some(Continuation::EnterForest { .. })),
            "a resume context never starts with a forest switch"
        );
        let node_start = match (next, cycle.source_node_id) {
            (Some(Continuation::StartNode(_)), Some(node)) => {
                let parent =
                    source.debug_info().and_then(|info| self.parent_of(forest, info, node));
                self.open.push((forest.clone(), node));
                Some(NodeStart { node, parent })
            }
            (
                Some(
                    Continuation::FinishJoin(_)
                    | Continuation::FinishSplit(_)
                    | Continuation::FinishCall(_)
                    | Continuation::FinishDyn(_)
                    | Continuation::FinishBasicBlock(_),
                ),
                Some(node),
            ) => {
                self.close(forest, node);
                None
            }
            // A loop's finish runs once per iteration and rechecks the condition, so the loop
            // stays open; it is popped when its parent finishes.
            _ => None,
        };

        CycleInfo {
            forest,
            debug_info: source.debug_info(),
            exec_node: cycle.exec_node,
            source_node: cycle.source_node_id,
            op: cycle.op,
            op_idx: cycle.op_idx.map(|idx| idx as u32),
            control: cycle.control_flow_kind,
            node_start,
            inline_frames: source.inline_frames(),
        }
    }

    /// The innermost open node of `forest` that lists `child` among its children, or the
    /// innermost open node of that forest when none does.
    ///
    /// The second case covers a node whose parent was never seen starting, such as the
    /// children of a root entered from another package. A loop that exited but is still open
    /// never lists its sibling as a child, so the first rule skips it.
    fn parent_of(
        &self,
        forest: &Arc<MastForest>,
        debug_info: &PackageDebugInfo,
        child: DebugSourceNodeId,
    ) -> Option<DebugSourceNodeId> {
        let same_forest = self
            .open
            .iter()
            .rev()
            .filter(|(open_forest, _)| Arc::ptr_eq(open_forest, forest))
            .map(|(_, node)| *node);
        let lists_child = |node: &DebugSourceNodeId| {
            debug_info
                .source_node(*node)
                .is_some_and(|source| source.children.contains(&child))
        };
        same_forest.clone().find(lists_child).or_else(|| same_forest.clone().next())
    }

    /// Pops the open nodes above and including `node` of `forest`. Leaves the stack alone when
    /// the node is not open.
    fn close(&mut self, forest: &Arc<MastForest>, node: DebugSourceNodeId) {
        let position = self
            .open
            .iter()
            .rposition(|(open_forest, open)| *open == node && Arc::ptr_eq(open_forest, forest));
        if let Some(position) = position {
            self.open.truncate(position);
        }
    }
}

#[cfg(test)]
mod tests;
