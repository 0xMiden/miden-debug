use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    fmt,
    ops::Deref,
    rc::Rc,
    sync::{Arc, Mutex},
};

use log::Level;
use miden_assembly_syntax::diagnostics::Report;
use miden_core::{operations::DebugVarInfo, program::StackInputs};
use miden_debug_types::{SourceManager, SourceManagerExt};
use miden_mast_package::Package;
use miden_package_registry::PackageCache;
use miden_processor::{
    ContextId, ExecutionError, ExecutionOptions, FastProcessor, Felt, LoadedMastForest,
    ProcessorState,
    advice::{AdviceInputs, AdviceMutation},
    event::{EventError, EventHandler, EventName},
    trace::RowIndex,
};

use super::{
    DebugExecutor, DebuggerHost, Event, ExecutionConfig, ExecutionTrace,
    event::{FRAME_END_EVENT, FRAME_START_EVENT, PRINTLN_EVENT},
    query::read_memory_bytes,
};
use crate::{
    HybridPackageRegistry,
    debug::{CallStack, DebugVarTracker, NativePtr},
    felt::FromMidenRepr,
    profiling::{Profiler, ProfilerConfig},
};

/// Maximum number of bytes for a single `println` output.
///
/// A limit is required as `u32::MAX` exceeds the size that strings can take in Miden VM. The limit
/// is generous and still permits use cases like formatting a large amount of data in storage.
///
/// Exceeding the limit likely indicates a bug in the corresponding trace event handling.
const MAX_PRINTLN_BYTES: usize = 512 * 1024;

/// The [Executor] is responsible for executing a program with the Miden VM.
///
/// It is used by either converting it into a [DebugExecutor], and using that to
/// manage execution step-by-step, such as is done by the debugger; or by running
/// the program to completion and obtaining an [ExecutionTrace], which can be used
/// to introspect the final program state.
pub struct Executor {
    stack: StackInputs,
    advice: AdviceInputs,
    options: ExecutionOptions,
    event_handlers: Vec<(EventName, Arc<dyn EventHandler>)>,
    registry: HybridPackageRegistry,
    record_event_mutations: bool,
    profiler_config: ProfilerConfig,
}

impl Executor {
    /// Construct an executor with the given arguments on the operand stack
    pub fn new(args: Vec<Felt>) -> Self {
        let config = ExecutionConfig {
            inputs: StackInputs::new(&args).expect("invalid stack inputs"),
            ..Default::default()
        };

        Self::from_config(config)
    }

    /// Construct an executor from the given configuration
    ///
    /// NOTE: The execution options for tracing/debugging will be set to true for you
    pub fn from_config(config: ExecutionConfig) -> Self {
        let ExecutionConfig {
            inputs,
            advice_inputs,
            options,
        } = config;

        Self {
            stack: inputs,
            advice: advice_inputs,
            options,
            event_handlers: Default::default(),
            registry: HybridPackageRegistry::empty(),
            record_event_mutations: false,
            profiler_config: Default::default(),
        }
    }

    #[inline]
    pub fn with_registry(mut self, registry: HybridPackageRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Set the contents of memory for the shadow stack frame of the entrypoint
    pub fn with_advice_inputs(&mut self, advice: AdviceInputs) -> &mut Self {
        self.advice.extend(advice);
        self
    }

    /// Add a [Package] to the execution context
    pub fn with_package(&mut self, package: Arc<Package>) -> Result<&mut Self, Report> {
        self.registry.cache_package(package)?;
        Ok(self)
    }

    /// Record the advice mutations produced by each event handler invocation during execution.
    ///
    /// Recording is a private detail of the debug host created by [Executor::into_debug]: once
    /// the program completes, take the log via [DebuggerHost::take_recorded_event_mutations] on
    /// the [DebugExecutor]'s host, and feed it back into [Executor::into_debug_with_replay] to
    /// debug the same execution later without the original event handlers (e.g. transaction
    /// debugging with event replay).
    ///
    /// Mutations are only recorded for live event handling; nothing is recorded while an event
    /// replay queue is being consumed.
    pub fn with_event_advice_mutations_recording(&mut self) -> &mut Self {
        self.record_event_mutations = true;
        self
    }

    /// Register a VM event handler to be available during execution.
    pub fn register_event_handler(
        &mut self,
        event: EventName,
        handler: Arc<dyn EventHandler>,
    ) -> Result<&mut Self, ExecutionError> {
        self.event_handlers.push((event, handler));
        Ok(self)
    }

    /// Set the profiler configuration for this executor.
    pub fn with_profiler_config(&mut self, profiler_config: ProfilerConfig) -> &mut Self {
        self.profiler_config = profiler_config;
        self
    }

    /// Convert this [Executor] into a [DebugExecutor], which captures much more information
    /// about the program being executed, and must be stepped manually.
    pub fn into_debug(
        mut self,
        package: Arc<Package>,
        source_manager: Arc<dyn SourceManager>,
    ) -> DebugExecutor {
        assert!(package.is_program());

        log::debug!("creating debug executor");

        let mut host = DebuggerHost::new(source_manager.clone());
        for lib in self.registry.all() {
            host.load_package(lib);
        }
        for (event, handler) in core::mem::take(&mut self.event_handlers) {
            host.register_event_handler(event, handler)
                .expect("failed to register debug executor event handler");
        }
        if self.record_event_mutations {
            host = host.with_event_advice_mutations_recording();
        }

        let events: Arc<Mutex<BTreeMap<RowIndex, Event>>> = Arc::new(Default::default());
        register_builtin_event_handlers(&mut host, Arc::clone(&events));

        // Set up debug variable tracking
        let debug_var_events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        let mut processor = FastProcessor::new_with_options(self.stack, self.advice, self.options)
            .expect("advice inputs should fit advice map limits");

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context_for_package(package)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new(events);
        let debug_vars = DebugVarTracker::new(debug_var_events);
        DebugExecutor {
            processor,
            host,
            resume_ctx: Some(resume_ctx),
            current_stack: vec![],
            current_op: None,
            current_asmop: None,
            stack_outputs: Default::default(),
            contexts: Default::default(),
            root_context,
            current_context: root_context,
            callstack,
            current_proc: None,
            debug_vars,
            last_debug_var_count: 0,
            recent: VecDeque::with_capacity(5),
            cycle: 0,
            stopped: false,
            profiler: Profiler::from_config(self.profiler_config),
        }
    }

    /// Convert this [Executor] into a [DebugExecutor] with event replay support.
    ///
    /// Like [`into_debug`](Self::into_debug), but additionally:
    /// - Loads `extra_forests` into the host's MAST forest store
    /// - Sets the event replay queue so that `on_event()` returns pre-recorded mutations
    ///
    /// This is used for transaction debugging where events were recorded during a prior
    /// execution with the real transaction host.
    pub fn into_debug_with_replay(
        self,
        package: Arc<Package>,
        source_manager: Arc<dyn SourceManager>,
        extra_mast_forests: Vec<LoadedMastForest>,
        event_replay: VecDeque<Vec<AdviceMutation>>,
    ) -> DebugExecutor {
        assert!(package.is_program());

        log::debug!("creating debug executor with event replay");

        let mut host = DebuggerHost::new(source_manager.clone());
        for lib in self.registry.all() {
            host.load_package(lib);
        }
        for forest in extra_mast_forests {
            host.load_mast_forest(forest);
        }
        host.set_event_replay(event_replay);

        let debug_var_events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        let events: Arc<Mutex<BTreeMap<RowIndex, Event>>> = Arc::new(Default::default());
        register_builtin_event_handlers(&mut host, Arc::clone(&events));

        let mut processor = FastProcessor::new_with_options(self.stack, self.advice, self.options)
            .expect("advice inputs should fit advice map limits");

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context_for_package(package)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new(events);
        let debug_vars = DebugVarTracker::new(debug_var_events);
        DebugExecutor {
            processor,
            host,
            resume_ctx: Some(resume_ctx),
            current_stack: vec![],
            current_op: None,
            current_asmop: None,
            stack_outputs: Default::default(),
            contexts: Default::default(),
            root_context,
            current_context: root_context,
            callstack,
            current_proc: None,
            debug_vars,
            last_debug_var_count: 0,
            recent: VecDeque::with_capacity(5),
            cycle: 0,
            stopped: false,
            profiler: Profiler::from_config(self.profiler_config),
        }
    }

    /// Execute the given program until termination, producing a trace
    pub fn capture_trace(
        self,
        package: Arc<Package>,
        source_manager: Arc<dyn SourceManager>,
    ) -> ExecutionTrace {
        let mut executor = self.into_debug(package, source_manager);
        loop {
            if executor.stopped {
                break;
            }
            match executor.step() {
                Ok(_) => continue,
                Err(err) => {
                    log::warn!(
                        target: "executor",
                        "capture_trace stopped early at cycle {}: {err}",
                        executor.cycle,
                    );
                    break;
                }
            }
        }
        executor.into_execution_trace()
    }

    /// Execute the given program, producing a trace
    #[track_caller]
    pub fn execute(
        self,
        package: Arc<Package>,
        source_manager: Arc<dyn SourceManager>,
    ) -> ExecutionTrace {
        let mut executor = self.into_debug(package, source_manager.clone());
        loop {
            if executor.stopped {
                break;
            }
            match executor.step() {
                Ok(_) => {
                    if log::log_enabled!(target: "executor", log::Level::Trace)
                        && let (Some(op), Some(asmop)) =
                            (executor.current_op, executor.current_asmop.as_ref())
                    {
                        log::trace!(target: "executor", "stack: {:?}", executor.current_stack);
                        let source_loc = asmop.location().map(|loc| {
                            let path = std::path::Path::new(loc.uri().path());
                            let file = source_manager.load_file(path).unwrap();
                            (file, loc.start)
                        });
                        if let Some((source_file, line_start)) = source_loc {
                            let line_number = source_file.content().line_index(line_start).number();
                            log::trace!(target: "executor", "in {} (located at {}:{})", asmop.context_name(), source_file.deref().uri().as_str(), line_number);
                        } else {
                            log::trace!(target: "executor", "in {} (no source location available)", asmop.context_name());
                        }
                        log::trace!(target: "executor", "  executed `{op:?}` of `{}` ({} cycles)", asmop.op(), asmop.num_cycles());
                        log::trace!(target: "executor", "  stack state: {:#?}", executor.current_stack);
                    }
                }
                Err(err) => {
                    render_execution_error(err, &executor, &source_manager);
                }
            }
        }

        executor.into_execution_trace()
    }

    /// Execute a program, parsing the operand stack outputs as a value of type `T`
    pub fn execute_into<T>(self, package: Arc<Package>, source_manager: Arc<dyn SourceManager>) -> T
    where
        T: FromMidenRepr + PartialEq,
    {
        let out = self.execute(package, source_manager);
        out.parse_result().expect("invalid result")
    }
}

#[derive(Debug, thiserror::Error)]
enum PrintLnError {
    #[error("address should fit in u32")]
    InvalidAddress,
    #[error("string length should fit in usize")]
    InvalidLength,
    #[error("string length {requested} exceeds maximum {max}")]
    LengthExceeded { requested: usize, max: usize },
    #[error("memory is not initialized")]
    MemoryNotInitialized,
    #[error("failed to read memory: {0}")]
    MemoryRead(#[from] super::trace::MemoryReadError),
    #[error("invalid UTF-8")]
    InvalidUtf8,
}

fn register_builtin_event_handlers(
    host: &mut DebuggerHost<dyn SourceManager>,
    events: Arc<Mutex<BTreeMap<RowIndex, Event>>>,
) {
    let println_handler = |process: &ProcessorState| -> Result<Vec<AdviceMutation>, EventError> {
        match decode_println(process) {
            Ok(content) => {
                log::log!(target: "stdout", Level::Info, "{content}");
            }
            Err(err) => {
                log::warn!(
                    target: "executor",
                    "emit.{PRINTLN_EVENT} failed at cycle {}: {err}",
                    process.clock(),
                );
            }
        }

        Ok(vec![])
    };

    // Keep builtin event handlers in sync with `Event::has_builtin_handler`

    host.register_event_handler(PRINTLN_EVENT, Arc::new(println_handler))
        .expect("failed to register println event handler");

    let frame_start_events = Arc::clone(&events);
    let frame_start_handler =
        move |process: &ProcessorState| -> Result<Vec<AdviceMutation>, EventError> {
            frame_start_events.lock().unwrap().insert(process.clock(), Event::FrameStart);
            Ok(vec![])
        };
    host.register_event_handler(FRAME_START_EVENT, Arc::new(frame_start_handler))
        .expect("failed to register frame start event handler");

    let frame_end_events = Arc::clone(&events);
    let frame_end_handler =
        move |process: &ProcessorState| -> Result<Vec<AdviceMutation>, EventError> {
            frame_end_events.lock().unwrap().insert(process.clock(), Event::FrameEnd);
            Ok(vec![])
        };
    host.register_event_handler(FRAME_END_EVENT, Arc::from(frame_end_handler))
        .expect("failed to register frame end event handler");

    /*
    let assertion_events = Rc::clone(&events);
    host.register_assert_failed_tracer(move |process, event| {
        assertion_events.borrow_mut().insert(process.clock(), event);
    });
     */
}

/// Decode a [`Event::PrintLn`] event into a UTF-8 string.
///
/// Expects `[event_id, address, length]` on the operand stack. Reads `length` bytes from `address`
/// in the current context's memory and returns them as a string.
fn decode_println(process: &ProcessorState<'_>) -> Result<String, PrintLnError> {
    let addr = u32::try_from(process.get_stack_item(1).as_canonical_u64())
        .map_err(|_| PrintLnError::InvalidAddress)?;
    let len = usize::try_from(process.get_stack_item(2).as_canonical_u64())
        .map_err(|_| PrintLnError::InvalidLength)?;
    if len > MAX_PRINTLN_BYTES {
        return Err(PrintLnError::LengthExceeded {
            requested: len,
            max: MAX_PRINTLN_BYTES,
        });
    }
    let ptr = NativePtr::from_ptr(addr);
    let ctx = process.ctx();

    let bytes = read_memory_bytes(ptr, len, |addr| {
        process.get_mem_value(ctx, addr).ok_or(PrintLnError::MemoryNotInitialized)
    })?;

    String::from_utf8(bytes).map_err(|_| PrintLnError::InvalidUtf8)
}

#[track_caller]
fn render_execution_error(
    err: ExecutionError,
    execution_state: &DebugExecutor,
    source_manager: &dyn SourceManager,
) -> ! {
    use miden_assembly_syntax::diagnostics::{
        LabeledSpan, miette::miette, reporting::PrintDiagnostic,
    };

    let stacktrace = execution_state.callstack.stacktrace(&execution_state.recent, source_manager);

    eprintln!("{stacktrace}");

    if !execution_state.current_stack.is_empty() {
        let stack = execution_state.current_stack.iter().map(|elem| elem.as_canonical_u64());
        let stack = DisplayValues::new(stack);
        eprintln!(
            "\nLast Known State (at most recent instruction which succeeded):
 | Operand Stack: [{stack}]
 "
        );

        let mut labels = vec![];
        if let Some(span) = stacktrace
            .current_frame()
            .and_then(|frame| frame.location.as_ref())
            .map(|loc| loc.span)
        {
            labels.push(LabeledSpan::new_with_span(
                None,
                span.start().to_usize()..span.end().to_usize(),
            ));
        }
        let report = miette!(
            labels = labels,
            "program execution failed at step {step} (cycle {cycle}): {err}",
            step = execution_state.cycle,
            cycle = execution_state.cycle,
        );
        let report = match stacktrace
            .current_frame()
            .and_then(|frame| frame.location.as_ref())
            .map(|loc| loc.source_file.clone())
        {
            Some(source) => report.with_source_code(source),
            None => report,
        };

        panic!("{}", PrintDiagnostic::new(report));
    } else {
        panic!("program execution failed at step {step}: {err}", step = execution_state.cycle);
    }
}

/// Render an iterator of `T`, comma-separated
struct DisplayValues<T>(Cell<Option<T>>);

impl<T> DisplayValues<T> {
    pub fn new(inner: T) -> Self {
        Self(Cell::new(Some(inner)))
    }
}

impl<T, I> fmt::Display for DisplayValues<I>
where
    T: fmt::Display,
    I: Iterator<Item = T>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let iter = self.0.take().unwrap();
        for (i, item) in iter.enumerate() {
            if i == 0 {
                write!(f, "{item}")?;
            } else {
                write!(f, ", {item}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One entry per `on_event` invocation, in execution order, and the recorded log replays to
    /// an identical result without the original event handlers.
    #[test]
    fn records_event_mutations_and_replays_them() {
        use std::sync::atomic::{AtomicU64, Ordering};

        use miden_assembly::DefaultSourceManager;
        use miden_core::events::EventId;
        use miden_processor::{ProcessorState, advice::AdviceMutation, event::EventError};

        struct CountingHandler {
            calls: AtomicU64,
        }

        impl EventHandler for CountingHandler {
            fn on_event(
                &self,
                _process: &ProcessorState<'_>,
            ) -> Result<Vec<AdviceMutation>, EventError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![AdviceMutation::ExtendStack {
                    values: vec![Felt::from(100u32 + call as u32)],
                }])
            }
        }

        let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
        let event_name = "miden-debug::test::record-replay";
        let event_id = EventId::from_name(event_name).as_u64();
        // Each emit invokes the handler, which pushes one value onto the advice stack;
        // adv_push moves it to the operand stack, and the sum of both values is the result.
        let source = format!(
            "begin push.{event_id} emit drop adv_push push.{event_id} emit drop adv_push add swap \
             drop end"
        );
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program("program", source)
            .map(Arc::from)
            .expect("failed to assemble test program");

        let mut executor = Executor::new(Vec::new());
        executor
            .register_event_handler(
                EventName::from_string(event_name.to_string()),
                Arc::new(CountingHandler {
                    calls: AtomicU64::new(0),
                }),
            )
            .expect("failed to register event handler");
        executor.with_event_advice_mutations_recording();

        // Run to completion through the debug executor: recording is an internal detail of its
        // host, and the log is taken from the host once execution finishes.
        let mut debug_executor = executor.into_debug(Arc::clone(&program), source_manager.clone());
        while !debug_executor.stopped {
            debug_executor.step().expect("recording step failed");
        }
        let recorded = debug_executor.host.take_recorded_event_mutations();
        let recorded_result: u32 =
            debug_executor.into_execution_trace().parse_result().expect("invalid result");
        assert_eq!(recorded_result, 201);

        assert_eq!(recorded.len(), 2, "expected one recorded entry per emit");
        for (index, batch) in recorded.iter().enumerate() {
            match batch.as_slice() {
                [AdviceMutation::ExtendStack { values }] => {
                    assert_eq!(values.as_slice(), &[Felt::from(100u32 + index as u32)]);
                }
                _ => panic!("unexpected mutations recorded for event {index}"),
            }
        }

        // Replay the recorded mutations without any event handlers registered: execution must
        // reach the same result, proving the log is sufficient for event replay.
        let replay_executor = Executor::new(Vec::new());
        let mut debug_executor = replay_executor.into_debug_with_replay(
            program,
            source_manager,
            Vec::new(),
            recorded.into(),
        );
        while !debug_executor.stopped {
            debug_executor.step().expect("replay step failed");
        }
        let replayed_result: u32 = debug_executor
            .into_execution_trace()
            .parse_result()
            .expect("invalid replay result");
        assert_eq!(replayed_result, recorded_result);
    }

    /// Replayed builtin events still reach their handlers so debugger state remains available.
    #[test]
    fn replay_invokes_builtin_event_handlers() {
        use miden_assembly::DefaultSourceManager;

        let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(
                "program",
                format!(
                    r#"
begin
    emit.event("{FRAME_START_EVENT}")
    emit.event("{FRAME_START_EVENT}")
end
"#
                ),
            )
            .map(Arc::<Package>::from)
            .expect("failed to assemble test program");

        let event_replay = VecDeque::from([Vec::new(), Vec::new()]);
        let mut debug_executor = Executor::new(Vec::new()).into_debug_with_replay(
            program,
            source_manager,
            Vec::new(),
            event_replay,
        );
        while !debug_executor.stopped {
            debug_executor.step().expect("replay step failed");
        }

        assert!(
            debug_executor.callstack.frames().len() >= 2,
            "expected replayed frame-start events to update the debugger call stack"
        );
    }

    /// A recorded execution serialized into a [ReplaySnapshot](crate::exec::ReplaySnapshot) and
    /// read back from bytes replays to the same result — the offline record→replay path, end to
    /// end, exactly what `miden-debug --replay <snapshot>` drives.
    #[test]
    fn replays_from_a_serialized_snapshot() {
        use std::sync::atomic::{AtomicU64, Ordering};

        use miden_assembly::DefaultSourceManager;
        use miden_core::events::EventId;
        use miden_processor::{ProcessorState, advice::AdviceMutation, event::EventError};

        use crate::exec::ReplaySnapshot;

        struct CountingHandler {
            calls: AtomicU64,
        }

        impl EventHandler for CountingHandler {
            fn on_event(
                &self,
                _process: &ProcessorState<'_>,
            ) -> Result<Vec<AdviceMutation>, EventError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![AdviceMutation::ExtendStack {
                    values: vec![Felt::from(100u32 + call as u32)],
                }])
            }
        }

        let source_manager: Arc<DefaultSourceManager> = Arc::new(DefaultSourceManager::default());
        let event_name = "miden-debug::test::snapshot-replay";
        let event_id = EventId::from_name(event_name).as_u64();
        let source = format!(
            "begin push.{event_id} emit drop adv_push push.{event_id} emit drop adv_push add add \
             end"
        );
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program("program", source)
            .map(Arc::<Package>::from)
            .expect("failed to assemble test program");
        let stack_inputs = StackInputs::new(&[Felt::from(7u32)]).unwrap();
        let advice_inputs = AdviceInputs::default();
        let options = ExecutionOptions::default();

        // Record the event mutations by running to completion with a live handler.
        let mut executor = Executor::from_config(ExecutionConfig {
            inputs: stack_inputs,
            advice_inputs: advice_inputs.clone(),
            options,
        });
        executor
            .register_event_handler(
                EventName::from_string(event_name.to_string()),
                Arc::new(CountingHandler {
                    calls: AtomicU64::new(0),
                }),
            )
            .expect("failed to register event handler");
        executor.with_event_advice_mutations_recording();
        let mut debug_executor = executor.into_debug(program.clone(), source_manager.clone());
        while !debug_executor.stopped {
            debug_executor.step().expect("recording step failed");
        }
        let event_log = debug_executor.host.take_recorded_event_mutations();
        let recorded_result: u32 =
            debug_executor.into_execution_trace().parse_result().expect("invalid result");

        // Persist the recording as a snapshot and read it back from its serialized bytes.
        let snapshot = ReplaySnapshot {
            package: program.clone(),
            stack_inputs,
            advice_inputs,
            options,
            mast_forests: vec![LoadedMastForest::with_package_debug_info(
                program.mast_forest().clone(),
                program.debug_info(),
            )],
            event_log,
        };
        let restored = ReplaySnapshot::read_from_bytes(&snapshot.to_bytes())
            .expect("snapshot failed to deserialize");

        // Replay from the deserialized snapshot, with no event handlers registered.
        let replay_executor = Executor::from_config(ExecutionConfig {
            inputs: restored.stack_inputs,
            advice_inputs: restored.advice_inputs,
            options: restored.options,
        });
        let mut debug_executor = replay_executor.into_debug_with_replay(
            restored.package.clone(),
            source_manager,
            restored.mast_forests.clone(),
            restored.event_log.into(),
        );
        while !debug_executor.stopped {
            debug_executor.step().expect("replay step failed");
        }
        let replayed_result: u32 = debug_executor
            .into_execution_trace()
            .parse_result()
            .expect("invalid replay result");
        assert_eq!(replayed_result, recorded_result);
        assert_eq!(replayed_result, 208);
    }
}
