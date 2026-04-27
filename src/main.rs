use std::env;

use clap::Parser;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report, WrapErr};
use miden_debug::DebuggerConfig;
#[cfg(feature = "repl")]
use miden_debug::run_repl_with_log_level as run_repl;
#[cfg(feature = "tui")]
use miden_debug::run_with_log_level as run;
#[cfg(feature = "dap")]
use miden_debug::{State, run_with_state_and_log_level as run_with_state};

pub fn main() -> Result<(), Report> {
    setup_diagnostics();

    // Initialize logger, but do not install it, leave that up to the command handler
    let mut builder = env_logger::Builder::from_env("MIDENC_TRACE");
    builder.format_indent(Some(2));
    if let Ok(precision) = env::var("MIDENC_TRACE_TIMING") {
        match precision.as_str() {
            "s" => builder.format_timestamp_secs(),
            "ms" => builder.format_timestamp_millis(),
            "us" => builder.format_timestamp_micros(),
            "ns" => builder.format_timestamp_nanos(),
            other => {
                return Err(Report::msg(format!(
                    "invalid MIDENC_TRACE_TIMING precision, expected one of [s, ms, us, ns], got \
                     '{other}'"
                )));
            }
        };
    } else {
        builder.format_timestamp(None);
    }

    let logger = builder.build();
    let log_level = logger.filter();
    let logger = Box::new(logger);
    let mut config = Box::new(DebuggerConfig::parse());

    if config.working_dir.is_none() {
        let cwd = env::current_dir()
            .into_diagnostic()
            .wrap_err("could not read current working directory")?;

        config.working_dir = Some(cwd);
    }

    #[cfg(feature = "dap")]
    if let Some(addr) = config.dap_connect.as_ref() {
        let state = State::new_for_dap(addr)?;
        return run_with_state(state, logger, log_level);
    }

    #[cfg(feature = "repl")]
    if config.repl {
        return run_repl(config, logger, log_level);
    }

    #[cfg(feature = "tui")]
    return run(config, logger, log_level);

    #[cfg(not(feature = "tui"))]
    Err(Report::msg(
        "no UI feature enabled: build with --features tui or --features repl",
    ))
}

fn setup_diagnostics() {
    use miden_assembly_syntax::diagnostics::reporting::{self, ReportHandlerOpts};

    let result = reporting::set_hook(Box::new(|_| Box::new(ReportHandlerOpts::new().build())));
    if result.is_ok() {
        reporting::set_panic_hook();
    }
}
