use std::path::PathBuf;

use miden_core::operations::Operation;

use crate::profiling::{ProfilerConfig, instrument::Instrument};

/// Holds the loaded [`Instrument`]s and dispatches event handlers to them.
///
/// The default is a no-op `Profiler`.
#[derive(Default)]
pub struct Profiler {
    instruments: Vec<Box<dyn Instrument>>,
    reports_dir: Option<PathBuf>,
}

impl Profiler {
    pub fn from_config(config: ProfilerConfig) -> Self {
        Self {
            instruments: config.instruments,
            reports_dir: config.reports_dir,
        }
    }

    /// Records the op with every active instrument, otherwise it's a no-op.
    pub fn on_operation_execution_cycle(&mut self, op: Operation) {
        for instrument in &mut self.instruments {
            instrument.on_operation_execution_cycle(op);
        }
    }

    /// Writes every instrument's report to the configured reports output directory.
    ///
    /// Failure to write a report should not abort execution, therefore this function always
    /// succeeds. If any errors occur they are logged.
    pub fn write_reports(&self) {
        if self.instruments.is_empty() {
            return;
        }

        let Some(ref reports_dir) = self.reports_dir else {
            log::warn!("cannot write profiler reports: no reports directory configured");
            return;
        };

        if let Err(e) = std::fs::create_dir_all(reports_dir) {
            log::error!(
                "failed to create profiler reports directory {}: {e}",
                reports_dir.display()
            );
            return;
        }

        for instrument in &self.instruments {
            let name = instrument.name();
            let path = reports_dir.join(name);
            let mut file = match std::fs::File::create(&path) {
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
    use std::collections::HashMap;

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
    fn profiler_writes_reports_to_directory() {
        let tmp_dir = tempfile::tempdir().unwrap();

        let config = ProfilerConfig {
            instruments: vec![
                Box::new(CountingInstrument {
                    name: "alpha",
                    ops: 0,
                }),
                Box::new(CountingInstrument {
                    name: "beta",
                    ops: 0,
                }),
            ],
            reports_dir: Some(tmp_dir.path().to_path_buf()),
        };
        let mut profiler = Profiler::from_config(config);

        // Record some operations so the instruments have data to report.
        profiler.on_operation_execution_cycle(Operation::Add);
        profiler.on_operation_execution_cycle(Operation::Noop);
        profiler.on_operation_execution_cycle(Operation::Add);

        profiler.write_reports();

        // Verify files were created with expected content.
        let alpha_content = std::fs::read_to_string(tmp_dir.path().join("alpha")).unwrap();
        assert_eq!(alpha_content, "alpha:3");

        let beta_content = std::fs::read_to_string(tmp_dir.path().join("beta")).unwrap();
        assert_eq!(beta_content, "beta:3");
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
