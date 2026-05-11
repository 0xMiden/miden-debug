use std::{
    io::{BufReader, BufWriter},
    net::TcpListener,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use dap::prelude::*;
use miden_core::{
    Word,
    operations::{AssemblyOp, DebugOptions},
    precompile::PrecompileTranscript,
    program::Program,
};
use miden_processor::{
    ExecutionError, ExecutionOptions, ExecutionOutput, FastProcessor, FutureMaybeSend, Host,
    ProcessorState, ResumeContext, StackInputs, StackOutputs, TraceError,
    advice::{AdviceInputs, AdviceMutation},
    event::EventError,
    mast::MastForest,
    trace::RowIndex,
};

use super::{TraceEvent, state::extract_current_op};
use crate::debug::{FormatType, ReadMemoryExpr};

// DAP CONFIG
// ================================================================================================

static DAP_CONFIG: OnceLock<DapConfig> = OnceLock::new();

/// Configuration for the DAP debug server.
#[derive(Clone, Debug)]
pub struct DapConfig {
    /// The address to listen on, e.g. "127.0.0.1:4711".
    pub listen_addr: String,
    /// Shared flag: set by the server when a Phase 2 restart (terminate-and-reconnect) is
    /// requested, read by the caller to decide whether to recompile and re-enter.
    restart_requested: Arc<AtomicBool>,
}

impl DapConfig {
    pub fn new(listen_addr: impl Into<String>) -> Self {
        Self {
            listen_addr: listen_addr.into(),
            restart_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns `true` if the server has signalled a Phase 2 restart.
    pub fn restart_requested(&self) -> bool {
        self.restart_requested.load(Ordering::Acquire)
    }

    /// Clear the restart flag (called by the caller after acting on it).
    pub fn reset_restart(&self) {
        self.restart_requested.store(false, Ordering::Release);
    }

    /// Set the global DAP configuration. Must be called before `execute_transaction()`.
    ///
    /// Only the first call takes effect; subsequent calls are ignored.
    pub fn set_global(config: DapConfig) {
        DAP_CONFIG.set(config).ok();
    }
}

impl Default for DapConfig {
    fn default() -> Self {
        Self::new("127.0.0.1:4711")
    }
}

// DAP HOST WRAPPER
// ================================================================================================

/// A single frame in the DAP call stack.
#[derive(Debug, Clone)]
struct DapCallFrame {
    name: String,
    source_path: Option<String>,
    line: i64,
    column: i64,
}

/// A host wrapper that intercepts trace events to track the call stack for DAP stack traces,
/// while delegating all other operations to the inner host.
struct DapHostWrapper<'a, H: Host> {
    inner: &'a mut H,
    call_depth: usize,
    frames: Vec<DapCallFrame>,
}

impl<'a, H: Host> DapHostWrapper<'a, H> {
    fn new(inner: &'a mut H) -> Self {
        Self {
            inner,
            call_depth: 0,
            frames: Vec::new(),
        }
    }
}

impl<H: Host> Host for DapHostWrapper<'_, H> {
    fn get_label_and_source_file(
        &self,
        location: &miden_debug_types::Location,
    ) -> (miden_debug_types::SourceSpan, Option<Arc<miden_debug_types::SourceFile>>) {
        self.inner.get_label_and_source_file(location)
    }

    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.inner.on_event(process)
    }

    fn on_debug(
        &mut self,
        process: &ProcessorState<'_>,
        options: &DebugOptions,
    ) -> Result<(), miden_processor::DebugError> {
        self.inner.on_debug(process, options)
    }

    fn on_trace(&mut self, process: &ProcessorState<'_>, trace_id: u32) -> Result<(), TraceError> {
        let event = TraceEvent::from(trace_id);
        match event {
            TraceEvent::FrameStart => {
                self.call_depth += 1;
                self.frames.push(DapCallFrame {
                    name: String::new(),
                    source_path: None,
                    line: 0,
                    column: 0,
                });
            }
            TraceEvent::FrameEnd => {
                self.call_depth = self.call_depth.saturating_sub(1);
                self.frames.pop();
            }
            _ => {}
        }
        self.inner.on_trace(process, trace_id)
    }

    fn resolve_event(
        &self,
        event_id: miden_core::events::EventId,
    ) -> Option<&miden_core::events::EventName> {
        self.inner.resolve_event(event_id)
    }
}

// POLL IMMEDIATELY
// ================================================================================================

/// Resolve a future that is expected to complete immediately (synchronous host methods).
fn poll_immediately<T>(fut: impl std::future::Future<Output = T>) -> T {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(val) => val,
        std::task::Poll::Pending => panic!("future was expected to complete immediately"),
    }
}

macro_rules! write_with_format_type {
    ($out:ident, $read_expr:ident, $value:expr) => {
        match $read_expr.format {
            FormatType::Decimal => write!(&mut $out, "{}", $value).unwrap(),
            FormatType::Hex => write!(&mut $out, "{:0x}", $value).unwrap(),
            FormatType::Binary => write!(&mut $out, "{:0b}", $value).unwrap(),
        }
    };
}

fn read_memory_at_current_state(
    processor: &mut FastProcessor,
    cycle: usize,
    expr: &ReadMemoryExpr,
) -> Result<String, String> {
    use core::fmt::Write;

    use miden_assembly_syntax::ast::types::Type;

    if expr.count > 1 {
        return Err("-count with value > 1 is not yet implemented".into());
    }

    let cycle = RowIndex::from(u32::try_from(cycle).map_err(|_| "cycle value overflowed u32")?);
    let ctx = processor.state().ctx();
    let mut output = String::new();

    if matches!(expr.ty, Type::Felt) {
        if !expr.addr.is_element_aligned() {
            return Err("read failed: type 'felt' must be aligned to an element boundary".into());
        }

        let felt = processor
            .memory()
            .read_element(ctx, miden_processor::Felt::new(u64::from(expr.addr.addr)))
            .ok()
            .unwrap_or(miden_processor::Felt::ZERO);
        write_with_format_type!(output, expr, felt.as_canonical_u64());
        return Ok(output);
    }

    if matches!(
        expr.ty,
        Type::Array(ref array_ty) if array_ty.element_type() == &Type::Felt && array_ty.len() == 4
    ) {
        if !expr.addr.is_word_aligned() {
            return Err("read failed: type 'word' must be aligned to a word boundary".into());
        }

        let word = processor
            .memory()
            .read_word(ctx, miden_processor::Felt::new(u64::from(expr.addr.addr)), cycle)
            .ok()
            .unwrap_or_default();

        output.push('[');
        for (i, elem) in word.iter().enumerate() {
            if i > 0 {
                output.push_str(", ");
            }
            write_with_format_type!(output, expr, elem.as_canonical_u64());
        }
        output.push(']');
        return Ok(output);
    }

    if !expr.addr.is_element_aligned() {
        return Err("invalid read: unaligned reads are not supported yet".into());
    }

    let mut elems = Vec::with_capacity(expr.ty.size_in_felts());
    for i in 0..expr.ty.size_in_felts() {
        let addr = expr
            .addr
            .addr
            .checked_add(u32::try_from(i).map_err(|_| "address overflow")?)
            .ok_or_else(|| {
                "invalid read: attempted to read beyond end of linear memory".to_string()
            })?;
        let felt = processor
            .memory()
            .read_element(ctx, miden_processor::Felt::new(u64::from(addr)))
            .ok()
            .unwrap_or_default();
        elems.push(felt);
    }

    let mut bytes = Vec::with_capacity(expr.ty.size_in_bytes());
    let mut needed = expr.ty.size_in_bytes();
    for elem in elems {
        let elem_bytes = super::query::felt_to_le_bytes(elem);
        let take = core::cmp::min(needed, 4);
        bytes.extend(&elem_bytes[..take]);
        needed -= take;
    }

    match &expr.ty {
        Type::I1 => match expr.format {
            FormatType::Decimal => write!(&mut output, "{}", bytes[0] != 0).unwrap(),
            FormatType::Hex => write!(&mut output, "{:#0x}", (bytes[0] != 0) as u8).unwrap(),
            FormatType::Binary => write!(&mut output, "{:#0b}", (bytes[0] != 0) as u8).unwrap(),
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
                write_with_format_type!(output, expr, val as i64);
            } else {
                write_with_format_type!(output, expr, val);
            }
        }
        ty => return Err(format!("support for reads of type '{ty}' are not implemented yet")),
    }

    Ok(output)
}

fn build_ui_state<H: Host>(
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    cycle: usize,
) -> crate::exec::DapUiState {
    // Build callstack from the host's frame stack (bottom-to-top order, reversed so the
    // top-of-stack / most-recent frame comes first — matching DAP convention).
    let callstack: Vec<crate::exec::DapUiFrame> = if host.frames.is_empty() {
        // Fallback when no frames have been recorded yet.
        let (name, source_path, line) = match current_asmop {
            Some(asmop) => {
                let loc = resolve_asmop_location(asmop, host);
                let (source_path, line) = loc.map_or((None, 0), |(path, line)| (Some(path), line));
                (asmop.context_name().to_string(), source_path, line)
            }
            None => (format!("cycle {cycle}"), None, 0),
        };
        vec![crate::exec::DapUiFrame {
            name,
            source_path,
            line,
            column: 0,
        }]
    } else {
        host.frames
            .iter()
            .rev()
            .map(|frame| crate::exec::DapUiFrame {
                name: frame.name.clone(),
                source_path: frame.source_path.clone(),
                line: frame.line,
                column: frame.column,
            })
            .collect()
    };

    let current_stack = processor
        .state()
        .get_stack_state()
        .iter()
        .map(|felt| felt.as_canonical_u64())
        .collect();

    crate::exec::DapUiState {
        cycle,
        current_stack,
        callstack,
    }
}

// BREAKPOINT STORAGE
// ================================================================================================

/// A source breakpoint stored from a SetBreakpoints request.
#[derive(Debug, Clone)]
struct StoredBreakpoint {
    /// The source file path as sent by the client.
    path: String,
    /// 1-indexed line number.
    line: i64,
}

/// A function/pattern breakpoint stored from a SetFunctionBreakpoints request.
///
/// Matching is done in two ways:
/// 1. Glob pattern match against the full context name and source path
/// 2. Suffix match — the raw name is checked as a suffix of the context name
///    (e.g. `prologue::foo` matches `::$kernel::prologue::foo`)
#[derive(Debug, Clone)]
struct StoredFunctionBreakpoint {
    /// The raw name string for suffix matching.
    name: String,
    /// Compiled glob pattern for matching.
    pattern: glob::Pattern,
}

// RESOLVE ASMOP LOCATION
// ================================================================================================

/// Resolve an `AssemblyOp`'s location to a (file_path, line_number) pair using the host.
fn resolve_asmop_location<H: Host>(asmop: &AssemblyOp, host: &H) -> Option<(String, i64)> {
    let location = asmop.location()?;
    let (span, source_file) = host.get_label_and_source_file(location);
    let source_file = source_file?;
    let file_line_col = source_file.location(span);
    let path = file_line_col.uri.as_ref().to_string();
    let line = file_line_col.line.to_u32() as i64;
    Some((path, line))
}

/// Update the top frame on the host's frame stack with the current asmop's name and location.
///
/// If the frame stack is empty (e.g. before the first FrameStart trace event), a root frame
/// is pushed so there is always at least one frame visible in the stack trace.
fn update_top_frame<H: Host>(host: &mut DapHostWrapper<'_, H>, current_asmop: Option<&AssemblyOp>) {
    let (name, source_path, line) = match current_asmop {
        Some(asmop) => {
            let loc = resolve_asmop_location(asmop, &*host);
            let (source_path, line) = loc.map_or((None, 0), |(p, l)| (Some(p), l));
            (asmop.context_name().to_string(), source_path, line)
        }
        None => (String::new(), None, 0),
    };

    if host.frames.is_empty() {
        host.frames.push(DapCallFrame {
            name,
            source_path,
            line,
            column: 0,
        });
    } else if let Some(top) = host.frames.last_mut() {
        top.name = name;
        top.source_path = source_path;
        top.line = line;
    }
}

// DAP EXECUTOR
// ================================================================================================

/// A [`ProgramExecutor`] that starts a DAP TCP server and steps through the program
/// interactively.
///
/// When used with `TransactionExecutor`, instead of running to completion, `execute()` binds a TCP
/// listener and waits for a DAP client (e.g. VS Code) to connect. The client then controls
/// execution via standard DAP commands (continue, step, breakpoints, inspect stack/memory).
pub struct DapExecutor {
    stack_inputs: StackInputs,
    advice_inputs: AdviceInputs,
    options: ExecutionOptions,
    config: DapConfig,
}

/// Variables reference IDs for scopes.
const SCOPE_STACK: i64 = 1;
const SCOPE_MEMORY: i64 = 2;

impl DapExecutor {
    pub fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        let config = DAP_CONFIG.get().cloned().unwrap_or_default();
        DapExecutor {
            stack_inputs,
            advice_inputs,
            options,
            config,
        }
    }

    pub fn execute_async<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move { self.run_dap_server(program, host) }
    }
}

impl DapExecutor {
    fn run_dap_server<H: Host>(
        self,
        program: &Program,
        host: &mut H,
    ) -> Result<ExecutionOutput, ExecutionError> {
        // Clone inputs so they can be reused across restarts.
        let stack_inputs = self.stack_inputs;
        let advice_inputs = self.advice_inputs;
        let options = self.options.with_debugging(true).with_tracing(true);

        // Bind TCP listener with SO_REUSEADDR to allow rebinding during Phase 2 restarts.
        let listener = {
            use std::net::ToSocketAddrs;

            use socket2::{Domain, Socket, Type};
            let addr: std::net::SocketAddr = self
                .config
                .listen_addr
                .to_socket_addrs()
                .unwrap_or_else(|e| {
                    panic!("invalid listen address '{}': {e}", self.config.listen_addr)
                })
                .next()
                .unwrap_or_else(|| {
                    panic!(
                        "listen address '{}' did not resolve to any address",
                        self.config.listen_addr
                    )
                });
            let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)
                .unwrap_or_else(|e| panic!("failed to create socket: {e}"));
            socket.set_reuse_address(true).ok();
            socket
                .bind(&addr.into())
                .unwrap_or_else(|e| panic!("DAP server failed to bind to {addr}: {e}"));
            socket
                .listen(1)
                .unwrap_or_else(|e| panic!("DAP server failed to listen on {addr}: {e}"));
            let listener: TcpListener = socket.into();
            listener
        };
        eprintln!(
            "DAP server listening on {}. Waiting for client connection...",
            self.config.listen_addr
        );

        // Accept one client connection (persists across restarts).
        let (stream, addr) =
            listener.accept().unwrap_or_else(|e| panic!("DAP server accept failed: {e}"));
        eprintln!("DAP client connected from {addr}");

        let reader = BufReader::new(
            stream.try_clone().unwrap_or_else(|e| panic!("Failed to clone TCP stream: {e}")),
        );
        let writer = BufWriter::new(stream);

        let mut server = Server::new(reader, writer);
        let mut breakpoints: Vec<StoredBreakpoint> = Vec::new();
        let mut function_breakpoints: Vec<StoredFunctionBreakpoint> = Vec::new();
        let mut is_restart = false;
        // VS Code's flow waits for `configurationDone` before stopping the VM at
        // the entry point; Zed's flow never sends `configurationDone`. Track
        // whether the entry stop has already been announced so it fires
        // exactly once regardless of which client we're talking to.
        let mut entry_announced = false;
        let restart_flag = self.config.restart_requested.clone();

        // Outer restart loop — on restart, the DapHostWrapper borrow is dropped,
        // a fresh FastProcessor is created, and the inner event loop re-enters.
        //
        // NOTE: The inner host `H` (e.g. from miden-client's transaction executor)
        // retains mutations from prior execution. A restarted session may therefore
        // diverge from a pristine fresh session. Phase 2 (terminate-and-reconnect)
        // solves this by creating a completely fresh host.
        loop {
            let mut processor = FastProcessor::new(stack_inputs)
                .with_advice(advice_inputs.clone())
                .with_options(options);

            let resume_ctx = processor.get_initial_resume_context(program)?;
            let mut wrapper = DapHostWrapper::new(host);

            let mut resume_ctx = Some(resume_ctx);
            let mut cycle: usize = 0;
            let mut current_asmop: Option<AssemblyOp> = None;

            // Extract initial asmop and populate the root frame.
            if let Some(ctx) = resume_ctx.as_ref() {
                let (_op, node_id, op_idx, _control) = extract_current_op(ctx);
                current_asmop = node_id
                    .and_then(|nid| ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
            }
            update_top_frame(&mut wrapper, current_asmop.as_ref());

            // On restart, emit initial state immediately (no handshake needed).
            if is_restart {
                send_ui_state_snapshot(
                    &mut server,
                    &mut processor,
                    &wrapper,
                    current_asmop.as_ref(),
                    cycle,
                );
                server
                    .send_event(Event::Stopped(events::StoppedEventBody {
                        reason: types::StoppedEventReason::Entry,
                        description: Some("Restarted at program entry".into()),
                        thread_id: Some(1),
                        preserve_focus_hint: None,
                        text: None,
                        all_threads_stopped: Some(true),
                        hit_breakpoint_ids: None,
                    }))
                    .ok();
            }

            let mut restart_requested = false;
            let mut phase2_requested = false;

            loop {
                let req = match server.poll_request() {
                    Ok(Some(req)) => req,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("DAP protocol error: {e:#?}");
                        break;
                    }
                };

                match req.command {
                    // --- Handshake ---
                    Command::Initialize(_) => {
                        let caps = types::Capabilities {
                            supports_configuration_done_request: Some(true),
                            supports_stepping_granularity: Some(true),
                            supports_restart_request: Some(true),
                            supports_function_breakpoints: Some(true),
                            ..Default::default()
                        };
                        let resp = req.success(ResponseBody::Initialize(caps));
                        server.respond(resp).ok();
                        server.send_event(Event::Initialized).ok();
                    }

                    Command::Launch(_) => {
                        server.respond(req.success(ResponseBody::Launch)).ok();
                        announce_entry_stop(
                            &mut server,
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            cycle,
                            &mut entry_announced,
                        );
                    }

                    // VS Code's default flow issues `attach` when launch.json uses
                    // `request: "attach"`. The server is already bound to a TCP port and
                    // running against a compiled script, so there is nothing to do beyond
                    // acknowledging the request — mirror the `Launch` handling.
                    Command::Attach(_) => {
                        server.respond(req.success(ResponseBody::Attach)).ok();
                        // Zed never sends `configurationDone`, so announce the entry
                        // stop here as well. The `entry_announced` guard makes this
                        // a no-op when `configurationDone` later fires (VS Code path).
                        announce_entry_stop(
                            &mut server,
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            cycle,
                            &mut entry_announced,
                        );
                    }

                    Command::ConfigurationDone => {
                        if let Ok(resp) = req.ack() {
                            server.respond(resp).ok();
                        }
                        announce_entry_stop(
                            &mut server,
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            cycle,
                            &mut entry_announced,
                        );
                    }

                    Command::Restart(ref args) => {
                        // `args.is_some()` means the client sent a non-null
                        // `RestartArguments` envelope; `arguments.arguments.is_some()`
                        // means that envelope carries an explicit launch/attach payload.
                        // Only the latter signals "Phase 2: recompile from disk".
                        let has_arguments = args
                            .as_ref()
                            .and_then(|a| a.arguments.as_ref())
                            .is_some();
                        server.respond(req.success(ResponseBody::Restart)).ok();
                        if has_arguments {
                            // Phase 2: terminate-and-reconnect for recompilation.
                            restart_flag.store(true, Ordering::Release);
                            server
                                .send_event(Event::Terminated(Some(events::TerminatedEventBody {
                                    restart: Some(serde_json::Value::Bool(true)),
                                })))
                                .ok();
                            phase2_requested = true;
                            break;
                        } else {
                            // Phase 1: in-process reset (same program, same host).
                            restart_requested = true;
                            break;
                        }
                    }

                    Command::Disconnect(_) => {
                        if let Ok(resp) = req.ack() {
                            server.respond(resp).ok();
                        }
                        break;
                    }

                    // --- Execution Control ---
                    Command::Continue(_) => {
                        let resp =
                            req.success(ResponseBody::Continue(responses::ContinueResponse {
                                all_threads_continued: Some(true),
                            }));
                        server.respond(resp).ok();

                        match step_until_breakpoint(
                            &mut processor,
                            &mut wrapper,
                            &mut resume_ctx,
                            &mut cycle,
                            &mut current_asmop,
                            &breakpoints,
                            &function_breakpoints,
                        ) {
                            StepResult::Stepped | StepResult::Breakpoint(_) => {
                                send_ui_state_snapshot(
                                    &mut server,
                                    &mut processor,
                                    &wrapper,
                                    current_asmop.as_ref(),
                                    cycle,
                                );
                                server
                                    .send_event(Event::Stopped(events::StoppedEventBody {
                                        reason: types::StoppedEventReason::Breakpoint,
                                        description: Some("Hit breakpoint".into()),
                                        thread_id: Some(1),
                                        preserve_focus_hint: None,
                                        text: None,
                                        all_threads_stopped: Some(true),
                                        hit_breakpoint_ids: None,
                                    }))
                                    .ok();
                            }
                            StepResult::Terminated => {
                                server.send_event(Event::Terminated(None)).ok();
                            }
                            StepResult::Error(e) => {
                                server.send_event(Event::Terminated(None)).ok();
                                return Err(e);
                            }
                        }
                    }

                    Command::Next(_) => {
                        let resp = req.success(ResponseBody::Next);
                        server.respond(resp).ok();

                        match step_over(
                            &mut processor,
                            &mut wrapper,
                            &mut resume_ctx,
                            &mut cycle,
                            &mut current_asmop,
                        ) {
                            StepResult::Stepped | StepResult::Breakpoint(_) => {
                                if resume_ctx.is_none() {
                                    server.send_event(Event::Terminated(None)).ok();
                                } else {
                                    send_ui_state_snapshot(
                                        &mut server,
                                        &mut processor,
                                        &wrapper,
                                        current_asmop.as_ref(),
                                        cycle,
                                    );
                                    send_stopped_step(&mut server);
                                }
                            }
                            StepResult::Terminated => {
                                server.send_event(Event::Terminated(None)).ok();
                            }
                            StepResult::Error(e) => {
                                server.send_event(Event::Terminated(None)).ok();
                                return Err(e);
                            }
                        }
                    }

                    Command::StepIn(_) => {
                        let resp = req.success(ResponseBody::StepIn);
                        server.respond(resp).ok();

                        match step_one(
                            &mut processor,
                            &mut wrapper,
                            &mut resume_ctx,
                            &mut cycle,
                            &mut current_asmop,
                        ) {
                            StepResult::Stepped | StepResult::Breakpoint(_) => {
                                if resume_ctx.is_none() {
                                    server.send_event(Event::Terminated(None)).ok();
                                } else {
                                    send_ui_state_snapshot(
                                        &mut server,
                                        &mut processor,
                                        &wrapper,
                                        current_asmop.as_ref(),
                                        cycle,
                                    );
                                    send_stopped_step(&mut server);
                                }
                            }
                            StepResult::Terminated => {
                                server.send_event(Event::Terminated(None)).ok();
                            }
                            StepResult::Error(e) => {
                                server.send_event(Event::Terminated(None)).ok();
                                return Err(e);
                            }
                        }
                    }

                    Command::StepOut(_) => {
                        let resp = req.success(ResponseBody::StepOut);
                        server.respond(resp).ok();

                        match step_out(
                            &mut processor,
                            &mut wrapper,
                            &mut resume_ctx,
                            &mut cycle,
                            &mut current_asmop,
                        ) {
                            StepResult::Stepped | StepResult::Breakpoint(_) => {
                                if resume_ctx.is_none() {
                                    server.send_event(Event::Terminated(None)).ok();
                                } else {
                                    send_ui_state_snapshot(
                                        &mut server,
                                        &mut processor,
                                        &wrapper,
                                        current_asmop.as_ref(),
                                        cycle,
                                    );
                                    send_stopped_step(&mut server);
                                }
                            }
                            StepResult::Terminated => {
                                server.send_event(Event::Terminated(None)).ok();
                            }
                            StepResult::Error(e) => {
                                server.send_event(Event::Terminated(None)).ok();
                                return Err(e);
                            }
                        }
                    }

                    // --- State Inspection ---
                    Command::Threads => {
                        let resp = req.success(ResponseBody::Threads(responses::ThreadsResponse {
                            threads: vec![types::Thread {
                                id: 1,
                                name: "main".into(),
                            }],
                        }));
                        server.respond(resp).ok();
                    }

                    Command::StackTrace(ref _args) => {
                        // Build stack frames from the host's frame stack (reversed: top-of-stack
                        // first, matching DAP convention).
                        let frames: Vec<types::StackFrame> = if wrapper.frames.is_empty() {
                            // Fallback when no frames have been recorded.
                            let (name, source, line) = if let Some(asmop) = current_asmop.as_ref() {
                                let loc = resolve_asmop_location(asmop, &wrapper);
                                let (path, line_num) =
                                    loc.unwrap_or_else(|| ("<unknown>".into(), 0));
                                let source = types::Source {
                                    name: Some(
                                        path.rsplit('/').next().unwrap_or(&path).to_string(),
                                    ),
                                    path: Some(path),
                                    ..Default::default()
                                };
                                (asmop.context_name().to_string(), Some(source), line_num)
                            } else {
                                (format!("cycle {cycle}"), None, 0)
                            };
                            vec![types::StackFrame {
                                id: 0,
                                name,
                                source,
                                line,
                                column: 0,
                                ..Default::default()
                            }]
                        } else {
                            wrapper
                                .frames
                                .iter()
                                .rev()
                                .enumerate()
                                .map(|(id, frame)| {
                                    let source =
                                        frame.source_path.as_ref().map(|path| types::Source {
                                            name: Some(
                                                path.rsplit('/').next().unwrap_or(path).to_string(),
                                            ),
                                            path: Some(path.clone()),
                                            ..Default::default()
                                        });
                                    types::StackFrame {
                                        id: id as i64,
                                        name: frame.name.clone(),
                                        source,
                                        line: frame.line,
                                        column: frame.column,
                                        ..Default::default()
                                    }
                                })
                                .collect()
                        };

                        let total = frames.len() as i64;
                        let resp =
                            req.success(ResponseBody::StackTrace(responses::StackTraceResponse {
                                stack_frames: frames,
                                total_frames: Some(total),
                            }));
                        server.respond(resp).ok();
                    }

                    Command::Scopes(ref _args) => {
                        let resp = req.success(ResponseBody::Scopes(responses::ScopesResponse {
                            scopes: vec![
                                types::Scope {
                                    name: "Operand Stack".into(),
                                    variables_reference: SCOPE_STACK,
                                    expensive: false,
                                    ..Default::default()
                                },
                                types::Scope {
                                    name: "Memory".into(),
                                    variables_reference: SCOPE_MEMORY,
                                    expensive: false,
                                    ..Default::default()
                                },
                            ],
                        }));
                        server.respond(resp).ok();
                    }

                    Command::Variables(ref args) => {
                        let variables = match args.variables_reference {
                            SCOPE_STACK => {
                                let state = processor.state();
                                let stack = state.get_stack_state();
                                stack
                                    .iter()
                                    .enumerate()
                                    .map(|(i, felt)| types::Variable {
                                        name: format!("[{i}]"),
                                        value: format!("{}", felt.as_canonical_u64()),
                                        type_field: Some("Felt".into()),
                                        variables_reference: 0,
                                        ..Default::default()
                                    })
                                    .collect()
                            }
                            SCOPE_MEMORY => {
                                let state = processor.state();
                                let ctx = state.ctx();
                                let mem = state.get_mem_state(ctx);
                                mem.iter()
                                    .map(|(addr, felt)| {
                                        let addr_u32: u32 = (*addr).into();
                                        types::Variable {
                                            name: format!("0x{addr_u32:08x}"),
                                            value: format!("{}", felt.as_canonical_u64()),
                                            type_field: Some("Felt".into()),
                                            variables_reference: 0,
                                            ..Default::default()
                                        }
                                    })
                                    .collect()
                            }
                            _ => Vec::new(),
                        };

                        let resp =
                            req.success(ResponseBody::Variables(responses::VariablesResponse {
                                variables,
                            }));
                        server.respond(resp).ok();
                    }

                    // --- Breakpoints ---
                    Command::SetBreakpoints(ref args) => {
                        let source_path = args.source.path.clone().unwrap_or_default();
                        breakpoints.retain(|bp| bp.path != source_path);

                        let mut confirmed = Vec::new();
                        if let Some(bps) = &args.breakpoints {
                            for sbp in bps {
                                breakpoints.push(StoredBreakpoint {
                                    path: source_path.clone(),
                                    line: sbp.line,
                                });
                                confirmed.push(types::Breakpoint {
                                    verified: true,
                                    line: Some(sbp.line),
                                    source: Some(types::Source {
                                        path: Some(source_path.clone()),
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                });
                            }
                        }

                        let resp = req.success(ResponseBody::SetBreakpoints(
                            responses::SetBreakpointsResponse {
                                breakpoints: confirmed,
                            },
                        ));
                        server.respond(resp).ok();
                    }

                    Command::SetFunctionBreakpoints(ref args) => {
                        function_breakpoints.clear();
                        let mut confirmed = Vec::new();
                        for fbp in &args.breakpoints {
                            let verified = match glob::Pattern::new(&fbp.name) {
                                Ok(pattern) => {
                                    function_breakpoints.push(StoredFunctionBreakpoint {
                                        name: fbp.name.clone(),
                                        pattern,
                                    });
                                    true
                                }
                                Err(_) => false,
                            };
                            confirmed.push(types::Breakpoint {
                                verified,
                                ..Default::default()
                            });
                        }

                        let resp = req.success(ResponseBody::SetFunctionBreakpoints(
                            responses::SetFunctionBreakpointsResponse {
                                breakpoints: confirmed,
                            },
                        ));
                        server.respond(resp).ok();
                    }

                    // --- Evaluate (custom state query) ---
                    Command::Evaluate(ref args) if args.expression == "__miden_ui_state" => {
                        let state_json = serde_json::to_string(&build_ui_state(
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            cycle,
                        ))
                        .expect("bundled DAP UI state should serialize");
                        let resp =
                            req.success(ResponseBody::Evaluate(responses::EvaluateResponse {
                                result: state_json,
                                type_field: Some("json".into()),
                                presentation_hint: None,
                                variables_reference: 0,
                                named_variables: None,
                                indexed_variables: None,
                                memory_reference: None,
                            }));
                        server.respond(resp).ok();
                    }

                    Command::Evaluate(ref args)
                        if args.expression.starts_with("__miden_read_memory ") =>
                    {
                        let expr = args
                            .expression
                            .strip_prefix("__miden_read_memory ")
                            .expect("prefix checked above");

                        match expr.parse::<ReadMemoryExpr>().and_then(|expr| {
                            read_memory_at_current_state(&mut processor, cycle, &expr)
                        }) {
                            Ok(result) => {
                                let resp = req.success(ResponseBody::Evaluate(
                                    responses::EvaluateResponse {
                                        result,
                                        type_field: Some("string".into()),
                                        presentation_hint: None,
                                        variables_reference: 0,
                                        named_variables: None,
                                        indexed_variables: None,
                                        memory_reference: None,
                                    },
                                ));
                                server.respond(resp).ok();
                            }
                            Err(err) => {
                                server.respond(req.error(&err)).ok();
                            }
                        }
                    }

                    Command::Evaluate(ref _args) => {
                        server.respond(req.error("Unsupported expression")).ok();
                    }

                    // --- Unhandled ---
                    _ => {
                        server.respond(req.error("Unsupported command")).ok();
                    }
                }
            }

            if phase2_requested {
                eprintln!("DAP Phase 2 restart: returning from execute() for recompilation...");
                return Ok(ExecutionOutput {
                    stack: StackOutputs::new(&[]).expect("empty stack outputs"),
                    advice: Default::default(),
                    memory: Default::default(),
                    final_pc_transcript: PrecompileTranscript::default(),
                });
            }

            if restart_requested {
                is_restart = true;
                eprintln!("DAP restart requested. Resetting processor...");
                // wrapper is dropped here, releasing the &mut H borrow.
                // The outer loop creates a fresh processor and wrapper.
                continue;
            }

            // Normal exit (disconnect or connection closed).
            eprintln!("DAP session ended. Building execution output...");

            // Run the program to completion if it hasn't finished.
            if let Some(ctx) = resume_ctx {
                let mut ctx = Some(ctx);
                while let Some(resume) = ctx.take() {
                    match poll_immediately(processor.step(&mut wrapper, resume)) {
                        Ok(Some(new_ctx)) => {
                            ctx = Some(new_ctx);
                        }
                        Ok(None) => break,
                        Err(e) => return Err(e),
                    }
                }
            }

            // Build ExecutionOutput from the processor's final state.
            let stack_top: Vec<_> = processor.stack_top().iter().rev().copied().collect();
            let stack = StackOutputs::new(&stack_top)
                .unwrap_or_else(|_| StackOutputs::new(&[]).expect("empty stack outputs"));

            let (advice, memory, final_pc_transcript) = processor.into_parts();
            return Ok(ExecutionOutput {
                stack,
                advice,
                memory,
                final_pc_transcript,
            });
        } // end outer restart loop
    }
}

// STEPPING HELPERS
// ================================================================================================

/// Execute a single VM step.
fn step_one<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
) -> StepResult {
    let ctx = match resume_ctx.take() {
        Some(ctx) => ctx,
        None => return StepResult::Terminated,
    };

    match poll_immediately(processor.step(host, ctx)) {
        Ok(Some(new_ctx)) => {
            *cycle += 1;
            let (_op, node_id, op_idx, _control) = extract_current_op(&new_ctx);
            *current_asmop = node_id
                .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
            *resume_ctx = Some(new_ctx);
            update_top_frame(host, current_asmop.as_ref());
            StepResult::Stepped
        }
        Ok(None) => {
            *cycle += 1;
            *current_asmop = None;
            StepResult::Terminated
        }
        Err(e) => StepResult::Error(e),
    }
}

/// Step over: advance until the current assembly operation changes.
fn step_over<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
) -> StepResult {
    let original_asmop = current_asmop.clone();

    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match poll_immediately(processor.step(host, ctx)) {
            Ok(Some(new_ctx)) => {
                *cycle += 1;
                let (_op, node_id, op_idx, _control) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                if *current_asmop != original_asmop {
                    update_top_frame(host, current_asmop.as_ref());
                    return StepResult::Stepped;
                }
            }
            Ok(None) => {
                *cycle += 1;
                *current_asmop = None;
                return StepResult::Terminated;
            }
            Err(e) => return StepResult::Error(e),
        }
    }
}

/// Step out: advance until call depth decreases.
fn step_out<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
) -> StepResult {
    let target_depth = host.call_depth.saturating_sub(1);

    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match poll_immediately(processor.step(host, ctx)) {
            Ok(Some(new_ctx)) => {
                *cycle += 1;
                let (_op, node_id, op_idx, _control) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                if host.call_depth <= target_depth {
                    update_top_frame(host, current_asmop.as_ref());
                    return StepResult::Stepped;
                }
            }
            Ok(None) => {
                *cycle += 1;
                *current_asmop = None;
                return StepResult::Terminated;
            }
            Err(e) => return StepResult::Error(e),
        }
    }
}

/// Continue: step until a breakpoint is hit or the program terminates.
fn step_until_breakpoint<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
    breakpoints: &[StoredBreakpoint],
    function_breakpoints: &[StoredFunctionBreakpoint],
) -> StepResult {
    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match poll_immediately(processor.step(host, ctx)) {
            Ok(Some(new_ctx)) => {
                *cycle += 1;
                let (_op, node_id, op_idx, _control) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                if let Some(asmop) = current_asmop.as_ref() {
                    let resolved = resolve_asmop_location(asmop, host);

                    // Check line breakpoints
                    if let Some((ref path, line)) = resolved {
                        for bp in breakpoints {
                            if bp.line == line
                                && (path.ends_with(&bp.path) || bp.path.ends_with(path))
                            {
                                update_top_frame(host, current_asmop.as_ref());
                                return StepResult::Breakpoint(line);
                            }
                        }
                    }

                    // Check function/pattern breakpoints — match against context name and
                    // source file path. Context names may have a leading `::` (absolute
                    // paths like `::prologue::foo`), so we also try matching without it.
                    if !function_breakpoints.is_empty() {
                        let context_name = asmop.context_name();
                        let stripped_name = context_name.strip_prefix("::").unwrap_or(context_name);
                        for fbp in function_breakpoints {
                            // Match via glob pattern or suffix (e.g. "prologue::foo"
                            // matches "::$kernel::prologue::foo").
                            if fbp.pattern.matches(context_name)
                                || fbp.pattern.matches(stripped_name)
                                || context_name.ends_with(&fbp.name)
                                || stripped_name.ends_with(&fbp.name)
                            {
                                update_top_frame(host, current_asmop.as_ref());
                                let line = resolved.as_ref().map_or(0, |(_, l)| *l);
                                return StepResult::Breakpoint(line);
                            }
                            if let Some((ref path, line)) = resolved
                                && fbp.pattern.matches(path)
                            {
                                update_top_frame(host, current_asmop.as_ref());
                                return StepResult::Breakpoint(line);
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                *cycle += 1;
                *current_asmop = None;
                return StepResult::Terminated;
            }
            Err(e) => return StepResult::Error(e),
        }
    }
}

/// Push a `miden/uiState` snapshot event to the DAP client.
///
/// This custom event carries the bundled UI state (cycle, stack, callstack) so
/// the client can refresh without an extra evaluate round-trip. It is emitted
/// immediately before the standard `stopped` event because the DAP `stopped`
/// event itself does not carry VM state — it only signals that execution paused.
fn send_ui_state_snapshot<R: std::io::Read, W: std::io::Write, H: Host>(
    server: &mut Server<R, W>,
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    cycle: usize,
) {
    let ui_state = build_ui_state(processor, host, current_asmop, cycle);
    if let Ok(json) = serde_json::to_value(&ui_state) {
        server.send_event(Event::MidenUiState(json)).ok();
    }
}

/// Announce the entry-point stop to the DAP client (UI snapshot + `Stopped(entry)`),
/// flipping `already_announced` so subsequent calls become no-ops.
///
/// Called from both `Attach`/`Launch` (so clients like Zed that skip
/// `configurationDone` still see the initial stop) and from `ConfigurationDone`
/// (the VS Code path).
fn announce_entry_stop<R: std::io::Read, W: std::io::Write, H: Host>(
    server: &mut Server<R, W>,
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    cycle: usize,
    already_announced: &mut bool,
) {
    if *already_announced {
        return;
    }
    *already_announced = true;
    send_ui_state_snapshot(server, processor, host, current_asmop, cycle);
    server
        .send_event(Event::Stopped(events::StoppedEventBody {
            reason: types::StoppedEventReason::Entry,
            description: Some("Paused at program entry".into()),
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        }))
        .ok();
}

/// Send a "stopped(step)" event to the DAP client.
fn send_stopped_step<R: std::io::Read, W: std::io::Write>(server: &mut Server<R, W>) {
    server
        .send_event(Event::Stopped(events::StoppedEventBody {
            reason: types::StoppedEventReason::Step,
            description: None,
            thread_id: Some(1),
            preserve_focus_hint: None,
            text: None,
            all_threads_stopped: Some(true),
            hit_breakpoint_ids: None,
        }))
        .ok();
}

/// Result of a stepping operation.
#[allow(dead_code)]
enum StepResult {
    /// Successfully advanced one or more steps.
    Stepped,
    /// Hit a breakpoint at the given line.
    Breakpoint(i64),
    /// Program terminated normally.
    Terminated,
    /// An execution error occurred.
    Error(ExecutionError),
}
