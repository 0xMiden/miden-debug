use alloc::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{
    cell::{Cell, RefCell},
    fmt,
    ops::Deref,
};

use log::Level;
use miden_assembly_syntax::{ast::DebugVarInfo, debuginfo::SourceFile, diagnostics::Report};
use miden_core::program::StackInputs;
use miden_debug_types::{ByteIndex, SourceManager};
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
    DebugExecutor, DebuggerHost, ExecutionConfig, ExecutionTrace,
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

        register_builtin_event_handlers(&mut host);

        // Set up debug variable tracking
        let debug_var_events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        let mut processor = FastProcessor::new_with_options(self.stack, self.advice, self.options)
            .expect("advice inputs should fit advice map limits");

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context_for_package(package)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new();
        let debug_vars = DebugVarTracker::new(debug_var_events);
        DebugExecutor {
            frame_resolver: miden_processor::DebugCallFrameResolver::new(),
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

        register_builtin_event_handlers(&mut host);

        let mut processor = FastProcessor::new_with_options(self.stack, self.advice, self.options)
            .expect("advice inputs should fit advice map limits");

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context_for_package(package)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new();
        let debug_vars = DebugVarTracker::new(debug_var_events);
        DebugExecutor {
            frame_resolver: miden_processor::DebugCallFrameResolver::new(),
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
                        let source_loc = asmop
                            .location()
                            .and_then(|loc| location_to_source_file(loc, &source_manager));
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

#[cfg(feature = "std")]
fn location_to_source_file(
    loc: &miden_debug_types::Location,
    source_manager: &dyn SourceManager,
) -> Option<(Arc<SourceFile>, ByteIndex)> {
    use miden_assembly_syntax::debuginfo::SourceManagerExt;
    let path = loc.uri().to_path()?;
    let file = source_manager.load_file(&path).ok()?;
    Some((file, loc.start))
}

#[cfg(not(feature = "std"))]
fn location_to_source_file(
    loc: &miden_debug_types::Location,
    source_manager: &dyn SourceManager,
) -> Option<(Arc<SourceFile>, ByteIndex)> {
    let file = source_manager.get_by_uri(loc.uri())?;
    Some((file, loc.start))
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

fn register_builtin_event_handlers(host: &mut DebuggerHost<dyn SourceManager>) {
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

    let handler = |_: &ProcessorState| -> Result<Vec<AdviceMutation>, EventError> { Ok(vec![]) };
    host.register_event_handler(FRAME_START_EVENT, Arc::new(handler))
        .expect("failed to register frame start event handler");
    host.register_event_handler(FRAME_END_EVENT, Arc::new(handler))
        .expect("failed to register frame end event handler");
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

#[cfg(feature = "std")]
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

#[cfg(not(feature = "std"))]
#[track_caller]
fn render_execution_error(
    err: ExecutionError,
    execution_state: &DebugExecutor,
    source_manager: &dyn SourceManager,
) -> ! {
    use core::fmt::Write;

    use miden_assembly_syntax::diagnostics::{
        LabeledSpan, miette::miette, reporting::PrintDiagnostic,
    };

    let stacktrace = execution_state.callstack.stacktrace(&execution_state.recent, source_manager);

    let mut buf = String::with_capacity(1024);
    writeln!(&mut buf, "{stacktrace}").unwrap();

    if !execution_state.current_stack.is_empty() {
        let stack = execution_state.current_stack.iter().map(|elem| elem.as_canonical_u64());
        let stack = DisplayValues::new(stack);
        writeln!(
            &mut buf,
            "\nLast Known State (at most recent instruction which succeeded):
 | Operand Stack: [{stack}]
 "
        )
        .unwrap();

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

        panic!("{buf}\n\n{}", PrintDiagnostic::new(report));
    } else {
        panic!(
            "{buf}\n\nprogram execution failed at step {step}: {err}",
            step = execution_state.cycle
        );
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
mod tests;
