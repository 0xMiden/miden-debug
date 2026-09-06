use std::{io::Write, path::PathBuf};

use super::engine::Outcome;
use crate::{
    config::DebuggerConfig,
    script::{PythonScriptSession, ScriptDebugger},
};

pub(crate) fn project_init_file(config: &DebuggerConfig) -> Option<PathBuf> {
    if config.no_user_python_init {
        return None;
    }

    let path = config.working_dir().join(".miden-debug.py");
    path.try_exists().ok().is_some_and(|exists| exists).then_some(path)
}

pub(crate) fn execute_line(
    debugger: &ScriptDebugger,
    python: &mut PythonScriptSession,
    line: &str,
    out: &mut dyn Write,
) -> Result<Outcome, String> {
    match parse_script_command(line) {
        Some(ScriptCommand::Snippet(code)) => {
            let output = python.execute_snippet(code)?;
            write!(out, "{output}").map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::InteractiveConsole) => {
            python.interact()?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::Import(path)) => {
            python.import_file(path)?;
            writeln!(out, "Imported Python script: {path}")
                .map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::AddCommand { name, function }) => {
            python.add_custom_command(name, function)?;
            writeln!(out, "Added Python command: {name}")
                .map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::ListCommands) => {
            let commands = python.custom_commands();
            if commands.is_empty() {
                writeln!(out, "No Python commands registered")
                    .map_err(|err| format!("failed to write output: {err}"))?;
            } else {
                writeln!(out, "Python commands:")
                    .map_err(|err| format!("failed to write output: {err}"))?;
                for command in commands {
                    writeln!(out, "  {command}")
                        .map_err(|err| format!("failed to write output: {err}"))?;
                }
            }
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::DeleteCommand(name)) => {
            python.delete_custom_command(name)?;
            writeln!(out, "Deleted Python command: {name}")
                .map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::AddBreakpointCallback { id, function }) => {
            python.add_breakpoint_callback(id, function)?;
            writeln!(out, "Added Python callback for breakpoint {id}")
                .map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::ListBreakpointCallbacks) => {
            let callbacks = python.breakpoint_callbacks();
            if callbacks.is_empty() {
                writeln!(out, "No Python breakpoint callbacks registered")
                    .map_err(|err| format!("failed to write output: {err}"))?;
            } else {
                writeln!(out, "Python breakpoint callbacks:")
                    .map_err(|err| format!("failed to write output: {err}"))?;
                for id in callbacks {
                    writeln!(out, "  breakpoint {id}")
                        .map_err(|err| format!("failed to write output: {err}"))?;
                }
            }
            Ok(Outcome::Continue)
        }
        Some(ScriptCommand::DeleteBreakpointCallback(id)) => {
            python.delete_breakpoint_callback(id)?;
            match id {
                Some(id) => writeln!(out, "Deleted Python callback for breakpoint {id}"),
                None => writeln!(out, "Deleted all Python breakpoint callbacks"),
            }
            .map_err(|err| format!("failed to write output: {err}"))?;
            Ok(Outcome::Continue)
        }
        None if is_resume_command(line) => execute_resume_command(debugger, python, line, out),
        None => match debugger.execute_repl_line(line, out) {
            Err(err) if is_unknown_command_error(line, &err) => {
                let (name, args) = split_command(line);
                match python.execute_custom_command(name, args)? {
                    Some(output) => {
                        write!(out, "{output}")
                            .map_err(|err| format!("failed to write output: {err}"))?;
                        Ok(Outcome::Continue)
                    }
                    None => Err(err),
                }
            }
            outcome => outcome,
        },
    }
}

enum ScriptCommand<'a> {
    Snippet(&'a str),
    InteractiveConsole,
    Import(&'a str),
    AddCommand { name: &'a str, function: &'a str },
    ListCommands,
    DeleteCommand(&'a str),
    AddBreakpointCallback { id: u8, function: &'a str },
    ListBreakpointCallbacks,
    DeleteBreakpointCallback(Option<u8>),
}

fn parse_script_command(line: &str) -> Option<ScriptCommand<'_>> {
    let line = line.trim();
    if let Some(command) = parse_command_script_command(line) {
        return Some(command);
    }
    if let Some(command) = parse_breakpoint_command(line) {
        return Some(command);
    }

    if line == "script" {
        return Some(ScriptCommand::InteractiveConsole);
    }

    let code = line.strip_prefix("script")?;
    if code.chars().next().is_some_and(char::is_whitespace) {
        let code = code.trim_start();
        if code.is_empty() {
            Some(ScriptCommand::InteractiveConsole)
        } else {
            Some(ScriptCommand::Snippet(code))
        }
    } else {
        None
    }
}

fn execute_resume_command(
    debugger: &ScriptDebugger,
    python: &mut PythonScriptSession,
    line: &str,
    out: &mut dyn Write,
) -> Result<Outcome, String> {
    let mut command = line.to_string();
    loop {
        let mut buffered_output = Vec::new();
        let outcome = debugger.execute_repl_line(&command, &mut buffered_output)?;
        if outcome == Outcome::Quit || debugger.terminated() {
            out.write_all(&buffered_output)
                .map_err(|err| format!("failed to write output: {err}"))?;
            return Ok(outcome);
        }

        if python.should_continue_after_breakpoint_callbacks()? {
            debugger.clear_hit_breakpoints();
            command = "continue".into();
            continue;
        }

        out.write_all(&buffered_output)
            .map_err(|err| format!("failed to write output: {err}"))?;
        return Ok(outcome);
    }
}

fn parse_breakpoint_command(line: &str) -> Option<ScriptCommand<'_>> {
    if let Some(rest) = line.strip_prefix("breakpoint command add")
        && rest.chars().next().is_some_and(char::is_whitespace)
        && let Some((id, function)) = parse_breakpoint_command_add_args(rest.trim_start())
    {
        return Some(ScriptCommand::AddBreakpointCallback { id, function });
    }

    if let Some(rest) = line.strip_prefix("breakpoint command list")
        && rest.trim().is_empty()
    {
        return Some(ScriptCommand::ListBreakpointCallbacks);
    }

    if let Some(rest) = line.strip_prefix("breakpoint command delete") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Some(ScriptCommand::DeleteBreakpointCallback(None));
        }
        if let Ok(id) = rest.parse::<u8>() {
            return Some(ScriptCommand::DeleteBreakpointCallback(Some(id)));
        }
    }

    None
}

fn parse_breakpoint_command_add_args(args: &str) -> Option<(u8, &str)> {
    let (id, rest) = split_command(args);
    let id = id.parse::<u8>().ok()?;
    let rest = rest.trim();
    let function = rest.strip_prefix("-f")?.trim_start();
    if function.is_empty() {
        None
    } else {
        Some((id, function))
    }
}

fn parse_command_script_command(line: &str) -> Option<ScriptCommand<'_>> {
    if let Some(path) = line.strip_prefix("command script import")
        && path.chars().next().is_some_and(char::is_whitespace)
    {
        let path = path.trim_start();
        if !path.is_empty() {
            return Some(ScriptCommand::Import(path));
        }
    }

    if let Some(rest) = line.strip_prefix("command script list")
        && rest.trim().is_empty()
    {
        return Some(ScriptCommand::ListCommands);
    }

    if let Some(name) = line.strip_prefix("command script delete")
        && name.chars().next().is_some_and(char::is_whitespace)
    {
        let name = name.trim_start();
        if !name.is_empty() {
            return Some(ScriptCommand::DeleteCommand(name));
        }
    }

    if let Some(rest) = line.strip_prefix("command script add")
        && rest.chars().next().is_some_and(char::is_whitespace)
        && let Some((name, function)) = parse_command_script_add_args(rest.trim_start())
    {
        return Some(ScriptCommand::AddCommand { name, function });
    }

    None
}

fn parse_command_script_add_args(args: &str) -> Option<(&str, &str)> {
    let (name, rest) = split_command(args);
    let rest = rest.trim();
    let function = rest.strip_prefix("-f")?.trim_start();
    if name.is_empty() || function.is_empty() {
        None
    } else {
        Some((name, function))
    }
}

fn split_command(line: &str) -> (&str, &str) {
    match line.trim().split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim_start()),
        None => (line.trim(), ""),
    }
}

fn is_unknown_command_error(line: &str, err: &str) -> bool {
    let (name, _) = split_command(line);
    err == format!("unknown command: {name}")
}

fn is_resume_command(line: &str) -> bool {
    let (name, _) = split_command(line);
    matches!(
        name,
        "c" | "continue" | "n" | "next" | "nl" | "next-line" | "nextline" | "e" | "finish"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_debugger() -> ScriptDebugger {
        crate::script::test_debugger()
    }

    #[test]
    fn parses_script_commands_only() {
        assert!(matches!(
            parse_script_command("script"),
            Some(ScriptCommand::InteractiveConsole)
        ));
        assert!(matches!(
            parse_script_command("script 1 + 1"),
            Some(ScriptCommand::Snippet("1 + 1"))
        ));
        assert!(matches!(
            parse_script_command("command script import /tmp/demo.py"),
            Some(ScriptCommand::Import("/tmp/demo.py"))
        ));
        assert!(parse_script_command("scripts").is_none());
        assert!(parse_script_command("step").is_none());
    }

    #[test]
    fn executes_script_snippets_through_repl_route() {
        let _guard = crate::script::python::python_test_lock();
        let debugger = test_debugger();
        let mut python = PythonScriptSession::new(debugger.clone()).unwrap();
        let mut output = Vec::new();

        execute_line(&debugger, &mut python, "script x = 1", &mut output).unwrap();
        execute_line(&debugger, &mut python, "script x + 1", &mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "2\n");
    }

    #[test]
    fn imports_script_and_runs_initializer() {
        let _guard = crate::script::python::python_test_lock();
        let debugger = test_debugger();
        let mut python = PythonScriptSession::new(debugger.clone()).unwrap();
        let temp_path = std::env::temp_dir().join(format!(
            "miden-debug-python-import-{}-{}.py",
            std::process::id(),
            0
        ));
        std::fs::write(
            &temp_path,
            r#"
def __miden_init_module(debugger, internal_dict):
    internal_dict["loaded_cycle"] = debugger.get_cycle()
"#,
        )
        .unwrap();

        let mut output = Vec::new();
        execute_line(
            &debugger,
            &mut python,
            &format!("command script import {}", temp_path.display()),
            &mut output,
        )
        .unwrap();
        execute_line(&debugger, &mut python, "script internal_dict['loaded_cycle']", &mut output)
            .unwrap();

        let _ = std::fs::remove_file(&temp_path);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Imported Python script:"));
        assert!(output.ends_with("0\n"));
    }

    #[test]
    fn registers_and_executes_custom_python_command() {
        let _guard = crate::script::python::python_test_lock();
        let debugger = test_debugger();
        let mut python = PythonScriptSession::new(debugger.clone()).unwrap();
        let temp_path = std::env::temp_dir()
            .join(format!("miden_debug_python_command_{}.py", std::process::id()));
        std::fs::write(
            &temp_path,
            r#"
def cycle(debugger, command, exe_ctx, result, internal_dict):
    print(f"{debugger.get_cycle()}:{command}", file=result)
"#,
        )
        .unwrap();

        let mut output = Vec::new();
        execute_line(
            &debugger,
            &mut python,
            &format!("command script import {}", temp_path.display()),
            &mut output,
        )
        .unwrap();
        execute_line(
            &debugger,
            &mut python,
            &format!(
                "command script add py-cycle -f {}.cycle",
                temp_path.file_stem().unwrap().to_str().unwrap()
            ),
            &mut output,
        )
        .unwrap();
        execute_line(&debugger, &mut python, "py-cycle hello", &mut output).unwrap();
        execute_line(&debugger, &mut python, "command script list", &mut output).unwrap();
        execute_line(&debugger, &mut python, "command script delete py-cycle", &mut output)
            .unwrap();

        let _ = std::fs::remove_file(&temp_path);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Added Python command: py-cycle"));
        assert!(output.contains("0:hello\n"));
        assert!(output.contains("Python commands:\n  py-cycle\n"));
        assert!(output.contains("Deleted Python command: py-cycle"));
    }

    #[test]
    fn breakpoint_callback_false_continues_execution() {
        let _guard = crate::script::python::python_test_lock();
        let debugger = test_debugger();
        let mut python = PythonScriptSession::new(debugger.clone()).unwrap();
        let temp_path =
            std::env::temp_dir().join(format!("miden_debug_bp_callback_{}.py", std::process::id()));
        std::fs::write(
            &temp_path,
            r#"
def never_stop(frame, breakpoint, internal_dict):
    internal_dict["called"] = internal_dict.get("called", 0) + 1
    return False
"#,
        )
        .unwrap();

        let mut output = Vec::new();
        execute_line(&debugger, &mut python, "b after 1", &mut output).unwrap();
        execute_line(
            &debugger,
            &mut python,
            &format!("command script import {}", temp_path.display()),
            &mut output,
        )
        .unwrap();
        execute_line(
            &debugger,
            &mut python,
            &format!(
                "breakpoint command add 0 -f {}.never_stop",
                temp_path.file_stem().unwrap().to_str().unwrap()
            ),
            &mut output,
        )
        .unwrap();
        execute_line(&debugger, &mut python, "continue", &mut output).unwrap();
        execute_line(&debugger, &mut python, "script internal_dict['called']", &mut output)
            .unwrap();

        let _ = std::fs::remove_file(&temp_path);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Added Python callback for breakpoint 0"), "output:\n{output}");
        assert!(output.contains("Program terminated successfully"), "output:\n{output}");
        assert!(output.ends_with("1\n"), "output:\n{output}");
    }
}
