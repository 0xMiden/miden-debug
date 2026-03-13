use std::{collections::VecDeque, sync::Arc};

use miden_assembly::{DefaultSourceManager, SourceManager};
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_core::{program::Program, serde::Deserializable};
use miden_processor::{
    Felt, StackInputs,
    advice::{AdviceInputs, AdviceMutation},
    mast::MastForest,
};

use crate::{
    config::DebuggerConfig,
    debug::{Breakpoint, BreakpointType, ReadMemoryExpr},
    exec::{DebugExecutor, ExecutionTrace, Executor},
    input::InputFile,
};

/// Whether the debugger is debugging a plain program or a transaction.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DebugMode {
    /// Debugging a plain MASM program loaded from a package.
    Program,
    /// Debugging a Miden transaction with pre-recorded event replay.
    Transaction,
    /// Debugging remotely via a DAP server connection.
    #[cfg(feature = "dap")]
    RemoteDap,
}

fn clone_advice_mutation(mutation: &AdviceMutation) -> AdviceMutation {
    match mutation {
        AdviceMutation::ExtendStack { values } => AdviceMutation::ExtendStack {
            values: values.clone(),
        },
        AdviceMutation::ExtendMap { other } => AdviceMutation::ExtendMap {
            other: other.clone(),
        },
        AdviceMutation::ExtendMerkleStore { infos } => AdviceMutation::ExtendMerkleStore {
            infos: infos.clone(),
        },
        AdviceMutation::ExtendPrecompileRequests { data } => {
            AdviceMutation::ExtendPrecompileRequests { data: data.clone() }
        }
    }
}

fn clone_event_replay_queue(event_replay: &[Vec<AdviceMutation>]) -> VecDeque<Vec<AdviceMutation>> {
    event_replay
        .iter()
        .map(|batch| batch.iter().map(clone_advice_mutation).collect())
        .collect()
}

pub struct State {
    pub package: Option<Arc<miden_mast_package::Package>>,
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
    pub debug_mode: DebugMode,
    #[cfg(feature = "dap")]
    pub dap_client: Option<crate::exec::DapClient>,
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
            // CLI args model sequential pushes, but StackInputs expects the top element first.
            let args = config.args.iter().rev().map(|felt| felt.0).collect::<Vec<_>>();
            inputs.inputs = StackInputs::new(&args).into_diagnostic()?;
        }
        let args = inputs.inputs.iter().copied().collect::<Vec<_>>();
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
            package: Some(package),
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
            debug_mode: DebugMode::Program,
            #[cfg(feature = "dap")]
            dap_client: None,
        })
    }

    /// Create a new debugger state for transaction debugging.
    ///
    /// This uses pre-recorded event mutations to replay host events during
    /// step-by-step debugging, since the debugger's host doesn't have access
    /// to the real transaction host.
    pub fn new_for_transaction(
        program: Arc<Program>,
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        source_manager: Arc<dyn SourceManager>,
        mast_forests: Vec<Arc<MastForest>>,
        event_replay: Vec<Vec<AdviceMutation>>,
    ) -> Result<Self, Report> {
        let args = stack_inputs.iter().copied().rev().collect::<Vec<_>>();

        // Create debug executor with event replay
        let mut executor = Executor::new(args.clone());
        executor.with_advice_inputs(advice_inputs.clone());
        let debug_executor = executor.into_debug_with_replay(
            &program,
            source_manager.clone(),
            mast_forests.clone(),
            clone_event_replay_queue(&event_replay),
        );

        // Create trace executor with a cloned replay queue
        let mut trace_executor = Executor::new(args);
        trace_executor.with_advice_inputs(advice_inputs);
        let trace_debug = trace_executor.into_debug_with_replay(
            &program,
            source_manager.clone(),
            mast_forests,
            clone_event_replay_queue(&event_replay),
        );

        // Run trace executor to completion to capture execution trace
        let execution_trace = run_to_trace(trace_debug);

        Ok(Self {
            package: None,
            source_manager,
            config: Box::new(DebuggerConfig::default()),
            executor: debug_executor,
            execution_trace,
            execution_failed: None,
            input_mode: InputMode::Normal,
            breakpoints: vec![],
            breakpoints_hit: vec![],
            next_breakpoint_id: 0,
            stopped: true,
            debug_mode: DebugMode::Transaction,
            #[cfg(feature = "dap")]
            dap_client: None,
        })
    }

    pub fn reload(&mut self) -> Result<(), Report> {
        if self.debug_mode == DebugMode::Transaction {
            return Err(Report::msg("reload is not supported in transaction debug mode"));
        }
        #[cfg(feature = "dap")]
        if self.debug_mode == DebugMode::RemoteDap {
            return Err(Report::msg("reload is not supported in DAP remote debug mode"));
        }

        log::debug!("reloading program");
        let package = load_package(&self.config)?;

        let mut inputs = self.config.inputs.clone().unwrap_or_default();
        if !self.config.args.is_empty() {
            // CLI args model sequential pushes, but StackInputs expects the top element first.
            let args = self.config.args.iter().rev().map(|felt| felt.0).collect::<Vec<_>>();
            inputs.inputs = StackInputs::new(&args).into_diagnostic()?;
        }
        let args = inputs.inputs.iter().copied().collect::<Vec<_>>();

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

        self.package = Some(package);
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

        #[cfg(feature = "dap")]
        if self.debug_mode == DebugMode::RemoteDap {
            return Err("memory reads are not supported in DAP remote debug mode".into());
        }

        let cycle = miden_processor::trace::RowIndex::from(self.executor.cycle);
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
            write_with_format_type!(output, expr, felt.as_canonical_u64());
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
                write_with_format_type!(output, expr, elem.as_canonical_u64());
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
                    write_with_format_type!(output, expr, i16::from_le_bytes([bytes[0], bytes[1]]))
                }
                Type::U16 => {
                    write_with_format_type!(output, expr, u16::from_le_bytes([bytes[0], bytes[1]]))
                }
                Type::I32 => write_with_format_type!(
                    output,
                    expr,
                    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                ),
                Type::U32 => write_with_format_type!(
                    output,
                    expr,
                    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                ),
                ty @ (Type::I64 | Type::U64) => {
                    let val = u64::from_le_bytes(bytes[..8].try_into().unwrap());
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

// DAP CLIENT MODE
// ================================================================================================

#[cfg(feature = "dap")]
impl State {
    /// Create a new debugger state for remote DAP debugging.
    ///
    /// Connects to a DAP server, performs the handshake, and queries the
    /// initial state to populate the executor fields that the TUI panes read.
    pub fn new_for_dap(addr: &str) -> Result<Self, Report> {
        use std::collections::BTreeSet;

        use miden_processor::{ContextId, FastProcessor};

        use crate::{
            debug::{CallFrame, CallStack},
            exec::{DebuggerHost, SCOPE_STACK},
        };

        let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());

        let mut client = crate::exec::DapClient::connect(addr).map_err(Report::msg)?;
        client.handshake().map_err(Report::msg)?;

        // Query initial state from DAP server
        let stack_frames = client.stack_trace().map_err(Report::msg)?;
        let stack_vars = client.variables(SCOPE_STACK).map_err(Report::msg)?;

        // Build call frames from DAP StackTrace response
        let call_frames: Vec<CallFrame> = stack_frames
            .iter()
            .map(|f| {
                let resolved = resolve_dap_frame(f, &source_manager);
                CallFrame::from_remote(Some(f.name.clone()), resolved)
            })
            .collect();

        // Build current_stack from Variables response
        let current_stack: Vec<Felt> = stack_vars
            .iter()
            .map(|v| Felt::new(v.value.parse::<u64>().unwrap_or(0)))
            .collect();

        // Query cycle from Evaluate
        let mut cycle = 0usize;
        if let Ok(state_json) = client.evaluate("__miden_state")
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&state_json)
        {
            cycle = parsed.get("cycle").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        }

        // Build a dummy DebugExecutor — the processor/host are defaults and never stepped.
        // Only the public "view" fields matter for the TUI panes.
        let processor = FastProcessor::new(StackInputs::default());
        let host = DebuggerHost::new(source_manager.clone());
        let callstack = CallStack::from_remote_frames(call_frames);

        let executor = DebugExecutor {
            processor,
            host,
            resume_ctx: None,
            current_stack,
            current_op: None,
            current_asmop: None,
            stack_outputs: Default::default(),
            contexts: BTreeSet::new(),
            root_context: ContextId::root(),
            current_context: ContextId::root(),
            callstack,
            recent: VecDeque::new(),
            cycle,
            stopped: false,
        };

        Ok(Self {
            package: None,
            source_manager,
            config: Box::new(DebuggerConfig::default()),
            executor,
            execution_trace: ExecutionTrace::empty(),
            execution_failed: None,
            input_mode: InputMode::Normal,
            breakpoints: vec![],
            breakpoints_hit: vec![],
            next_breakpoint_id: 0,
            stopped: true,
            debug_mode: DebugMode::RemoteDap,
            dap_client: Some(client),
        })
    }

    /// Refresh the executor state from the DAP server after a step command.
    pub fn refresh_from_dap(&mut self) -> Result<(), Report> {
        use crate::{
            debug::{CallFrame, CallStack},
            exec::SCOPE_STACK,
        };

        let client = self.dap_client.as_mut().ok_or_else(|| Report::msg("no DAP client"))?;

        // Update stack
        let vars = client.variables(SCOPE_STACK).map_err(Report::msg)?;
        self.executor.current_stack =
            vars.iter().map(|v| Felt::new(v.value.parse::<u64>().unwrap_or(0))).collect();

        // Update call stack from StackTrace response
        let frames = client.stack_trace().map_err(Report::msg)?;
        let call_frames: Vec<CallFrame> = frames
            .iter()
            .map(|f| {
                let resolved = resolve_dap_frame(f, &self.source_manager);
                CallFrame::from_remote(Some(f.name.clone()), resolved)
            })
            .collect();
        self.executor.callstack = CallStack::from_remote_frames(call_frames);

        // Update cycle from Evaluate
        if let Ok(state_json) = client.evaluate("__miden_state")
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&state_json)
            && let Some(c) = parsed.get("cycle").and_then(|v| v.as_u64())
        {
            self.executor.cycle = c as usize;
        }

        Ok(())
    }
}

/// Resolve a DAP StackFrame to a [ResolvedLocation] by loading the source file from disk.
#[cfg(feature = "dap")]
fn resolve_dap_frame(
    frame: &dap::types::StackFrame,
    source_manager: &Arc<dyn SourceManager>,
) -> Option<crate::debug::ResolvedLocation> {
    use std::path::Path;

    use miden_debug_types::{SourceManagerExt, SourceSpan};

    let path_str = frame.source.as_ref()?.path.as_ref()?;
    let path = Path::new(path_str);
    let source_file = source_manager.load_file(path).ok()?;
    let line = frame.line.max(1) as u32;
    let col = frame.column.max(1) as u32;

    // Compute a span from the line number — use the byte range of the line
    let content = source_file.content();
    let line_index = miden_debug_types::LineIndex::from(line.saturating_sub(1));
    let range = content.line_range(line_index)?;
    let span = SourceSpan::new(source_file.id(), range);

    Some(crate::debug::ResolvedLocation {
        source_file,
        line,
        col,
        span,
    })
}

/// Attempts to load the standard library from the sysroot/toolchain directory.
///
/// Supports both formats:
/// - `.masp` (package format) - used by the midenup toolchain
/// - `.masl` (serialized Library) - legacy format
///   Load all library files (.masp and .masl) from the sysroot directory.
///
/// The toolchain determines what libraries are available in the sysroot.
fn load_sysroot_libs(
    toolchain_dir: &std::path::Path,
) -> Result<Vec<Arc<miden_assembly_syntax::Library>>, Report> {
    let mut libs = Vec::new();

    let entries = match std::fs::read_dir(toolchain_dir) {
        Ok(entries) => entries,
        Err(_) => {
            log::debug!(target: "state", "could not read sysroot directory: {}", toolchain_dir.display());
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
            log::debug!(target: "state", "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let package = miden_mast_package::Package::read_from_bytes(&bytes).map_err(|e| {
                Report::msg(format!("failed to load package '{}': {e}", path.display()))
            })?;
            match package.mast {
                miden_mast_package::MastArtifact::Library(lib) => {
                    libs.push(lib.clone());
                }
                miden_mast_package::MastArtifact::Executable(_) => {
                    log::debug!(target: "state", "skipping executable package: {}", path.display());
                }
            }
        } else if ext == "masl" {
            log::debug!(target: "state", "loading library from sysroot: {}", path.display());
            let bytes = std::fs::read(&path).into_diagnostic()?;
            let lib = miden_assembly_syntax::Library::read_from_bytes(&bytes).map_err(|e| {
                Report::msg(format!("failed to load library '{}': {e}", path.display()))
            })?;
            libs.push(Arc::new(lib));
        }
    }

    if libs.is_empty() {
        log::debug!(target: "state", "no libraries found in sysroot: {}", toolchain_dir.display());
    }

    Ok(libs)
}

/// Run a [DebugExecutor] to completion and return the [ExecutionTrace].
fn run_to_trace(mut executor: DebugExecutor) -> ExecutionTrace {
    loop {
        if executor.stopped {
            break;
        }
        match executor.step() {
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    executor.into_execution_trace()
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
