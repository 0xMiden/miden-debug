#![allow(unused)]
mod config;
mod debug;
mod exec;
mod felt;
mod input;
mod linker;
mod logger;
#[cfg(feature = "repl")]
mod repl;
#[cfg(feature = "tui")]
mod ui;

use std::env;

use clap::Parser;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report, WrapErr};

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

    let logger = Box::new(builder.build());
    let mut config = Box::new(config::DebuggerConfig::parse());

    if config.working_dir.is_none() {
        let cwd = env::current_dir()
            .into_diagnostic()
            .wrap_err("could not read current working directory")?;

        config.working_dir = Some(cwd);
    }

    #[cfg(all(feature = "tui", feature = "repl"))]
    {
        if config.repl {
            repl::run(config, logger)
        } else {
            ui::run(config, logger)
        }
    }

    #[cfg(all(feature = "tui", not(feature = "repl")))]
    {
        if config.repl {
            return Err(Report::msg(
                "--repl flag requires the 'repl' feature. Rebuild with: cargo build --features repl",
            ));
        }
        ui::run(config, logger)
    }

    #[cfg(all(feature = "repl", not(feature = "tui")))]
    {
        repl::run(config, logger)
    }
}

fn setup_diagnostics() {
    use miden_assembly_syntax::diagnostics::reporting::{self, ReportHandlerOpts};

    let result = reporting::set_hook(Box::new(|_| Box::new(ReportHandlerOpts::new().build())));
    if result.is_ok() {
        reporting::set_panic_hook();
    }
}
