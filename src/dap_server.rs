use std::{path::Path, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_core::{Word, events::EventId};
use miden_debug_engine::HybridPackageRegistry;
use miden_debug_types::{Location, SourceFile, SourceManagerExt, SourceSpan};
use miden_mast_package::{Package, PackageId};
use miden_package_registry::{PackageProvider, PackageRegistry};
use miden_processor::{
    BaseHost, DefaultHost, FutureMaybeSend, Host, HostLibrary, LoadedMastForest, ProcessorState,
    advice::AdviceMutation, event::EventError,
};

use crate::{DapConfig, DapExecutor, DebuggerConfig, InputFile};

/// Start a DAP server for a local Miden program.
///
/// This is the non-transaction counterpart to `miden-client exec --start-debug-adapter`.
/// It accepts standalone MASM source files as well as compiled package artifacts.
pub fn run(config: Box<DebuggerConfig>) -> Result<(), Report> {
    let addr = config
        .start_debug_adapter
        .as_ref()
        .ok_or_else(|| Report::msg("missing --start-debug-adapter address"))?;
    DapConfig::set_global(
        DapConfig::new(addr).with_source_path_prefixes(config.source_path_prefixes.clone()),
    );

    let source_manager = Arc::new(DefaultSourceManager::default());
    let inputs = crate::program_loader::execution_inputs(&config)?;
    let mut registry = HybridPackageRegistry::new(
        config.sysroot.as_deref(),
        &config.search_path,
        &config.link_libraries,
    )?;
    let program = load_program(&config, source_manager.clone(), &mut registry)?;
    let mut host = StandaloneDapHost::new(source_manager);

    for package in registry.all() {
        let name = package.name.clone();
        host.load_library(package).map_err(|err| {
            Report::msg(format!("failed to load package '{name}' into DAP host: {err}"))
        })?;
    }

    let executor = DapExecutor::new(inputs.inputs, inputs.advice_inputs, inputs.options);
    futures::executor::block_on(executor.execute_async(program, &mut host))
        .map(|_| ())
        .map_err(|err| Report::msg(format!("program execution failed: {err}")))
}

struct StandaloneDapHost {
    inner: DefaultHost<DefaultSourceManager>,
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
        lib: impl Into<HostLibrary>,
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

    fn resolve_event(&self, event_id: EventId) -> Option<&miden_core::events::EventName> {
        self.inner.resolve_event(event_id)
    }
}

impl Host for StandaloneDapHost {
    fn get_mast_forest(
        &self,
        node_digest: &Word,
    ) -> impl FutureMaybeSend<Option<LoadedMastForest>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.inner.on_event(process)
    }
}

fn load_program(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    registry: &mut HybridPackageRegistry,
) -> Result<Arc<Package>, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    if let InputFile::Real(path) = input
        && path.extension().and_then(|ext| ext.to_str()) == Some("masm")
    {
        return assemble_masm_program(path, source_manager, registry);
    }

    let package = load_package(config)?;
    verify_package_dependencies(&package, registry)?;
    assert!(package.is_program());
    Ok(package)
}

fn assemble_masm_program(
    path: &Path,
    source_manager: Arc<dyn SourceManager>,
    registry: &HybridPackageRegistry,
) -> Result<Arc<Package>, Report> {
    let mut assembler = Assembler::new(source_manager.clone());

    let mut parser =
        miden_assembly_syntax::ModuleParser::new(Some(miden_assembly::ast::ModuleKind::Executable));
    let module = parser.parse_file(None, path, source_manager)?;

    for extern_package in module.required_packages() {
        let package_id = PackageId::from(extern_package.clone().into_inner());
        let package = registry
            .find_latest(&package_id, &miden_project::VersionReq::STAR.into())
            .ok_or_else(|| Report::msg(format!("extern package '{package_id}' is not available")))
            .and_then(|record| registry.load_package(&package_id, record.version()))?;
        assembler.link_package(package, miden_project::Linkage::Dynamic)?;
    }

    assembler.assemble_program("program", module).map(Arc::from)
}

fn load_package(config: &DebuggerConfig) -> Result<Arc<Package>, Report> {
    let input = config.input.as_ref().ok_or_else(|| Report::msg("no input file specified"))?;
    let package = match input {
        InputFile::Real(path) => {
            let bytes = std::fs::read(path).into_diagnostic()?;
            Package::read_from_bytes_trusted(&bytes).map(Arc::new).map_err(|err| {
                Report::msg(format!("failed to load Miden package from {}: {err}", path.display()))
            })?
        }
        InputFile::Stdin(bytes) => {
            Package::read_from_bytes_trusted(bytes).map(Arc::new).map_err(|err| {
                Report::msg(format!("failed to load Miden package from stdin: {err}"))
            })?
        }
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
    package: &Package,
    registry: &HybridPackageRegistry,
) -> Result<(), Report> {
    for dependency in package.manifest.dependencies() {
        let version = miden_project::Version::new(dependency.version().clone(), dependency.digest);
        if !registry.is_version_available(&dependency.name, &version) {
            return Err(Report::msg(format!(
                "dependency {}@{version} not found in loaded libraries",
                dependency.name
            )));
        }
    }

    Ok(())
}
