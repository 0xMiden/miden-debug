use std::{collections::HashMap, path::PathBuf};

use miden_core::operations::Operation;

use crate::profiling::{ProfilerConfig, instrument::Instrument};

/// Holds the loaded [`Instrument`]s and dispatches event handlers to them.
///
/// The default is a no-op `Profiler`.
#[derive(Default)]
pub struct Profiler {
    instruments: Vec<Box<dyn Instrument>>,
    output_paths: HashMap<&'static str, PathBuf>,
}

impl Profiler {
    pub fn from_config(config: ProfilerConfig) -> Self {
        Self {
            instruments: config.instruments,
            output_paths: config.output_paths,
        }
    }

    /// Records the op with every active instrument, otherwise it's a no-op.
    pub fn on_operation_execution_cycle(&mut self, op: Operation) {
        for instrument in &mut self.instruments {
            instrument.on_operation_execution_cycle(op);
        }
    }

    /// Writes every instrument's report to its configured output file.
    ///
    /// Failure to write a report should not abort execution, therefore this function always
    /// succeeds. If any errors occur they are logged.
    pub fn write_reports(&self) {
        for instrument in &self.instruments {
            let name = instrument.name();
            let Some(path) = self.output_paths.get(name) else {
                log::warn!("cannot write report for instrument `{name}`: no output file");
                continue;
            };
            let mut file = match std::fs::File::create(path) {
                Ok(file) => file,
                Err(e) => {
                    log::error!(
                        "failed to create profiler output file for `{name}` at {}: {e}",
                        path.display()
                    );
                    continue;
                }
            };
            if let Err(e) = instrument.write_report_to(&mut file) {
                log::error!("failed to write `{name}` report to {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use miden_core::operations::Operation;

    use super::*;
    use crate::profiling::ProfilerConfig;

    /// A minimal `Instrument` to test event dispatch.
    struct CountingInstrument {
        name: &'static str,
        ops: u32,
    }

    impl Instrument for CountingInstrument {
        fn name(&self) -> &'static str {
            self.name
        }

        fn on_operation_execution_cycle(&mut self, _op: Operation) {
            self.ops += 1;
        }

        fn write_report_to(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()> {
            writer.write_fmt(format_args!("{}:{}", self.name, self.ops))
        }
    }

    #[test]
    fn profiler_dispatches_events_to_all_instruments() {
        let config = ProfilerConfig {
            instruments: vec![
                Box::new(CountingInstrument { name: "a", ops: 0 }),
                Box::new(CountingInstrument { name: "b", ops: 0 }),
            ],
            ..Default::default()
        };
        let mut profiler = Profiler::from_config(config);

        // Each instrument must observe every recorded op.
        profiler.on_operation_execution_cycle(Operation::Add);
        profiler.on_operation_execution_cycle(Operation::Noop);

        // Collect each report into an in-memory buffer and construct string from there.
        fn report_of(instrument: &dyn Instrument) -> String {
            let mut buf = Vec::new();
            instrument.write_report_to(&mut buf).unwrap();
            String::from_utf8(buf).unwrap()
        }

        let reports: HashMap<&'static str, String> = profiler
            .instruments
            .iter()
            .map(|instrument| (instrument.name(), report_of(instrument.as_ref())))
            .collect();

        assert_eq!(reports.get("a").unwrap(), "a:2");
        assert_eq!(reports.get("b").unwrap(), "b:2");
    }
}
