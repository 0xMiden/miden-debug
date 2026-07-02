use std::sync::Arc;

use miden_assembly::SourceManager;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_core::{program::StackInputs, serde::Deserializable};
use miden_mast_package::Package;

use crate::{
    config::DebuggerConfig,
    exec::{DebugExecutor, ExecutionConfig, Executor},
    input::InputFile,
};

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
    let package = load_package(config)?;
    // The registry indexes explicitly linked libraries, the toolchain sysroot, and local build
    // artifacts, then resolves the package's manifest dependencies against that index, so
    // missing dependencies are reported before execution with a hint on how to provide them.
    //
    // TODO: When upgrading to miden-assembly/miden-mast-package 0.24, extract any embedded
    // kernel package via `Package::try_embedded_kernel_package()` and register it if the
    // dependency resolver does not already have it.
    let packages = crate::package_registry::load_packages(
        config,
        source_manager.clone(),
        Some(&package),
        log_target,
    )?;

    let mut executor = Executor::new(args);
    for package in packages {
        let lib = package.mast.clone();
        executor.register_library_dependency(lib.clone());
        executor.with_library(lib);
    }

    executor.with_dependencies(package.manifest.dependencies())?;
    executor.with_advice_inputs(inputs.advice_inputs);

    let program = package.unwrap_program();
    Ok(executor.into_debug(&program, source_manager))
}

pub(crate) fn load_package(config: &DebuggerConfig) -> Result<Arc<Package>, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    let (bytes, source) = match input {
        InputFile::Real(path) => {
            let bytes = std::fs::read(path).into_diagnostic()?;
            (bytes, path.display().to_string())
        }
        InputFile::Stdin(bytes) => (bytes.to_vec(), "stdin".to_string()),
    };

    load_package_from_bytes(&bytes, &source, config)
}

fn load_package_from_bytes(
    bytes: &[u8],
    source: &str,
    config: &DebuggerConfig,
) -> Result<Arc<Package>, Report> {
    let package = Package::read_from_bytes(bytes)
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
    } else if package.is_program() {
        Ok(package)
    } else {
        Err(Report::msg(format!(
            "input package '{source}' is not executable; pass --entrypoint <module>::<procedure> \
             to debug a library package"
        )))
    }
}
