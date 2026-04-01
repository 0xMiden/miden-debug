use std::str::FromStr;

use crate::debug::{BreakpointType, ReadMemoryExpr};

/// Commands available in the REPL debugger.
#[derive(Debug, Clone)]
pub enum ReplCommand {
    /// Execute one VM cycle
    Step,
    /// Execute N VM cycles
    StepN(usize),
    /// Execute until next instruction boundary
    Next,
    /// Run until breakpoint or end
    Continue,
    /// Run until current function returns
    Finish,
    /// Set a breakpoint
    Break(BreakpointType),
    /// List all breakpoints
    Breakpoints,
    /// Delete breakpoint(s) - None means delete all
    Delete(Option<u8>),
    /// Show operand stack
    Stack,
    /// Show memory at address with optional count
    Memory(ReadMemoryExpr),
    /// Show local variables
    Locals,
    /// Show all debug variables
    Vars,
    /// Show current source location
    Where,
    /// Show recent instructions
    List,
    /// Show call stack / backtrace
    Backtrace,
    /// Restart program
    Reload,
    /// Show help
    Help,
    /// Exit debugger
    Quit,
}

impl FromStr for ReplCommand {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty command".into());
        }

        // Split into command and arguments
        let (cmd, args) = match s.split_once(char::is_whitespace) {
            Some((cmd, args)) => (cmd, Some(args.trim())),
            None => (s, None),
        };

        match cmd {
            // Step commands
            "s" | "step" => match args {
                Some(n) => {
                    let n = n.parse::<usize>().map_err(|e| format!("invalid step count: {e}"))?;
                    Ok(ReplCommand::StepN(n))
                }
                None => Ok(ReplCommand::Step),
            },
            "n" | "next" => Ok(ReplCommand::Next),
            "c" | "continue" => Ok(ReplCommand::Continue),
            "e" | "finish" => Ok(ReplCommand::Finish),

            // Breakpoint commands
            "b" | "break" | "breakpoint" => {
                let args = args.ok_or("breakpoint requires a specification")?;
                let bp_type = args.parse::<BreakpointType>()?;
                Ok(ReplCommand::Break(bp_type))
            }
            "bp" | "breakpoints" => Ok(ReplCommand::Breakpoints),
            "d" | "delete" => match args {
                Some(id) => {
                    let id = id.parse::<u8>().map_err(|e| format!("invalid breakpoint id: {e}"))?;
                    Ok(ReplCommand::Delete(Some(id)))
                }
                None => Ok(ReplCommand::Delete(None)),
            },

            // Inspection commands
            "stack" => Ok(ReplCommand::Stack),
            "mem" | "memory" => {
                let args = args.ok_or("memory command requires an address")?;
                let expr = args.parse::<ReadMemoryExpr>()?;
                Ok(ReplCommand::Memory(expr))
            }
            "locals" => Ok(ReplCommand::Locals),
            "vars" | "variables" => Ok(ReplCommand::Vars),
            "where" | "w" => Ok(ReplCommand::Where),
            "l" | "list" => Ok(ReplCommand::List),
            "bt" | "backtrace" => Ok(ReplCommand::Backtrace),

            // Control commands
            "reload" => Ok(ReplCommand::Reload),
            "h" | "help" | "?" => Ok(ReplCommand::Help),
            "q" | "quit" | "exit" => Ok(ReplCommand::Quit),

            _ => Err(format!("unknown command: {cmd}")),
        }
    }
}

impl ReplCommand {
    /// Returns the help text for all commands.
    pub fn help_text() -> &'static str {
        r#"Available commands:

Execution:
  s, step [N]        Execute one (or N) VM cycle(s)
  n, next            Execute until next instruction boundary
  c, continue        Run until breakpoint or end
  e, finish          Run until current function returns
  reload             Restart program execution

Breakpoints:
  b, break <spec>    Set a breakpoint
                     Specs: at <cycle>, after <N>, in <proc>, <file>:<line>, <file>
  bp, breakpoints    List all breakpoints
  d, delete [id]     Delete breakpoint by id, or all if no id given

Inspection:
  stack              Show operand stack
  mem <addr> [type]  Show memory at address (e.g., mem 0x100 u32)
  locals             Show local variables
  vars               Show all debug variables
  where              Show current source location
  l, list            Show recent instructions
  bt, backtrace      Show call stack

Other:
  h, help            Show this help
  q, quit            Exit debugger
"#
    }
}
