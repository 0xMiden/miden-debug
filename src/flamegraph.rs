use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use miden_assembly::{DefaultSourceManager, SourceManager};
use miden_assembly_syntax::{
    Library,
    diagnostics::{IntoDiagnostic, Report, WrapErr},
};
use miden_core::serde::Deserializable;
use miden_processor::{ExecutionError, StackInputs};

use crate::{
    config::{ColorChoice, DebuggerConfig},
    debug::CallFrame,
    exec::{DebugExecutor, ExecutionConfig, Executor},
    felt::Felt,
    input::InputFile,
    linker::LinkLibrary,
};

/// Folded stack samples keyed by semicolon-separated stack paths.
pub type Samples = BTreeMap<String, usize>;

/// A collected VM cycle profile that can be rendered as folded stacks or an SVG flamegraph.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlamegraphProfile {
    samples: Samples,
    total_cycles: usize,
}

/// The output format selected by [`FlamegraphProfile::write_to_path`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlamegraphOutput {
    Svg,
    FoldedStacks,
}

impl FlamegraphProfile {
    /// Execute `executor` to completion, sampling its call stack for every VM cycle.
    pub fn collect(executor: &mut DebugExecutor) -> Result<Self, ExecutionError> {
        let mut profile = Self::default();

        loop {
            if executor.stopped {
                break;
            }

            let previous_cycle = executor.cycle;
            match executor.step() {
                Ok(_) if executor.cycle > previous_cycle => {
                    let cycle_delta = executor.cycle - previous_cycle;
                    profile.record_call_stack(executor.callstack.frames(), cycle_delta);
                }
                Ok(_) => {
                    if executor.stopped {
                        break;
                    }
                }
                Err(err) => return Err(err),
            }
        }

        Ok(profile)
    }

    /// Record `cycles` against a call stack from the debugger engine.
    pub fn record_call_stack(&mut self, frames: &[CallFrame], cycles: usize) {
        let path = build_stack_path(frames);
        self.record_stack_path(path, cycles);
    }

    /// Record `cycles` against a stack of frame names.
    ///
    /// Frame names are sanitized for folded stack output, then joined with `;`.
    pub fn record_stack<I, S>(&mut self, frames: I, cycles: usize)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let path = build_stack_path_from_names(frames);
        self.record_stack_path(path, cycles);
    }

    /// Record `cycles` against an already formatted folded-stack path.
    pub fn record_stack_path(&mut self, stack_path: impl Into<String>, cycles: usize) {
        if cycles == 0 {
            return;
        }

        self.total_cycles += cycles;
        *self.samples.entry(stack_path.into()).or_default() += cycles;
    }

    pub fn samples(&self) -> &Samples {
        &self.samples
    }

    pub fn total_cycles(&self) -> usize {
        self.total_cycles
    }

    pub fn unique_stack_paths(&self) -> usize {
        self.samples.len()
    }

    /// Write this profile as an SVG when `path` ends in `.svg`, otherwise as folded stack text.
    pub fn write_to_path(&self, path: impl AsRef<Path>) -> Result<FlamegraphOutput, Report> {
        let path = path.as_ref();
        if is_svg_path(path) {
            self.write_svg(path)?;
            Ok(FlamegraphOutput::Svg)
        } else {
            self.write_folded_stacks(path)?;
            Ok(FlamegraphOutput::FoldedStacks)
        }
    }

    /// Write this profile in folded stack format.
    pub fn write_folded_stacks(&self, path: impl AsRef<Path>) -> Result<(), Report> {
        write_folded_stacks(&self.samples, path.as_ref())
    }

    /// Render this profile as an SVG flamegraph.
    pub fn write_svg(&self, path: impl AsRef<Path>) -> Result<(), Report> {
        generate_svg(&self.samples, path.as_ref())
    }
}

#[derive(clap::Args, Debug)]
pub struct FlamegraphArgs {
    /// Specify the path to a Miden program file to execute.
    #[arg(value_name = "FILE")]
    pub input: InputFile,

    /// Write the generated flame graph SVG or folded stack text to this path.
    #[arg(short, long, default_value = "flamegraph.svg")]
    pub output: PathBuf,

    /// Specify the path to a file containing program inputs.
    #[arg(long, value_name = "FILE")]
    pub inputs: Option<ExecutionConfig>,

    /// Arguments to place on the operand stack before calling the program entrypoint.
    #[arg(last(true), value_name = "ARGV")]
    pub args: Vec<Felt>,

    /// The working directory for execution.
    #[arg(long, value_name = "DIR", help_heading = "Execution")]
    pub working_dir: Option<PathBuf>,

    /// The path to the root directory of the current Miden toolchain.
    #[arg(
        long,
        value_name = "DIR",
        env = "MIDEN_SYSROOT",
        help_heading = "Linker"
    )]
    pub sysroot: Option<PathBuf>,

    /// Specify the function to call as the entrypoint for the program.
    #[arg(long, help_heading = "Execution")]
    pub entrypoint: Option<String>,

    /// Specify one or more search paths for link libraries requested via `-l`.
    #[arg(
        long = "search-path",
        short = 'L',
        value_name = "PATH",
        help_heading = "Linker"
    )]
    pub search_path: Vec<PathBuf>,

    /// Link compiled projects to the specified library NAME.
    #[arg(
        long = "link-library",
        short = 'l',
        value_name = "[KIND=]NAME",
        value_delimiter = ',',
        next_line_help(true),
        help_heading = "Linker"
    )]
    pub link_libraries: Vec<LinkLibrary>,
}

impl FlamegraphArgs {
    fn into_debugger_config(self) -> DebuggerConfig {
        DebuggerConfig {
            input: Some(self.input),
            inputs: self.inputs,
            args: self.args,
            working_dir: self.working_dir,
            sysroot: self.sysroot,
            color: ColorChoice::Auto,
            entrypoint: self.entrypoint,
            #[cfg(feature = "dap")]
            dap_connect: None,
            search_path: self.search_path,
            link_libraries: self.link_libraries,
            repl: false,
        }
    }
}

pub fn run(args: FlamegraphArgs) -> Result<(), Report> {
    let output = args.output.clone();
    let mut config = args.into_debugger_config();
    ensure_working_dir(&mut config)?;

    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    let mut inputs = execution_inputs(&config)?;
    let args = inputs.inputs.iter().copied().collect::<Vec<_>>();
    let package = load_package(&config)?;

    let libs = load_libraries(&config, source_manager.clone())?;

    let mut executor = Executor::new(args);
    for lib in libs.iter() {
        executor.register_library_dependency(lib.clone());
        executor.with_library(lib.clone());
    }
    executor.with_dependencies(package.manifest.dependencies())?;
    executor.with_advice_inputs(core::mem::take(&mut inputs.advice_inputs));

    let program = package.unwrap_program();
    let mut executor = executor.into_debug(&program, source_manager);

    let profile = match FlamegraphProfile::collect(&mut executor) {
        Ok(profile) => profile,
        Err(err) => {
            return Err(Report::msg(format!(
                "program execution failed at cycle {}: {err}",
                executor.cycle
            )));
        }
    };

    eprintln!(
        "Executed {} cycles across {} unique stack paths",
        profile.total_cycles(),
        profile.unique_stack_paths()
    );

    profile.write_to_path(&output)?;

    Ok(())
}

fn ensure_working_dir(config: &mut DebuggerConfig) -> Result<(), Report> {
    if config.working_dir.is_none() {
        let cwd = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("could not read current working directory")?;
        config.working_dir = Some(cwd);
    }

    Ok(())
}

fn execution_inputs(config: &DebuggerConfig) -> Result<ExecutionConfig, Report> {
    let mut inputs = config.inputs.clone().unwrap_or_default();
    if !config.args.is_empty() {
        // CLI args model sequential pushes, but StackInputs expects the top element first.
        let args = config.args.iter().rev().map(|felt| felt.0).collect::<Vec<_>>();
        inputs.inputs = StackInputs::new(&args).into_diagnostic()?;
    }

    Ok(inputs)
}

fn load_libraries(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
) -> Result<Vec<Arc<Library>>, Report> {
    let mut libs = Vec::with_capacity(config.link_libraries.len());
    for link_library in config.link_libraries.iter() {
        log::debug!(target: "flamegraph", "loading link library {}", link_library.name());
        libs.push(link_library.load(config, source_manager.clone())?);
    }

    if let Some(toolchain_dir) = config.toolchain_dir() {
        libs.extend(load_sysroot_libs(&toolchain_dir)?);
    }

    Ok(libs)
}

fn load_sysroot_libs(toolchain_dir: &Path) -> Result<Vec<Arc<Library>>, Report> {
    let mut libs = Vec::new();

    let entries = match std::fs::read_dir(toolchain_dir) {
        Ok(entries) => entries,
        Err(_) => {
            log::debug!(target: "flamegraph", "could not read sysroot directory: {}", toolchain_dir.display());
            return Ok(libs);
        }
    };

    for entry in entries {
        let entry = entry.into_diagnostic()?;
        let path = entry.path();
        let Some(ext) = path.extension() else {
            continue;
        };

        if ext == "masp" {
            log::debug!(target: "flamegraph", "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let package = miden_mast_package::Package::read_from_bytes(&bytes).map_err(|e| {
                Report::msg(format!("failed to load package '{}': {e}", path.display()))
            })?;
            libs.push(package.mast.clone());
        } else if ext == "masl" {
            log::debug!(target: "flamegraph", "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let lib = Library::read_from_bytes(&bytes).map_err(|e| {
                Report::msg(format!("failed to load library '{}': {e}", path.display()))
            })?;
            libs.push(Arc::new(lib));
        }
    }

    Ok(libs)
}

fn load_package(config: &DebuggerConfig) -> Result<Arc<miden_mast_package::Package>, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    let package = match input {
        InputFile::Real(path) => {
            let bytes = std::fs::read(path).into_diagnostic()?;
            miden_mast_package::Package::read_from_bytes(&bytes)
                .map(Arc::new)
                .map_err(|e| {
                    Report::msg(format!(
                        "failed to load Miden package from {}: {e}",
                        path.display()
                    ))
                })?
        }
        InputFile::Stdin(bytes) => miden_mast_package::Package::read_from_bytes(bytes)
            .map(Arc::new)
            .map_err(|e| Report::msg(format!("failed to load Miden package from stdin: {e}")))?,
    };

    if let Some(entry) = config.entrypoint.as_ref() {
        let id = entry
            .parse::<miden_assembly::ast::QualifiedProcedureName>()
            .map_err(|_| Report::msg(format!("invalid function identifier: '{entry}'")))?;
        if !package.is_library() {
            return Err(Report::msg("cannot use --entrypoint with executable packages"));
        }

        package.make_executable(&id).map(Arc::new)
    } else {
        Ok(package)
    }
}

fn build_stack_path(frames: &[CallFrame]) -> String {
    build_stack_path_from_names(frames.iter().filter_map(|frame| frame.procedure("")))
}

fn build_stack_path_from_names<I, S>(frames: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut path = String::new();
    for frame in frames {
        append_frame(&mut path, frame.as_ref());
    }

    if path.is_empty() {
        "[unknown]".to_string()
    } else {
        path
    }
}

fn append_frame(path: &mut String, name: &str) {
    if !path.is_empty() {
        path.push(';');
    }
    append_sanitized_frame(path, name);
}

fn append_sanitized_frame(path: &mut String, name: &str) {
    for ch in name.chars() {
        match ch {
            ';' => path.push(':'),
            '\n' | '\r' => path.push(' '),
            _ => path.push(ch),
        }
    }
}

fn is_svg_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

fn write_folded_stacks(samples: &Samples, path: &Path) -> Result<(), Report> {
    let file = File::create(path).into_diagnostic()?;
    let mut writer = BufWriter::new(file);
    for (stack, count) in samples {
        writeln!(writer, "{stack} {count}").into_diagnostic()?;
    }
    writer.flush().into_diagnostic()?;

    eprintln!("Wrote folded stacks to {}", path.display());
    Ok(())
}

fn generate_svg(samples: &Samples, path: &Path) -> Result<(), Report> {
    let input = samples
        .iter()
        .map(|(stack, count)| format!("{stack} {count}"))
        .collect::<Vec<_>>()
        .join("\n");

    let mut opts = inferno::flamegraph::Options::default();
    opts.title = "Miden VM Flame Graph (cycles)".to_string();
    opts.count_name = "cycles".to_string();

    let file = File::create(path).into_diagnostic()?;
    let mut writer = BufWriter::new(file);
    inferno::flamegraph::from_reader(&mut opts, input.as_bytes(), &mut writer).into_diagnostic()?;
    writer.flush().into_diagnostic()?;

    eprintln!("Wrote flame graph to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::FlamegraphProfile;

    #[test]
    fn record_stack_sanitizes_folded_stack_frames() {
        let mut profile = FlamegraphProfile::default();

        profile.record_stack(["root;proc", "child\nproc"], 3);
        profile.record_stack(["root;proc", "child\nproc"], 2);
        profile.record_stack(["ignored"], 0);

        assert_eq!(profile.total_cycles(), 5);
        assert_eq!(profile.unique_stack_paths(), 1);
        assert_eq!(profile.samples().get("root:proc;child proc"), Some(&5));
        assert!(!profile.samples().contains_key("ignored"));
    }
}
