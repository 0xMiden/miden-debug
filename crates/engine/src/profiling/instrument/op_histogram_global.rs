use miden_core::operations::Operation;

use super::{Instrument, InstrumentRegistration};
use crate::{profiling::helpers::op_histogram::OpHistogram, register_instrument};

/// An [`Instrument`] to create operation histograms.
///
/// At each cycle, it records the current operation and produces a histogram of executed operations
/// weighted by cycles per operation. If `opX` takes 4 cycles and was executed twice, its count
/// will be 8.
#[derive(Default)]
pub struct OpHistogramGlobal {
    hist: OpHistogram,
}

impl InstrumentRegistration for OpHistogramGlobal {
    const NAME: &'static str = "op-histogram-global";

    fn build(_config: &crate::profiling::ProfilerConfig) -> Result<Self, super::InstrumentError> {
        Ok(Self::default())
    }
}

register_instrument!(OpHistogramGlobal);

impl Instrument for OpHistogramGlobal {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn on_operation_execution_cycle(&mut self, op: Operation) {
        self.hist.record(op);
    }

    fn write_report_to(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
        writer.write_all(self.hist.report().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use miden_core::operations::Operation;

    use super::OpHistogramGlobal;
    use crate::profiling::instrument::Instrument;

    #[test]
    fn op_histogram_reports_recorded_ops() {
        let mut hist = OpHistogramGlobal::default();

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
}
