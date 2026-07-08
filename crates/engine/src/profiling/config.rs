use std::{collections::HashMap, path::PathBuf};

use crate::profiling::instrument::{Instrument, OpHistogram};

/// Profiler options parsed from the command line.
#[derive(Default, Clone, Debug)]
#[cfg_attr(feature = "tui", derive(clap::Args))]
pub struct ProfilerCliArgs {
    /// Generate an op histogram weighted by cycles and write it to the given path.
    #[cfg_attr(
        feature = "tui",
        arg(long = "profile-op-histogram-out", value_name = "FILE")
    )]
    pub op_histogram_out: Option<PathBuf>,
}

#[derive(Default)]
pub struct ProfilerConfig {
    /// The active instrumentations.
    pub instruments: Vec<Box<dyn Instrument>>,
    /// Optional output file per instrument.
    pub output_paths: HashMap<&'static str, PathBuf>,
}

impl ProfilerConfig {
    /// Associates an output file `path` with `instrument`, keyed by its name.
    pub fn register_output_path(&mut self, instrument: &dyn Instrument, path: PathBuf) {
        self.output_paths.insert(instrument.name(), path);
    }
}

impl From<ProfilerCliArgs> for ProfilerConfig {
    fn from(args: ProfilerCliArgs) -> Self {
        let mut config = ProfilerConfig::default();

        if let Some(path) = args.op_histogram_out {
            let op_histogram: Box<OpHistogram> = Box::default();
            config.register_output_path(op_histogram.as_ref(), path);
            config.instruments.push(op_histogram);
        }

        config
    }
}

impl std::fmt::Debug for ProfilerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let instrument_names: Vec<&'static str> =
            self.instruments.iter().map(|i| i.name()).collect();
        f.debug_struct("ProfilerConfig")
            .field("instruments", &instrument_names)
            .field("output_paths", &self.output_paths)
            .finish()
    }
}
