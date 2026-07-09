mod commands;
pub(crate) mod engine;
#[cfg(feature = "python")]
mod python;
mod script;
mod session;

use log::LevelFilter;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};

pub use self::script::run_commands;
use self::session::ReplSession;
use crate::config::DebuggerConfig;

/// Run the REPL debugger with the given configuration.
pub fn run(config: Box<DebuggerConfig>, logger: Box<dyn log::Log>) -> Result<(), Report> {
    run_with_log_level(config, logger, LevelFilter::Trace)
}

/// Run the REPL debugger with the given configuration and log level filter.
pub fn run_with_log_level(
    config: Box<DebuggerConfig>,
    logger: Box<dyn log::Log>,
    max_level: LevelFilter,
) -> Result<(), Report> {
    // Install the logger; propagate any failure as a diagnostic so the
    // session doesn't silently start with a half-installed logger.
    crate::logger::DebugLogger::install_with_max_level(logger, max_level).into_diagnostic()?;

    let mut session = ReplSession::new(config)?;
    session.run()
}
