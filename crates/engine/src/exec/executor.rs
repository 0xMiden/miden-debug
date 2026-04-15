use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    fmt,
    ops::Deref,
    rc::Rc,
    sync::Arc,
};

use miden_assembly_syntax::{Library, diagnostics::Report};
use miden_core::{
    Word,
    operations::DebugVarInfo,
    program::{Program, StackInputs},
};
use miden_debug_types::{SourceManager, SourceManagerExt};
use miden_mast_package::Dependency;
use miden_processor::{
    ContextId, ExecutionError, ExecutionOptions, FastProcessor, Felt, ProcessorState,
    advice::{AdviceInputs, AdviceMutation},
    event::{EventHandler, EventName},
    mast::MastForest,
    trace::RowIndex,
};

use super::{
    DebugExecutor, DebuggerHost, ExecutionConfig, ExecutionTrace, TraceEvent,
    trace::read_memory_bytes, trace_event::TRACE_PRINT_LN,
};
use crate::{
    debug::{CallStack, DebugVarTracker, NativePtr},
    felt::FromMidenRepr,
};

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
    libraries: Vec<Arc<Library>>,
    event_handlers: Vec<(EventName, Arc<dyn EventHandler>)>,
    dependency_resolver: BTreeMap<Word, Arc<Library>>,
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
        let options = options.with_tracing(true).with_debugging(true);
        let dependency_resolver = BTreeMap::new();

        Self {
            stack: inputs,
            advice: advice_inputs,
            options,
            libraries: Default::default(),
            event_handlers: Default::default(),
            dependency_resolver,
        }
    }

    /// Construct the executor with the given inputs and adds dependencies from the given package
    pub fn for_package<I>(package: &miden_mast_package::Package, args: I) -> Result<Self, Report>
    where
        I: IntoIterator<Item = Felt>,
    {
        use miden_assembly_syntax::DisplayHex;
        log::debug!(
            "creating executor for package '{}' (digest={})",
            package.name,
            DisplayHex::new(&package.digest().as_bytes())
        );
        let mut exec = Self::new(args.into_iter().collect());
        let dependencies = package.manifest.dependencies();
        exec.with_dependencies(dependencies)?;
        log::debug!("executor created");
        Ok(exec)
    }

    /// Adds dependencies to the executor
    pub fn with_dependencies<'a>(
        &mut self,
        dependencies: impl Iterator<Item = &'a Dependency>,
    ) -> Result<&mut Self, Report> {
        for dep in dependencies {
            let digest = dep.digest;
            match self.dependency_resolver.get(&digest) {
                Some(lib) => {
                    log::debug!("dependency {dep:?} resolved");
                    self.with_library(lib.clone());
                }
                None => panic!("{dep:?} not found in resolver"),
            }
        }

        log::debug!("executor created");

        Ok(self)
    }

    /// Set the contents of memory for the shadow stack frame of the entrypoint
    pub fn with_advice_inputs(&mut self, advice: AdviceInputs) -> &mut Self {
        self.advice.extend(advice);
        self
    }

    /// Add a [Library] to the execution context
    pub fn with_library(&mut self, lib: Arc<Library>) -> &mut Self {
        self.libraries.push(lib);
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

    /// Convert this [Executor] into a [DebugExecutor], which captures much more information
    /// about the program being executed, and must be stepped manually.
    pub fn into_debug(
        mut self,
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
    ) -> DebugExecutor {
        log::debug!("creating debug executor");

        let mut host = DebuggerHost::new(source_manager.clone());
        for lib in core::mem::take(&mut self.libraries) {
            host.load_mast_forest(lib.mast_forest().clone());
        }
        for (event, handler) in core::mem::take(&mut self.event_handlers) {
            host.register_event_handler(event, handler)
                .expect("failed to register debug executor event handler");
        }

        let trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>> = Rc::new(Default::default());
        let printed_lines: Rc<RefCell<BTreeMap<RowIndex, String>>> = Rc::new(Default::default());
        register_builtin_trace_handlers(
            &mut host,
            Rc::clone(&trace_events),
            Rc::clone(&printed_lines),
        );

        // Set up debug variable tracking
        // Note: Currently no debug var events are emitted (requires new miden-core),
        // but we set up the infrastructure for when they become available.
        let debug_var_events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        let mut processor = FastProcessor::new(self.stack)
            .with_advice(self.advice)
            .with_options(self.options)
            .with_debugging(true)
            .with_tracing(true);

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context(program)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new(trace_events);
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
            printed_lines,
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
        mut self,
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
        extra_forests: Vec<Arc<MastForest>>,
        event_replay: VecDeque<Vec<AdviceMutation>>,
    ) -> DebugExecutor {
        log::debug!("creating debug executor with event replay");

        let mut host = DebuggerHost::new(source_manager.clone());
        for lib in core::mem::take(&mut self.libraries) {
            host.load_mast_forest(lib.mast_forest().clone());
        }
        for forest in extra_forests {
            host.load_mast_forest(forest);
        }
        host.set_event_replay(event_replay);

        let debug_var_events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        let trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>> = Rc::new(Default::default());
        let printed_lines: Rc<RefCell<BTreeMap<RowIndex, String>>> = Rc::new(Default::default());
        register_builtin_trace_handlers(
            &mut host,
            Rc::clone(&trace_events),
            Rc::clone(&printed_lines),
        );

        let mut processor = FastProcessor::new(self.stack)
            .with_advice(self.advice)
            .with_options(self.options)
            .with_debugging(true)
            .with_tracing(true);

        let root_context = ContextId::root();
        let resume_ctx = processor
            .get_initial_resume_context(program)
            .expect("failed to get initial resume context");

        let callstack = CallStack::new(trace_events);
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
            printed_lines,
        }
    }

    /// Execute the given program until termination, producing a trace
    pub fn capture_trace(
        self,
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
    ) -> ExecutionTrace {
        let mut executor = self.into_debug(program, source_manager);
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
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
    ) -> ExecutionTrace {
        let mut executor = self.into_debug(program, source_manager.clone());
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
                        dbg!(&executor.current_stack);
                        let source_loc = asmop.location().map(|loc| {
                            let path = std::path::Path::new(loc.uri().path());
                            let file = source_manager.load_file(path).unwrap();
                            (file, loc.start)
                        });
                        if let Some((source_file, line_start)) = source_loc {
                            let line_number = source_file.content().line_index(line_start).number();
                            log::trace!(target: "executor", "in {} (located at {}:{})", asmop.context_name(), source_file.deref().uri().as_str(), &line_number);
                        } else {
                            log::trace!(target: "executor", "in {} (no source location available)", asmop.context_name());
                        }
                        log::trace!(target: "executor", "  executed `{op:?}` of `{}` ({} cycles)", asmop.op(), asmop.num_cycles());
                        log::trace!(target: "executor", "  stack state: {:#?}", &executor.current_stack);
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
    pub fn execute_into<T>(self, program: &Program, source_manager: Arc<dyn SourceManager>) -> T
    where
        T: FromMidenRepr + PartialEq,
    {
        let out = self.execute(program, source_manager);
        out.parse_result().expect("invalid result")
    }

    pub fn dependency_resolver_mut(&mut self) -> &mut BTreeMap<Word, Arc<Library>> {
        &mut self.dependency_resolver
    }

    /// Register a library with the dependency resolver so it can be found when resolving package dependencies
    pub fn register_library_dependency(&mut self, lib: Arc<Library>) {
        let digest = *lib.digest();
        self.dependency_resolver.insert(digest, lib);
    }
}

fn register_builtin_trace_handlers(
    host: &mut DebuggerHost<dyn SourceManager>,
    trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>>,
    printed_lines: Rc<RefCell<BTreeMap<RowIndex, String>>>,
) {
    let frame_start_events = Rc::clone(&trace_events);
    host.register_trace_handler(TraceEvent::FrameStart, move |process, event| {
        frame_start_events.borrow_mut().insert(process.clock(), event);
    });
    let frame_end_events = Rc::clone(&trace_events);
    host.register_trace_handler(TraceEvent::FrameEnd, move |process, event| {
        frame_end_events.borrow_mut().insert(process.clock(), event);
    });

    host.register_trace_handler(TraceEvent::PrintLn, move |process, _event| {
        let line = decode_println(process);
        printed_lines.borrow_mut().insert(process.clock(), line);
    });

    let assertion_events = Rc::clone(&trace_events);
    host.register_assert_failed_tracer(move |process, event| {
        assertion_events.borrow_mut().insert(process.clock(), event);
    });
}

/// Decode a `TRACE_PRINT_LN` event into a UTF-8 string.
///
/// Expects `[address, length]` on the operand stack. Reads `length` bytes from
/// `address` in the current context's memory and returns them as a string.
///
/// # Panics
///
/// Panics if inputs are invalid or if reading from memory fails.
fn decode_println(process: &ProcessorState<'_>) -> String {
    let addr = u32::try_from(process.get_stack_item(0).as_canonical_u64())
        .unwrap_or_else(|_| panic!("trace.{TRACE_PRINT_LN:#x} address should fit in u32"));
    let len = usize::try_from(process.get_stack_item(1).as_canonical_u64())
        .unwrap_or_else(|_| panic!("trace.{TRACE_PRINT_LN:#x} string length should fit in usize"));
    let ptr = NativePtr::from_ptr(addr);
    let ctx = process.ctx();

    let bytes = read_memory_bytes(ptr, len, |addr| {
        process.get_mem_value(ctx, addr).unwrap_or_else(|| {
            panic!("trace.{TRACE_PRINT_LN:#x} tried to read unwritten memory at element {addr}")
        })
    })
    .unwrap_or_else(|err| panic!("trace.{TRACE_PRINT_LN:#x} failed to read memory: {err}"));

    String::from_utf8(bytes)
        .unwrap_or_else(|_| panic!("trace.{TRACE_PRINT_LN:#x} should produce valid UTF-8"))
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
