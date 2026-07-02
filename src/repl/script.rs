use std::{io::Write, path::Path};

use miden_assembly_syntax::diagnostics::Report;

use super::engine::{Outcome, ReplEngine};
use crate::config::DebuggerConfig;

/// Run a script of debugger commands non-interactively, then return.
///
/// This is the batch / "source a command file" mode, analogous to
/// `gdb -x <file> -batch` or `lldb -s <file>`. The program to debug is loaded
/// from `config` exactly as for an interactive session.
///
/// Each line of the file is one REPL command, using the same syntax as the
/// interactive prompt. Blank lines and lines beginning with `#` are ignored, so
/// scripts can be commented — and a script can double as a lit/FileCheck test
/// input, where the `# RUN:` and `# CHECK:` lines are skipped here and consumed
/// by the test runner instead. A `quit` command, or end of file, ends the run.
///
/// Command output goes to stdout; command/parse errors go to stderr and do not
/// abort the script.
pub fn run_commands(config: Box<DebuggerConfig>, script_path: &Path) -> Result<(), Report> {
    let script = std::fs::read_to_string(script_path).map_err(|e| {
        Report::msg(format!("failed to read command file {}: {e}", script_path.display()))
    })?;

    let mut engine = ReplEngine::from_config(config)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    run_lines(&mut engine, &script, &mut out);
    Ok(())
}

fn run_lines(engine: &mut ReplEngine, script: &str, out: &mut dyn Write) {
    for line in script.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        match engine.execute_line(line, out) {
            Ok(Outcome::Quit) => break,
            Ok(Outcome::Continue) => {}
            Err(e) => eprintln!("error: {e}"),
        }
    }
}
