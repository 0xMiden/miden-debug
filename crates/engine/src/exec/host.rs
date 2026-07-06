use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use miden_assembly::SourceManager;
use miden_core::{
    Word,
    events::{EventId, EventName},
};
use miden_debug_types::{Location, SourceFile, SourceSpan};
use miden_processor::{
    BaseHost, ExecutionError, FutureMaybeSend, Host, MastForestStore, MemMastForestStore,
    ProcessorState, TraceError,
    advice::AdviceMutation,
    event::{EventError, EventHandler, EventHandlerRegistry},
    mast::MastForest,
};

use super::{TraceEvent, TraceHandler};

/// Clone a single [AdviceMutation].
///
/// [AdviceMutation] does not implement [Clone] upstream, so recording and replaying event
/// mutations requires reconstructing each variant by hand.
pub fn clone_advice_mutation(mutation: &AdviceMutation) -> AdviceMutation {
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

/// Clone a batch of [AdviceMutation]s. See [clone_advice_mutation].
pub fn clone_advice_mutations(mutations: &[AdviceMutation]) -> Vec<AdviceMutation> {
    mutations.iter().map(clone_advice_mutation).collect()
}

/// A shared, cloneable log of the advice mutations produced by event handlers.
///
/// One entry is recorded per `on_event` invocation, in execution order, **including empty
/// mutation sets**: event replay pops exactly one entry per event, so the log must stay aligned
/// with the event stream of the recorded execution.
///
/// Attach a recorder to a live execution (see [DebuggerHost::set_event_recorder] or
/// `DapExecutor::record_event_mutations`), run the program to completion, and feed the recorded
/// log into [DebuggerHost::set_event_replay] (or `Executor::into_debug_with_replay`) to debug
/// the same execution later without access to the original host's event handlers.
#[derive(Clone, Default)]
pub struct EventMutationRecorder {
    log: Arc<Mutex<Vec<Vec<AdviceMutation>>>>,
}

impl EventMutationRecorder {
    /// Create a new, empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the recorded mutation batches, leaving the recorder empty.
    pub fn take(&self) -> Vec<Vec<AdviceMutation>> {
        core::mem::take(&mut *self.log.lock().expect("event mutation log poisoned"))
    }

    /// Returns a copy of the recorded mutation batches, leaving the recorder intact.
    pub fn snapshot(&self) -> Vec<Vec<AdviceMutation>> {
        self.log
            .lock()
            .expect("event mutation log poisoned")
            .iter()
            .map(|batch| clone_advice_mutations(batch))
            .collect()
    }

    /// The number of `on_event` invocations recorded so far.
    pub fn len(&self) -> usize {
        self.log.lock().expect("event mutation log poisoned").len()
    }

    /// Returns true if no `on_event` invocations have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record the mutations produced by one `on_event` invocation.
    pub(crate) fn record(&self, mutations: Vec<AdviceMutation>) {
        self.log.lock().expect("event mutation log poisoned").push(mutations);
    }

    /// Discard everything recorded so far, e.g. when execution restarts from the beginning.
    pub(crate) fn clear(&self) {
        self.log.lock().expect("event mutation log poisoned").clear();
    }
}

/// This is an implementation of [Host] which is essentially [miden_processor::DefaultHost],
/// but extended with additional functionality for debugging, in particular it manages trace
/// events that record the entry or exit of a procedure call frame.
pub struct DebuggerHost<S: SourceManager + ?Sized> {
    store: MemMastForestStore,
    event_handlers: EventHandlerRegistry,
    tracing_callbacks: BTreeMap<u32, Vec<Box<TraceHandler>>>,
    on_assert_failed: Option<Box<TraceHandler>>,
    source_manager: Arc<S>,
    event_replay: VecDeque<Vec<AdviceMutation>>,
    event_recorder: Option<EventMutationRecorder>,
}
impl<S> DebuggerHost<S>
where
    S: SourceManager + ?Sized,
{
    /// Construct a new instance of [DebuggerHost] with the given source manager.
    pub fn new(source_manager: Arc<S>) -> Self {
        Self {
            store: Default::default(),
            event_handlers: EventHandlerRegistry::default(),
            tracing_callbacks: Default::default(),
            on_assert_failed: None,
            source_manager,
            event_replay: VecDeque::new(),
            event_recorder: None,
        }
    }

    /// Set the event replay queue.
    ///
    /// When non-empty, `on_event()` will pop mutations from this queue instead of
    /// returning empty results. This is used for transaction debugging where events
    /// were recorded during a prior execution.
    pub fn set_event_replay(&mut self, events: VecDeque<Vec<AdviceMutation>>) {
        self.event_replay = events;
    }

    /// Record the advice mutations produced by each event handler invocation into `recorder`.
    ///
    /// Mutations are only recorded for live event handling; nothing is recorded while an event
    /// replay queue is being consumed.
    pub fn set_event_recorder(&mut self, recorder: EventMutationRecorder) {
        self.event_recorder = Some(recorder);
    }

    /// Register a trace handler for `event`
    pub fn register_trace_handler<F>(&mut self, event: TraceEvent, callback: F)
    where
        F: FnMut(&ProcessorState<'_>, TraceEvent) + 'static,
    {
        let key = match event {
            TraceEvent::AssertionFailed(None) => u32::MAX,
            ev => ev.into(),
        };
        self.tracing_callbacks.entry(key).or_default().push(Box::new(callback));
    }

    /// Register a handler to be called when an assertion in the VM fails
    pub fn register_assert_failed_tracer<F>(&mut self, callback: F)
    where
        F: FnMut(&ProcessorState<'_>, TraceEvent) + 'static,
    {
        self.on_assert_failed = Some(Box::new(callback));
    }

    /// Invoke the assert-failed handler, if registered.
    ///
    /// This is called externally when `step()` returns an assertion error, since
    /// `on_assert_failed` no longer exists on the Host trait in 0.21.
    pub fn handle_assert_failed(
        &mut self,
        process: &ProcessorState<'_>,
        err_code: Option<NonZeroU32>,
    ) {
        if let Some(handler) = self.on_assert_failed.as_mut() {
            handler(process, TraceEvent::AssertionFailed(err_code));
        }
    }

    /// Load `forest` into the MAST store for this host
    pub fn load_mast_forest(&mut self, forest: Arc<MastForest>) {
        self.store.insert(forest);
    }

    /// Registers an event handler for use during program execution.
    pub fn register_event_handler(
        &mut self,
        event: EventName,
        handler: Arc<dyn EventHandler>,
    ) -> Result<(), ExecutionError> {
        self.event_handlers.register(event, handler)
    }
}

impl<S> BaseHost for DebuggerHost<S>
where
    S: SourceManager + ?Sized,
{
    fn get_label_and_source_file(
        &self,
        location: &Location,
    ) -> (SourceSpan, Option<Arc<SourceFile>>) {
        let maybe_file = self.source_manager.get_by_uri(location.uri());
        let span = self.source_manager.location_to_span(location.clone()).unwrap_or_default();
        (span, maybe_file)
    }

    fn on_trace(&mut self, process: &ProcessorState<'_>, trace_id: u32) -> Result<(), TraceError> {
        let event = TraceEvent::from(trace_id);
        if let Some(handlers) = self.tracing_callbacks.get_mut(&trace_id) {
            for handler in handlers.iter_mut() {
                handler(process, event);
            }
        }
        Ok(())
    }

    fn resolve_event(&self, event_id: EventId) -> Option<&EventName> {
        self.event_handlers.resolve_event(event_id)
    }
}

impl<S> Host for DebuggerHost<S>
where
    S: SourceManager + ?Sized,
{
    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        std::future::ready(self.store.get(node_digest))
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        if !self.event_replay.is_empty() {
            let mutations = self.event_replay.pop_front().unwrap_or_default();
            return std::future::ready(Ok(mutations));
        }

        let event_id = EventId::from_felt(process.get_stack_item(0));
        let result = match self.event_handlers.handle_event(event_id, process) {
            Ok(Some(mutations)) => Ok(mutations),
            Ok(None) => {
                #[derive(Debug, thiserror::Error)]
                #[error("no event handler registered")]
                struct UnhandledEvent;

                Err(UnhandledEvent.into())
            }
            Err(err) => Err(err),
        };
        if let (Some(recorder), Ok(mutations)) = (&self.event_recorder, &result) {
            recorder.record(clone_advice_mutations(mutations));
        }
        std::future::ready(result)
    }
}
