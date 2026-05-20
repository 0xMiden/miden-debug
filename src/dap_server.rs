use std::{collections::BTreeSet, path::Path, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_assembly_syntax::{
    Library,
    diagnostics::{IntoDiagnostic, Report},
};
use miden_core::{
    Word, events::EventId, mast::MastForest, operations::DebugOptions, program::Program,
    serde::Deserializable,
};
use miden_debug_types::{Location, SourceFile, SourceManagerExt, SourceSpan};
use miden_processor::{
    BaseHost, DefaultDebugHandler, DefaultHost, FutureMaybeSend, Host, ProcessorState, StackInputs,
    TraceError, advice::AdviceMutation, event::EventError,
};

use crate::{DapConfig, DapExecutor, DebuggerConfig, InputFile, exec::ExecutionConfig};

/// Start a DAP server for a local Miden program.
///
/// This is the non-transaction counterpart to `miden-client exec --start-debug-adapter`.
/// It accepts standalone MASM source files as well as compiled `.masp` packages.
pub fn run(config: Box<DebuggerConfig>) -> Result<(), Report> {
    let addr = config
        .start_debug_adapter
        .as_ref()
        .ok_or_else(|| Report::msg("missing --start-debug-adapter address"))?;
    DapConfig::set_global(
        DapConfig::new(addr).with_source_path_prefixes(config.source_path_prefixes.clone()),
    );

    let source_manager = Arc::new(DefaultSourceManager::default());
    let inputs = execution_inputs(&config)?;
    let libs = load_libraries(&config, source_manager.clone())?;
    let program = load_program(&config, source_manager.clone(), &libs)?;
    let mut host = StandaloneDapHost::new(source_manager);

    for lib in libs {
        host.load_library(lib.mast_forest().clone()).map_err(|err| {
            Report::msg(format!("failed to load linked library into DAP host: {err}"))
        })?;
    }

    let executor = DapExecutor::new(inputs.inputs, inputs.advice_inputs, inputs.options);
    futures::executor::block_on(executor.execute_async(&program, &mut host))
        .map(|_| ())
        .map_err(|err| Report::msg(format!("program execution failed: {err}")))
}

struct StandaloneDapHost {
    inner: DefaultHost<DefaultDebugHandler, DefaultSourceManager>,
    source_manager: Arc<DefaultSourceManager>,
}

impl StandaloneDapHost {
    fn new(source_manager: Arc<DefaultSourceManager>) -> Self {
        let inner = DefaultHost::default().with_source_manager(source_manager.clone());
        Self {
            inner,
            source_manager,
        }
    }

    fn load_library(
        &mut self,
        lib: Arc<MastForest>,
    ) -> Result<(), miden_processor::ExecutionError> {
        self.inner.load_library(lib)
    }

    fn ensure_source_file(&self, location: &Location) -> Option<Arc<SourceFile>> {
        if let Some(file) = self.source_manager.get_by_uri(location.uri()) {
            return Some(file);
        }

        let path = location
            .uri()
            .as_str()
            .strip_prefix("file://")
            .unwrap_or_else(|| location.uri().as_str());
        self.source_manager.load_file(Path::new(path)).ok()
    }
}

impl BaseHost for StandaloneDapHost {
    fn get_label_and_source_file(
        &self,
        location: &Location,
    ) -> (SourceSpan, Option<Arc<SourceFile>>) {
        let maybe_file = self.ensure_source_file(location);
        let span = self.source_manager.location_to_span(location.clone()).unwrap_or_default();
        (span, maybe_file)
    }

    fn on_debug(
        &mut self,
        process: &ProcessorState,
        options: &DebugOptions,
    ) -> Result<(), miden_processor::DebugError> {
        self.inner.on_debug(process, options)
    }

    fn on_trace(&mut self, process: &ProcessorState, trace_id: u32) -> Result<(), TraceError> {
        self.inner.on_trace(process, trace_id)
    }

    fn resolve_event(&self, event_id: EventId) -> Option<&miden_core::events::EventName> {
        self.inner.resolve_event(event_id)
    }
}

impl Host for StandaloneDapHost {
    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.inner.on_event(process)
    }
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

fn load_program(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    libs: &[Arc<Library>],
) -> Result<Program, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    if let InputFile::Real(path) = input
        && path.extension().and_then(|ext| ext.to_str()) == Some("masm")
    {
        return assemble_masm_program(path, source_manager, libs);
    }

    let package = load_package(config)?;
    verify_package_dependencies(&package, libs)?;
    Ok(package.unwrap_program())
}

fn assemble_masm_program(
    path: &Path,
    source_manager: Arc<dyn SourceManager>,
    libs: &[Arc<Library>],
) -> Result<Program, Report> {
    let mut assembler = Assembler::new(source_manager);
    for lib in libs {
        assembler.link_dynamic_library(lib.as_ref())?;
    }

    assembler.assemble_program(path)
}

fn load_libraries(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
) -> Result<Vec<Arc<Library>>, Report> {
    let mut libs = Vec::with_capacity(config.link_libraries.len());
    for link_library in config.link_libraries.iter() {
        log::debug!(target: "dap", "loading link library {}", link_library.name());
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
            log::debug!(target: "dap", "could not read sysroot directory: {}", toolchain_dir.display());
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
            log::debug!(target: "dap", "loading package from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let package = miden_mast_package::Package::read_from_bytes(&bytes).map_err(|err| {
                Report::msg(format!("failed to load package '{}': {err}", path.display()))
            })?;
            libs.push(package.mast.clone());
        } else if ext == "masl" {
            log::debug!(target: "dap", "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let lib = Library::read_from_bytes(&bytes).map_err(|err| {
                Report::msg(format!("failed to load library '{}': {err}", path.display()))
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
                .map_err(|err| {
                    Report::msg(format!(
                        "failed to load Miden package from {}: {err}",
                        path.display()
                    ))
                })?
        }
        InputFile::Stdin(bytes) => miden_mast_package::Package::read_from_bytes(bytes)
            .map(Arc::new)
            .map_err(|err| {
                Report::msg(format!("failed to load Miden package from stdin: {err}"))
            })?,
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

fn verify_package_dependencies(
    package: &miden_mast_package::Package,
    libs: &[Arc<Library>],
) -> Result<(), Report> {
    let available = libs.iter().map(|lib| *lib.digest()).collect::<BTreeSet<_>>();
    for dependency in package.manifest.dependencies() {
        if !available.contains(&dependency.digest) {
            return Err(Report::msg(format!(
                "dependency {dependency:?} not found in loaded libraries"
            )));
        }
    }

    Ok(())
}
