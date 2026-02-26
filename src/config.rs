use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    str::FromStr,
};

use crate::{exec::ExecutionConfig, felt::Felt, input::InputFile, linker::LinkLibrary};

/// Run a compiled Miden program with the Miden VM
#[derive(Default, Debug)]
#[cfg_attr(any(feature = "tui", feature = "repl"), derive(clap::Parser))]
#[cfg_attr(any(feature = "tui", feature = "repl"), command(author, version, about = "The interactive Miden debugger", long_about = None))]
pub struct DebuggerConfig {
    /// Specify the path to a Miden program file to execute.
    ///
    /// Miden Assembly programs are emitted by the compiler with a `.masp` extension.
    ///
    /// You may use `-` as a file name to read a file from stdin.
    #[cfg_attr(
        any(feature = "tui", feature = "repl"),
        arg(required(true), value_name = "FILE")
    )]
    pub input: InputFile,
    /// Specify the path to a file containing program inputs.
    ///
    /// Program inputs are stack and advice provider values which the program can
    /// access during execution. The inputs file is a TOML file which describes
    /// what the inputs are, or where to source them from.
    #[cfg_attr(any(feature = "tui", feature = "repl"), arg(long, value_name = "FILE"))]
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
        any(feature = "tui", feature = "repl"),
        arg(last(true), value_name = "ARGV")
    )]
    pub args: Vec<Felt>,
    /// The working directory for the debugger
    ///
    /// By default this will be the working directory the debugger is executed from
    #[cfg_attr(
        any(feature = "tui", feature = "repl"),
        arg(long, value_name = "DIR", help_heading = "Execution")
    )]
    pub working_dir: Option<PathBuf>,
    /// The path to the root directory of the current Miden toolchain
    ///
    /// By default this is assumed to be `$(midenup show home)/toolchains/$(midenup show active-toolchain)
    #[cfg_attr(
        any(feature = "tui", feature = "repl"),
        arg(
            long,
            value_name = "DIR",
            env = "MIDEN_SYSROOT",
            help_heading = "Linker"
        )
    )]
    pub sysroot: Option<PathBuf>,
    /// Whether, and how, to color terminal output
    #[cfg_attr(any(feature = "tui", feature = "repl"), arg(
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
        any(feature = "tui", feature = "repl"),
        arg(long, help_heading = "Execution")
    )]
    pub entrypoint: Option<String>,
    /// Specify one or more search paths for link libraries requested via `-l`
    #[cfg_attr(
        any(feature = "tui", feature = "repl"),
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
        any(feature = "tui", feature = "repl"),
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
    /// Run in REPL mode instead of TUI
    #[cfg_attr(any(feature = "tui", feature = "repl"), arg(long, short = 'r'))]
    pub repl: bool,
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
#[cfg_attr(any(feature = "tui", feature = "repl"), derive(clap::ValueEnum))]
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

    #[cfg(all(any(feature = "tui", feature = "repl"), not(windows)))]
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

    #[cfg(all(any(feature = "tui", feature = "repl"), windows))]
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
    #[cfg(all(any(feature = "tui", feature = "repl"), windows))]
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
    #[cfg(not(any(feature = "tui", feature = "repl")))]
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
        } else if let Some((midenup_home, midenup_channel)) =
            midenup_home().and_then(|home| midenup_channel().map(|channel| (home, channel)))
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
