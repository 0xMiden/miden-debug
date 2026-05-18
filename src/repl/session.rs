use std::rc::Rc;

use miden_assembly_syntax::diagnostics::Report;
use rustyline::{DefaultEditor, error::ReadlineError};

use super::commands::ReplCommand;
use crate::{
    config::DebuggerConfig,
    debug::{Breakpoint, BreakpointType},
    ui::state::State,
};

/// Interactive REPL session for the debugger.
pub struct ReplSession {
    state: State,
    editor: DefaultEditor,
}

impl ReplSession {
    /// Create a new REPL session from the given config.
    pub fn new(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        let state = State::new(config)?;
        let editor = DefaultEditor::new()
            .map_err(|e| Report::msg(format!("failed to create editor: {e}")))?;

        Ok(Self { state, editor })
    }

    /// Run the main REPL loop.
    pub fn run(&mut self) -> Result<(), Report> {
        self.print_welcome();
        self.print_location();

        loop {
            let prompt = self.make_prompt();
            match self.editor.readline(&prompt) {
                Ok(line) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    // Add to history
                    let _ = self.editor.add_history_entry(line);

                    // Parse and execute command
                    match line.parse::<ReplCommand>() {
                        Ok(cmd) => {
                            if matches!(cmd, ReplCommand::Quit) {
                                println!("\x1b[36mGoodbye!\x1b[0m");
                                break;
                            }
                            if let Err(e) = self.execute_command(cmd) {
                                eprintln!("\x1b[31mError:\x1b[0m {e}");
                            }
                        }
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

    fn make_prompt(&self) -> String {
        let cycle = self.state.executor().cycle;

        if self.state.executor().stopped {
            if self.state.execution_failed().is_some() {
                format!("\x1b[36m[\x1b[0mcycle {cycle} \x1b[1;31mERR\x1b[0m\x1b[36m]\x1b[0m > ")
            } else {
                format!("\x1b[36m[\x1b[0mcycle {cycle} \x1b[1;32mEND\x1b[0m\x1b[36m]\x1b[0m > ")
            }
        } else if self.state.stopped {
            format!("\x1b[36m[\x1b[0mcycle {cycle} \x1b[1;33mSTOP\x1b[0m\x1b[36m]\x1b[0m > ")
        } else {
            format!("\x1b[36m[\x1b[0mcycle {cycle}\x1b[36m]\x1b[0m > ")
        }
    }

    fn print_location(&self) {
        let proc_name = self.state.current_procedure().unwrap_or_else(|| Rc::from("<unknown>"));
        if let Some(resolved) = self.state.current_display_location() {
            println!("at {} in {}", resolved, proc_name);
        } else if self.state.executor().callstack.current_frame().is_some() {
            println!("in {}", proc_name);
        }
    }

    fn execute_command(&mut self, cmd: ReplCommand) -> Result<(), String> {
        match cmd {
            ReplCommand::Step => self.cmd_step(1),
            ReplCommand::StepN(n) => self.cmd_step(n),
            ReplCommand::Next => self.cmd_next(),
            ReplCommand::NextLine => self.cmd_next_line(),
            ReplCommand::Continue => self.cmd_continue(),
            ReplCommand::Finish => self.cmd_finish(),
            ReplCommand::Break(bp_type) => self.cmd_break(bp_type),
            ReplCommand::Breakpoints => self.cmd_breakpoints(),
            ReplCommand::Delete(id) => self.cmd_delete(id),
            ReplCommand::Stack => self.cmd_stack(),
            ReplCommand::Memory(expr) => self.cmd_memory(&expr),
            ReplCommand::Locals => self.cmd_locals(),
            ReplCommand::Vars(show_all) => self.cmd_vars(show_all),
            ReplCommand::Where => self.cmd_where(),
            ReplCommand::List => self.cmd_list(),
            ReplCommand::Backtrace => self.cmd_backtrace(),
            ReplCommand::Reload => self.cmd_reload(),
            ReplCommand::Help => self.cmd_help(),
            ReplCommand::Quit => unreachable!("quit handled in run loop"),
        }
    }

    fn cmd_step(&mut self, n: usize) -> Result<(), String> {
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

        self.print_location();
        Ok(())
    }

    fn cmd_next(&mut self) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::Next);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location();
        Ok(())
    }

    fn cmd_next_line(&mut self) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::NextLine);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location();
        Ok(())
    }

    fn cmd_continue(&mut self) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.stopped = false;
        self.run_until_stopped();

        if self.state.executor().stopped {
            if let Some(err) = self.state.execution_failed() {
                println!("Program terminated with error: {}", err);
            } else {
                println!("Program terminated successfully");
            }
        } else {
            self.print_location();
        }

        Ok(())
    }

    fn cmd_finish(&mut self) -> Result<(), String> {
        if self.state.executor().stopped {
            return Err("program has terminated, cannot continue".into());
        }

        self.state.create_breakpoint(BreakpointType::Finish);
        self.state.stopped = false;
        self.run_until_stopped();
        self.print_location();
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
                {
                    if State::is_next_source_line(
                        start_proc.as_deref(),
                        start_line_loc.as_ref(),
                        proc.as_deref(),
                        line_loc.as_ref(),
                        &source_path_prefixes,
                        minimum_source_line,
                    ) {
                        self.state.breakpoints_hit.push(core::mem::take(bp));
                        return false;
                    }
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
                        && self.state.executor().procedure_has_debug_vars(proc)
                        && self.state.executor().last_debug_var_count == 0
                    {
                        if !pending {
                            pending_called_breakpoints.push(bp.id);
                        }
                        return true;
                    }

                    if entered_matching_proc
                        || (pending && self.state.executor().last_debug_var_count > 0)
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

    fn cmd_break(&mut self, bp_type: BreakpointType) -> Result<(), String> {
        self.state.create_breakpoint(bp_type.clone());
        let id = self.state.breakpoints.last().map(|bp| bp.id).unwrap_or(0);
        println!("Breakpoint {} set: {}", id, format_bp_type(&bp_type));
        Ok(())
    }

    fn cmd_breakpoints(&mut self) -> Result<(), String> {
        if self.state.breakpoints.is_empty() {
            println!("No breakpoints set");
            return Ok(());
        }

        println!("Breakpoints:");
        for bp in &self.state.breakpoints {
            if !bp.is_internal() {
                println!("  [{}] {}", bp.id, format_bp_type(&bp.ty));
            }
        }
        Ok(())
    }

    fn cmd_delete(&mut self, id: Option<u8>) -> Result<(), String> {
        match id {
            Some(id) => {
                let count_before = self.state.breakpoints.len();
                self.state.breakpoints.retain(|bp| bp.id != id);
                if self.state.breakpoints.len() < count_before {
                    println!("Deleted breakpoint {}", id);
                } else {
                    return Err(format!("no breakpoint with id {}", id));
                }
            }
            None => {
                // Delete only user-created (non-internal) breakpoints
                self.state.breakpoints.retain(|bp| bp.is_internal());
                println!("Deleted all breakpoints");
            }
        }
        Ok(())
    }

    fn cmd_stack(&mut self) -> Result<(), String> {
        let stack = &self.state.executor().current_stack;

        if stack.is_empty() {
            println!("Stack is empty");
            return Ok(());
        }

        println!("Operand Stack ({} elements):", stack.len());
        for (i, elem) in stack.iter().enumerate() {
            let val = elem.as_canonical_u64();
            let marker = if i == 0 { ">" } else { " " };
            println!("  {} [{}] {} (0x{:x})", marker, i, val, val);
        }
        Ok(())
    }

    fn cmd_memory(&mut self, expr: &crate::debug::ReadMemoryExpr) -> Result<(), String> {
        let result = self.state.read_memory(expr)?;
        println!("{}", result);
        Ok(())
    }

    fn cmd_locals(&mut self) -> Result<(), String> {
        let output = self.state.format_variables(false);
        println!("{}", output);
        Ok(())
    }

    fn cmd_vars(&mut self, show_all: bool) -> Result<(), String> {
        let output = self.state.format_variables(show_all);
        println!("{}", output);
        Ok(())
    }

    fn cmd_where(&mut self) -> Result<(), String> {
        if self.state.executor().callstack.current_frame().is_some() {
            let proc_name = self.state.current_procedure().unwrap_or_else(|| Rc::from("<unknown>"));

            if let Some(resolved) = self.state.current_display_location() {
                println!(
                    "{}:{}:{} in {}",
                    resolved.source_file.uri().as_str(),
                    resolved.line,
                    resolved.col,
                    proc_name
                );
            } else {
                println!("in {} (no source location available)", proc_name);
            }
        } else {
            println!("No current frame");
        }
        Ok(())
    }

    fn cmd_list(&mut self) -> Result<(), String> {
        if let Some(frame) = self.state.executor().callstack.current_frame() {
            let recent = frame.recent();
            if recent.is_empty() {
                println!("No recent instructions");
                return Ok(());
            }

            println!("Recent instructions:");
            for (i, op) in recent.iter().enumerate() {
                let marker = if i == recent.len() - 1 { ">" } else { " " };
                println!("  {} {}", marker, op.display());
            }
        } else {
            println!("No current frame");
        }
        Ok(())
    }

    fn cmd_backtrace(&mut self) -> Result<(), String> {
        let frames = self.state.executor().callstack.frames();
        if frames.is_empty() {
            println!("No call stack");
            return Ok(());
        }

        println!("Backtrace ({} frames):", frames.len());
        for (i, frame) in frames.iter().rev().enumerate() {
            let proc_name = frame.procedure("").unwrap_or_else(|| Rc::from("<unknown>"));
            let loc_str = frame
                .last_resolved(&*self.state.source_manager)
                .map(|r| format!(" at {}", r))
                .unwrap_or_default();

            println!("  #{} {}{}", i, proc_name, loc_str);
        }
        Ok(())
    }

    fn cmd_reload(&mut self) -> Result<(), String> {
        self.state.reload().map_err(|e| format!("reload failed: {e}"))?;
        println!("Program reloaded");
        self.print_location();
        Ok(())
    }

    fn cmd_help(&mut self) -> Result<(), String> {
        println!("{}", ReplCommand::help_text());
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
        BreakpointType::Opcode(op) => format!("opcode {:?}", op),
        BreakpointType::Called(pat) => format!("call {}", pat.as_str()),
        BreakpointType::Trace(event) => format!("trace event {event:?}"),
    }
}
