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
    Remote,
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
    pub source_manager: Arc<dyn SourceManager>,
    pub config: Box<DebuggerConfig>,
    pub input_mode: InputMode,
    pub breakpoints: Vec<Breakpoint>,
    pub breakpoints_hit: Vec<Breakpoint>,
    pub next_breakpoint_id: u8,
    pub stopped: bool,
    pub debug_mode: DebugMode,
    session: SessionState,
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    Normal,
    #[allow(dead_code)]
    Insert,
    Command,
}

struct LocalState {
    executor: DebugExecutor,
    execution_trace: ExecutionTrace,
    execution_failed: Option<miden_processor::ExecutionError>,
}

#[cfg(feature = "dap")]
struct RemoteState {
    client: crate::exec::DapClient,
    executor: DebugExecutor,
    addr: String,
    /// Tracks which source files have had breakpoints synced to the DAP server,
    /// so we can send empty breakpoint lists when all breakpoints for a file are removed.
    synced_bp_files: std::collections::BTreeSet<String>,
}

enum SessionState {
    Local(Box<LocalState>),
    #[cfg(feature = "dap")]
    Remote(Box<RemoteState>),
}

#[cfg(feature = "dap")]
struct RemoteSnapshot {
    callstack: crate::debug::CallStack,
    current_stack: Vec<Felt>,
    cycle: usize,
}

#[cfg(feature = "dap")]
impl RemoteState {
    fn connect(addr: &str, source_manager: &Arc<dyn SourceManager>) -> Result<Self, Report> {
        use std::collections::BTreeSet;

        use miden_processor::{ContextId, FastProcessor};

        use crate::exec::DebuggerHost;

        let mut client = crate::exec::DapClient::connect(addr).map_err(Report::msg)?;
        let ui_state = client.handshake().map_err(Report::msg)?;
        let snapshot = convert_ui_state(&ui_state, source_manager);

        let processor = FastProcessor::new(StackInputs::default());
        let host = DebuggerHost::new(source_manager.clone());
        let executor = DebugExecutor {
            processor,
            host,
            resume_ctx: None,
            current_stack: snapshot.current_stack,
            current_op: None,
            current_asmop: None,
            stack_outputs: Default::default(),
            contexts: BTreeSet::new(),
            root_context: ContextId::root(),
            current_context: ContextId::root(),
            callstack: snapshot.callstack,
            recent: VecDeque::new(),
            cycle: snapshot.cycle,
            stopped: false,
        };

        Ok(Self {
            client,
            executor,
            addr: addr.to_string(),
            synced_bp_files: std::collections::BTreeSet::new(),
        })
    }

    fn read_memory(&mut self, expr: &ReadMemoryExpr) -> Result<String, String> {
        self.client.read_memory(expr)
    }

    fn sync_breakpoints(&mut self, breakpoints: &[Breakpoint]) {
        use std::collections::BTreeMap;

        // Group Line breakpoints by their file pattern string.
        let mut by_file: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for bp in breakpoints {
            if let BreakpointType::Line { pattern, line } = &bp.ty {
                by_file.entry(pattern.as_str().to_string()).or_default().push(*line as i64);
            }
        }

        // Send empty breakpoint lists for files that were previously synced but no longer have
        // breakpoints.
        let stale_files: Vec<String> = self
            .synced_bp_files
            .iter()
            .filter(|f| !by_file.contains_key(f.as_str()))
            .cloned()
            .collect();
        for file in &stale_files {
            let _ = self.client.set_breakpoints(file, &[]);
        }

        // Send breakpoints for each file.
        for (file, lines) in &by_file {
            let _ = self.client.set_breakpoints(file, lines);
        }

        // Update tracked set.
        self.synced_bp_files = by_file.into_keys().collect();
    }

    fn resume(&mut self, breakpoints: &[Breakpoint]) -> Result<crate::exec::DapStopReason, String> {
        // Sync user-defined breakpoints to the DAP server before choosing a step command.
        self.sync_breakpoints(breakpoints);

        let has_step = breakpoints.iter().any(|bp| matches!(bp.ty, BreakpointType::Step));
        let has_next = breakpoints.iter().any(|bp| matches!(bp.ty, BreakpointType::Next));
        let has_finish = breakpoints.iter().any(|bp| matches!(bp.ty, BreakpointType::Finish));

        if has_step {
            self.client.step_in()
        } else if has_next {
            self.client.step_over()
        } else if has_finish {
            self.client.step_out()
        } else {
            self.client.continue_()
        }
    }

    fn refresh_executor(
        &mut self,
        source_manager: &Arc<dyn SourceManager>,
        pushed: &crate::exec::DapUiState,
    ) {
        // Standard DAP `stopped` events tell us execution paused, but do not
        // carry the refreshed VM state (stack, callstack, cycle). The server
        // pushes a custom `miden/uiState` event with the bundled snapshot
        // immediately before each `stopped` event, so we consume that here
        // instead of issuing an extra evaluate round-trip.
        let snapshot = convert_ui_state(pushed, source_manager);
        self.executor.current_stack = snapshot.current_stack;
        self.executor.callstack = snapshot.callstack;
        self.executor.cycle = snapshot.cycle;
    }

    fn reconnect(&mut self, source_manager: &Arc<dyn SourceManager>) -> Result<(), Report> {
        let timeout = std::time::Duration::from_secs(30);
        let mut new_client =
            crate::exec::DapClient::connect_with_retry(&self.addr, timeout).map_err(Report::msg)?;
        let ui_state = new_client.handshake().map_err(Report::msg)?;
        let snapshot = convert_ui_state(&ui_state, source_manager);

        self.client = new_client;
        self.executor.current_stack = snapshot.current_stack;
        self.executor.callstack = snapshot.callstack;
        self.executor.cycle = snapshot.cycle;
        Ok(())
    }
}

impl State {
    fn new_local(
        source_manager: Arc<dyn SourceManager>,
        config: Box<DebuggerConfig>,
        debug_mode: DebugMode,
        local: LocalState,
    ) -> Self {
        Self {
            source_manager,
            config,
            input_mode: InputMode::Normal,
            breakpoints: vec![],
            breakpoints_hit: vec![],
            next_breakpoint_id: 0,
            stopped: true,
            debug_mode,
            session: SessionState::Local(Box::new(local)),
        }
    }

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

        Ok(Self::new_local(
            source_manager,
            config,
            DebugMode::Program,
            LocalState {
                executor,
                execution_trace,
                execution_failed: None,
            },
        ))
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

        Ok(Self::new_local(
            source_manager,
            Box::default(),
            DebugMode::Transaction,
            LocalState {
                executor: debug_executor,
                execution_trace,
                execution_failed: None,
            },
        ))
    }

    pub fn reload(&mut self) -> Result<(), Report> {
        if self.debug_mode == DebugMode::Transaction {
            return Err(Report::msg("reload is not supported in transaction debug mode"));
        }
        if self.debug_mode == DebugMode::Remote {
            #[cfg(feature = "dap")]
            {
                let source_manager = self.source_manager.clone();
                let SessionState::Remote(remote) = &mut self.session else {
                    return Err(Report::msg("no remote debug session"));
                };
                let result = remote.client.restart_phase2().map_err(Report::msg)?;
                match result {
                    crate::exec::DapStopReason::Restarting => {
                        remote.reconnect(&source_manager)?;
                    }
                    crate::exec::DapStopReason::Stopped(snapshot) => {
                        // Fallback: server treated it as Phase 1.
                        remote.refresh_executor(&source_manager, &snapshot);
                    }
                    crate::exec::DapStopReason::Terminated => {
                        return Err(Report::msg("server terminated without restart signal"));
                    }
                }
                self.breakpoints_hit.clear();
                self.stopped = true;
                return Ok(());
            }
            #[cfg(not(feature = "dap"))]
            return Err(Report::msg("remote debug mode requires the `dap` feature"));
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

        self.session = SessionState::Local(Box::new(LocalState {
            executor,
            execution_trace,
            execution_failed: None,
        }));
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
        let creation_cycle = self.executor().cycle;
        log::trace!("created breakpoint with id {id} at cycle {creation_cycle}");
        if matches!(ty, BreakpointType::Finish)
            && let Some(frame) = self.executor_mut().callstack.current_frame_mut()
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

    pub fn executor(&self) -> &DebugExecutor {
        match &self.session {
            SessionState::Local(local) => &local.executor,
            #[cfg(feature = "dap")]
            SessionState::Remote(remote) => &remote.executor,
        }
    }

    pub fn executor_mut(&mut self) -> &mut DebugExecutor {
        match &mut self.session {
            SessionState::Local(local) => &mut local.executor,
            #[cfg(feature = "dap")]
            SessionState::Remote(remote) => &mut remote.executor,
        }
    }

    pub fn execution_failed(&self) -> Option<&miden_processor::ExecutionError> {
        match &self.session {
            SessionState::Local(local) => local.execution_failed.as_ref(),
            #[cfg(feature = "dap")]
            SessionState::Remote(_) => None,
        }
    }

    pub fn set_execution_failed(&mut self, error: miden_processor::ExecutionError) {
        match &mut self.session {
            SessionState::Local(local) => local.execution_failed = Some(error),
            #[cfg(feature = "dap")]
            SessionState::Remote(_) => {
                panic!("cannot record local execution failure while in remote mode")
            }
        }
    }

    fn local_session(&self) -> &LocalState {
        match &self.session {
            SessionState::Local(local) => local,
            #[cfg(feature = "dap")]
            SessionState::Remote(_) => panic!("local session requested while in remote mode"),
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
    pub fn read_memory(&mut self, expr: &ReadMemoryExpr) -> Result<String, String> {
        use core::fmt::Write;

        use miden_assembly_syntax::ast::types::Type;

        use crate::debug::FormatType;

        #[cfg(feature = "dap")]
        if self.debug_mode == DebugMode::Remote {
            let SessionState::Remote(remote) = &mut self.session else {
                return Err("no remote debug session".into());
            };
            return remote.read_memory(expr);
        }

        #[cfg(not(feature = "dap"))]
        if self.debug_mode == DebugMode::Remote {
            return Err("remote debug mode requires the `dap` feature".into());
        }

        let cycle = miden_processor::trace::RowIndex::from(self.executor().cycle);
        let context = self.executor().current_context;
        let local = self.local_session();
        let mut output = String::new();
        if expr.count > 1 {
            return Err("-count with value > 1 is not yet implemented".into());
        } else if matches!(expr.ty, Type::Felt) {
            if !expr.addr.is_element_aligned() {
                return Err(
                    "read failed: type 'felt' must be aligned to an element boundary".into()
                );
            }
            let felt = local
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
            let word = local.execution_trace.read_memory_word(expr.addr.addr).unwrap_or_default();
            output.push('[');
            for (i, elem) in word.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                write_with_format_type!(output, expr, elem.as_canonical_u64());
            }
            output.push(']');
        } else {
            let bytes = local
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
        let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
        let remote = RemoteState::connect(addr, &source_manager)?;

        Ok(Self {
            source_manager,
            config: Box::default(),
            input_mode: InputMode::Normal,
            breakpoints: vec![],
            breakpoints_hit: vec![],
            next_breakpoint_id: 0,
            stopped: true,
            debug_mode: DebugMode::Remote,
            session: SessionState::Remote(Box::new(remote)),
        })
    }

    pub fn step_remote(&mut self) -> Result<crate::exec::DapStopReason, Report> {
        let source_manager = self.source_manager.clone();
        let SessionState::Remote(remote) = &mut self.session else {
            return Err(Report::msg("no remote debug session"));
        };
        let result = remote.resume(&self.breakpoints).map_err(Report::msg)?;

        self.breakpoints.retain(|bp| !bp.is_one_shot());

        match &result {
            crate::exec::DapStopReason::Stopped(snapshot) => {
                remote.refresh_executor(&source_manager, snapshot);
                self.stopped = true;
            }
            crate::exec::DapStopReason::Terminated => {
                remote.executor.stopped = true;
                self.stopped = true;
            }
            crate::exec::DapStopReason::Restarting => {
                return Err(Report::msg("unexpected Phase 2 restart signal during step"));
            }
        }

        Ok(result)
    }
}

/// Convert a server-pushed [`DapUiState`](crate::exec::DapUiState) snapshot into a
/// [`RemoteSnapshot`] that the TUI executor can consume.
#[cfg(feature = "dap")]
fn convert_ui_state(
    snapshot: &crate::exec::DapUiState,
    source_manager: &Arc<dyn SourceManager>,
) -> RemoteSnapshot {
    use crate::debug::{CallFrame, CallStack};

    let call_frames: Vec<CallFrame> = snapshot
        .callstack
        .iter()
        .map(|frame| {
            let resolved = resolve_remote_frame(frame, source_manager);
            CallFrame::from_remote(Some(frame.name.clone()), resolved)
        })
        .collect();

    let current_stack = snapshot.current_stack.iter().copied().map(Felt::new).collect();

    RemoteSnapshot {
        callstack: CallStack::from_remote_frames(call_frames),
        current_stack,
        cycle: snapshot.cycle,
    }
}

/// Resolve a remote frame to a [ResolvedLocation] by loading the source file from disk.
#[cfg(feature = "dap")]
fn resolve_remote_frame(
    frame: &crate::exec::DapUiFrame,
    source_manager: &Arc<dyn SourceManager>,
) -> Option<crate::debug::ResolvedLocation> {
    use std::path::Path;

    use miden_debug_types::{SourceManagerExt, SourceSpan};

    let path_str = frame.source_path.as_ref()?;
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
