use std::sync::Arc;

use miden_assembly::{DefaultSourceManager, SourceManager};
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_core::{FieldElement, utils::Deserializable};
use miden_processor::{Felt, StackInputs};

use crate::{
    config::DebuggerConfig,
    debug::{Breakpoint, BreakpointType, ReadMemoryExpr},
    exec::{DebugExecutor, ExecutionTrace, Executor},
    input::InputFile,
};

pub struct State {
    pub package: Arc<miden_mast_package::Package>,
    pub source_manager: Arc<dyn SourceManager>,
    pub config: Box<DebuggerConfig>,
    pub executor: DebugExecutor,
    pub execution_trace: ExecutionTrace,
    pub execution_failed: Option<miden_processor::ExecutionError>,
    pub input_mode: InputMode,
    pub breakpoints: Vec<Breakpoint>,
    pub breakpoints_hit: Vec<Breakpoint>,
    pub next_breakpoint_id: u8,
    pub stopped: bool,
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Normal,
    #[allow(dead_code)]
    Insert,
    Command,
}

impl State {
    pub fn new(config: Box<DebuggerConfig>) -> Result<Self, Report> {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let mut inputs = config.inputs.clone().unwrap_or_default();
        if !config.args.is_empty() {
            inputs.inputs =
                StackInputs::new(config.args.iter().map(|n| n.0).collect()).into_diagnostic()?;
        }
        let args = inputs.inputs.iter().copied().rev().collect::<Vec<_>>();
        let package = load_package(&config)?;

        // Load libraries from link_libraries and sysroot BEFORE resolving dependencies
        let mut libs = Vec::with_capacity(config.link_libraries.len());
        for link_library in config.link_libraries.iter() {
            log::debug!(target: "state", "loading link library {}", link_library.name());
            let lib = link_library.load(&config, source_manager.clone())?;
            libs.push(lib.clone());
        }

        // Load std and base libraries from sysroot if available
        if let Some(toolchain_dir) = config.toolchain_dir() {
            libs.extend(load_sysroot_libs(&toolchain_dir)?);
        }

        // Create executor and register libraries with dependency resolver before resolving
        let mut executor = Executor::new(args.clone());
        for lib in libs.iter() {
            executor.register_library_dependency(lib.clone());
            executor.with_library(lib.clone());
        }

        // Now resolve package dependencies (they should find the registered libraries)
        let dependencies = package.manifest.dependencies();
        executor.with_dependencies(dependencies)?;
        executor.with_advice_inputs(inputs.advice_inputs.clone());

        let program = package.unwrap_program();
        let executor = executor.into_debug(&program, source_manager.clone());

        // Execute the program until it terminates to capture a full trace for use during debugging
        let mut trace_executor = Executor::new(args);
        for lib in libs.iter() {
            trace_executor.register_library_dependency(lib.clone());
            trace_executor.with_library(lib.clone());
        }
        let dependencies = package.manifest.dependencies();
        trace_executor.with_dependencies(dependencies)?;
        trace_executor.with_advice_inputs(inputs.advice_inputs.clone());

        let execution_trace = trace_executor.capture_trace(&program, source_manager.clone());

        Ok(Self {
            package,
            source_manager,
            config,
            executor,
            execution_trace,
            execution_failed: None,
            input_mode: InputMode::Normal,
            breakpoints: vec![],
            breakpoints_hit: vec![],
            next_breakpoint_id: 0,
            stopped: true,
        })
    }

    pub fn reload(&mut self) -> Result<(), Report> {
        log::debug!("reloading program");
        let package = load_package(&self.config)?;

        let mut inputs = self.config.inputs.clone().unwrap_or_default();
        if !self.config.args.is_empty() {
            inputs.inputs =
                StackInputs::new(self.config.args.iter().copied().map(|n| n.0).collect())
                    .into_diagnostic()?;
        }
        let args = inputs.inputs.iter().copied().rev().collect::<Vec<_>>();

        // Load libraries from link_libraries and sysroot BEFORE resolving dependencies
        let mut libs = Vec::with_capacity(self.config.link_libraries.len());
        for link_library in self.config.link_libraries.iter() {
            let lib = link_library.load(&self.config, self.source_manager.clone())?;
            libs.push(lib.clone());
        }

        // Load std and base libraries from sysroot if available
        if let Some(toolchain_dir) = self.config.toolchain_dir() {
            libs.extend(load_sysroot_libs(&toolchain_dir)?);
        }

        // Create executor and register libraries with dependency resolver before resolving
        let mut executor = Executor::new(args.clone());
        for lib in libs.iter() {
            executor.register_library_dependency(lib.clone());
            executor.with_library(lib.clone());
        }

        // Now resolve package dependencies
        let dependencies = package.manifest.dependencies();
        executor.with_dependencies(dependencies)?;
        executor.with_advice_inputs(inputs.advice_inputs.clone());

        let program = package.unwrap_program();
        let executor = executor.into_debug(&program, self.source_manager.clone());

        // Execute the program until it terminates to capture a full trace for use during debugging
        let mut trace_executor = Executor::new(args);
        for lib in libs.iter() {
            trace_executor.register_library_dependency(lib.clone());
            trace_executor.with_library(lib.clone());
        }
        let dependencies = package.manifest.dependencies();
        trace_executor.with_dependencies(dependencies)?;
        trace_executor.with_advice_inputs(core::mem::take(&mut inputs.advice_inputs));
        let execution_trace = trace_executor.capture_trace(&program, self.source_manager.clone());

        self.package = package;
        self.executor = executor;
        self.execution_trace = execution_trace;
        self.execution_failed = None;
        self.breakpoints_hit.clear();
        let breakpoints = core::mem::take(&mut self.breakpoints);
        self.breakpoints.reserve(breakpoints.len());
        self.next_breakpoint_id = 0;
        self.stopped = true;
        for bp in breakpoints {
            self.create_breakpoint(bp.ty);
        }
        Ok(())
    }

    pub fn create_breakpoint(&mut self, ty: BreakpointType) {
        let id = self.next_breakpoint_id();
        let creation_cycle = self.executor.cycle;
        log::trace!("created breakpoint with id {id} at cycle {creation_cycle}");
        if matches!(ty, BreakpointType::Finish)
            && let Some(frame) = self.executor.callstack.current_frame_mut()
        {
            frame.break_on_exit();
        }
        self.breakpoints.push(Breakpoint {
            id,
            creation_cycle,
            ty,
        });
    }

    fn next_breakpoint_id(&mut self) -> u8 {
        let mut candidate = self.next_breakpoint_id;
        let initial = candidate;
        let mut next = candidate.wrapping_add(1);
        loop {
            assert_ne!(initial, next, "unable to allocate a breakpoint id: too many breakpoints");
            if self
                .breakpoints
                .iter()
                .chain(self.breakpoints_hit.iter())
                .any(|bp| bp.id == candidate)
            {
                candidate = next;
                next = candidate.wrapping_add(1);
                continue;
            }
            self.next_breakpoint_id = next;
            break candidate;
        }
    }
}

macro_rules! write_with_format_type {
    ($out:ident, $read_expr:ident, $value:expr) => {
        match $read_expr.format {
            crate::debug::FormatType::Decimal => write!(&mut $out, "{}", $value).unwrap(),
            crate::debug::FormatType::Hex => write!(&mut $out, "{:0x}", $value).unwrap(),
            crate::debug::FormatType::Binary => write!(&mut $out, "{:0b}", $value).unwrap(),
        }
    };
}

impl State {
    pub fn read_memory(&self, expr: &ReadMemoryExpr) -> Result<String, String> {
        use core::fmt::Write;

        use miden_assembly_syntax::ast::types::Type;

        use crate::debug::FormatType;

        let cycle = miden_processor::RowIndex::from(self.executor.cycle);
        let context = self.executor.current_context;
        let mut output = String::new();
        if expr.count > 1 {
            return Err("-count with value > 1 is not yet implemented".into());
        } else if matches!(expr.ty, Type::Felt) {
            if !expr.addr.is_element_aligned() {
                return Err(
                    "read failed: type 'felt' must be aligned to an element boundary".into()
                );
            }
            let felt = self
                .execution_trace
                .read_memory_element_in_context(expr.addr.addr, context, cycle)
                .unwrap_or(Felt::ZERO);
            write_with_format_type!(output, expr, felt.as_int());
        } else if matches!(
            expr.ty,
            Type::Array(ref array_ty) if array_ty.element_type() == &Type::Felt && array_ty.len() == 4
        ) {
            if !expr.addr.is_word_aligned() {
                return Err("read failed: type 'word' must be aligned to a word boundary".into());
            }
            let word = self.execution_trace.read_memory_word(expr.addr.addr).unwrap_or_default();
            output.push('[');
            for (i, elem) in word.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                write_with_format_type!(output, expr, elem.as_int());
            }
            output.push(']');
        } else {
            let bytes = self
                .execution_trace
                .read_bytes_for_type(expr.addr, &expr.ty, context, cycle)
                .map_err(|err| format!("invalid read: {err}"))?;
            match &expr.ty {
                Type::I1 => match expr.format {
                    FormatType::Decimal => write!(&mut output, "{}", bytes[0] != 0).unwrap(),
                    FormatType::Hex => {
                        write!(&mut output, "{:#0x}", (bytes[0] != 0) as u8).unwrap()
                    }
                    FormatType::Binary => {
                        write!(&mut output, "{:#0b}", (bytes[0] != 0) as u8).unwrap()
                    }
                },
                Type::I8 => write_with_format_type!(output, expr, bytes[0] as i8),
                Type::U8 => write_with_format_type!(output, expr, bytes[0]),
                Type::I16 => {
                    write_with_format_type!(output, expr, i16::from_be_bytes([bytes[0], bytes[1]]))
                }
                Type::U16 => {
                    write_with_format_type!(output, expr, u16::from_be_bytes([bytes[0], bytes[1]]))
                }
                Type::I32 => write_with_format_type!(
                    output,
                    expr,
                    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                ),
                Type::U32 => write_with_format_type!(
                    output,
                    expr,
                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                ),
                ty @ (Type::I64 | Type::U64) => {
                    let hi = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
                    let lo = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as u64;
                    let val = (hi * 2u64.pow(32)) + lo;
                    if matches!(ty, Type::I64) {
                        write_with_format_type!(output, expr, val as i64)
                    } else {
                        write_with_format_type!(output, expr, val)
                    }
                }
                ty => {
                    return Err(format!(
                        "support for reads of type '{ty}' are not implemented yet"
                    ));
                }
            }
        }

        Ok(output)
    }
}

// TODO: In v0.20, std.masp will be renamed to core.masp - handle this when upgrading.

/// Attempts to load the standard library from the sysroot/toolchain directory.
///
/// Supports both formats:
/// - `.masp` (package format) - used by the midenup toolchain
/// - `.masl` (serialized Library) - legacy format
///
/// Returns `Ok(None)` if the library is not found in the sysroot.
fn load_sysroot_libs(
    toolchain_dir: &std::path::Path,
) -> Result<Vec<Arc<miden_assembly_syntax::Library>>, Report> {
    let mut libs = Vec::new();

    // Load each known sysroot library: std and base (miden.masl)
    for (name, filenames) in [
        ("std", &["std.masp", "std.masl"][..]),
        ("base", &["miden.masp", "miden.masl"][..]),
    ] {
        if let Some(lib) = load_sysroot_lib(toolchain_dir, name, filenames)? {
            libs.push(lib);
        }
    }

    Ok(libs)
}

fn load_sysroot_lib(
    toolchain_dir: &std::path::Path,
    name: &str,
    filenames: &[&str],
) -> Result<Option<Arc<miden_assembly_syntax::Library>>, Report> {
    for filename in filenames {
        let path = toolchain_dir.join(filename);
        if !path.exists() {
            continue;
        }

        log::debug!(target: "state", "loading {name} library from sysroot: {}", path.display());
        let bytes = std::fs::read(&path).into_diagnostic()?;

        if filename.ends_with(".masp") {
            let package = miden_mast_package::Package::read_from_bytes(&bytes)
                .map_err(|e| Report::msg(format!("failed to load {name} package: {e}")))?;
            let lib = match package.mast {
                miden_mast_package::MastArtifact::Library(lib) => lib.clone(),
                miden_mast_package::MastArtifact::Executable(_) => {
                    return Err(Report::msg(format!(
                        "expected {filename} to contain a Library, got Executable: '{}'",
                        path.display()
                    )));
                }
            };
            return Ok(Some(lib));
        } else {
            let lib = miden_assembly_syntax::Library::read_from_bytes(&bytes)
                .map_err(|e| Report::msg(format!("failed to load {name} library: {e}")))?;
            return Ok(Some(Arc::new(lib)));
        }
    }

    log::debug!(target: "state", "no {name} library found in sysroot: {}", toolchain_dir.display());
    Ok(None)
}

fn load_package(config: &DebuggerConfig) -> Result<Arc<miden_mast_package::Package>, Report> {
    let package = match config.input {
        InputFile::Real(ref path) => {
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
        InputFile::Stdin(ref bytes) => miden_mast_package::Package::read_from_bytes(bytes)
            .map(Arc::new)
            .map_err(|e| Report::msg(format!("failed to load Miden package from stdin: {e}")))?,
    };

    if let Some(entry) = config.entrypoint.as_ref() {
        // Input must be a library, not a program
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
