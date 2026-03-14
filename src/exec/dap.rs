use std::{
    io::{BufReader, BufWriter},
    net::TcpListener,
    sync::{Arc, OnceLock},
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

use super::{ProgramExecutor, TraceEvent, state::extract_current_op};
use crate::debug::{FormatType, ReadMemoryExpr};

// DAP CONFIG
// ================================================================================================

static DAP_CONFIG: OnceLock<DapConfig> = OnceLock::new();

/// Configuration for the DAP debug server.
#[derive(Clone, Debug)]
pub struct DapConfig {
    /// The address to listen on, e.g. "127.0.0.1:4711".
    pub listen_addr: String,
}

impl Default for DapConfig {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:4711".into(),
        }
    }
}

impl DapConfig {
    /// Set the global DAP configuration. Must be called before `execute_transaction()`.
    ///
    /// Only the first call takes effect; subsequent calls are ignored.
    pub fn set_global(config: DapConfig) {
        DAP_CONFIG.set(config).ok();
    }
}

// DAP HOST WRAPPER
// ================================================================================================

/// A host wrapper that intercepts trace events to track call depth for DAP stack traces,
/// while delegating all other operations to the inner host.
struct DapHostWrapper<'a, H: Host> {
    inner: &'a mut H,
    call_depth: usize,
}

impl<'a, H: Host> DapHostWrapper<'a, H> {
    fn new(inner: &'a mut H) -> Self {
        Self {
            inner,
            call_depth: 0,
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
            TraceEvent::FrameStart => self.call_depth += 1,
            TraceEvent::FrameEnd => self.call_depth = self.call_depth.saturating_sub(1),
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

    const U32_MASK: u64 = u32::MAX as u64;

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
        let elem_bytes = ((elem.as_canonical_u64() & U32_MASK) as u32).to_be_bytes();
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
            let val = (hi << 32) | lo;
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

impl ProgramExecutor for DapExecutor {
    fn new(
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

    fn execute<H: Host + Send>(
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
        // 1. Create FastProcessor with debugging and tracing enabled
        let options = self.options.with_debugging(true).with_tracing(true);
        let mut processor = FastProcessor::new(self.stack_inputs)
            .with_advice(self.advice_inputs)
            .with_options(options);

        // 2. Get initial resume context
        let resume_ctx = processor.get_initial_resume_context(program)?;

        // 3. Wrap host with DapHostWrapper for call depth tracking
        let mut wrapper = DapHostWrapper::new(host);

        // 4. Bind TCP listener
        let listener = TcpListener::bind(&self.config.listen_addr).unwrap_or_else(|e| {
            panic!("DAP server failed to bind to {}: {e}", self.config.listen_addr)
        });
        eprintln!(
            "DAP server listening on {}. Waiting for client connection...",
            self.config.listen_addr
        );

        // 5. Accept one client connection
        let (stream, addr) =
            listener.accept().unwrap_or_else(|e| panic!("DAP server accept failed: {e}"));
        eprintln!("DAP client connected from {addr}");

        let reader = BufReader::new(
            stream.try_clone().unwrap_or_else(|e| panic!("Failed to clone TCP stream: {e}")),
        );
        let writer = BufWriter::new(stream);

        // 6. Create DAP server
        let mut server = Server::new(reader, writer);

        // 7. Enter DAP event loop
        let mut resume_ctx = Some(resume_ctx);
        let mut breakpoints: Vec<StoredBreakpoint> = Vec::new();
        let mut cycle: usize = 0;
        let mut current_asmop: Option<AssemblyOp> = None;

        // Extract initial asmop
        if let Some(ctx) = resume_ctx.as_ref() {
            let (_op, node_id, op_idx) = extract_current_op(ctx);
            current_asmop =
                node_id.and_then(|nid| ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
        }

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
                        ..Default::default()
                    };
                    let resp = req.success(ResponseBody::Initialize(caps));
                    server.respond(resp).ok();
                    server.send_event(Event::Initialized).ok();
                }

                Command::Launch(_) => {
                    server.respond(req.success(ResponseBody::Launch)).ok();
                }

                Command::ConfigurationDone => {
                    if let Ok(resp) = req.ack() {
                        server.respond(resp).ok();
                    }
                    // Pause at entry point
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

                Command::Disconnect(_) => {
                    if let Ok(resp) = req.ack() {
                        server.respond(resp).ok();
                    }
                    break;
                }

                // --- Execution Control ---
                Command::Continue(_) => {
                    let resp = req.success(ResponseBody::Continue(responses::ContinueResponse {
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
                    ) {
                        StepResult::Stepped | StepResult::Breakpoint(_) => {
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
                    let mut frames = Vec::new();

                    let (name, source, line) = if let Some(asmop) = current_asmop.as_ref() {
                        let loc = resolve_asmop_location(asmop, &wrapper);
                        let (path, line_num) = loc.unwrap_or_else(|| ("<unknown>".into(), 0));
                        let source = types::Source {
                            name: Some(path.rsplit('/').next().unwrap_or(&path).to_string()),
                            path: Some(path),
                            ..Default::default()
                        };
                        (asmop.context_name().to_string(), Some(source), line_num)
                    } else {
                        (format!("cycle {cycle}"), None, 0)
                    };

                    frames.push(types::StackFrame {
                        id: 0,
                        name,
                        source,
                        line,
                        column: 0,
                        ..Default::default()
                    });

                    let resp =
                        req.success(ResponseBody::StackTrace(responses::StackTraceResponse {
                            stack_frames: frames,
                            total_frames: Some(1),
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

                    let resp = req.success(ResponseBody::Variables(responses::VariablesResponse {
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

                // --- Evaluate (custom state query) ---
                Command::Evaluate(ref args) if args.expression == "__miden_state" => {
                    let state_json = serde_json::json!({
                        "cycle": cycle,
                        "stopped": resume_ctx.is_none(),
                    });
                    let resp = req.success(ResponseBody::Evaluate(responses::EvaluateResponse {
                        result: state_json.to_string(),
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

                    match expr
                        .parse::<ReadMemoryExpr>()
                        .and_then(|expr| read_memory_at_current_state(&mut processor, cycle, &expr))
                    {
                        Ok(result) => {
                            let resp =
                                req.success(ResponseBody::Evaluate(responses::EvaluateResponse {
                                    result,
                                    type_field: Some("string".into()),
                                    presentation_hint: None,
                                    variables_reference: 0,
                                    named_variables: None,
                                    indexed_variables: None,
                                    memory_reference: None,
                                }));
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

        eprintln!("DAP session ended. Building execution output...");

        // Run the program to completion if it hasn't finished
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
        //
        // NOTE: The advice provider and precompile transcript are not accessible from the
        // processor's public API after stepping, so we use defaults. This is acceptable because
        // the DAP executor is a debugging tool — the returned ExecutionOutput is used to satisfy
        // the transaction executor trait, not for proof generation.
        let stack_top: Vec<_> = processor.stack_top().iter().rev().copied().collect();
        let stack = StackOutputs::new(&stack_top)
            .unwrap_or_else(|_| StackOutputs::new(&[]).expect("empty stack outputs"));

        Ok(ExecutionOutput {
            stack,
            advice: Default::default(),
            memory: Default::default(),
            final_pc_transcript: PrecompileTranscript::default(),
        })
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
            let (_op, node_id, op_idx) = extract_current_op(&new_ctx);
            *current_asmop = node_id
                .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
            *resume_ctx = Some(new_ctx);
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
                let (_op, node_id, op_idx) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                if *current_asmop != original_asmop {
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
                let (_op, node_id, op_idx) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                if host.call_depth <= target_depth {
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
) -> StepResult {
    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match poll_immediately(processor.step(host, ctx)) {
            Ok(Some(new_ctx)) => {
                *cycle += 1;
                let (_op, node_id, op_idx) = extract_current_op(&new_ctx);
                *current_asmop = node_id
                    .and_then(|nid| new_ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
                *resume_ctx = Some(new_ctx);

                // Check breakpoints
                if !breakpoints.is_empty()
                    && let Some(asmop) = current_asmop.as_ref()
                    && let Some((path, line)) = resolve_asmop_location(asmop, host)
                {
                    for bp in breakpoints {
                        if bp.line == line && (path.ends_with(&bp.path) || bp.path.ends_with(&path))
                        {
                            return StepResult::Breakpoint(line);
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
