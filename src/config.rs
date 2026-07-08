use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    str::FromStr,
};

use miden_debug_engine::{LinkLibrary, profiling::ProfilerCliArgs};

use crate::{exec::ExecutionConfig, felt::Felt, input::InputFile};

/// Run a compiled Miden package with the Miden VM
#[derive(Default, Debug)]
#[cfg_attr(
    any(feature = "tui", feature = "repl", feature = "flamegraph"),
    derive(clap::Args)
)]
pub struct DebuggerConfig {
    /// Specify the path to a Miden package artifact to execute.
    ///
    /// Miden Assembly packages are emitted by the compiler with a `.masp` extension.
    ///
    /// You may use `-` as a file name to read a file from stdin.
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(value_name = "FILE")
    )]
    pub input: Option<InputFile>,
    /// Specify the path to a file containing program inputs.
    ///
    /// Program inputs are stack and advice provider values which the program can
    /// access during execution. The inputs file is a TOML file which describes
    /// what the inputs are, or where to source them from.
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(long, value_name = "FILE")
    )]
    pub inputs: Option<ExecutionConfig>,
    /// Arguments to place on the operand stack before calling the program entrypoint.
    ///
    /// Arguments will be pushed on the operand stack in the order of appearance,
    ///
    /// Example: `-- a b` will push `a` on the stack, then `b`.
    ///
    /// These arguments must be valid field element values expressed in decimal format.
    ///
    /// NOTE: These arguments will override any stack values provided via --inputs
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(last(true), value_name = "ARGV")
    )]
    pub args: Vec<Felt>,
    /// The working directory for the debugger
    ///
    /// By default this will be the working directory the debugger is executed from
    #[cfg_attr(
        any(feature = "tui", feature = "flamegraph"),
        arg(long, value_name = "DIR", help_heading = "Execution")
    )]
    pub working_dir: Option<PathBuf>,
    /// The path to the root directory of the current Miden toolchain
    ///
    /// By default this is assumed to be `$(midenup show home)/toolchains/$(midenup show active-toolchain)
    #[cfg_attr(
        any(feature = "tui", feature = "flamegraph"),
        arg(
            long,
            value_name = "DIR",
            env = "MIDEN_SYSROOT",
            help_heading = "Linker"
        )
    )]
    pub sysroot: Option<PathBuf>,
    /// Whether, and how, to color terminal output
    #[cfg_attr(any(feature = "tui", feature = "repl", feature = "flamegraph"), arg(
        long,
        value_enum,
        default_value_t = ColorChoice::Auto,
        default_missing_value = "auto",
        num_args(0..=1),
        help_heading = "Output"
    ))]
    pub color: ColorChoice,
    /// Specify the function to call as the entrypoint for the program
    /// in the format `<module_name>::<function>`
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(long, help_heading = "Execution")
    )]
    pub entrypoint: Option<String>,
    /// Connect to a remote DAP debug server instead of running a local program.
    ///
    /// Specify the address of the DAP server (e.g. "127.0.0.1:4711").
    /// When this flag is set, the debugger connects to an existing remote session.
    #[cfg(feature = "dap")]
    #[cfg_attr(
        any(feature = "tui", feature = "flamegraph"),
        arg(long, value_name = "ADDR", help_heading = "Execution")
    )]
    pub dap_connect: Option<String>,
    /// Start a DAP debug server for the local program and wait for a client to connect.
    ///
    /// Specify the address to listen on (e.g. "127.0.0.1:4711").
    #[cfg(feature = "dap")]
    #[cfg_attr(
        feature = "tui",
        arg(long, value_name = "ADDR", help_heading = "Execution")
    )]
    pub start_debug_adapter: Option<String>,
    /// Source path prefixes used by the compiler's `-Zremap-path-prefix` option.
    ///
    /// When debug info stores trimmed source paths, DAP clients may still send
    /// absolute editor paths. These prefixes provide an explicit mapping between
    /// the two forms.
    #[cfg(feature = "dap")]
    #[cfg_attr(
        feature = "tui",
        arg(
            long = "source-path-prefix",
            alias = "trim-path-prefix",
            value_name = "PATH",
            help_heading = "Debugging"
        )
    )]
    pub source_path_prefixes: Vec<PathBuf>,
    /// Replay a recorded execution snapshot in the TUI debugger.
    ///
    /// FILE is a snapshot written during a recorded debug session (e.g.
    /// `miden-client exec --start-debug-adapter <ADDR> --record <FILE>`). The recorded program,
    /// inputs, resolved code, and event log are replayed so the same execution can be stepped
    /// through offline, without the original host.
    #[cfg_attr(
        feature = "tui",
        arg(long, value_name = "FILE", help_heading = "Execution")
    )]
    pub replay: Option<PathBuf>,
    /// Specify one or more search paths for link libraries requested via `-l`
    #[cfg_attr(
        any(feature = "tui", feature = "flamegraph"),
        arg(
            long = "search-path",
            short = 'L',
            value_name = "PATH",
            help_heading = "Linker"
        )
    )]
    pub search_path: Vec<PathBuf>,
    /// Link compiled projects to the specified library NAME.
    ///
    /// The optional KIND can be provided to indicate what type of library it is.
    ///
    /// NAME must either be an absolute path (with extension when applicable), or
    /// a library namespace (no extension). The former will be used as the path
    /// to load the library, without looking for it in the library search paths,
    /// while the latter will be located in the search path based on its KIND.
    ///
    /// See below for valid KINDs:
    #[cfg_attr(
        any(feature = "tui", feature = "flamegraph"),
        arg(
            long = "link-library",
            short = 'l',
            value_name = "[KIND=]NAME",
            value_delimiter = ',',
            next_line_help(true),
            help_heading = "Linker"
        )
    )]
    pub link_libraries: Vec<LinkLibrary>,
    /// Use the REPL (text-mode) debugger instead of the TUI
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(long, help_heading = "Output")
    )]
    pub repl: bool,
    /// Run a script of debugger commands non-interactively, then exit.
    ///
    /// FILE is a list of debugger commands, one per line, using the same syntax
    /// as the interactive REPL prompt. Blank lines and lines beginning with `#`
    /// are ignored, so scripts may be commented. This is analogous to
    /// `gdb -x <file> -batch` or `lldb -s <file>`, and is primarily used to
    /// drive the debugger from lit/FileCheck tests.
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        arg(
            long = "commands",
            visible_alias = "source",
            short = 'x',
            value_name = "FILE",
            help_heading = "Execution"
        )
    )]
    pub commands: Option<PathBuf>,
    /// Do not auto-load the project-local `.miden-debug.py` file.
    #[cfg(feature = "python")]
    #[cfg_attr(feature = "python", arg(long, help_heading = "Scripting"))]
    pub no_user_python_init: bool,
    /// Profiler configuration.
    #[cfg_attr(
        any(feature = "tui", feature = "repl", feature = "flamegraph"),
        command(flatten)
    )]
    pub profiler_cli_args: ProfilerCliArgs,
}

/// ColorChoice represents the color preferences of an end user.
///
/// The `Default` implementation for this type will select `Auto`, which tries
/// to do the right thing based on the current environment.
///
/// The `FromStr` implementation for this type converts a lowercase kebab-case
/// string of the variant name to the corresponding variant. Any other string
/// results in an error.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(
    any(feature = "tui", feature = "repl", feature = "flamegraph"),
    derive(clap::ValueEnum)
)]
pub enum ColorChoice {
    /// Try very hard to emit colors. This includes emitting ANSI colors
    /// on Windows if the console API is unavailable.
    Always,
    /// AlwaysAnsi is like Always, except it never tries to use anything other
    /// than emitting ANSI color codes.
    AlwaysAnsi,
    /// Try to use colors, but don't force the issue. If the console isn't
    /// available on Windows, or if TERM=dumb, or if `NO_COLOR` is defined, for
    /// example, then don't use colors.
    #[default]
    Auto,
    /// Never emit colors.
    Never,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid color choice: {0}")]
pub struct ColorChoiceParseError(std::borrow::Cow<'static, str>);

impl FromStr for ColorChoice {
    type Err = ColorChoiceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "always" => Ok(ColorChoice::Always),
            "always-ansi" => Ok(ColorChoice::AlwaysAnsi),
            "never" => Ok(ColorChoice::Never),
            "auto" => Ok(ColorChoice::Auto),
            unknown => Err(ColorChoiceParseError(unknown.to_string().into())),
        }
    }
}

impl ColorChoice {
    /// Returns true if we should attempt to write colored output.
    pub fn should_attempt_color(&self) -> bool {
        match *self {
            ColorChoice::Always => true,
            ColorChoice::AlwaysAnsi => true,
            ColorChoice::Never => false,
            #[cfg(feature = "std")]
            ColorChoice::Auto => self.env_allows_color(),
            #[cfg(not(feature = "std"))]
            ColorChoice::Auto => false,
        }
    }

    #[cfg(not(windows))]
    pub fn env_allows_color(&self) -> bool {
        match std::env::var_os("TERM") {
            // If TERM isn't set, then we are in a weird environment that
            // probably doesn't support colors.
            None => return false,
            Some(k) => {
                if k == "dumb" {
                    return false;
                }
            }
        }
        // If TERM != dumb, then the only way we don't allow colors at this
        // point is if NO_COLOR is set.
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        true
    }

    #[cfg(windows)]
    pub fn env_allows_color(&self) -> bool {
        // On Windows, if TERM isn't set, then we shouldn't automatically
        // assume that colors aren't allowed. This is unlike Unix environments
        // where TERM is more rigorously set.
        if let Some(k) = std::env::var_os("TERM") {
            if k == "dumb" {
                return false;
            }
        }
        // If TERM != dumb, then the only way we don't allow colors at this
        // point is if NO_COLOR is set.
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        true
    }

    /// Returns true if this choice should forcefully use ANSI color codes.
    ///
    /// It's possible that ANSI is still the correct choice even if this
    /// returns false.
    #[cfg(all(feature = "tui", windows))]
    pub fn should_ansi(&self) -> bool {
        match *self {
            ColorChoice::Always => false,
            ColorChoice::AlwaysAnsi => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                match std::env::var("TERM") {
                    Err(_) => false,
                    // cygwin doesn't seem to support ANSI escape sequences
                    // and instead has its own variety. However, the Windows
                    // console API may be available.
                    Ok(k) => k != "dumb" && k != "cygwin",
                }
            }
        }
    }

    /// Returns true if this choice should forcefully use ANSI color codes.
    ///
    /// It's possible that ANSI is still the correct choice even if this
    /// returns false.
    #[cfg(not(feature = "tui"))]
    pub fn should_ansi(&self) -> bool {
        match *self {
            ColorChoice::Always => false,
            ColorChoice::AlwaysAnsi => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => false,
        }
    }
}

impl DebuggerConfig {
    pub fn working_dir(&self) -> Cow<'_, Path> {
        match self.working_dir.as_deref() {
            Some(path) => Cow::Borrowed(path),
            None => std::env::current_dir()
                .map(Cow::Owned)
                .unwrap_or(Cow::Borrowed(Path::new("./"))),
        }
    }

    pub fn toolchain_dir(&self) -> Option<PathBuf> {
        let sysroot = if let Some(sysroot) = self.sysroot.as_deref() {
            Cow::Borrowed(sysroot)
        } else if let Some((midenup_home, midenup_channel)) = midenup_home().zip(midenup_channel())
        {
            Cow::Owned(midenup_home.join("toolchains").join(midenup_channel))
        } else {
            return None;
        };

        if sysroot.try_exists().ok().is_some_and(|exists| exists) {
            Some(sysroot.into_owned())
        } else {
            None
        }
    }
}

fn midenup_home() -> Option<PathBuf> {
    use std::process::Command;

    let mut cmd = Command::new("midenup");
    let mut output = cmd.args(["show", "home"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(core::mem::take(&mut output.stdout)).ok()?;
    let trimmed = output.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    PathBuf::from_str(trimmed).ok()
}

fn midenup_channel() -> Option<String> {
    use std::process::Command;

    let mut cmd = Command::new("midenup");
    let mut output = cmd.args(["show", "active-toolchain"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let output = String::from_utf8(core::mem::take(&mut output.stdout)).ok()?;
    let trimmed = output.trim_ascii();
    if trimmed.is_empty() {
        return None;
    }
    if output.len() == trimmed.len() {
        Some(output)
    } else {
        Some(trimmed.to_string())
    }
}
