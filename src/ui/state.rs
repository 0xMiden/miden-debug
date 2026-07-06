use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use miden_assembly::{DefaultSourceManager, SourceManager};
use miden_assembly_syntax::diagnostics::Report;
use miden_core::{
    mast::{MastNode, MastNodeId},
    operations::AssemblyOp,
    program::Program,
};
use miden_debug_types::{Location, SourceManagerExt, SourceSpan};
use miden_processor::{
    Felt, StackInputs,
    advice::{AdviceInputs, AdviceMutation},
    mast::MastForest,
};

use crate::{
    config::DebuggerConfig,
    debug::{Breakpoint, BreakpointType, ReadMemoryExpr, ResolvedLocation, resolve_variable_value},
    exec::{DebugExecutor, Executor},
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

fn clone_event_replay_queue(event_replay: &[Vec<AdviceMutation>]) -> VecDeque<Vec<AdviceMutation>> {
    event_replay
        .iter()
        .map(|batch| crate::exec::clone_advice_mutations(batch))
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
        use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

        use miden_debug_engine::debug::DebugVarTracker;
        use miden_processor::{ContextId, FastProcessor};

        use crate::exec::DebuggerHost;

        let mut client = crate::exec::DapClient::connect(addr).map_err(Report::msg)?;
        let ui_state = client.handshake().map_err(Report::msg)?;
        let snapshot = convert_ui_state(&ui_state, source_manager);

        let debug_vars = DebugVarTracker::new(Rc::new(RefCell::new(Default::default())));
        let executor = DebugExecutor {
            processor: FastProcessor::new(StackInputs::default()),
            host: DebuggerHost::new(source_manager.clone()),
            resume_ctx: None,
            current_stack: snapshot.current_stack,
            current_op: None,
            current_asmop: None,
            stack_outputs: Default::default(),
            contexts: BTreeSet::new(),
            root_context: ContextId::root(),
            current_context: ContextId::root(),
            callstack: snapshot.callstack,
            current_proc: None,
            debug_vars,
            last_debug_var_count: 0,
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
        // Collect Called and File patterns as function breakpoints.
        let mut func_names: Vec<String> = Vec::new();

        for bp in breakpoints {
            match &bp.ty {
                BreakpointType::Line { pattern, line } => {
                    by_file.entry(pattern.as_str().to_string()).or_default().push(*line as i64);
                }
                BreakpointType::Called(pattern) | BreakpointType::File(pattern) => {
                    func_names.push(pattern.as_str().to_string());
                }
                _ => {}
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

        // Send function/pattern breakpoints (replaces the full set each time).
        let _ = self.client.set_function_breakpoints(&func_names);

        // Update tracked set.
        self.synced_bp_files = by_file.into_keys().collect();
    }

    fn resume(&mut self, breakpoints: &[Breakpoint]) -> Result<crate::exec::DapStopReason, String> {
        // Sync user-defined breakpoints to the DAP server before choosing a step command.
        self.sync_breakpoints(breakpoints);

        let has_step = breakpoints.iter().any(|bp| matches!(bp.ty, BreakpointType::Step));
        let has_next = breakpoints
            .iter()
            .any(|bp| matches!(bp.ty, BreakpointType::Next | BreakpointType::NextLine));
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
        let local = create_local_state(&config, source_manager.clone())?;

        Ok(Self::new_local(source_manager, config, DebugMode::Program, local))
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
        let mut executor = Executor::new(args);
        executor.with_advice_inputs(advice_inputs);
        let debug_executor = executor.into_debug_with_replay(
            &program,
            source_manager.clone(),
            mast_forests,
            clone_event_replay_queue(&event_replay),
        );

        Ok(Self::new_local(
            source_manager,
            Box::default(),
            DebugMode::Transaction,
            LocalState {
                executor: debug_executor,
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
        let local = create_local_state(&self.config, self.source_manager.clone())?;

        self.session = SessionState::Local(Box::new(local));
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

    /// Resume local execution until the VM terminates, errors, or a breakpoint is hit.
    pub fn run_until_stopped(&mut self) {
        let start_cycle = self.executor().cycle;
        let start_asmop = self.executor().current_asmop.clone();
        let start_proc = self.current_procedure();
        let start_line_loc = self.current_display_location();
        let source_path_prefixes = self.source_path_prefixes();
        let minimum_source_line =
            start_proc.as_deref().zip(start_line_loc.as_ref()).and_then(|(proc, loc)| {
                self.minimum_source_line_for_proc(proc, loc.source_file.uri().as_str())
            });
        let mut previous_proc = self.current_procedure();
        let mut pending_called_breakpoints = Vec::new();
        let mut breakpoints = core::mem::take(&mut self.breakpoints);
        self.breakpoints_hit.clear();
        self.stopped = false;

        let stopped = loop {
            if self.executor().stopped {
                break true;
            }

            let mut consume_most_recent_finish = false;
            match self.executor_mut().step() {
                Ok(Some(exited)) if exited.should_break_on_exit() => {
                    consume_most_recent_finish = true;
                }
                Ok(_) => {}
                Err(err) => {
                    self.set_execution_failed(err);
                    break true;
                }
            }

            if breakpoints.is_empty() {
                continue;
            }

            let is_op_boundary = self.executor().current_asmop.is_some();
            let loc = self.current_location();
            let line_loc = self.current_display_location();
            let proc = self.current_procedure();
            let current_cycle = self.executor().cycle;
            let cycles_stepped = current_cycle - start_cycle;
            let has_internal_breakpoint = breakpoints.iter().any(|bp| bp.is_internal());

            breakpoints.retain_mut(|bp| {
                if let Some(n) = bp.cycles_to_skip(current_cycle) {
                    if cycles_stepped >= n {
                        let retained = !bp.is_one_shot();
                        if retained {
                            self.breakpoints_hit.push(bp.clone());
                        } else {
                            self.breakpoints_hit.push(core::mem::take(bp));
                        }
                        return retained;
                    }
                    return true;
                }

                if cycles_stepped > 0
                    && is_op_boundary
                    && matches!(&bp.ty, BreakpointType::Next)
                    && self.executor().current_asmop != start_asmop
                {
                    self.breakpoints_hit.push(core::mem::take(bp));
                    return false;
                }

                if cycles_stepped > 0
                    && is_op_boundary
                    && matches!(&bp.ty, BreakpointType::NextLine)
                    && Self::is_next_source_line(
                        start_proc.as_deref(),
                        start_line_loc.as_ref(),
                        proc.as_deref(),
                        line_loc.as_ref(),
                        &source_path_prefixes,
                        minimum_source_line,
                    )
                {
                    self.breakpoints_hit.push(core::mem::take(bp));
                    return false;
                }

                if has_internal_breakpoint && !bp.is_internal() {
                    return true;
                }

                if let Some(loc) = loc.as_ref()
                    && bp.should_break_at(loc)
                {
                    let retained = !bp.is_one_shot();
                    if retained {
                        self.breakpoints_hit.push(bp.clone());
                    } else {
                        self.breakpoints_hit.push(core::mem::take(bp));
                    }
                    return retained;
                }

                if matches!(&bp.ty, BreakpointType::Called(_))
                    && let Some(proc) = proc.as_deref()
                {
                    let matched = bp.should_break_in(proc);
                    if !matched {
                        pending_called_breakpoints.retain(|id| *id != bp.id);
                        return true;
                    }

                    let was_matched = previous_proc
                        .as_deref()
                        .is_some_and(|previous| bp.should_break_in(previous));
                    let matched_at_start =
                        start_proc.as_deref().is_some_and(|start| bp.should_break_in(start));
                    let pending = pending_called_breakpoints.contains(&bp.id);
                    let entered_matching_proc = !was_matched && !matched_at_start;

                    if entered_matching_proc
                        && self.should_defer_called_breakpoint(proc, line_loc.as_ref())
                    {
                        if !pending {
                            pending_called_breakpoints.push(bp.id);
                        }
                        return true;
                    }

                    if entered_matching_proc
                        || (pending && self.deferred_called_breakpoint_is_ready(line_loc.as_ref()))
                    {
                        pending_called_breakpoints.retain(|id| *id != bp.id);
                        let retained = !bp.is_one_shot();
                        if retained {
                            self.breakpoints_hit.push(bp.clone());
                        } else {
                            self.breakpoints_hit.push(core::mem::take(bp));
                        }
                        return retained;
                    }
                }

                true
            });

            if consume_most_recent_finish
                && let Some(id) = breakpoints.iter().rev().find_map(|bp| {
                    if matches!(bp.ty, BreakpointType::Finish) {
                        Some(bp.id)
                    } else {
                        None
                    }
                })
            {
                breakpoints.retain(|bp| bp.id != id);
                break true;
            }

            if !self.breakpoints_hit.is_empty() {
                break true;
            }

            previous_proc = proc;
        };

        self.breakpoints = breakpoints;
        self.stopped = stopped;
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

    pub fn current_procedure(&self) -> Option<Rc<str>> {
        let live_proc = self
            .executor()
            .current_asmop
            .as_ref()
            .map(|op| Rc::from(op.context_name()))
            .or_else(|| self.executor().current_proc.clone());
        let frame_proc =
            self.executor().callstack.current_frame().and_then(|frame| frame.procedure(""));
        live_proc.or(frame_proc)
    }

    pub fn current_location(&self) -> Option<ResolvedLocation> {
        self.executor()
            .callstack
            .current_frame()
            .and_then(|frame| frame.recent().back())
            .and_then(|detail| self.resolve_op_location(detail.location()?))
    }

    pub fn current_display_location(&self) -> Option<ResolvedLocation> {
        let frame = self.executor().callstack.current_frame()?;
        for detail in frame.recent().iter().rev() {
            if let Some(location) = detail.location()
                && let Some(resolved) = self.resolve_op_location(location)
            {
                return Some(resolved);
            }
        }
        None
    }

    pub fn is_next_source_line(
        start_proc: Option<&str>,
        start_loc: Option<&ResolvedLocation>,
        current_proc: Option<&str>,
        current_loc: Option<&ResolvedLocation>,
        source_path_prefixes: &[String],
        minimum_source_line: Option<u32>,
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
            && current.line < minimum_source_line
        {
            return false;
        }

        match (start_loc, current_loc) {
            (Some(start), Some(current)) => {
                source_paths_match(
                    start.source_file.uri().as_str(),
                    current.source_file.uri().as_str(),
                    source_path_prefixes,
                ) && start.line != current.line
            }
            (None, Some(_)) => true,
            _ => false,
        }
    }

    pub(crate) fn minimum_source_line_for_proc(
        &self,
        procedure: &str,
        source_path: &str,
    ) -> Option<u32> {
        let forest = self.executor().resume_ctx.as_ref()?.current_forest();
        let source_path_prefixes = self.source_path_prefixes();
        let mut min_line = None;

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
                let Some((path, line)) = self.resolve_asmop_location(asmop) else {
                    continue;
                };
                if line > 1 && source_paths_match(&path, source_path, &source_path_prefixes) {
                    min_line = Some(min_line.map_or(line, |current: u32| current.min(line)));
                }
            }
        }

        min_line
    }

    pub(crate) fn source_path_prefixes(&self) -> Vec<String> {
        #[cfg(feature = "dap")]
        {
            let mut prefixes = self
                .config
                .source_path_prefixes
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            if let Ok(cwd) = std::env::current_dir() {
                let cwd = cwd.to_string_lossy().into_owned();
                if !prefixes
                    .iter()
                    .any(|prefix| normalize_source_path(prefix) == normalize_source_path(&cwd))
                {
                    prefixes.push(cwd);
                }
            }
            prefixes
        }

        #[cfg(not(feature = "dap"))]
        {
            Vec::new()
        }
    }

    fn resolve_op_location(&self, loc: &Location) -> Option<ResolvedLocation> {
        let source_file = self.load_source_file_for_uri(loc.uri())?;
        let span = SourceSpan::new(source_file.id(), loc.start..loc.end);
        let file_line_col = source_file.location(span);
        Some(ResolvedLocation {
            source_file,
            line: file_line_col.line.to_u32(),
            col: file_line_col.column.to_u32(),
            span,
        })
    }

    fn resolve_asmop_location(&self, asmop: &AssemblyOp) -> Option<(String, u32)> {
        let resolved = self.resolve_op_location(asmop.location()?)?;
        Some((resolved.source_file.uri().as_str().to_string(), resolved.line))
    }

    fn load_source_file_for_uri(
        &self,
        uri: &miden_debug_types::Uri,
    ) -> Option<Arc<miden_debug_types::SourceFile>> {
        let uri_str = uri.as_str();
        let normalized_uri = uri_str.strip_prefix("file://").unwrap_or(uri_str);
        let path = Path::new(normalized_uri);
        if path.exists() {
            return self.source_manager.load_file(path).ok();
        }

        if let Some(source_file) = self.source_manager.get_by_uri(uri) {
            return Some(source_file);
        }

        for candidate in source_path_candidates(normalized_uri, &self.source_path_prefixes()) {
            if candidate.exists()
                && let Ok(source_file) = self.source_manager.load_file(&candidate)
            {
                return Some(source_file);
            }
        }

        None
    }

    pub fn should_defer_called_breakpoint(
        &self,
        proc: &str,
        current_loc: Option<&ResolvedLocation>,
    ) -> bool {
        let executor = self.executor();
        (!is_internal_procedure(proc)
            && current_loc
                .is_none_or(|loc| crate::debug::is_internal_source_uri(loc.source_file.uri())))
            || (executor.procedure_has_debug_vars(proc) && executor.last_debug_var_count == 0)
    }

    pub fn deferred_called_breakpoint_is_ready(
        &self,
        current_loc: Option<&ResolvedLocation>,
    ) -> bool {
        current_loc.is_some_and(|loc| !crate::debug::is_internal_source_uri(loc.source_file.uri()))
            || self.executor().last_debug_var_count > 0
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

        let executor = self.executor();
        let cycle = miden_processor::trace::RowIndex::from(executor.cycle);
        let context = executor.current_context;
        let memory = executor.processor.memory();
        let read_element = |addr: u32| -> Option<Felt> {
            memory
                .read_element(context, Felt::new(addr as u64).expect("value exceeds field modulus"))
                .ok()
        };
        let mut output = String::new();
        if expr.count > 1 {
            return Err("-count with value > 1 is not yet implemented".into());
        } else if matches!(expr.ty, Type::Felt) {
            if !expr.addr.is_element_aligned() {
                return Err(
                    "read failed: type 'felt' must be aligned to an element boundary".into()
                );
            }
            let felt = read_element(expr.addr.addr).unwrap_or(Felt::ZERO);
            write_with_format_type!(output, expr, felt.as_canonical_u64());
        } else if matches!(
            expr.ty,
            Type::Array(ref array_ty) if array_ty.element_type() == &Type::Felt && array_ty.len() == 4
        ) {
            if !expr.addr.is_word_aligned() {
                return Err("read failed: type 'word' must be aligned to a word boundary".into());
            }
            let word = memory
                .read_word(
                    context,
                    Felt::new(expr.addr.addr as u64).expect("value exceeds field modulus"),
                    cycle,
                )
                .unwrap_or_default();
            output.push('[');
            for (i, elem) in word.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                write_with_format_type!(output, expr, elem.as_canonical_u64());
            }
            output.push(']');
        } else {
            if !expr.addr.is_element_aligned() {
                return Err("invalid read: unaligned reads are not supported yet".into());
            }

            const U32_MASK: u64 = u32::MAX as u64;
            let size = expr.ty.size_in_bytes();
            let size_in_felts = expr.ty.size_in_felts();
            let mut bytes = Vec::with_capacity(size);
            let mut needed = size;
            for i in 0..size_in_felts {
                let addr = expr.addr.addr.checked_add(i as u32).ok_or_else(|| {
                    "invalid read: attempted to read beyond end of linear memory".to_string()
                })?;
                let elem = read_element(addr).unwrap_or_default();
                let elem_bytes = ((elem.as_canonical_u64() & U32_MASK) as u32).to_le_bytes();
                let take = core::cmp::min(needed, 4);
                bytes.extend(&elem_bytes[..take]);
                needed -= take;
            }

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

    /// Format the current debug variables as a string for display.
    ///
    /// When `show_all` is false, compiler-generated locals (named `local0`, `local1`, etc.)
    /// are hidden. Use `show_all` = true (`:vars all`) to include them.
    pub fn format_variables(&self, show_all: bool) -> String {
        use core::fmt::Write;

        let executor = self.executor();
        let debug_vars = &executor.debug_vars;

        let mut output = String::new();
        let stack = executor.current_stack.clone();
        let context = executor.current_context;

        // Use live processor state, not the pre-recorded trace, for current-cycle values.
        let read_mem = |addr: u32| -> Option<Felt> {
            executor
                .processor
                .memory()
                .read_element(context, Felt::new(addr as u64).expect("value exceeds field modulus"))
                .ok()
        };

        let current_source = if show_all {
            None
        } else {
            self.current_display_location()
        };
        let source_path_prefixes = self.source_path_prefixes();

        if !debug_vars.has_variables() {
            return "No debug variables tracked".to_string();
        }

        for var_snapshot in debug_vars.current_variables() {
            let name = var_snapshot.info.name();

            if !show_all && is_compiler_generated_name(name) {
                continue;
            }

            if let (Some(current), Some(var_loc)) =
                (current_source.as_ref(), var_snapshot.info.location())
                && !source_var_location_is_visible(
                    var_loc.uri.as_str(),
                    var_loc.line.to_u32(),
                    current.source_file.uri().as_str(),
                    current.line,
                    &source_path_prefixes,
                )
            {
                continue;
            }

            if !output.is_empty() {
                output.push_str(", ");
            }

            let location = var_snapshot.info.value_location();

            let value = resolve_variable_value(location, &stack, read_mem, |offset| {
                // Read FMP from live memory, then compute address as FMP + offset
                let fmp_addr = miden_core::FMP_ADDR.as_canonical_u64() as u32;
                let fmp = read_mem(fmp_addr)?;
                let addr = (fmp.as_canonical_u64() as i64 + offset as i64) as u32;
                read_mem(addr)
            });

            match value {
                Some(felt) => {
                    write!(&mut output, "{name}={}", felt.as_canonical_u64()).unwrap();
                }
                None => {
                    write!(&mut output, "{name}={location}").unwrap();
                }
            }
        }

        if output.is_empty() {
            "No source-level variables (use ':vars all' to show compiler locals)".to_string()
        } else {
            output
        }
    }
}

fn is_internal_procedure(proc: &str) -> bool {
    proc.contains("::intrinsics::")
}

/// Returns true if the variable name looks compiler-generated (e.g. "local0", "local12").
/// Source-level variables have DWARF-derived names like "a", "sum", "_info".
fn is_compiler_generated_name(name: &str) -> bool {
    name.strip_prefix("local")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()))
}

fn source_var_location_is_visible(
    var_path: &str,
    var_line: u32,
    current_path: &str,
    current_line: u32,
    source_path_prefixes: &[String],
) -> bool {
    source_paths_match(var_path, current_path, source_path_prefixes) && var_line < current_line
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

fn source_path_candidates(uri: &str, source_path_prefixes: &[String]) -> Vec<PathBuf> {
    let normalized = normalize_source_path(uri);
    if normalized.is_empty() || Path::new(&normalized).is_absolute() {
        return Vec::new();
    }

    source_path_prefixes
        .iter()
        .map(|prefix| Path::new(prefix).join(&normalized))
        .collect()
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

    let current_stack = snapshot
        .current_stack
        .iter()
        .copied()
        .map(|v| Felt::new(v).expect("value exceeds field modulus"))
        .collect();

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

    use miden_debug_types::{SourceManagerExt, SourceSpan, Uri};

    let path_str = frame.source_path.as_ref()?;
    let path = crate::debug::resolve_source_path(&Uri::new(path_str))
        .unwrap_or_else(|| Path::new(path_str).to_path_buf());
    let source_file = source_manager.load_file(&path).ok()?;
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

fn create_local_state(
    config: &DebuggerConfig,
    source_manager: Arc<dyn SourceManager>,
) -> Result<LocalState, Report> {
    let executor = crate::program_loader::load_debug_executor(config, source_manager, "state")?;
    Ok(LocalState {
        executor,
        execution_failed: None,
    })
}
