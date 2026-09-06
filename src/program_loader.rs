use std::sync::Arc;

use miden_assembly::SourceManager;
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_core::program::StackInputs;
use miden_debug_engine::{HybridPackageRegistry, read_package_from_bytes};
use miden_mast_package::Package;

use crate::{
    config::DebuggerConfig,
    debug::TypedProcedure,
    exec::{DebugExecutor, ExecutionConfig, Executor},
    input::InputFile,
};

pub(crate) struct LoadedDebugExecutor {
    pub executor: DebugExecutor,
    pub typed_procedure: Option<TypedProcedure>,
}

pub(crate) fn execution_inputs(
    config: &DebuggerConfig,
    typed_procedure: Option<&TypedProcedure>,
) -> Result<ExecutionConfig, Report> {
    let mut inputs = config.inputs.clone().unwrap_or_default();
    let should_encode_typed_args = config.inputs.is_none() && typed_procedure.is_some();
    if !config.args.is_empty() || should_encode_typed_args {
        let args = if let Some(procedure) = typed_procedure {
            procedure.encode_args(&config.args).map_err(|err| {
                let signature =
                    procedure.display_signature().unwrap_or_else(|_| "typed entrypoint".into());
                Report::msg(format!("invalid arguments for {signature}: {err}"))
            })?
        } else {
            // Raw CLI args model sequential pushes, but StackInputs expects the top element first.
            config
                .args
                .iter()
                .rev()
                .map(|arg| arg.parse::<crate::felt::Felt>().map(|felt| felt.0).map_err(Report::msg))
                .collect::<Result<Vec<_>, _>>()?
        };
        inputs.inputs = StackInputs::new(&args).into_diagnostic()?;
    }

    Ok(inputs)
}

pub(crate) fn load_debug_executor(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
    _log_target: &'static str,
) -> Result<LoadedDebugExecutor, Report> {
    let registry = HybridPackageRegistry::new(
        config.sysroot.as_deref(),
        &config.search_path,
        &config.link_libraries,
    )?;
    let package = load_package(config)?;
    let typed_procedure = TypedProcedure::for_package_entrypoint(&package);
    let inputs = execution_inputs(config, typed_procedure.as_ref())?;
    let args = inputs.inputs.iter().copied().collect::<Vec<_>>();

    let mut executor = Executor::new(args).with_registry(registry);
    executor
        .with_profiler_config(config.profiler_cli_args.clone().try_into()?)
        .with_advice_inputs(inputs.advice_inputs);

    Ok(LoadedDebugExecutor {
        executor: executor.into_debug(package, source_manager),
        typed_procedure,
    })
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
    let package = read_package_from_bytes(bytes, source)?;

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

#[cfg(test)]
mod tests {
    use miden_assembly_syntax::ast::types::{CallConv, FunctionType, Type};
    use miden_core::Felt;

    use super::*;

    #[test]
    fn typed_arguments_use_the_entrypoint_abi() {
        let config = DebuggerConfig {
            args: vec!["4294967303".into(), "true".into()],
            ..Default::default()
        };
        let procedure = TypedProcedure::new(
            "entrypoint",
            FunctionType::new(CallConv::ComponentModel, [Type::U64, Type::I1], []),
        )
        .unwrap();

        let inputs = execution_inputs(&config, Some(&procedure)).unwrap();

        assert_eq!(
            &inputs.inputs.as_ref()[..3],
            [Felt::from_u32(7), Felt::from_u32(1), Felt::from_u32(1)]
        );
    }

    #[test]
    fn untyped_arguments_keep_sequential_push_order() {
        let config = DebuggerConfig {
            args: vec!["3".into(), "4".into()],
            ..Default::default()
        };

        let inputs = execution_inputs(&config, None).unwrap();

        assert_eq!(&inputs.inputs.as_ref()[..2], [Felt::from_u32(4), Felt::from_u32(3)]);
    }
}
