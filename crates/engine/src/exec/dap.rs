use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    io::{BufReader, BufWriter},
    net::TcpListener,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use dap::prelude::*;
use miden_core::{
    Word,
    mast::{MastNode, MastNodeId},
    operations::{AssemblyOp, DebugOptions, DebugVarInfo, DebugVarLocation},
    precompile::PrecompileTranscript,
    program::Program,
};
use miden_processor::{
    BaseHost, ExecutionError, ExecutionOptions, ExecutionOutput, FastProcessor, FutureMaybeSend,
    Host, ProcessorState, ResumeContext, StackInputs, StackOutputs, TraceError,
    advice::{AdviceInputs, AdviceMutation},
    event::EventError,
    mast::MastForest,
    trace::RowIndex,
};

use super::{
    EventMutationRecorder, MastForestRecorder, ReplaySnapshot, ReplaySnapshotRecorder,
    ReplaySnapshotWrite, TraceEvent, state::extract_current_op,
};
use crate::debug::{
    DebugVarSnapshot, DebugVarTracker, FormatType, ReadMemoryExpr, resolve_variable_value,
    snapshot_transient_debug_values,
};

// DAP CONFIG
// ================================================================================================

static DAP_CONFIG: OnceLock<DapConfig> = OnceLock::new();

/// Configuration for the DAP debug server.
#[derive(Clone, Debug)]
pub struct DapConfig {
    /// The address to listen on, e.g. "127.0.0.1:4711".
    pub listen_addr: String,
    /// Source path prefixes passed from the compiler/debug plugin. If debug info
    /// stores paths after `-Ztrim-path-prefix`, clients may still send absolute
    /// editor paths; these prefixes describe that explicit mapping.
    source_path_prefixes: Vec<String>,
    /// Shared flag: set by the server when a Phase 2 restart (terminate-and-reconnect) is
    /// requested, read by the caller to decide whether to recompile and re-enter.
    restart_requested: Arc<AtomicBool>,
    /// When set, the advice mutations produced by the host's event handlers are recorded into
    /// this shared log during the session. See [DapConfig::record_event_mutations].
    event_recorder: Option<EventMutationRecorder>,
    /// When set, a [ReplaySnapshot] of the session is written to this path once execution
    /// completes. See [DapConfig::record_snapshot].
    snapshot_path: Option<PathBuf>,
    /// Shared status for the configured replay snapshot write.
    snapshot_recorder: Option<ReplaySnapshotRecorder>,
}

impl DapConfig {
    pub fn new(listen_addr: impl Into<String>) -> Self {
        Self {
            listen_addr: listen_addr.into(),
            source_path_prefixes: Vec::new(),
            restart_requested: Arc::new(AtomicBool::new(false)),
            event_recorder: None,
            snapshot_path: None,
            snapshot_recorder: None,
        }
    }

    pub fn with_source_path_prefixes(mut self, prefixes: Vec<PathBuf>) -> Self {
        self.source_path_prefixes = prefixes
            .into_iter()
            .map(|prefix| normalize_source_path(&prefix.to_string_lossy()))
            .filter(|prefix| !prefix.is_empty())
            .collect();
        self
    }

    /// Record the advice mutations produced by the host's event handlers during the session.
    ///
    /// Returns a shared handle that outlives the executor: the [DapExecutor] is constructed
    /// internally (e.g. by a transaction executor) from the global configuration and consumed
    /// by execution, so the handle obtained here — by whoever installs the configuration via
    /// [DapConfig::set_global] — is how the recorded log is read once execution returns.
    ///
    /// One entry is recorded per `on_event` invocation, in execution order, including empty
    /// mutation sets. The log always describes the final run (it is cleared whenever the
    /// session restarts execution from the beginning) and can be fed into an event-replay
    /// debugging session (e.g. `Executor::into_debug_with_replay` or
    /// `State::new_for_transaction`) to re-execute the same program without the original
    /// host's event handlers.
    pub fn record_event_mutations(&mut self) -> EventMutationRecorder {
        self.event_recorder.get_or_insert_with(EventMutationRecorder::new).clone()
    }

    /// Write a [ReplaySnapshot] of the session to `path` once execution completes.
    ///
    /// The snapshot captures the program, its stack and advice inputs, the MAST forests the host
    /// resolved, and the recorded event log — everything an event-replay debugging session needs
    /// to re-execute the same program later without the original host (e.g.
    /// `miden-debug --replay <path>`). It always describes the final run, since the recorded state
    /// is reset whenever the session restarts execution from the beginning.
    pub fn record_snapshot(&mut self, path: impl Into<PathBuf>) -> ReplaySnapshotRecorder {
        self.snapshot_path = Some(path.into());
        self.snapshot_recorder.get_or_insert_with(ReplaySnapshotRecorder::new).clone()
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
    /// When set, the advice mutations produced by each `on_event` invocation of the inner host
    /// are recorded here, in execution order, so they can be replayed later (e.g. transaction
    /// debugging with event replay).
    event_recorder: Option<EventMutationRecorder>,
    /// When set, the MAST forests the inner host resolves are recorded here, so a replay session
    /// can load the same code for `call`/`dyncall` targets.
    forest_recorder: Option<MastForestRecorder>,
}

impl<'a, H: Host> DapHostWrapper<'a, H> {
    fn new(
        inner: &'a mut H,
        event_recorder: Option<EventMutationRecorder>,
        forest_recorder: Option<MastForestRecorder>,
    ) -> Self {
        Self {
            inner,
            call_depth: 0,
            frames: Vec::new(),
            event_recorder,
            forest_recorder,
        }
    }
}

impl<H: Host> BaseHost for DapHostWrapper<'_, H> {
    fn get_label_and_source_file(
        &self,
        location: &miden_debug_types::Location,
    ) -> (miden_debug_types::SourceSpan, Option<Arc<miden_debug_types::SourceFile>>) {
        self.inner.get_label_and_source_file(location)
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

impl<H: Host> Host for DapHostWrapper<'_, H> {
    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        // Record every forest the inner host resolves, so a replay session can load the same
        // code for `call`/`dyncall` targets. The recorder deduplicates.
        let forest_recorder = self.forest_recorder.clone();
        let fut = self.inner.get_mast_forest(node_digest);
        async move {
            let forest = fut.await;
            if let (Some(recorder), Some(forest)) = (&forest_recorder, &forest) {
                recorder.record(forest.clone());
            }
            forest
        }
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        // Every invocation is recorded, including empty mutation sets: event replay pops one
        // entry per event, so the log must stay aligned with the event stream.
        let recorder = self.event_recorder.clone();
        let fut = self.inner.on_event(process);
        async move {
            let result = fut.await;
            if let (Some(recorder), Ok(mutations)) = (&recorder, &result) {
                recorder.record(crate::exec::clone_advice_mutations(mutations));
            }
            result
        }
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
            FormatType::Hex => write!(&mut $out, "{:#x}", $value).unwrap(),
            FormatType::Binary => write!(&mut $out, "{:#b}", $value).unwrap(),
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
            .read_element(
                ctx,
                miden_processor::Felt::new(u64::from(expr.addr.addr))
                    .expect("value exceeds field modulus"),
            )
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
            .read_word(
                ctx,
                miden_processor::Felt::new(u64::from(expr.addr.addr))
                    .expect("value exceeds field modulus"),
                cycle,
            )
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
            .read_element(
                ctx,
                miden_processor::Felt::new(u64::from(addr)).expect("value exceeds field modulus"),
            )
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

struct ContinueBreakpoints<'a> {
    source: &'a [StoredBreakpoint],
    function: &'a [StoredFunctionBreakpoint],
    source_path_prefixes: &'a [String],
}

// RESOLVE ASMOP LOCATION
// ================================================================================================

/// Resolve an `AssemblyOp`'s location to a (file_path, line_number) pair using the host.
fn resolve_asmop_location<H: Host>(asmop: &AssemblyOp, host: &H) -> Option<(String, i64)> {
    let location = asmop.location()?;
    let (span, source_file) = host.get_label_and_source_file(location);
    if let Some(source_file) = source_file {
        let file_line_col = source_file.location(span);
        let path = file_line_col.uri.as_ref().to_string();
        let line = file_line_col.line.to_u32() as i64;
        return Some((path, line));
    }

    crate::debug::resolve_location_from_filesystem(location)
        .map(|(path, line)| (path.display().to_string(), line as i64))
}

fn should_defer_function_breakpoint(resolved: Option<&(String, i64)>, context_name: &str) -> bool {
    !is_internal_procedure(context_name)
        && resolved.is_none_or(|(path, _)| {
            crate::debug::is_internal_source_uri(&miden_debug_types::Uri::new(path))
        })
}

fn is_internal_procedure(context_name: &str) -> bool {
    context_name.contains("::intrinsics::")
}

fn normalize_source_path(path: &str) -> String {
    let path = path.trim();
    let path = path.strip_prefix("file://").unwrap_or(path);
    let path = path.replace('\\', "/");

    let is_absolute = path.starts_with('/');
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }

    let normalized = parts.join("/");
    if is_absolute && !normalized.is_empty() {
        format!("/{normalized}")
    } else {
        normalized
    }
}

fn strip_source_prefix(path: &str, prefix: &str) -> Option<String> {
    let path = path.trim_start_matches('/');
    let prefix = prefix.trim_start_matches('/').trim_end_matches('/');
    path.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(ToOwned::to_owned)
}

fn source_paths_match(left: &str, right: &str, trim_prefixes: &[String]) -> bool {
    let left = normalize_source_path(left);
    let right = normalize_source_path(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }

    if left == right {
        return true;
    }

    for prefix in trim_prefixes {
        if strip_source_prefix(&left, prefix).is_some_and(|stripped| stripped == right) {
            return true;
        }
        if strip_source_prefix(&right, prefix).is_some_and(|stripped| stripped == left) {
            return true;
        }
    }

    false
}

fn breakable_source_lines<H: Host>(
    forest: &MastForest,
    host: &H,
    source_path: &str,
    trim_prefixes: &[String],
) -> BTreeSet<i64> {
    let mut lines = BTreeSet::new();

    for (node_idx, node) in forest.nodes().iter().enumerate() {
        let MastNode::Block(block) = node else {
            continue;
        };

        let node_id = MastNodeId::new_unchecked(node_idx as u32);
        for op_idx in 0..block.num_operations() as usize {
            let Some(asmop) = forest.get_assembly_op(node_id, Some(op_idx)) else {
                continue;
            };
            let Some((path, line)) = resolve_asmop_location(asmop, host) else {
                continue;
            };
            if source_paths_match(&path, source_path, trim_prefixes) {
                lines.insert(line);
            }
        }
    }

    lines
}

fn minimum_source_line_for_proc<H: Host>(
    forest: &MastForest,
    host: &H,
    procedure: &str,
    source_path: &str,
    trim_prefixes: &[String],
) -> Option<i64> {
    let mut min_line: Option<i64> = None;

    for (node_idx, node) in forest.nodes().iter().enumerate() {
        let MastNode::Block(block) = node else {
            continue;
        };

        let node_id = MastNodeId::new_unchecked(node_idx as u32);
        for op_idx in 0..block.num_operations() as usize {
            let Some(asmop) = forest.get_assembly_op(node_id, Some(op_idx)) else {
                continue;
            };
            if asmop.context_name() != procedure {
                continue;
            }
            let Some((path, line)) = resolve_asmop_location(asmop, host) else {
                continue;
            };
            if line > 1 && source_paths_match(&path, source_path, trim_prefixes) {
                min_line = Some(min_line.map_or(line, |current| current.min(line)));
            }
        }
    }

    min_line
}

fn resolve_breakpoint_line(lines: &BTreeSet<i64>, requested_line: i64) -> Option<i64> {
    if lines.contains(&requested_line) {
        return Some(requested_line);
    }

    lines
        .range(requested_line..)
        .next()
        .copied()
        .or_else(|| lines.range(..requested_line).next_back().copied())
}

fn debug_var_infos_for_context(ctx: &ResumeContext) -> Vec<DebugVarInfo> {
    let (_op, node_id, op_idx, _control) = extract_current_op(ctx);
    let Some(node_id) = node_id else {
        return Vec::new();
    };
    let Some(op_idx) = op_idx else {
        return Vec::new();
    };

    let forest = ctx.current_forest();
    forest
        .debug_vars_for_operation(node_id, op_idx)
        .iter()
        .filter_map(|var_id| forest.debug_var(*var_id).cloned())
        .collect()
}

fn record_debug_vars(
    debug_state: &mut DapDebugVarState,
    cycle: usize,
    debug_var_infos: Vec<DebugVarInfo>,
) {
    let clk = RowIndex::from(cycle as u32);
    debug_state.debug_vars.record_events(clk, debug_var_infos);
    debug_state.debug_vars.update_to_cycle(clk);
}

fn is_compiler_generated_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("local") else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
}

fn is_visible_source_var<H: Host>(
    var: &DebugVarSnapshot,
    current_asmop: Option<&AssemblyOp>,
    host: &DapHostWrapper<'_, H>,
    source_path_prefixes: &[String],
    show_all: bool,
) -> bool {
    if show_all {
        return true;
    }

    let name = var.info.name();
    if is_compiler_generated_name(name) {
        return false;
    }

    let Some(current) = current_asmop.and_then(|asmop| resolve_asmop_location(asmop, host)) else {
        return true;
    };

    let Some(var_loc) = var.info.location() else {
        return true;
    };

    source_var_location_is_visible(
        var_loc.uri.as_str(),
        i64::from(var_loc.line.to_u32()),
        &current.0,
        current.1,
        source_path_prefixes,
    )
}

fn source_var_location_is_visible(
    var_path: &str,
    var_line: i64,
    current_path: &str,
    current_line: i64,
    source_path_prefixes: &[String],
) -> bool {
    source_paths_match(var_path, current_path, source_path_prefixes) && var_line < current_line
}

fn resolve_debug_var_value(
    processor: &mut FastProcessor,
    location: &DebugVarLocation,
) -> Option<miden_processor::Felt> {
    let state = processor.state();
    let stack = state.get_stack_state();
    let context = state.ctx();

    let read_mem = |addr: u32| -> Option<miden_processor::Felt> {
        processor
            .memory()
            .read_element(
                context,
                miden_processor::Felt::new(u64::from(addr)).expect("value exceeds field modulus"),
            )
            .ok()
    };

    resolve_variable_value(location, &stack, read_mem, |offset| {
        let fmp_addr = miden_core::FMP_ADDR.as_canonical_u64() as u32;
        let fmp = read_mem(fmp_addr)?;
        let addr = (fmp.as_canonical_u64() as i64 + i64::from(offset)) as u32;
        read_mem(addr)
    })
}

fn debug_var_to_dap_variable(
    processor: &mut FastProcessor,
    var: &DebugVarSnapshot,
) -> types::Variable {
    let name = var.info.name().to_string();
    let location = var.info.value_location();
    let value = resolve_debug_var_value(processor, location)
        .map(|felt| felt.as_canonical_u64().to_string())
        .unwrap_or_else(|| location.to_string());

    types::Variable {
        name,
        value,
        type_field: Some("Felt".into()),
        variables_reference: 0,
        ..Default::default()
    }
}

fn debug_variables<H: Host>(
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    debug_state: &DapDebugVarState,
    source_path_prefixes: &[String],
    show_all: bool,
) -> Vec<types::Variable> {
    debug_state
        .debug_vars
        .current_variables()
        .filter(|var| {
            is_visible_source_var(var, current_asmop, host, source_path_prefixes, show_all)
        })
        .map(|var| debug_var_to_dap_variable(processor, var))
        .collect()
}

fn format_debug_variables<H: Host>(
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    debug_state: &DapDebugVarState,
    source_path_prefixes: &[String],
    show_all: bool,
) -> String {
    let variables = debug_variables(
        processor,
        host,
        current_asmop,
        debug_state,
        source_path_prefixes,
        show_all,
    );

    if variables.is_empty() {
        if show_all {
            "No debug variables tracked".into()
        } else {
            "No source-level variables (use 'vars all' to show compiler locals)".into()
        }
    } else {
        variables
            .into_iter()
            .map(|var| format!("{}={}", var.name, var.value))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn evaluate_debug_variable<H: Host>(
    processor: &mut FastProcessor,
    host: &DapHostWrapper<'_, H>,
    current_asmop: Option<&AssemblyOp>,
    debug_state: &DapDebugVarState,
    source_path_prefixes: &[String],
    expression: &str,
) -> Option<types::Variable> {
    let var = debug_state.debug_vars.get_variable(expression)?;
    if !is_visible_source_var(var, current_asmop, host, source_path_prefixes, false) {
        return None;
    }
    Some(debug_var_to_dap_variable(processor, var))
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
    event_recorder: Option<EventMutationRecorder>,
    forest_recorder: Option<MastForestRecorder>,
}

/// Variables reference IDs for scopes.
const SCOPE_STACK: i64 = 1;
const SCOPE_MEMORY: i64 = 2;
const SCOPE_LOCALS: i64 = 3;

struct DapDebugVarState {
    debug_vars: DebugVarTracker,
}

impl DapDebugVarState {
    fn new() -> Self {
        Self {
            debug_vars: DebugVarTracker::new(Rc::new(RefCell::new(BTreeMap::new()))),
        }
    }
}

impl DapExecutor {
    pub fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        let config = DAP_CONFIG.get().cloned().unwrap_or_default();
        // Writing a snapshot requires both the event log and the resolved forests. Ensure an
        // event recorder exists (reusing the config's shared one if present) and turn on forest
        // recording whenever a snapshot path is configured.
        let (event_recorder, forest_recorder) = if config.snapshot_path.is_some() {
            (
                Some(config.event_recorder.clone().unwrap_or_default()),
                Some(MastForestRecorder::new()),
            )
        } else {
            (config.event_recorder.clone(), None)
        };
        DapExecutor {
            stack_inputs,
            advice_inputs,
            options,
            config,
            event_recorder,
            forest_recorder,
        }
    }

    /// Record the advice mutations produced by each event handler invocation of the wrapped
    /// host during execution.
    ///
    /// Returns a handle to the shared log. When recording was enabled on the installed
    /// [DapConfig] (see [DapConfig::record_event_mutations]), this is the same log, so
    /// embedders that never see the executor instance read the recording through their config
    /// handle instead.
    ///
    /// One entry is recorded per `on_event` invocation, in execution order, including empty
    /// mutation sets, so the log can be fed directly into an event replay queue (e.g.
    /// `Executor::into_debug_with_replay` or `State::new_for_transaction`) to debug the same
    /// execution later without access to the original host. The log is cleared automatically
    /// whenever the DAP session restarts execution from the beginning, so after execution
    /// completes it always describes the final run.
    pub fn record_event_mutations(&mut self) -> EventMutationRecorder {
        self.event_recorder.get_or_insert_with(EventMutationRecorder::new).clone()
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
    /// Bind the DAP listen socket, reporting failures (bad address, port in
    /// use, ...) as errors instead of aborting the process.
    fn bind_listener(listen_addr: &str) -> Result<TcpListener, String> {
        use std::net::ToSocketAddrs;

        use socket2::{Domain, Socket, Type};
        let addr: std::net::SocketAddr = listen_addr
            .to_socket_addrs()
            .map_err(|e| format!("invalid listen address '{listen_addr}': {e}"))?
            .next()
            .ok_or_else(|| {
                format!("listen address '{listen_addr}' did not resolve to any address")
            })?;
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)
            .map_err(|e| format!("failed to create socket: {e}"))?;
        socket.set_reuse_address(true).ok();
        socket
            .bind(&addr.into())
            .map_err(|e| format!("DAP server failed to bind to {addr}: {e}"))?;
        socket
            .listen(1)
            .map_err(|e| format!("DAP server failed to listen on {addr}: {e}"))?;
        Ok(socket.into())
    }

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
        let listener = match Self::bind_listener(&self.config.listen_addr) {
            Ok(listener) => listener,
            Err(err) => {
                log::error!("{err}");
                return Err(ExecutionError::Internal("failed to start DAP server"));
            }
        };
        log::info!(
            "DAP server listening on {}. Waiting for client connection...",
            self.config.listen_addr
        );

        // Accept one client connection (persists across restarts).
        let (stream, addr) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(err) => {
                log::error!("DAP server accept failed: {err}");
                return Err(ExecutionError::Internal("DAP server accept failed"));
            }
        };
        log::info!("DAP client connected from {addr}");

        let reader = BufReader::new(match stream.try_clone() {
            Ok(stream) => stream,
            Err(err) => {
                log::error!("failed to clone TCP stream: {err}");
                return Err(ExecutionError::Internal("failed to clone TCP stream"));
            }
        });
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
        let source_path_prefixes = self.config.source_path_prefixes.clone();

        // Outer restart loop — on restart, the DapHostWrapper borrow is dropped,
        // a fresh FastProcessor is created, and the inner event loop re-enters.
        //
        // NOTE: The inner host `H` (e.g. from miden-client's transaction executor)
        // retains mutations from prior execution. A restarted session may therefore
        // diverge from a pristine fresh session. Phase 2 (terminate-and-reconnect)
        // solves this by creating a completely fresh host.
        loop {
            let mut processor =
                FastProcessor::new_with_options(stack_inputs, advice_inputs.clone(), options)
                    .expect("advice inputs should fit advice map limits");

            let resume_ctx = processor.get_initial_resume_context(program)?;
            // Each pass through this loop starts execution from the beginning, so any mutations
            // or forests recorded by a previous (restarted) run no longer apply.
            if let Some(recorder) = self.event_recorder.as_ref() {
                recorder.clear();
            }
            if let Some(recorder) = self.forest_recorder.as_ref() {
                recorder.clear();
            }
            let mut wrapper = DapHostWrapper::new(
                host,
                self.event_recorder.clone(),
                self.forest_recorder.clone(),
            );

            let mut resume_ctx = Some(resume_ctx);
            let mut cycle: usize = 0;
            let mut current_asmop: Option<AssemblyOp> = None;
            let mut debug_state = DapDebugVarState::new();

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
                        log::error!("DAP protocol error: {e:#?}");
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
                            supports_evaluate_for_hovers: Some(true),
                            ..Default::default()
                        };
                        let resp = req.success(ResponseBody::Initialize(caps));
                        server.respond(resp).ok();
                        server.send_event(Event::Initialized).ok();
                    }

                    Command::Launch(_) => {
                        server.respond(req.success(ResponseBody::Launch)).ok();
                        // Re-emit Initialized — some clients only listen for it after
                        // launch/attach has been acknowledged. VS Code tolerates the
                        // duplicate (it ignores Initialized after the first one).
                        server.send_event(Event::Initialized).ok();
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
                        // See the Launch arm — re-emit Initialized for clients that
                        // miss the first one.
                        server.send_event(Event::Initialized).ok();
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
                        let has_arguments =
                            args.as_ref().and_then(|a| a.arguments.as_ref()).is_some();
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

                        let continue_breakpoints = ContinueBreakpoints {
                            source: &breakpoints,
                            function: &function_breakpoints,
                            source_path_prefixes: &source_path_prefixes,
                        };
                        match step_until_breakpoint(
                            &mut processor,
                            &mut wrapper,
                            &mut resume_ctx,
                            &mut cycle,
                            &mut current_asmop,
                            &continue_breakpoints,
                            &mut debug_state,
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
                                // Capture the failed run so it can still be replayed offline.
                                if let Some(path) = self.config.snapshot_path.as_ref() {
                                    write_replay_snapshot(ReplaySnapshotWriteContext {
                                        path,
                                        snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                                        event_recorder: self.event_recorder.as_ref(),
                                        forest_recorder: self.forest_recorder.as_ref(),
                                        program,
                                        stack_inputs,
                                        advice_inputs: &advice_inputs,
                                        options,
                                    });
                                }
                                return Err(e);
                            }
                        }
                    }

                    Command::Next(ref args) => {
                        let is_instruction_step = matches!(
                            args.granularity,
                            Some(types::SteppingGranularity::Instruction)
                        );
                        let resp = req.success(ResponseBody::Next);
                        server.respond(resp).ok();

                        let step_result = if is_instruction_step {
                            step_over(
                                &mut processor,
                                &mut wrapper,
                                &mut resume_ctx,
                                &mut cycle,
                                &mut current_asmop,
                                &mut debug_state,
                            )
                        } else {
                            step_next_line(
                                &mut processor,
                                &mut wrapper,
                                &mut resume_ctx,
                                &mut cycle,
                                &mut current_asmop,
                                &source_path_prefixes,
                                &mut debug_state,
                            )
                        };

                        match step_result {
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
                                // Capture the failed run so it can still be replayed offline.
                                if let Some(path) = self.config.snapshot_path.as_ref() {
                                    write_replay_snapshot(ReplaySnapshotWriteContext {
                                        path,
                                        snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                                        event_recorder: self.event_recorder.as_ref(),
                                        forest_recorder: self.forest_recorder.as_ref(),
                                        program,
                                        stack_inputs,
                                        advice_inputs: &advice_inputs,
                                        options,
                                    });
                                }
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
                            &mut debug_state,
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
                                // Capture the failed run so it can still be replayed offline.
                                if let Some(path) = self.config.snapshot_path.as_ref() {
                                    write_replay_snapshot(ReplaySnapshotWriteContext {
                                        path,
                                        snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                                        event_recorder: self.event_recorder.as_ref(),
                                        forest_recorder: self.forest_recorder.as_ref(),
                                        program,
                                        stack_inputs,
                                        advice_inputs: &advice_inputs,
                                        options,
                                    });
                                }
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
                            &mut debug_state,
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
                                // Capture the failed run so it can still be replayed offline.
                                if let Some(path) = self.config.snapshot_path.as_ref() {
                                    write_replay_snapshot(ReplaySnapshotWriteContext {
                                        path,
                                        snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                                        event_recorder: self.event_recorder.as_ref(),
                                        forest_recorder: self.forest_recorder.as_ref(),
                                        program,
                                        stack_inputs,
                                        advice_inputs: &advice_inputs,
                                        options,
                                    });
                                }
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
                                    name: "Local Variables".into(),
                                    variables_reference: SCOPE_LOCALS,
                                    presentation_hint: Some(types::ScopePresentationhint::Locals),
                                    named_variables: Some(
                                        debug_variables(
                                            &mut processor,
                                            &wrapper,
                                            current_asmop.as_ref(),
                                            &debug_state,
                                            &source_path_prefixes,
                                            false,
                                        )
                                        .len() as i64,
                                    ),
                                    expensive: false,
                                    ..Default::default()
                                },
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
                            SCOPE_LOCALS => debug_variables(
                                &mut processor,
                                &wrapper,
                                current_asmop.as_ref(),
                                &debug_state,
                                &source_path_prefixes,
                                false,
                            ),
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
                        breakpoints.retain(|bp| {
                            !source_paths_match(&bp.path, &source_path, &source_path_prefixes)
                        });
                        let breakable_lines = breakable_source_lines(
                            program.mast_forest(),
                            &wrapper,
                            &source_path,
                            &source_path_prefixes,
                        );

                        let mut confirmed = Vec::new();
                        if let Some(bps) = &args.breakpoints {
                            for sbp in bps {
                                let resolved_line =
                                    resolve_breakpoint_line(&breakable_lines, sbp.line);
                                let verified = resolved_line.is_some();
                                let actual_line = resolved_line.unwrap_or(sbp.line);
                                let message = match resolved_line {
                                    Some(line) if line != sbp.line => {
                                        Some(format!("Moved to executable line {line}."))
                                    }
                                    Some(_) => None,
                                    None => Some(
                                        "No executable Miden operation is mapped to this source \
                                         file."
                                            .into(),
                                    ),
                                };

                                if verified {
                                    breakpoints.push(StoredBreakpoint {
                                        path: source_path.clone(),
                                        line: actual_line,
                                    });
                                }

                                confirmed.push(types::Breakpoint {
                                    verified,
                                    message,
                                    line: Some(actual_line),
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

                    Command::Evaluate(ref args)
                        if args.expression == "vars" || args.expression == ":vars" =>
                    {
                        let result = format_debug_variables(
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            &debug_state,
                            &source_path_prefixes,
                            false,
                        );
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

                    Command::Evaluate(ref args)
                        if args.expression == "vars all" || args.expression == ":vars all" =>
                    {
                        let result = format_debug_variables(
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            &debug_state,
                            &source_path_prefixes,
                            true,
                        );
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

                    Command::Evaluate(ref args) => {
                        if let Some(variable) = evaluate_debug_variable(
                            &mut processor,
                            &wrapper,
                            current_asmop.as_ref(),
                            &debug_state,
                            &source_path_prefixes,
                            args.expression.as_str(),
                        ) {
                            let resp =
                                req.success(ResponseBody::Evaluate(responses::EvaluateResponse {
                                    result: variable.value,
                                    type_field: variable.type_field,
                                    presentation_hint: None,
                                    variables_reference: 0,
                                    named_variables: None,
                                    indexed_variables: None,
                                    memory_reference: None,
                                }));
                            server.respond(resp).ok();
                        } else {
                            server.respond(req.error("Unsupported expression")).ok();
                        }
                    }

                    // --- Unhandled ---
                    _ => {
                        server.respond(req.error("Unsupported command")).ok();
                    }
                }
            }

            if phase2_requested {
                log::debug!("DAP Phase 2 restart: returning from execute() for recompilation...");
                return Ok(ExecutionOutput {
                    stack: StackOutputs::new(&[]).expect("empty stack outputs"),
                    advice: Default::default(),
                    memory: Default::default(),
                    final_precompile_transcript: PrecompileTranscript::default(),
                });
            }

            if restart_requested {
                is_restart = true;
                log::debug!("DAP restart requested. Resetting processor...");
                // wrapper is dropped here, releasing the &mut H borrow.
                // The outer loop creates a fresh processor and wrapper.
                continue;
            }

            // Normal exit (disconnect or connection closed).
            log::debug!("DAP session ended. Building execution output...");

            // Run the program to completion if it hasn't finished.
            if let Some(ctx) = resume_ctx {
                let mut ctx = Some(ctx);
                while let Some(resume) = ctx.take() {
                    match poll_immediately(processor.step(&mut wrapper, resume)) {
                        Ok(Some(new_ctx)) => {
                            ctx = Some(new_ctx);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            // Capture the run up to the failure so it can still be replayed.
                            if let Some(path) = self.config.snapshot_path.as_ref() {
                                write_replay_snapshot(ReplaySnapshotWriteContext {
                                    path,
                                    snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                                    event_recorder: self.event_recorder.as_ref(),
                                    forest_recorder: self.forest_recorder.as_ref(),
                                    program,
                                    stack_inputs,
                                    advice_inputs: &advice_inputs,
                                    options,
                                });
                            }
                            return Err(e);
                        }
                    }
                }
            }

            // If a snapshot path is configured, persist a self-contained replay snapshot of this
            // (final) run: program, inputs, resolved forests, and the recorded event log.
            if let Some(path) = self.config.snapshot_path.as_ref() {
                write_replay_snapshot(ReplaySnapshotWriteContext {
                    path,
                    snapshot_recorder: self.config.snapshot_recorder.as_ref(),
                    event_recorder: self.event_recorder.as_ref(),
                    forest_recorder: self.forest_recorder.as_ref(),
                    program,
                    stack_inputs,
                    advice_inputs: &advice_inputs,
                    options,
                });
            }

            // Build ExecutionOutput from the processor's final state.
            let stack_top: Vec<_> = processor.stack_top().iter().rev().copied().collect();
            let stack = StackOutputs::new(&stack_top)
                .unwrap_or_else(|_| StackOutputs::new(&[]).expect("empty stack outputs"));

            let (advice, memory, final_precompile_transcript) = processor.into_parts();
            return Ok(ExecutionOutput {
                stack,
                advice,
                memory,
                final_precompile_transcript,
            });
        } // end outer restart loop
    }
}

/// Build and write a [ReplaySnapshot] of the current run to `path`.
///
/// Reads the recorders without draining them (via their `snapshot()`), so a caller that also
/// reads the shared event recorder afterwards — e.g. an embedder reporting how many mutation sets
/// were recorded — still sees the full log. Called on every terminal outcome, clean exit or a
/// failed step, so a failing transaction is captured and replayable too.
struct ReplaySnapshotWriteContext<'a> {
    path: &'a Path,
    snapshot_recorder: Option<&'a ReplaySnapshotRecorder>,
    event_recorder: Option<&'a EventMutationRecorder>,
    forest_recorder: Option<&'a MastForestRecorder>,
    program: &'a Program,
    stack_inputs: StackInputs,
    advice_inputs: &'a AdviceInputs,
    options: ExecutionOptions,
}

fn write_replay_snapshot(context: ReplaySnapshotWriteContext<'_>) {
    let event_log = context.event_recorder.map(EventMutationRecorder::snapshot).unwrap_or_default();
    let mast_forests =
        context.forest_recorder.map(MastForestRecorder::snapshot).unwrap_or_default();
    let snapshot = ReplaySnapshot {
        program: context.program.clone(),
        stack_inputs: context.stack_inputs,
        advice_inputs: context.advice_inputs.clone(),
        options: context.options,
        mast_forests,
        event_log,
    };
    match snapshot.write_to_file(context.path) {
        Ok(()) => {
            let write = ReplaySnapshotWrite {
                path: context.path.to_path_buf(),
                event_count: snapshot.event_log.len(),
                forest_count: snapshot.mast_forests.len(),
            };
            if let Some(recorder) = context.snapshot_recorder {
                recorder.record_success(write.clone());
            }
            eprintln!(
                "Wrote replay snapshot ({} event(s), {} forest(s)) to {}",
                write.event_count,
                write.forest_count,
                write.path.display()
            );
        }
        Err(err) => {
            if let Some(recorder) = context.snapshot_recorder {
                recorder.record_error(context.path.to_path_buf(), err.to_string());
            }
            eprintln!("Failed to write replay snapshot to {}: {err}", context.path.display());
        }
    }
}

// STEPPING HELPERS
// ================================================================================================

fn advance_one<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    ctx: ResumeContext,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
    debug_state: &mut DapDebugVarState,
) -> Result<Option<ResumeContext>, ExecutionError> {
    let (_op, node_id, op_idx, _control) = extract_current_op(&ctx);
    let executed_asmop =
        node_id.and_then(|nid| ctx.current_forest().get_assembly_op(nid, op_idx).cloned());
    let mut debug_var_infos = debug_var_infos_for_context(&ctx);
    let pre_step_stack = processor.state().get_stack_state();
    snapshot_transient_debug_values(&mut debug_var_infos, &pre_step_stack);
    match poll_immediately(processor.step(host, ctx)) {
        Ok(Some(new_ctx)) => {
            *cycle += 1;
            record_debug_vars(debug_state, *cycle, debug_var_infos);
            *current_asmop = executed_asmop;
            Ok(Some(new_ctx))
        }
        Ok(None) => {
            *cycle += 1;
            *current_asmop = None;
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn is_next_source_line(
    start_proc: Option<&str>,
    start_loc: Option<&(String, i64)>,
    current_proc: Option<&str>,
    current_loc: Option<&(String, i64)>,
    source_path_prefixes: &[String],
    minimum_source_line: Option<i64>,
) -> bool {
    let same_proc = match (start_proc, current_proc) {
        (Some(start), Some(current)) => start == current,
        (Some(_), None) => false,
        _ => true,
    };
    if !same_proc {
        return false;
    }

    if let (Some(minimum_source_line), Some(current)) = (minimum_source_line, current_loc)
        && current.1 < minimum_source_line
    {
        return false;
    }

    match (start_loc, current_loc) {
        (Some(start), Some(current)) => {
            source_paths_match(&start.0, &current.0, source_path_prefixes) && start.1 != current.1
        }
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Execute a single VM step.
fn step_one<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
    debug_state: &mut DapDebugVarState,
) -> StepResult {
    let ctx = match resume_ctx.take() {
        Some(ctx) => ctx,
        None => return StepResult::Terminated,
    };

    match advance_one(processor, host, ctx, cycle, current_asmop, debug_state) {
        Ok(Some(new_ctx)) => {
            *resume_ctx = Some(new_ctx);
            update_top_frame(host, current_asmop.as_ref());
            StepResult::Stepped
        }
        Ok(None) => StepResult::Terminated,
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
    debug_state: &mut DapDebugVarState,
) -> StepResult {
    let original_asmop = current_asmop.clone();

    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match advance_one(processor, host, ctx, cycle, current_asmop, debug_state) {
            Ok(Some(new_ctx)) => {
                *resume_ctx = Some(new_ctx);

                if *current_asmop != original_asmop {
                    update_top_frame(host, current_asmop.as_ref());
                    return StepResult::Stepped;
                }
            }
            Ok(None) => return StepResult::Terminated,
            Err(e) => return StepResult::Error(e),
        }
    }
}

/// Step over source lines: advance until the current source line changes.
fn step_next_line<H: Host>(
    processor: &mut FastProcessor,
    host: &mut DapHostWrapper<'_, H>,
    resume_ctx: &mut Option<ResumeContext>,
    cycle: &mut usize,
    current_asmop: &mut Option<AssemblyOp>,
    source_path_prefixes: &[String],
    debug_state: &mut DapDebugVarState,
) -> StepResult {
    let original_asmop = current_asmop.clone();
    let start_proc = original_asmop.as_ref().map(|asmop| asmop.context_name().to_string());
    let start_loc = original_asmop.as_ref().and_then(|asmop| resolve_asmop_location(asmop, host));
    let minimum_source_line = if let (Some(ctx), Some(proc), Some((source_path, _line))) =
        (resume_ctx.as_ref(), start_proc.as_deref(), start_loc.as_ref())
    {
        minimum_source_line_for_proc(
            ctx.current_forest(),
            host,
            proc,
            source_path,
            source_path_prefixes,
        )
    } else {
        None
    };

    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match advance_one(processor, host, ctx, cycle, current_asmop, debug_state) {
            Ok(Some(new_ctx)) => {
                *resume_ctx = Some(new_ctx);

                let current_proc =
                    current_asmop.as_ref().map(|asmop| asmop.context_name().to_string());
                let current_loc =
                    current_asmop.as_ref().and_then(|asmop| resolve_asmop_location(asmop, host));
                let has_source_context = start_loc.is_some() || current_loc.is_some();
                let reached_next = if has_source_context {
                    is_next_source_line(
                        start_proc.as_deref(),
                        start_loc.as_ref(),
                        current_proc.as_deref(),
                        current_loc.as_ref(),
                        source_path_prefixes,
                        minimum_source_line,
                    )
                } else {
                    *current_asmop != original_asmop
                };

                if reached_next {
                    update_top_frame(host, current_asmop.as_ref());
                    return StepResult::Stepped;
                }
            }
            Ok(None) => return StepResult::Terminated,
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
    debug_state: &mut DapDebugVarState,
) -> StepResult {
    let target_depth = host.call_depth.saturating_sub(1);

    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match advance_one(processor, host, ctx, cycle, current_asmop, debug_state) {
            Ok(Some(new_ctx)) => {
                *resume_ctx = Some(new_ctx);

                if host.call_depth <= target_depth {
                    update_top_frame(host, current_asmop.as_ref());
                    return StepResult::Stepped;
                }
            }
            Ok(None) => return StepResult::Terminated,
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
    breakpoints: &ContinueBreakpoints<'_>,
    debug_state: &mut DapDebugVarState,
) -> StepResult {
    loop {
        let ctx = match resume_ctx.take() {
            Some(ctx) => ctx,
            None => return StepResult::Terminated,
        };

        match advance_one(processor, host, ctx, cycle, current_asmop, debug_state) {
            Ok(Some(new_ctx)) => {
                *resume_ctx = Some(new_ctx);

                if let Some(asmop) = current_asmop.as_ref() {
                    let resolved = resolve_asmop_location(asmop, host);

                    // Check line breakpoints
                    if let Some((ref path, line)) = resolved {
                        for bp in breakpoints.source {
                            if bp.line == line
                                && source_paths_match(
                                    path,
                                    &bp.path,
                                    breakpoints.source_path_prefixes,
                                )
                            {
                                update_top_frame(host, current_asmop.as_ref());
                                return StepResult::Breakpoint(line);
                            }
                        }
                    }

                    // Check function/pattern breakpoints — match against context name and
                    // source file path. Context names may have a leading `::` (absolute
                    // paths like `::prologue::foo`), so we also try matching without it.
                    if !breakpoints.function.is_empty() {
                        let context_name = asmop.context_name();
                        let stripped_name = context_name.strip_prefix("::").unwrap_or(context_name);
                        for fbp in breakpoints.function {
                            // Match via glob pattern or suffix (e.g. "prologue::foo"
                            // matches "::$kernel::prologue::foo").
                            if fbp.pattern.matches(context_name)
                                || fbp.pattern.matches(stripped_name)
                                || context_name.ends_with(&fbp.name)
                                || stripped_name.ends_with(&fbp.name)
                            {
                                if should_defer_function_breakpoint(resolved.as_ref(), context_name)
                                {
                                    continue;
                                }
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
            Ok(None) => return StepResult::Terminated,
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
    // Some DAP clients (Zed) only react to `stopped` events for thread IDs they
    // already know about; explicitly announce thread 1 as started before the
    // initial stop. VS Code tolerates the extra event.
    server
        .send_event(Event::Thread(events::ThreadEventBody {
            reason: types::ThreadEventReason::Started,
            thread_id: 1,
        }))
        .ok();
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

#[cfg(test)]
mod tests {
    use miden_assembly::DefaultSourceManager;
    use miden_core::{
        Felt,
        events::{EventId, EventName},
    };
    use miden_processor::event::EventHandler;

    use super::*;
    use crate::exec::{DebuggerHost, EventMutationRecorder};

    struct PushSeven;

    impl EventHandler for PushSeven {
        fn on_event(
            &self,
            _process: &ProcessorState<'_>,
        ) -> Result<Vec<AdviceMutation>, EventError> {
            Ok(vec![AdviceMutation::ExtendStack {
                values: vec![Felt::from(7u32)],
            }])
        }
    }

    /// The DAP host wrapper is what a client-provided host (e.g. a transaction executor host)
    /// is driven through; recording must capture the mutations its event handlers produce.
    #[test]
    fn dap_host_wrapper_records_event_mutations() {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let event_name = "miden-debug::test::dap-record";
        let event_id = EventId::from_name(event_name).as_u64();
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(format!("begin push.{event_id} emit drop adv_push drop end"))
            .expect("failed to assemble test program");

        let mut host = DebuggerHost::new(source_manager);
        host.register_event_handler(
            EventName::from_string(event_name.to_string()),
            Arc::new(PushSeven),
        )
        .expect("failed to register event handler");

        let recorder = EventMutationRecorder::new();
        let mut wrapper = DapHostWrapper::new(&mut host, Some(recorder.clone()), None);

        let mut processor = FastProcessor::new_with_options(
            StackInputs::new(&[]).unwrap(),
            AdviceInputs::default(),
            ExecutionOptions::default().with_debugging(true).with_tracing(true),
        )
        .expect("invalid inputs");
        let mut resume_ctx =
            Some(processor.get_initial_resume_context(&program).expect("invalid program"));
        while let Some(ctx) = resume_ctx.take() {
            match poll_immediately(processor.step(&mut wrapper, ctx)).expect("execution failed") {
                Some(next) => resume_ctx = Some(next),
                None => break,
            }
        }

        assert_eq!(recorder.len(), 1, "expected one recorded entry for the emitted event");
        let batches = recorder.take();
        match batches[0].as_slice() {
            [AdviceMutation::ExtendStack { values }] => {
                assert_eq!(values.as_slice(), &[Felt::from(7u32)]);
            }
            _ => panic!("unexpected mutations recorded"),
        }
        assert!(recorder.is_empty(), "take() should leave the recorder empty");
    }

    /// The executor is constructed internally (e.g. by a transaction executor) and consumed by
    /// execution, so embedders read the recording through the handle obtained from the global
    /// [DapConfig] before execution: both must share one log.
    #[test]
    fn dap_config_recorder_is_shared_with_the_executor() {
        let mut config = DapConfig::new("127.0.0.1:0");
        let config_handle = config.record_event_mutations();
        DapConfig::set_global(config);

        let mut executor = DapExecutor::new(
            StackInputs::new(&[]).unwrap(),
            AdviceInputs::default(),
            ExecutionOptions::default(),
        );
        executor.record_event_mutations().record(vec![]);

        assert_eq!(
            config_handle.len(),
            1,
            "recording through the executor must be visible through the config handle"
        );
    }

    #[test]
    fn source_paths_match_only_uses_declared_trim_prefixes() {
        let trim_prefixes = vec!["/workspace/compiler/examples/fibonacci".into()];

        assert!(source_paths_match(
            "/workspace/compiler/examples/fibonacci/src/lib.rs",
            "src/lib.rs",
            &trim_prefixes,
        ));
        assert!(source_paths_match(
            "file:///workspace/compiler/examples/fibonacci/src/lib.rs",
            "/workspace/compiler/examples/fibonacci/src/lib.rs",
            &[],
        ));
        assert!(!source_paths_match(
            "/workspace/compiler/examples/fibonacci/src/lib.rs",
            "src/lib.rs",
            &[],
        ));
        assert!(!source_paths_match(
            "/workspace/compiler/examples/fibonacci/src/lib.rs",
            "other/src/lib.rs",
            &trim_prefixes,
        ));
    }

    #[test]
    fn resolve_breakpoint_line_moves_to_next_executable_line() {
        let lines = BTreeSet::from([39, 40, 41]);

        assert_eq!(resolve_breakpoint_line(&lines, 38), Some(39));
        assert_eq!(resolve_breakpoint_line(&lines, 40), Some(40));
        assert_eq!(resolve_breakpoint_line(&lines, 99), Some(41));
        assert_eq!(resolve_breakpoint_line(&BTreeSet::new(), 38), None);
    }

    #[test]
    fn source_var_is_visible_after_its_declaration_line() {
        let prefixes = Vec::new();
        let path = "/tmp/src/lib.rs";

        assert!(!source_var_location_is_visible(path, 40, path, 39, &prefixes));
        assert!(!source_var_location_is_visible(path, 40, path, 40, &prefixes));
        assert!(source_var_location_is_visible(path, 40, path, 41, &prefixes));
    }

    #[test]
    fn next_source_line_ignores_pre_body_mappings() {
        let prefixes = Vec::new();
        let start = ("/tmp/src/lib.rs".to_string(), 39);
        let pre_body = ("/tmp/src/lib.rs".to_string(), 1);
        let body = ("/tmp/src/lib.rs".to_string(), 40);

        assert!(!is_next_source_line(
            Some("entrypoint"),
            Some(&start),
            Some("entrypoint"),
            Some(&pre_body),
            &prefixes,
            Some(39),
        ));
        assert!(is_next_source_line(
            Some("entrypoint"),
            Some(&start),
            Some("entrypoint"),
            Some(&body),
            &prefixes,
            Some(39),
        ));
    }
}
