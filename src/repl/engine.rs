use std::{io::Write, rc::Rc};

use miden_assembly_syntax::diagnostics::Report;

use super::commands::ReplCommand;
use crate::{
    config::DebuggerConfig,
    debug::{Breakpoint, BreakpointType},
    ui::state::State,
};

/// The result of executing a single REPL line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Continue reading commands.
    Continue,
    /// The user requested to quit the session.
    Quit,
}

/// The core debugger REPL logic, decoupled from any particular I/O frontend.
///
/// Command output is written to a caller-provided [`Write`] sink rather than
/// directly to stdout. This lets the same command set drive both the
/// interactive session (writing to stdout, see [`super::session::ReplSession`])
/// and the scriptable test harness (writing to an in-memory buffer, see
/// [`super::script::run_script`]).
pub struct ReplEngine {
    state: State,
}

impl ReplEngine {
    /// Create an engine from a debugger configuration (loads a program package).
    pub fn new(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        Ok(Self {
            state: State::new(config)?,
        })
    }

    /// Create an engine from a debugger configuration, assembling the program
    /// input directly when it is Miden Assembly source rather than a compiled
    /// `.masp` package.
    ///
    /// This lets the batch command runner debug a `.masm` file straight from
    /// disk (used by the lit/FileCheck tests), while still loading `.masp`
    /// packages via the normal [`State::new`] path.
    pub fn from_config(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        use crate::{input::InputFile, linker::LibraryKind};

        // A `.masp` package is the only binary program format `State::new` loads;
        // treat any other input (a `.masm` file) as MASM source to assemble.
        let is_masp = matches!(
            config.input.as_ref().and_then(InputFile::library_kind),
            Some(LibraryKind::Masp)
        );

        match config.input.as_ref() {
            Some(input) if !is_masp => {
                let bytes = input
                    .bytes()
                    .ok_or_else(|| Report::msg("failed to read MASM program input"))?;
                let source = core::str::from_utf8(&bytes)
                    .map_err(|e| Report::msg(format!("MASM source is not valid UTF-8: {e}")))?;
                // `config.args` use the debugger's public `Felt` wrapper; `State`
                // works with the raw processor field element.
                let args = config.args.iter().map(|f| f.0).collect();
                Ok(Self {
                    state: State::from_masm_source(source, args)?,
                })
            }
            _ => Self::new(config),
        }
    }

    /// Render the prompt for the current execution state.
    ///
    /// When `color` is true, ANSI escape codes are emitted (for interactive
    /// use). When false, the prompt is plain text, which keeps scripted
    /// transcripts stable and matchable.
    pub fn make_prompt(&self, color: bool) -> String {
        let cycle = self.state.executor().cycle;

        let (status, fg) = if self.state.executor().stopped {
            if self.state.execution_failed().is_some() {
                ("ERR", "1;31")
            } else {
                ("END", "1;32")
            }
        } else if self.state.stopped {
            ("STOP", "1;33")
        } else {
            ("", "")
        };

        if !color {
            return if status.is_empty() {
                format!("[cycle {cycle}] > ")
            } else {
                format!("[cycle {cycle} {status}] > ")
            };
        }

        if status.is_empty() {
            format!("\x1b[36m[\x1b[0mcycle {cycle}\x1b[36m]\x1b[0m > ")
        } else {
            format!("\x1b[36m[\x1b[0mcycle {cycle} \x1b[{fg}m{status}\x1b[0m\x1b[36m]\x1b[0m > ")
        }
    }

    /// Print the current source location / procedure to `out`.
    pub fn print_location(&self, out: &mut dyn Write) {
        let proc_name = self.state.current_procedure().unwrap_or_else(|| Rc::from("<unknown>"));
        if let Some(resolved) = self.state.current_display_location() {
            let _ = writeln!(out, "at {} in {}", resolved, proc_name);
        } else if self.state.executor().callstack.current_frame().is_some() {
            let _ = writeln!(out, "in {}", proc_name);
        }
    }

    /// Parse and execute a single command line, writing any output to `out`.
    ///
    /// Returns [`Outcome::Quit`] when the user asked to exit. Parse errors and
    /// command errors are returned as `Err` for the caller to surface.
    pub fn execute_line(&mut self, line: &str, out: &mut dyn Write) -> Result<Outcome, String> {
        let cmd = line.parse::<ReplCommand>()?;
        if matches!(cmd, ReplCommand::Quit) {
            return Ok(Outcome::Quit);
        }
        self.execute_command(cmd, out)?;
        Ok(Outcome::Continue)
    }

    fn execute_command(&mut self, cmd: ReplCommand, out: &mut dyn Write) -> Result<(), String> {
        match cmd {
            ReplCommand::Step => self.cmd_step(1, out),
            ReplCommand::StepN(n) => self.cmd_step(n, out),
            ReplCommand::Next => self.cmd_next(out),
            ReplCommand::NextLine => self.cmd_next_line(out),
            ReplCommand::Continue => self.cmd_continue(out),
            ReplCommand::Finish => self.cmd_finish(out),
            ReplCommand::Break(bp_type) => self.cmd_break(bp_type, out),
            ReplCommand::Breakpoints => self.cmd_breakpoints(out),
            ReplCommand::Delete(id) => self.cmd_delete(id, out),
            ReplCommand::Stack => self.cmd_stack(out),
            ReplCommand::Memory(expr) => self.cmd_memory(&expr, out),
            ReplCommand::Locals => self.cmd_locals(out),
            ReplCommand::Vars(show_all) => self.cmd_vars(show_all, out),
            ReplCommand::Where => self.cmd_where(out),
            ReplCommand::List => self.cmd_list(out),
            ReplCommand::Backtrace => self.cmd_backtrace(out),
            ReplCommand::Reload => self.cmd_reload(out),
            ReplCommand::Help => self.cmd_help(out),
            ReplCommand::Quit => unreachable!("quit handled in execute_line"),
        }
    }

    fn cmd_step(&mut self, n: usize, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot step".into());
        }

        for _ in 0..n {
            if self.state.executor().stopped {
                break;
            }
            match self.state.executor_mut().step() {
                Ok(_) => {}
                Err(err) => {
                    let msg = format!("execution error: {err}");
                    self.state.set_execution_failed(err);
                    return Err(msg);
                }
            }
        }

        self.print_location(out);
        Ok(())
    }

    fn cmd_next(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::Next);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location(out);
        Ok(())
    }

    fn cmd_next_line(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::NextLine);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location(out);
        Ok(())
    }

    fn cmd_continue(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.stopped = false;
        self.run_until_stopped();

        if self.state.executor().stopped {
            if let Some(err) = self.state.execution_failed() {
                let _ = writeln!(out, "Program terminated with error: {}", err);
            } else {
                let _ = writeln!(out, "Program terminated successfully");
            }
        } else {
            self.print_location(out);
        }

        Ok(())
    }

    fn cmd_finish(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::Finish);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location(out);
        Ok(())
    }

    fn run_until_stopped(&mut self) {
        let start_cycle = self.state.executor().cycle;
        let start_asmop = self.state.executor().current_asmop.clone();
        let start_proc = self.state.current_procedure();
        let start_line_loc = self.state.current_display_location();
        let source_path_prefixes = self.state.source_path_prefixes();
        let minimum_source_line =
            start_proc.as_deref().zip(start_line_loc.as_ref()).and_then(|(proc, loc)| {
                self.state.minimum_source_line_for_proc(proc, loc.source_file.uri().as_str())
            });
        let mut previous_proc = self.state.current_procedure();
        let mut pending_called_breakpoints = Vec::new();
        let mut breakpoints: Vec<Breakpoint> = core::mem::take(&mut self.state.breakpoints);
        self.state.breakpoints_hit.clear();

        loop {
            // Check if program has terminated
            if self.state.executor().stopped {
                self.state.stopped = true;
                break;
            }

            let mut consume_most_recent_finish = false;
            match self.state.executor_mut().step() {
                Ok(Some(ref exited)) if exited.should_break_on_exit() => {
                    consume_most_recent_finish = true;
                }
                Ok(_) => {}
                Err(err) => {
                    self.state.set_execution_failed(err);
                    self.state.stopped = true;
                    break;
                }
            }

            if breakpoints.is_empty() {
                continue;
            }

            // Get current execution state for breakpoint checking
            let is_op_boundary = self.state.executor().current_asmop.is_some();
            let loc = self.state.current_location();
            let line_loc = self.state.current_display_location();
            let proc = self.state.current_procedure();

            // Check breakpoints
            let current_cycle = self.state.executor().cycle;
            let cycles_stepped = current_cycle - start_cycle;
            let has_internal_breakpoint = breakpoints.iter().any(|bp| bp.is_internal());

            breakpoints.retain_mut(|bp| {
                if let Some(n) = bp.cycles_to_skip(current_cycle) {
                    if cycles_stepped >= n {
                        let retained = !bp.is_one_shot();
                        if retained {
                            self.state.breakpoints_hit.push(bp.clone());
                        } else {
                            self.state.breakpoints_hit.push(core::mem::take(bp));
                        }
                        return retained;
                    }
                    return true;
                }

                if cycles_stepped > 0
                    && is_op_boundary
                    && matches!(&bp.ty, BreakpointType::Next)
                    && self.state.executor().current_asmop != start_asmop
                {
                    self.state.breakpoints_hit.push(core::mem::take(bp));
                    return false;
                }

                if cycles_stepped > 0
                    && is_op_boundary
                    && matches!(&bp.ty, BreakpointType::NextLine)
                    && State::is_next_source_line(
                        start_proc.as_deref(),
                        start_line_loc.as_ref(),
                        proc.as_deref(),
                        line_loc.as_ref(),
                        &source_path_prefixes,
                        minimum_source_line,
                    )
                {
                    self.state.breakpoints_hit.push(core::mem::take(bp));
                    return false;
                }

                if has_internal_breakpoint && !bp.is_internal() {
                    return true;
                }

                if let Some(loc) = loc.as_ref()
                    && bp.should_break_at(loc)
                {
                    let retained = !bp.is_one_shot();
                    if retained {
                        self.state.breakpoints_hit.push(bp.clone());
                    } else {
                        self.state.breakpoints_hit.push(core::mem::take(bp));
                    }
                    return retained;
                }

                if matches!(&bp.ty, BreakpointType::Called(_))
                    && let Some(proc) = proc.as_deref()
                {
                    let matched = bp.should_break_in(proc);
                    if !matched {
                        pending_called_breakpoints.retain(|id| *id != bp.id);
                        return true;
                    }

                    let was_matched = previous_proc
                        .as_deref()
                        .is_some_and(|previous| bp.should_break_in(previous));
                    let matched_at_start =
                        start_proc.as_deref().is_some_and(|start| bp.should_break_in(start));
                    let pending = pending_called_breakpoints.contains(&bp.id);
                    let entered_matching_proc = !was_matched && !matched_at_start;

                    if entered_matching_proc
                        && self.state.should_defer_called_breakpoint(proc, line_loc.as_ref())
                    {
                        if !pending {
                            pending_called_breakpoints.push(bp.id);
                        }
                        return true;
                    }

                    if entered_matching_proc
                        || (pending
                            && self.state.deferred_called_breakpoint_is_ready(line_loc.as_ref()))
                    {
                        pending_called_breakpoints.retain(|id| *id != bp.id);
                        let retained = !bp.is_one_shot();
                        if retained {
                            self.state.breakpoints_hit.push(bp.clone());
                        } else {
                            self.state.breakpoints_hit.push(core::mem::take(bp));
                        }
                        return retained;
                    }
                }

                true
            });

            // Handle Finish breakpoint
            if consume_most_recent_finish
                && let Some(id) = breakpoints.iter().rev().find_map(|bp| {
                    if matches!(bp.ty, BreakpointType::Finish) {
                        Some(bp.id)
                    } else {
                        None
                    }
                })
            {
                breakpoints.retain(|bp| bp.id != id);
                self.state.stopped = true;
                break;
            }

            if !self.state.breakpoints_hit.is_empty() {
                self.state.stopped = true;
                break;
            }

            previous_proc = proc;
        }

        // Restore breakpoints
        self.state.breakpoints = breakpoints;
    }

    fn cmd_break(&mut self, bp_type: BreakpointType, out: &mut dyn Write) -> Result<(), String> {
        self.state.create_breakpoint(bp_type.clone());
        let id = self.state.breakpoints.last().map(|bp| bp.id).unwrap_or(0);
        let _ = writeln!(out, "Breakpoint {} set: {}", id, format_bp_type(&bp_type));
        Ok(())
    }

    fn cmd_breakpoints(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.breakpoints.is_empty() {
            let _ = writeln!(out, "No breakpoints set");
            return Ok(());
        }

        let _ = writeln!(out, "Breakpoints:");
        for bp in &self.state.breakpoints {
            if !bp.is_internal() {
                let _ = writeln!(out, "  [{}] {}", bp.id, format_bp_type(&bp.ty));
            }
        }
        Ok(())
    }

    fn cmd_delete(&mut self, id: Option<u8>, out: &mut dyn Write) -> Result<(), String> {
        match id {
            Some(id) => {
                let count_before = self.state.breakpoints.len();
                self.state.breakpoints.retain(|bp| bp.id != id);
                if self.state.breakpoints.len() < count_before {
                    let _ = writeln!(out, "Deleted breakpoint {}", id);
                } else {
                    return Err(format!("no breakpoint with id {}", id));
                }
            }
            None => {
                // Delete only user-created (non-internal) breakpoints
                self.state.breakpoints.retain(|bp| bp.is_internal());
                let _ = writeln!(out, "Deleted all breakpoints");
            }
        }
        Ok(())
    }

    fn cmd_stack(&mut self, out: &mut dyn Write) -> Result<(), String> {
        let stack = &self.state.executor().current_stack;

        if stack.is_empty() {
            let _ = writeln!(out, "Stack is empty");
            return Ok(());
        }

        let _ = writeln!(out, "Operand Stack ({} elements):", stack.len());
        for (i, elem) in stack.iter().enumerate() {
            let val = elem.as_canonical_u64();
            let marker = if i == 0 { ">" } else { " " };
            let _ = writeln!(out, "  {} [{}] {} (0x{:x})", marker, i, val, val);
        }
        Ok(())
    }

    fn cmd_memory(
        &mut self,
        expr: &crate::debug::ReadMemoryExpr,
        out: &mut dyn Write,
    ) -> Result<(), String> {
        let result = self.state.read_memory(expr)?;
        let _ = writeln!(out, "{}", result);
        Ok(())
    }

    fn cmd_locals(&mut self, out: &mut dyn Write) -> Result<(), String> {
        let output = self.state.format_variables(false);
        let _ = writeln!(out, "{}", output);
        Ok(())
    }

    fn cmd_vars(&mut self, show_all: bool, out: &mut dyn Write) -> Result<(), String> {
        let output = self.state.format_variables(show_all);
        let _ = writeln!(out, "{}", output);
        Ok(())
    }

    fn cmd_where(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if self.state.executor().callstack.current_frame().is_some() {
            let proc_name = self.state.current_procedure().unwrap_or_else(|| Rc::from("<unknown>"));

            if let Some(resolved) = self.state.current_display_location() {
                let _ = writeln!(
                    out,
                    "{}:{}:{} in {}",
                    resolved.source_file.uri().as_str(),
                    resolved.line,
                    resolved.col,
                    proc_name
                );
            } else {
                let _ = writeln!(out, "in {} (no source location available)", proc_name);
            }
        } else {
            let _ = writeln!(out, "No current frame");
        }
        Ok(())
    }

    fn cmd_list(&mut self, out: &mut dyn Write) -> Result<(), String> {
        if let Some(frame) = self.state.executor().callstack.current_frame() {
            let recent = frame.recent();
            if recent.is_empty() {
                let _ = writeln!(out, "No recent instructions");
                return Ok(());
            }

            let _ = writeln!(out, "Recent instructions:");
            for (i, op) in recent.iter().enumerate() {
                let marker = if i == recent.len() - 1 { ">" } else { " " };
                let _ = writeln!(out, "  {} {}", marker, op.display());
            }
        } else {
            let _ = writeln!(out, "No current frame");
        }
        Ok(())
    }

    fn cmd_backtrace(&mut self, out: &mut dyn Write) -> Result<(), String> {
        let frames = self.state.executor().callstack.frames();
        if frames.is_empty() {
            let _ = writeln!(out, "No call stack");
            return Ok(());
        }

        let _ = writeln!(out, "Backtrace ({} frames):", frames.len());
        for (i, frame) in frames.iter().rev().enumerate() {
            let proc_name = frame.procedure("").unwrap_or_else(|| Rc::from("<unknown>"));
            let loc_str = frame
                .last_resolved(&*self.state.source_manager)
                .map(|r| format!(" at {}", r))
                .unwrap_or_default();

            let _ = writeln!(out, "  #{} {}{}", i, proc_name, loc_str);
        }
        Ok(())
    }

    fn cmd_reload(&mut self, out: &mut dyn Write) -> Result<(), String> {
        self.state.reload().map_err(|e| format!("reload failed: {e}"))?;
        let _ = writeln!(out, "Program reloaded");
        self.print_location(out);
        Ok(())
    }

    fn cmd_help(&mut self, out: &mut dyn Write) -> Result<(), String> {
        let _ = writeln!(out, "{}", ReplCommand::help_text());
        Ok(())
    }
}

fn format_bp_type(ty: &BreakpointType) -> String {
    match ty {
        BreakpointType::Step => "next cycle".into(),
        BreakpointType::StepN(n) => format!("after {} cycles", n),
        BreakpointType::StepTo(c) => format!("at cycle {}", c),
        BreakpointType::Next => "next instruction".into(),
        BreakpointType::NextLine => "next source line".into(),
        BreakpointType::Finish => "function return".into(),
        BreakpointType::File(pat) => pat.as_str().to_string(),
        BreakpointType::Line { pattern, line } => format!("{}:{}", pattern.as_str(), line),
        BreakpointType::Opcode(matcher) => format!("opcode {matcher}"),
        BreakpointType::Called(pat) => format!("call {}", pat.as_str()),
        BreakpointType::Trace(event) => format!("trace event {event:?}"),
    }
}
