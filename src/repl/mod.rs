mod commands;
mod session;

use log::LevelFilter;
use miden_assembly_syntax::diagnostics::Report;

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
    // Install the logger
    crate::logger::DebugLogger::install_with_max_level(logger, max_level);

    let mut session = ReplSession::new(config)?;
    session.run()
}
