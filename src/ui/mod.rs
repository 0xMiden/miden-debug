mod action;
mod app;
mod duration;
mod pages;
mod panes;
pub mod state;
mod syntax_highlighting;
mod tui;

use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};

use self::{action::Action, app::App};
use crate::config::DebuggerConfig;

pub fn run(config: Box<DebuggerConfig>, logger: Box<dyn log::Log>) -> Result<(), Report> {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let rt = builder.enable_all().build().into_diagnostic()?;
    rt.block_on(async move { start_ui(config, logger).await })
}

pub async fn start_ui(
    config: Box<DebuggerConfig>,
    logger: Box<dyn log::Log>,
) -> Result<(), Report> {
    use ratatui::crossterm as term;

    crate::logger::DebugLogger::install(logger);

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
