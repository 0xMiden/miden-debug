use miden_core::{Felt, operations::Operation};

use super::{Instrument, InstrumentRegistration};
use crate::register_instrument;

/// An [`Instrument`] to create operation histograms.
///
/// At each cycle, it records the current operation and produces a histogram of executed operations
/// weighted by cycles per operation. If `opX` takes 4 cycles and was executed twice, its count
/// will be 8.
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

impl InstrumentRegistration for OpHistogram {
    const NAME: &'static str = "op-histogram";

    fn build(_config: &crate::profiling::ProfilerConfig) -> Result<Self, super::InstrumentError> {
        Ok(Self::default())
    }
}

register_instrument!(OpHistogram);

impl Instrument for OpHistogram {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn on_operation_execution_cycle(&mut self, op: Operation) {
        self.total_cycles += 1;
        // `op.op_code` returns u8 which can safely be used as index here
        self.counts[usize::from(op.op_code())] += 1;
    }

    fn write_report_to(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
        const OP_COL_WIDTH: usize = 16;
        const SHARE_COL_WIDTH: usize = 7;

        let total = self.total_cycles;

        // Header row: the total accounts for 100% of the cycles. Every row is laid out in three
        // columns (op | share | count) with fixed widths so the output stays aligned. When `total
        // == 0` there are no data rows, so only this header line is written.
        writeln!(
            writer,
            "{:<col1$} {:>col2$} {}",
            "total_cycles",
            "100%",
            total,
            col1 = OP_COL_WIDTH,
            col2 = SHARE_COL_WIDTH,
        )?;

        for (op, count) in self.sorted_counts() {
            let share = 100.0 * (count as f64) / (total as f64);
            // Any payload on the reconstructed `Operation` is just a meaningless placeholder, so
            // remove it. The report then only contains `push` instead of `push(0)`, for example.
            let label = op.to_string();
            let label = label.split('(').next().unwrap();
            writeln!(
                writer,
                "{:<col1$} {:>col2$} {}",
                label,
                format!("{share:.2}%"),
                count,
                col1 = OP_COL_WIDTH,
                col2 = SHARE_COL_WIDTH,
            )?;
        }
        Ok(())
    }
}

impl OpHistogram {
    fn sorted_counts(&self) -> SortedCounts {
        let mut counts: SortedCounts = ALL_OPERATIONS
            .iter()
            .map(|&op| (op, self.counts[usize::from(op.op_code())]))
            .filter(|&(_, count)| count > 0)
            .collect();
        // Sort by count descending; break ties by opcode for a stable order.
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.op_code().cmp(&b.0.op_code())));
        counts
    }
}

/// Counts per operation, sorted in descending order by count.
type SortedCounts = Vec<(Operation, u64)>;

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
    Operation::LogPrecompile,
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use miden_core::{Felt, operations::Operation, serde::Deserializable};

    use super::ALL_OPERATIONS;
    use crate::profiling::instrument::Instrument;

    /// `ALL_OPERATIONS` must contain exactly the set of opcodes that map to a
    /// valid `Operation`.
    ///
    /// We use `Operation`'s `Deserializable` impl as the source of truth.
    #[test]
    fn all_operations_covers_every_valid_opcode() {
        // The payload-carrying variants (Push, Assert, MpVerify, U32assert2)
        // read an extra `Felt` after the opcode byte, so pad with enough zero
        // bytes for them to deserialize. Trailing bytes are ignored by the
        // reader, so this is harmless for the 1-byte variants.
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
    fn op_histogram_reports_recorded_ops() {
        let mut hist = super::OpHistogram::default();

        // 3 cycles
        hist.on_operation_execution_cycle(Operation::Add);
        hist.on_operation_execution_cycle(Operation::Add);
        hist.on_operation_execution_cycle(Operation::Noop);

        let mut buf = Vec::new();
        hist.write_report_to(&mut buf).unwrap();
        let report = String::from_utf8(buf).unwrap();

        let lines: Vec<&str> = report.lines().collect();
        // First line is the header reporting 100% of 3 cycles.
        assert!(lines[0].contains("total_cycles") && lines[0].contains('3'));

        // `Add` must preceed `Noop`, due to higher count.
        assert!(lines[1].contains(Operation::Add.to_string().as_str()) && lines[1].contains("2"));
        assert!(lines[2].contains(Operation::Noop.to_string().as_str()) && lines[2].contains("1"));

        // no further lines
        assert_eq!(lines.len(), 3);
    }

    /// Payload-carrying operations must render with just their mnemonic, e.g. `push` rather than
    /// `push(0)`.
    #[test]
    fn op_histogram_omits_payload_from_mnemonic() {
        let mut hist = super::OpHistogram::default();

        // All payload carrying variants
        hist.on_operation_execution_cycle(Operation::Push(Felt::ZERO));
        hist.on_operation_execution_cycle(Operation::Assert(Felt::ZERO));
        hist.on_operation_execution_cycle(Operation::MpVerify(Felt::ZERO));
        hist.on_operation_execution_cycle(Operation::U32assert2(Felt::ZERO));

        let mut buf = Vec::new();
        hist.write_report_to(&mut buf).unwrap();
        let report = String::from_utf8(buf).unwrap();

        assert!(
            !report.contains("(0)"),
            "report must not contain placeholder payloads: {report}"
        );
        for mnemonic in ["push", "assert", "mpverify", "u32assert2"] {
            assert!(report.contains(mnemonic), "report missing `{mnemonic}`: {report}");
        }
    }
}
