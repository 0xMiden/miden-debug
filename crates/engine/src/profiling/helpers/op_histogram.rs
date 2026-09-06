use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::fmt::Write;

use miden_core::{Felt, operations::Operation};

/// A histogram of executed operations weighted by cycles per operation.
///
/// At each cycle, [`OpHistogram::record`] records the current operation. If `opX` takes 4 cycles
/// and was executed twice, its count will be 8.
pub struct OpHistogram {
    total_cycles: u128,
    counts: [u64; 256],
}

impl Default for OpHistogram {
    fn default() -> Self {
        Self {
            total_cycles: 0,
            counts: [0; 256],
        }
    }
}

impl OpHistogram {
    /// Records `op` as executed for one cycle.
    pub fn record(&mut self, op: Operation) {
        self.total_cycles += 1;
        // `op.op_code` returns u8 which can safely be used as index here
        self.counts[usize::from(op.op_code())] += 1;
    }

    /// Total number of recorded cycles.
    pub fn total_cycles(&self) -> u128 {
        self.total_cycles
    }

    /// Counts per operation, sorted in descending order by count.
    ///
    /// Only operations with a non-zero count are included.
    pub fn sorted_counts(&self) -> SortedCounts {
        let mut counts: SortedCounts = ALL_OPERATIONS
            .iter()
            .map(|&op| (op, self.counts[usize::from(op.op_code())]))
            .filter(|&(_, count)| count > 0)
            .collect();
        // Sort by count descending; break ties by opcode for a stable order.
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.op_code().cmp(&b.0.op_code())));
        counts
    }

    /// Renders the histogram as a report.
    pub fn report(&self) -> String {
        const OP_COL_WIDTH: usize = 16;
        const SHARE_COL_WIDTH: usize = 7;

        let total = self.total_cycles;
        let mut report = String::new();

        // Every row is laid out in three columns (op | share | count) with fixed widths so the
        // output stays aligned.
        writeln!(
            report,
            "{:<col1$} {:>col2$} {}",
            "total_cycles",
            "100%",
            total,
            col1 = OP_COL_WIDTH,
            col2 = SHARE_COL_WIDTH,
        )
        .unwrap();

        for (op, count) in self.sorted_counts() {
            let share = 100.0 * (count as f64) / (total as f64);
            // Any payload on the reconstructed `Operation` is just a meaningless placeholder, so
            // remove it. The report then only contains `push` instead of `push(0)`, for example.
            let label = op.to_string();
            let label = label.split('(').next().unwrap();
            writeln!(
                report,
                "{:<col1$} {:>col2$} {}",
                label,
                format!("{share:.2}%"),
                count,
                col1 = OP_COL_WIDTH,
                col2 = SHARE_COL_WIDTH,
            )
            .unwrap();
        }
        report
    }
}

/// Counts per operation, sorted in descending order by count.
pub type SortedCounts = Vec<(Operation, u64)>;

/// Every basic-block [`Operation`] variant, with placeholder payloads (`Felt::ZERO`) for the
/// value-carrying variants. Used to map the `counts` array back into typed operations for
/// reporting.
///
/// A unit test ensures that this list contains *all* relevant operations from `miden-core`.
const ALL_OPERATIONS: &[Operation] = &[
    Operation::Noop,
    Operation::Assert(Felt::ZERO),
    Operation::SDepth,
    Operation::Caller,
    Operation::Clk,
    Operation::Emit,
    Operation::Add,
    Operation::Neg,
    Operation::Mul,
    Operation::Inv,
    Operation::Incr,
    Operation::And,
    Operation::Or,
    Operation::Not,
    Operation::Eq,
    Operation::Eqz,
    Operation::Expacc,
    Operation::Ext2Mul,
    Operation::U32split,
    Operation::U32add,
    Operation::U32add3,
    Operation::U32sub,
    Operation::U32mul,
    Operation::U32madd,
    Operation::U32div,
    Operation::U32and,
    Operation::U32xor,
    Operation::U32assert2(Felt::ZERO),
    Operation::Pad,
    Operation::Drop,
    Operation::Dup0,
    Operation::Dup1,
    Operation::Dup2,
    Operation::Dup3,
    Operation::Dup4,
    Operation::Dup5,
    Operation::Dup6,
    Operation::Dup7,
    Operation::Dup9,
    Operation::Dup11,
    Operation::Dup13,
    Operation::Dup15,
    Operation::Swap,
    Operation::SwapW,
    Operation::SwapW2,
    Operation::SwapW3,
    Operation::SwapDW,
    Operation::MovUp2,
    Operation::MovUp3,
    Operation::MovUp4,
    Operation::MovUp5,
    Operation::MovUp6,
    Operation::MovUp7,
    Operation::MovUp8,
    Operation::MovDn2,
    Operation::MovDn3,
    Operation::MovDn4,
    Operation::MovDn5,
    Operation::MovDn6,
    Operation::MovDn7,
    Operation::MovDn8,
    Operation::CSwap,
    Operation::CSwapW,
    Operation::Push(Felt::ZERO),
    Operation::AdvPop,
    Operation::AdvPopW,
    Operation::MLoadW,
    Operation::MStoreW,
    Operation::MLoad,
    Operation::MStore,
    Operation::MStream,
    Operation::Pipe,
    Operation::CryptoStream,
    Operation::HPerm,
    Operation::MpVerify(Felt::ZERO),
    Operation::MrUpdate,
    Operation::FriE2F4,
    Operation::HornerBase,
    Operation::HornerExt,
    Operation::EvalCircuit,
    Operation::LogDeferred,
];

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};
    use std::collections::BTreeSet;

    use miden_core::{Felt, operations::Operation, serde::Deserializable};

    use super::{ALL_OPERATIONS, OpHistogram};

    /// `ALL_OPERATIONS` must contain exactly the set of opcodes that map to a valid `Operation`.
    ///
    /// We use `Operation`'s `Deserializable` impl as the source of truth.
    #[test]
    fn all_operations_covers_every_valid_opcode() {
        // The payload-carrying variants (Push, Assert, MpVerify, U32assert2) read an extra `Felt`
        // after the opcode byte, so pad with enough zero bytes for them to deserialize. Trailing
        // bytes are ignored by the reader, so this is harmless for the 1-byte variants.
        let valid: BTreeSet<u8> = (0u8..=u8::MAX)
            .filter(|&op| Operation::read_from_bytes(&[op, 0, 0, 0, 0, 0, 0, 0, 0]).is_ok())
            .collect();

        let ours: BTreeSet<u8> = ALL_OPERATIONS.iter().map(|op| op.op_code()).collect();

        // `ALL_OPERATIONS` holds real `Operation` values, so  `ours ⊆ valid`. The list can
        // therefore only fall out of sync by *missing* a variant.
        let missing_in_ours: Vec<String> = valid
            .difference(&ours)
            .map(|&op| {
                format!(
                    "{}",
                    Operation::read_from_bytes(&[op, 0, 0, 0, 0, 0, 0, 0, 0])
                        .expect("valid opcode deserializes to an Operation")
                )
            })
            .collect();

        if !missing_in_ours.is_empty() {
            panic!(
                "ALL_OPERATIONS is out of sync with miden-core's Operation enum.\n  missing from \
                 ALL_OPERATIONS (add these): {missing_in_ours:?}"
            );
        }
    }

    #[test]
    fn sorted_counts_orders_by_count_desc_then_opcode() {
        let mut hist = OpHistogram::default();

        hist.record(Operation::Add);
        hist.record(Operation::Add);
        hist.record(Operation::Noop);
        hist.record(Operation::Eq);

        let (ops, counts): (Vec<Operation>, Vec<u64>) = hist.sorted_counts().into_iter().unzip();

        // Most frequent op comes first.
        assert_eq!(ops[0], Operation::Add);
        assert_eq!(counts[0], 2);

        // The two single-cycle ops are ordered by opcode, since their counts tie.
        assert_eq!(counts[1], 1);
        assert_eq!(counts[2], 1);
        assert!(ops[1].op_code() < ops[2].op_code());

        // `total_cycles` accounts for every recorded cycle.
        assert_eq!(hist.total_cycles(), 4);
    }

    #[test]
    fn sorted_counts_excludes_unrecorded_operations() {
        let hist = OpHistogram::default();
        assert!(hist.sorted_counts().is_empty());
        assert_eq!(hist.total_cycles(), 0);
    }

    /// Payload-carrying operations must render with just their mnemonic, e.g. `push` rather than
    /// `push(0)`.
    #[test]
    fn op_histogram_omits_payload_from_mnemonic() {
        let mut hist = OpHistogram::default();

        // All payload carrying variants
        hist.record(Operation::Push(Felt::ZERO));
        hist.record(Operation::Assert(Felt::ZERO));
        hist.record(Operation::MpVerify(Felt::ZERO));
        hist.record(Operation::U32assert2(Felt::ZERO));

        let report = hist.report();

        assert!(
            !report.contains("(0)"),
            "report must not contain placeholder payloads: {report}"
        );
        for mnemonic in ["push", "assert", "mpverify", "u32assert2"] {
            assert!(report.contains(mnemonic), "report missing `{mnemonic}`: {report}");
        }
    }
}
