use std::{cell::RefCell, io::Write, rc::Rc, str::FromStr};

use miden_assembly_syntax::diagnostics::Report;

use crate::{
    DebuggerConfig,
    debug::{Breakpoint, BreakpointType, ReadMemoryExpr},
    repl::engine::{Outcome, ReplEngine, format_bp_type},
};

/// Source location snapshot exposed to scripting frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSourceLocation {
    pub path: String,
    pub line: u32,
    pub column: u32,
}

/// Variable snapshot exposed to scripting frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptValue {
    pub name: String,
    pub value: Option<u64>,
    pub location: String,
    pub source: Option<ScriptSourceLocation>,
}

/// Current frame snapshot exposed to scripting frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptFrame {
    pub function_name: Option<String>,
    pub source_location: Option<ScriptSourceLocation>,
    pub variables: Vec<ScriptValue>,
}

/// Breakpoint snapshot exposed to scripting frontends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptBreakpoint {
    pub id: u8,
    pub spec: String,
    pub internal: bool,
    pub one_shot: bool,
}

/// Execution context snapshot exposed to scripting callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptExecutionContext {
    pub cycle: usize,
    pub stopped: bool,
    pub terminated: bool,
    pub frame: ScriptFrame,
}

/// Stable facade used by debugger scripting integrations.
#[derive(Clone)]
pub struct ScriptDebugger {
    engine: Rc<RefCell<ReplEngine>>,
}

impl ScriptDebugger {
    /// Create a script debugger from the same configuration used by the REPL.
    pub fn new(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        Self::from_config(config)
    }

    /// Create a script debugger from a debugger configuration.
    pub fn from_config(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        Ok(Self {
            engine: Rc::new(RefCell::new(ReplEngine::from_config(config)?)),
        })
    }

    /// Create a script debugger from inline MASM source.
    ///
    /// This is useful for tests and small programmatic debugging harnesses.
    pub fn from_masm_source(
        source: &str,
        args: Vec<crate::processor::Felt>,
    ) -> Result<Self, Report> {
        let state = crate::ui::state::State::from_masm_source(source, args)?;
        Ok(Self {
            engine: Rc::new(RefCell::new(ReplEngine::from_state(state))),
        })
    }

    /// Render the normal debugger prompt.
    pub(crate) fn make_prompt(&self, color: bool) -> String {
        self.engine.borrow().make_prompt(color)
    }

    /// Print the current source location / procedure.
    pub(crate) fn print_location(&self, out: &mut dyn Write) {
        self.engine.borrow().print_location(out);
    }

    /// Execute a REPL command line while preserving the raw REPL outcome.
    pub(crate) fn execute_repl_line(
        &self,
        line: &str,
        out: &mut dyn Write,
    ) -> Result<Outcome, String> {
        self.engine.borrow_mut().execute_line(line, out)
    }

    /// Execute a debugger command and capture its textual output.
    pub fn handle_command(&self, command: &str) -> Result<String, String> {
        let mut output = Vec::new();
        match self.engine.borrow_mut().execute_line(command, &mut output)? {
            Outcome::Continue => {}
            Outcome::Quit => return Err("quit requested".into()),
        }

        String::from_utf8(output).map_err(|err| format!("command output was not UTF-8: {err}"))
    }

    /// Current VM cycle.
    pub fn cycle(&self) -> usize {
        self.engine.borrow().state().executor().cycle
    }

    /// Whether execution is currently stopped at a debugger stop point.
    pub fn stopped(&self) -> bool {
        self.engine.borrow().state().stopped
    }

    /// Whether the debuggee has terminated.
    pub fn terminated(&self) -> bool {
        self.engine.borrow().state().executor().stopped
    }

    /// Current operand stack snapshot, in debugger display order.
    pub fn stack(&self) -> Vec<u64> {
        self.engine
            .borrow()
            .state()
            .executor()
            .current_stack
            .iter()
            .map(|felt| felt.as_canonical_u64())
            .collect()
    }

    /// Source path prefix mappings currently configured for this debugger.
    pub fn source_path_prefixes(&self) -> Vec<String> {
        self.engine.borrow().state().source_path_prefixes()
    }

    /// Current frame snapshot.
    pub fn frame(&self) -> ScriptFrame {
        self.frame_with_variables(false)
    }

    /// Current frame snapshot with either source-visible or all debug variables.
    pub fn frame_with_variables(&self, show_all: bool) -> ScriptFrame {
        let engine = self.engine.borrow();
        let state = engine.state();
        let source_location = state.current_display_location().map(|loc| ScriptSourceLocation {
            path: loc.source_file.uri().as_str().to_string(),
            line: loc.line,
            column: loc.col,
        });
        let function_name = state.current_procedure().map(|name| name.to_string());
        let variables = state
            .current_variables(show_all)
            .into_iter()
            .map(|variable| ScriptValue {
                name: variable.name,
                value: variable.value.map(|felt| felt.as_canonical_u64()),
                location: variable.location,
                source: variable.source.map(|source| ScriptSourceLocation {
                    path: source.path,
                    line: source.line,
                    column: source.column,
                }),
            })
            .collect();

        ScriptFrame {
            function_name,
            source_location,
            variables,
        }
    }

    /// Current execution context snapshot.
    pub fn execution_context(&self) -> ScriptExecutionContext {
        ScriptExecutionContext {
            cycle: self.cycle(),
            stopped: self.stopped(),
            terminated: self.terminated(),
            frame: self.frame(),
        }
    }

    /// Current user-visible breakpoints.
    pub fn breakpoints(&self) -> Vec<ScriptBreakpoint> {
        self.engine
            .borrow()
            .state()
            .breakpoints
            .iter()
            .filter(|bp| !bp.is_internal())
            .map(script_breakpoint_from)
            .collect()
    }

    /// Breakpoints hit at the current stop.
    pub fn hit_breakpoints(&self) -> Vec<ScriptBreakpoint> {
        self.engine
            .borrow()
            .state()
            .breakpoints_hit
            .iter()
            .filter(|bp| !bp.is_internal())
            .map(script_breakpoint_from)
            .collect()
    }

    /// Clear the current hit-breakpoint list.
    pub fn clear_hit_breakpoints(&self) {
        self.engine.borrow_mut().state_mut().breakpoints_hit.clear();
    }

    /// Set a breakpoint from the normal debugger breakpoint grammar.
    pub fn set_breakpoint(&self, spec: &str) -> Result<ScriptBreakpoint, String> {
        let ty = BreakpointType::from_str(spec)?;
        let mut engine = self.engine.borrow_mut();
        engine.state_mut().create_breakpoint(ty);
        let bp = engine
            .state()
            .breakpoints
            .last()
            .ok_or_else(|| "breakpoint was not created".to_string())?;
        Ok(script_breakpoint_from(bp))
    }

    /// Delete one breakpoint by id, or all user breakpoints if `id` is `None`.
    pub fn delete_breakpoint(&self, id: Option<u8>) -> Result<(), String> {
        let mut engine = self.engine.borrow_mut();
        let state = engine.state_mut();
        match id {
            Some(id) => {
                let before = state.breakpoints.len();
                state.breakpoints.retain(|bp| bp.id != id);
                if state.breakpoints.len() == before {
                    return Err(format!("no breakpoint with id {id}"));
                }
            }
            None => {
                state.breakpoints.retain(|bp| bp.is_internal());
            }
        }
        Ok(())
    }

    /// Read memory using the debugger memory expression grammar.
    pub fn read_memory(&self, expression: &str) -> Result<String, String> {
        let expression = expression.parse::<ReadMemoryExpr>()?;
        self.engine.borrow_mut().state_mut().read_memory(&expression)
    }

    /// Step one or more VM cycles.
    pub fn step(&self, count: usize) -> Result<String, String> {
        if count <= 1 {
            self.handle_command("step")
        } else {
            self.handle_command(&format!("step {count}"))
        }
    }

    /// Step to the next instruction boundary.
    pub fn next(&self) -> Result<String, String> {
        self.handle_command("next")
    }

    /// Step to the next source line.
    pub fn next_line(&self) -> Result<String, String> {
        self.handle_command("next-line")
    }

    /// Continue execution until the next breakpoint or termination.
    pub fn continue_(&self) -> Result<String, String> {
        self.handle_command("continue")
    }

    /// Continue execution until the current frame returns.
    pub fn finish(&self) -> Result<String, String> {
        self.handle_command("finish")
    }

    /// Reload the debuggee.
    pub fn reload(&self) -> Result<String, String> {
        self.handle_command("reload")
    }
}

fn script_breakpoint_from(bp: &Breakpoint) -> ScriptBreakpoint {
    ScriptBreakpoint {
        id: bp.id,
        spec: format_bp_type(&bp.ty),
        internal: bp.is_internal(),
        one_shot: bp.is_one_shot(),
    }
}

#[cfg(test)]
mod tests {
    use miden_core::Felt;

    use super::*;

    #[test]
    fn script_debugger_executes_commands_and_exposes_state() {
        let debugger = ScriptDebugger::from_masm_source(
            r#"
begin
    push.3
    push.4
    add
end
"#,
            Vec::<Felt>::new(),
        )
        .unwrap();

        assert_eq!(debugger.cycle(), 0);

        let output = debugger.handle_command("step").unwrap();
        assert!(output.contains("in") || output.is_empty(), "unexpected output: {output}");
        assert_eq!(debugger.cycle(), 1);

        let stack_output = debugger.handle_command("stack").unwrap();
        assert!(stack_output.contains("Operand Stack"));
    }

    #[test]
    fn script_debugger_can_manage_breakpoints() {
        let debugger = ScriptDebugger::from_masm_source(
            r#"
begin
    push.3
end
"#,
            Vec::<Felt>::new(),
        )
        .unwrap();

        let bp = debugger.set_breakpoint("after 1").unwrap();
        assert_eq!(bp.id, 0);
        assert_eq!(debugger.breakpoints().len(), 1);

        debugger.delete_breakpoint(Some(bp.id)).unwrap();
        assert!(debugger.breakpoints().is_empty());
    }
}
