mod commands;
mod session;

use miden_assembly_syntax::diagnostics::Report;

use self::session::ReplSession;
use crate::config::DebuggerConfig;

/// Run the REPL debugger with the given configuration.
pub fn run(config: Box<DebuggerConfig>, logger: Box<dyn log::Log>) -> Result<(), Report> {
    // Install the logger
    crate::logger::DebugLogger::install(logger);

    let mut session = ReplSession::new(config)?;
    session.run()
}
