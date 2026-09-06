use alloc::{borrow::ToOwned, string::String, vec::Vec};
use std::collections::HashMap;

use miden_core::operations::Operation;

use super::{Instrument, InstrumentRegistration};
use crate::{profiling::helpers::op_histogram::OpHistogram, register_instrument};

/// Map key under which operations that cannot be attributed to a procedure are collected. The
/// angle brackets cannot occur in a MASM identifier, so this never collides with a procedure name
const UNKNOWN_PROCEDURE: &str = "<unknown>";

/// An [`Instrument`] to create per-procedure operation histograms.
///
/// At each cycle, it records the current operation into the histogram of the most recent live
/// procedure. Operations that cannot be attributed to a procedure are collected into a separate
/// histogram, reported under [`UNKNOWN_PROCEDURE`].
///
/// The report contains one section per procedure, sorted by the number of cycles spent
/// in that procedure (highest first).
#[derive(Default)]
pub struct OpHistogramProc {
    histograms: HashMap<String, OpHistogram>,
}

impl InstrumentRegistration for OpHistogramProc {
    const NAME: &'static str = "op-histogram-proc";

    fn build(_config: &crate::profiling::ProfilerConfig) -> Result<Self, super::InstrumentError> {
        Ok(Self::default())
    }
}

register_instrument!(OpHistogramProc);

impl Instrument for OpHistogramProc {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn on_operation_execution_cycle(&mut self, op: Operation, proc: Option<&str>) {
        let key = proc.unwrap_or(UNKNOWN_PROCEDURE);
        // Look up by borrow first so the key is only cloned when a new procedure is seen.
        match self.histograms.get_mut(key) {
            Some(hist) => hist.record(op),
            None => {
                let mut hist = OpHistogram::default();
                hist.record(op);
                self.histograms.insert(key.to_owned(), hist);
            }
        }
    }

    fn write_report_to(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
        let mut entries: Vec<(&str, &OpHistogram)> =
            self.histograms.iter().map(|(name, hist)| (name.as_str(), hist)).collect();
        // Print the histogram with the highest total cycle count first, break ties by procedure
        // name for a stable order.
        entries
            .sort_by(|a, b| b.1.total_cycles().cmp(&a.1.total_cycles()).then_with(|| a.0.cmp(b.0)));

        for (name, hist) in entries {
            writeln!(writer, "procedure: {name}")?;
            writer.write_all(hist.report().as_bytes())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, vec::Vec};

    use miden_core::operations::Operation;

    use super::OpHistogramProc;
    use crate::profiling::instrument::Instrument;

    /// Returns the part of `report` that belongs to the section started by `procedure: <name>`.
    fn section<'a>(report: &'a str, name: &str) -> &'a str {
        let marker = format!("procedure: {name}\n");
        let start = report.find(&marker).expect("section exists") + marker.len();
        match report[start..].find("procedure: ") {
            Some(rel) => &report[start..start + rel],
            None => &report[start..],
        }
    }

    /// Returns the count shown on the report line for `label` within `section`, or `None` if
    /// the section has no line for `label`.
    ///
    /// Matching op and count on a single line ensures the count is actually attributed to
    /// `label` and not to another op in the same section.
    fn count_for(section: &str, label: &str) -> Option<u64> {
        section
            .lines()
            .find(|line| line.split_whitespace().next() == Some(label))
            .map(parse_count_from_line)
    }

    /// Sums `total_cycles` over all procedure sections in `report`, including `<unknown>`.
    fn sum_total_cycles(report: &str) -> u64 {
        report
            .lines()
            .filter(|line| line.split_whitespace().next() == Some("total_cycles"))
            .map(parse_count_from_line)
            .sum()
    }

    /// Parses the count from the last column of a report line.
    fn parse_count_from_line(line: &str) -> u64 {
        line.split_whitespace()
            .last()
            .and_then(|count| count.parse().ok())
            .expect("report line ends with the count")
    }

    #[test]
    fn op_histogram_proc_reports_per_procedure_histograms() {
        let mut hist = OpHistogramProc::default();

        hist.on_operation_execution_cycle(Operation::Add, Some("main"));
        hist.on_operation_execution_cycle(Operation::Add, Some("main"));
        hist.on_operation_execution_cycle(Operation::Noop, Some("sum"));
        hist.on_operation_execution_cycle(Operation::Mul, Some("sum"));

        let mut buf = Vec::new();
        hist.write_report_to(&mut buf).unwrap();
        let report = String::from_utf8(buf).unwrap();

        // `main` recorded 2 cycles, both `add`.
        let main = section(&report, "main");
        assert_eq!(count_for(main, "total_cycles"), Some(2));
        assert_eq!(count_for(main, "add"), Some(2));
        assert_eq!(count_for(main, "noop"), None);

        // `sum` recorded 2 cycles, one `noop` and one `mul`.
        let sum = section(&report, "sum");
        assert_eq!(count_for(sum, "total_cycles"), Some(2));
        assert_eq!(count_for(sum, "noop"), Some(1));
        assert_eq!(count_for(sum, "mul"), Some(1));

        // All 4 recorded cycles are accounted for across the sections.
        assert_eq!(sum_total_cycles(&report), 4);
    }

    #[test]
    fn op_histogram_proc_sorts_by_total_cycles() {
        let mut hist = OpHistogramProc::default();

        // `z` has more cycles than `a`, so it's printed first despite the alphabetical order.
        hist.on_operation_execution_cycle(Operation::Add, Some("z"));
        hist.on_operation_execution_cycle(Operation::Add, Some("z"));
        hist.on_operation_execution_cycle(Operation::Add, Some("z"));
        hist.on_operation_execution_cycle(Operation::Noop, Some("a"));

        let mut buf = Vec::new();
        hist.write_report_to(&mut buf).unwrap();
        let report = String::from_utf8(buf).unwrap();

        let z_pos = report.find("procedure: z").unwrap();
        let a_pos = report.find("procedure: a").unwrap();
        assert!(z_pos < a_pos, "histogram with more cycles must be printed first:\n{report}");

        // All 4 recorded cycles are accounted for across the sections.
        assert_eq!(sum_total_cycles(&report), 4);
    }

    #[test]
    fn op_histogram_proc_collects_unattributed_ops_separately() {
        let mut hist = OpHistogramProc::default();

        hist.on_operation_execution_cycle(Operation::Add, None);
        hist.on_operation_execution_cycle(Operation::Add, None);
        hist.on_operation_execution_cycle(Operation::Noop, Some("main"));

        let mut buf = Vec::new();
        hist.write_report_to(&mut buf).unwrap();
        let report = String::from_utf8(buf).unwrap();

        // The `<unknown>` section holds both unattributed `add`s and is not merged into `main`.
        let unknown = section(&report, "<unknown>");
        assert_eq!(count_for(unknown, "total_cycles"), Some(2));
        assert_eq!(count_for(unknown, "add"), Some(2));
        assert_eq!(count_for(unknown, "noop"), None);

        let main = section(&report, "main");
        assert_eq!(count_for(main, "noop"), Some(1));
        assert_eq!(count_for(main, "add"), None);

        // All 3 recorded cycles (2 unattributed, 1 in `main`) are accounted for.
        assert_eq!(sum_total_cycles(&report), 3);
    }
}
