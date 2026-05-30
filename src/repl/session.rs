use miden_assembly_syntax::diagnostics::Report;
use rustyline::{DefaultEditor, error::ReadlineError};

use super::engine::{Outcome, ReplEngine};
use crate::config::DebuggerConfig;

/// Interactive REPL session for the debugger.
///
/// This is the interactive frontend over [`ReplEngine`]: it reads lines via
/// `rustyline` and writes command output to stdout. All command execution and
/// output formatting lives in [`ReplEngine`], so it can be reused by the
/// scriptable test harness without a terminal.
pub struct ReplSession {
    engine: ReplEngine,
    editor: DefaultEditor,
}

impl ReplSession {
    /// Create a new REPL session from the given config.
    pub fn new(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        let engine = ReplEngine::new(config)?;
        let editor = DefaultEditor::new()
            .map_err(|e| Report::msg(format!("failed to create editor: {e}")))?;

        Ok(Self { engine, editor })
    }

    /// Run the main REPL loop.
    pub fn run(&mut self) -> Result<(), Report> {
        self.print_welcome();

        let mut stdout = std::io::stdout();
        self.engine.print_location(&mut stdout);

        loop {
            let prompt = self.engine.make_prompt(true);
            match self.editor.readline(&prompt) {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    // Add to history
                    let _ = self.editor.add_history_entry(line);

                    // Parse and execute command
                    match self.engine.execute_line(line, &mut stdout) {
                        Ok(Outcome::Quit) => {
                            println!("\x1b[36mGoodbye!\x1b[0m");
                            break;
                        }
                        Ok(Outcome::Continue) => {}
                        Err(e) => {
                            eprintln!("\x1b[31mError:\x1b[0m {e}");
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!("^C");
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!("\x1b[36mGoodbye!\x1b[0m");
                    break;
                }
                Err(e) => {
                    eprintln!("\x1b[31mError reading line:\x1b[0m {e}");
                    break;
                }
            }
        }

        Ok(())
    }

    fn print_welcome(&self) {
        println!("\x1b[1;36mMiden Debugger REPL\x1b[0m");
        println!("Type \x1b[33mhelp\x1b[0m for available commands.");
        println!();
    }
}
