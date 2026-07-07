use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU32,
    sync::Arc,
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

use super::{TraceEvent, TraceHandler, advice::clone_advice_mutations};

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
    event_recording: Option<Vec<Vec<AdviceMutation>>>,
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
            event_recording: None,
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

    /// Record the advice mutations produced by each event handler invocation.
    ///
    /// One entry is recorded per `on_event` invocation, in execution order, **including empty
    /// mutation sets**, so the recorded log can be fed directly back into
    /// [DebuggerHost::set_event_replay] to replay this execution later. Take the log with
    /// [DebuggerHost::take_recorded_event_mutations] once execution completes.
    ///
    /// Mutations are only recorded for live event handling; nothing is recorded while an event
    /// replay queue is being consumed.
    pub fn with_event_advice_mutations_recording(mut self) -> Self {
        self.event_recording = Some(Vec::new());
        self
    }

    /// Returns the advice mutations recorded so far, leaving the recording empty.
    ///
    /// Returns an empty log when recording was not enabled via
    /// [DebuggerHost::with_event_advice_mutations_recording].
    pub fn take_recorded_event_mutations(&mut self) -> Vec<Vec<AdviceMutation>> {
        self.event_recording.as_mut().map(core::mem::take).unwrap_or_default()
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
        if let (Some(log), Ok(mutations)) = (self.event_recording.as_mut(), &result) {
            log.push(clone_advice_mutations(mutations));
        }
        std::future::ready(result)
    }
}
