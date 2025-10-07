use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    fmt,
    ops::Deref,
    rc::Rc,
    sync::Arc,
};

use miden_assembly_syntax::{Library, diagnostics::Report};
use miden_core::{Program, StackInputs};
use miden_debug_types::{SourceManager, SourceManagerExt};
use miden_mast_package::{
    Dependency, DependencyResolver, LocalResolvedDependency, MastArtifact,
    MemDependencyResolverByDigest, ResolvedDependency,
};
use miden_processor::{
    AdviceInputs, AdviceProvider, ExecutionError, ExecutionOptions, Felt, Process, ProcessState,
    RowIndex, VmStateIterator,
};

use super::{DebugExecutor, DebuggerHost, ExecutionConfig, ExecutionTrace, TraceEvent};
use crate::{debug::CallStack, felt::FromMidenRepr};

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
    dependency_resolver: MemDependencyResolverByDigest,
}
impl Executor {
    /// Construct an executor with the given arguments on the operand stack
    pub fn new(args: Vec<Felt>) -> Self {
        let config = ExecutionConfig {
            inputs: StackInputs::new(args).expect("invalid stack inputs"),
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
        let options = options.with_tracing().with_debugging(true);
        let dependency_resolver = MemDependencyResolverByDigest::default();

        Self {
            stack: inputs,
            advice: advice_inputs,
            options,
            libraries: Default::default(),
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
            match self.dependency_resolver.resolve(dep) {
                Some(resolution) => {
                    log::debug!("dependency {dep:?} resolved to {resolution:?}");
                    log::debug!("loading library from package dependency: {dep:?}");
                    match resolution {
                        ResolvedDependency::Local(LocalResolvedDependency::Library(lib)) => {
                            self.with_library(lib);
                        }
                        ResolvedDependency::Local(LocalResolvedDependency::Package(pkg)) => {
                            if let MastArtifact::Library(lib) = &pkg.mast {
                                self.with_library(lib.clone());
                            } else {
                                Err(Report::msg(format!(
                                    "expected package {} to contain library",
                                    pkg.name
                                )))?;
                            }
                        }
                    }
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

    /// Convert this [Executor] into a [DebugExecutor], which captures much more information
    /// about the program being executed, and must be stepped manually.
    pub fn into_debug(
        mut self,
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
    ) -> DebugExecutor {
        log::debug!("creating debug executor");

        let advice_provider = AdviceProvider::from(self.advice.clone());
        let mut host = DebuggerHost::new(advice_provider, source_manager.clone());
        for lib in core::mem::take(&mut self.libraries) {
            host.load_mast_forest(lib.mast_forest().clone());
        }

        let trace_events: Rc<RefCell<BTreeMap<RowIndex, TraceEvent>>> = Rc::new(Default::default());
        let frame_start_events = Rc::clone(&trace_events);
        host.register_trace_handler(TraceEvent::FrameStart, move |clk, event| {
            frame_start_events.borrow_mut().insert(clk, event);
        });
        let frame_end_events = Rc::clone(&trace_events);
        host.register_trace_handler(TraceEvent::FrameEnd, move |clk, event| {
            frame_end_events.borrow_mut().insert(clk, event);
        });
        let assertion_events = Rc::clone(&trace_events);
        host.register_assert_failed_tracer(move |clk, event| {
            assertion_events.borrow_mut().insert(clk, event);
        });

        let mut process =
            Process::new(program.kernel().clone(), self.stack, self.advice, self.options);
        let process_state: ProcessState = (&mut process).into();
        let root_context = process_state.ctx();
        let result = process.execute(program, &mut host);
        let stack_outputs = result.as_ref().map(|so| so.clone()).unwrap_or_default();
        let iter = VmStateIterator::new(process, result);
        let callstack = CallStack::new(trace_events);
        DebugExecutor {
            iter,
            stack_outputs,
            contexts: Default::default(),
            root_context,
            current_context: root_context,
            callstack,
            recent: VecDeque::with_capacity(5),
            last: None,
            cycle: 0,
            stopped: false,
        }
    }

    /// Execute the given program until termination, producing a trace
    pub fn capture_trace(
        self,
        program: &Program,
        source_manager: Arc<dyn SourceManager>,
    ) -> ExecutionTrace {
        let mut executor = self.into_debug(program, source_manager);
        while let Some(step) = executor.next() {
            if step.is_err() {
                return executor.into_execution_trace();
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
        while let Some(step) = executor.next() {
            if let Err(err) = step {
                render_execution_error(err, &executor, &source_manager);
            }

            if log::log_enabled!(target: "executor", log::Level::Trace) {
                let step = step.unwrap();
                if let Some((op, asmop)) = step.op.as_ref().zip(step.asmop.as_ref()) {
                    dbg!(&step.stack);
                    let source_loc = asmop.as_ref().location().map(|loc| {
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
                    log::trace!(target: "executor", "  executed `{op:?}` of `{}` (cycle {}/{})", asmop.op(), asmop.cycle_idx(), asmop.num_cycles());
                    log::trace!(target: "executor", "  stack state: {:#?}", &step.stack);
                }
            }

            /*
            if let Some(op) = state.op {
                match op {
                    miden_core::Operation::MLoad => {
                        let load_addr = last_state
                            .as_ref()
                            .map(|state| state.stack[0].as_int())
                            .unwrap();
                        let loaded = match state
                            .memory
                            .binary_search_by_key(&load_addr, |&(addr, _)| addr)
                        {
                            Ok(index) => state.memory[index].1[0].as_int(),
                            Err(_) => 0,
                        };
                        //dbg!(load_addr, loaded, format!("{loaded:08x}"));
                    }
                    miden_core::Operation::MLoadW => {
                        let load_addr = last_state
                            .as_ref()
                            .map(|state| state.stack[0].as_int())
                            .unwrap();
                        let loaded = match state
                            .memory
                            .binary_search_by_key(&load_addr, |&(addr, _)| addr)
                        {
                            Ok(index) => {
                                let word = state.memory[index].1;
                                [
                                    word[0].as_int(),
                                    word[1].as_int(),
                                    word[2].as_int(),
                                    word[3].as_int(),
                                ]
                            }
                            Err(_) => [0; 4],
                        };
                        let loaded_bytes = {
                            let word = loaded;
                            let a = (word[0] as u32).to_be_bytes();
                            let b = (word[1] as u32).to_be_bytes();
                            let c = (word[2] as u32).to_be_bytes();
                            let d = (word[3] as u32).to_be_bytes();
                            let bytes = [
                                a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1],
                                c[2], c[3], d[0], d[1], d[2], d[3],
                            ];
                            u128::from_be_bytes(bytes)
                        };
                        //dbg!(load_addr, loaded, format!("{loaded_bytes:032x}"));
                    }
                    miden_core::Operation::MStore => {
                        let store_addr = last_state
                            .as_ref()
                            .map(|state| state.stack[0].as_int())
                            .unwrap();
                        let stored = match state
                            .memory
                            .binary_search_by_key(&store_addr, |&(addr, _)| addr)
                        {
                            Ok(index) => state.memory[index].1[0].as_int(),
                            Err(_) => 0,
                        };
                        //dbg!(store_addr, stored, format!("{stored:08x}"));
                    }
                    miden_core::Operation::MStoreW => {
                        let store_addr = last_state
                            .as_ref()
                            .map(|state| state.stack[0].as_int())
                            .unwrap();
                        let stored = {
                            let memory = state
                                .memory
                                .iter()
                                .find_map(|(addr, word)| {
                                    if addr == &store_addr {
                                        Some(word)
                                    } else {
                                        None
                                    }
                                })
                                .unwrap();
                            let a = memory[0].as_int();
                            let b = memory[1].as_int();
                            let c = memory[2].as_int();
                            let d = memory[3].as_int();
                            [a, b, c, d]
                        };
                        let stored_bytes = {
                            let word = stored;
                            let a = (word[0] as u32).to_be_bytes();
                            let b = (word[1] as u32).to_be_bytes();
                            let c = (word[2] as u32).to_be_bytes();
                            let d = (word[3] as u32).to_be_bytes();
                            let bytes = [
                                a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1],
                                c[2], c[3], d[0], d[1], d[2], d[3],
                            ];
                            u128::from_be_bytes(bytes)
                        };
                        //dbg!(store_addr, stored, format!("{stored_bytes:032x}"));
                    }
                    _ => (),
                }
            }
            */
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

    pub fn dependency_resolver_mut(&mut self) -> &mut MemDependencyResolverByDigest {
        &mut self.dependency_resolver
    }
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

    if let Some(last_state) = execution_state.last.as_ref() {
        let stack = last_state.stack.iter().map(|elem| elem.as_int());
        let stack = DisplayValues::new(stack);
        let fmp = last_state.fmp.as_int();
        eprintln!(
            "\nLast Known State (at most recent instruction which succeeded):
 | Frame Pointer: {fmp} (starts at 2^30)
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
            cycle = last_state.clk,
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
