mod action;
mod app;
mod duration;
mod pages;
mod panes;
pub(crate) mod state;
mod syntax_highlighting;
mod tui;

use log::LevelFilter;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};

pub use self::state::{DebugMode, State};
use self::{action::Action, app::App};
use crate::config::DebuggerConfig;

#[allow(dead_code)]
pub fn run(config: Box<DebuggerConfig>, logger: Box<dyn log::Log>) -> Result<(), Report> {
    run_with_log_level(config, logger, LevelFilter::Trace)
}

pub fn run_with_log_level(
    config: Box<DebuggerConfig>,
    logger: Box<dyn log::Log>,
    max_level: LevelFilter,
) -> Result<(), Report> {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder.enable_all().build().into_diagnostic()?;
    rt.block_on(async move { start_ui(config, logger, max_level).await })
}

/// Launch the TUI debugger with a pre-built [State].
///
/// This is the programmatic entry point used by transaction debugging, where
/// the caller constructs a [State] with pre-recorded event replay data.
pub fn run_with_state(state: State, logger: Box<dyn log::Log>) -> Result<(), Report> {
    run_with_state_and_log_level(state, logger, LevelFilter::Trace)
}

/// Launch the TUI debugger with a pre-built [State] and log level filter.
pub fn run_with_state_and_log_level(
    state: State,
    logger: Box<dyn log::Log>,
    max_level: LevelFilter,
) -> Result<(), Report> {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder.enable_all().build().into_diagnostic()?;
    rt.block_on(async move { start_ui_with_state(state, logger, max_level).await })
}

#[allow(dead_code)]
pub async fn start_ui(
    config: Box<DebuggerConfig>,
    logger: Box<dyn log::Log>,
    max_level: LevelFilter,
) -> Result<(), Report> {
    use ratatui::crossterm as term;

    crate::logger::DebugLogger::install_with_max_level(logger, max_level).into_diagnostic()?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = term::terminal::disable_raw_mode();
        let _ = term::execute!(std::io::stdout(), term::terminal::LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let mut app = App::new(config).await?;
    app.run().await?;

    Ok(())
}

async fn start_ui_with_state(
    state: State,
    logger: Box<dyn log::Log>,
    max_level: LevelFilter,
) -> Result<(), Report> {
    use ratatui::crossterm as term;

    crate::logger::DebugLogger::install_with_max_level(logger, max_level).into_diagnostic()?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = term::terminal::disable_raw_mode();
        let _ = term::execute!(std::io::stdout(), term::terminal::LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let mut app = App::from_state(state).await?;
    app.run().await?;

    Ok(())
}
