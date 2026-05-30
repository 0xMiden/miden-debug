use std::io::Write;

use miden_assembly_syntax::diagnostics::Report;

use super::engine::{Outcome, ReplEngine};

/// Execute a sequence of REPL commands against an inline MASM program and
/// return the full transcript.
///
/// The transcript interleaves, for each command, a plain-text prompt reflecting
/// the execution state *before* the command runs, the echoed command itself,
/// and any output the command produced. This mirrors what an interactive
/// session would display, which makes it convenient to assert against with the
/// `# CHECK:` directives understood by the REPL test harness (`tests/repl/`).
///
/// `commands` should contain only actual REPL commands; comment and directive
/// lines are expected to have been stripped by the caller. Blank lines are
/// ignored. A `quit` command (or end of input) stops execution.
///
/// Command errors are written into the transcript as `Error: <message>` lines
/// rather than aborting, so that scripts can assert on expected failures.
pub fn run_script(masm: &str, commands: &[&str]) -> Result<String, Report> {
    let mut engine = ReplEngine::from_masm(masm, vec![])?;
    let mut out: Vec<u8> = Vec::new();

    engine.print_location(&mut out);

    for line in commands {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let _ = write!(out, "{}", engine.make_prompt(false));
        let _ = writeln!(out, "{line}");

        match engine.execute_line(line, &mut out) {
            Ok(Outcome::Quit) => break,
            Ok(Outcome::Continue) => {}
            Err(e) => {
                let _ = writeln!(out, "Error: {e}");
            }
        }
    }

    String::from_utf8(out)
        .map_err(|e| Report::msg(format!("REPL transcript was not valid UTF-8: {e}")))
}
