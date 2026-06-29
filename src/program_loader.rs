use std::{path::Path, sync::Arc};

use miden_assembly::SourceManager;
use miden_assembly_syntax::{
    Library,
    diagnostics::{IntoDiagnostic, Report},
};
use miden_core::{
    program::{Program, StackInputs},
    serde::Deserializable,
};

use crate::{
    config::DebuggerConfig,
    exec::{DebugExecutor, ExecutionConfig, Executor},
    input::InputFile,
};

pub(crate) enum ProgramArtifact {
    Package(Arc<miden_mast_package::Package>),
    Program(Program),
}

impl ProgramArtifact {
    pub(crate) fn package(&self) -> Option<&miden_mast_package::Package> {
        match self {
            Self::Package(package) => Some(package),
            Self::Program(_) => None,
        }
    }

    pub(crate) fn into_program(self) -> Program {
        match self {
            Self::Package(package) => package.unwrap_program(),
            Self::Program(program) => program,
        }
    }
}

pub(crate) fn execution_inputs(config: &DebuggerConfig) -> Result<ExecutionConfig, Report> {
    let mut inputs = config.inputs.clone().unwrap_or_default();
    if !config.args.is_empty() {
        // CLI args model sequential pushes, but StackInputs expects the top element first.
        let args = config.args.iter().rev().map(|felt| felt.0).collect::<Vec<_>>();
        inputs.inputs = StackInputs::new(&args).into_diagnostic()?;
    }

    Ok(inputs)
}

pub(crate) fn load_debug_executor(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    log_target: &'static str,
) -> Result<DebugExecutor, Report> {
    let inputs = execution_inputs(config)?;
    let args = inputs.inputs.iter().copied().collect::<Vec<_>>();
    let artifact = load_program_artifact(config)?;

    let mut executor = Executor::new(args);
    for lib in load_libraries(config, source_manager.clone(), log_target)? {
        executor.register_library_dependency(lib.clone());
        executor.with_library(lib);
    }

    if let Some(package) = artifact.package() {
        executor.with_dependencies(package.manifest.dependencies())?;
    }
    executor.with_advice_inputs(inputs.advice_inputs);

    let program = artifact.into_program();
    Ok(executor.into_debug(&program, source_manager))
}

pub(crate) fn load_libraries(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    log_target: &'static str,
) -> Result<Vec<Arc<Library>>, Report> {
    let mut libs = Vec::with_capacity(config.link_libraries.len());
    for link_library in config.link_libraries.iter() {
        log::debug!(target: log_target, "loading link library {}", link_library.name());
        libs.push(link_library.load(config, source_manager.clone())?);
    }

    if let Some(toolchain_dir) = config.toolchain_dir() {
        libs.extend(load_sysroot_libs(&toolchain_dir, log_target)?);
    }

    Ok(libs)
}

pub(crate) fn load_program_artifact(config: &DebuggerConfig) -> Result<ProgramArtifact, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    let (bytes, source, extension) = match input {
        InputFile::Real(path) => {
            let bytes = std::fs::read(path).into_diagnostic()?;
            let extension = path.extension().and_then(|ext| ext.to_str()).map(str::to_owned);
            (bytes, path.display().to_string(), extension)
        }
        InputFile::Stdin(bytes) => (bytes.to_vec(), "stdin".to_string(), None),
    };

    let is_package = bytes.starts_with(b"MASP\0") || extension.as_deref() == Some("masp");
    if is_package {
        return load_package_from_bytes(&bytes, &source, config).map(ProgramArtifact::Package);
    }

    if config.entrypoint.is_some() {
        return Err(Report::msg("--entrypoint requires a .masp package input"));
    }

    if extension.as_deref() == Some("masb") || matches!(input, InputFile::Stdin(_)) {
        return Program::read_from_bytes(&bytes).map(ProgramArtifact::Program).map_err(|err| {
            Report::msg(format!("failed to load Miden program from {source}: {err}"))
        });
    }

    Err(Report::msg(format!(
        "unsupported input artifact {source}: expected a .masp package or .masb compiled program; compile MASM sources with `miden-vm compile -a <file.masm> -o <file.masb>`"
    )))
}

#[cfg(feature = "dap")]
pub(crate) fn verify_package_dependencies(
    package: &miden_mast_package::Package,
    libs: &[Arc<Library>],
) -> Result<(), Report> {
    let available = libs.iter().map(|lib| *lib.digest()).collect::<std::collections::BTreeSet<_>>();
    for dependency in package.manifest.dependencies() {
        if !available.contains(&dependency.digest) {
            return Err(Report::msg(format!(
                "dependency {dependency:?} not found in loaded libraries"
            )));
        }
    }

    Ok(())
}

fn load_sysroot_libs(
    toolchain_dir: &Path,
    log_target: &'static str,
) -> Result<Vec<Arc<Library>>, Report> {
    let mut libs = Vec::new();

    let entries = match std::fs::read_dir(toolchain_dir) {
        Ok(entries) => entries,
        Err(_) => {
            log::debug!(target: log_target, "could not read sysroot directory: {}", toolchain_dir.display());
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
            log::debug!(target: log_target, "loading package from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let package = miden_mast_package::Package::read_from_bytes(&bytes).map_err(|err| {
                Report::msg(format!("failed to load package '{}': {err}", path.display()))
            })?;
            libs.push(package.mast.clone());
        } else if ext == "masl" {
            log::debug!(target: log_target, "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let lib = Library::read_from_bytes(&bytes).map_err(|err| {
                Report::msg(format!("failed to load library '{}': {err}", path.display()))
            })?;
            libs.push(Arc::new(lib));
        }
    }

    if libs.is_empty() {
        log::debug!(target: log_target, "no libraries found in sysroot: {}", toolchain_dir.display());
    }

    Ok(libs)
}

fn load_package_from_bytes(
    bytes: &[u8],
    source: &str,
    config: &DebuggerConfig,
) -> Result<Arc<miden_mast_package::Package>, Report> {
    let package = miden_mast_package::Package::read_from_bytes(bytes)
        .map(Arc::new)
        .map_err(|err| Report::msg(format!("failed to load Miden package from {source}: {err}")))?;

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
