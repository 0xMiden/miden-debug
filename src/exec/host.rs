use std::{collections::BTreeMap, sync::Arc};

use miden_assembly::SourceManager;
use miden_core::Word;
use miden_debug_types::{Location, SourceFile, SourceSpan};
use miden_processor::{
    AdviceProvider, BaseHost, EventHandlerRegistry, MastForest, MastForestStore,
    MemMastForestStore, ProcessState, RowIndex, SyncHost, TraceError,
};

use super::{TraceEvent, TraceHandler};

/// This is an implementation of [BaseHost] which is essentially [miden_processor::DefaultHost],
/// but extended with additional functionality for debugging, in particular it manages trace
/// events that record the entry or exit of a procedure call frame.
#[derive(Default)]
pub struct DebuggerHost<S: SourceManager> {
    _adv_provider: AdviceProvider,
    store: MemMastForestStore,
    tracing_callbacks: BTreeMap<u32, Vec<Box<TraceHandler>>>,
    _event_handlers: EventHandlerRegistry,
    source_manager: Arc<S>,
}
impl<S> DebuggerHost<S>
where
    S: SourceManager,
{
    /// Construct a new instance of [DebuggerHost] with the given advice provider.
    pub fn new(_adv_provider: AdviceProvider, source_manager: S) -> Self {
        Self {
            _adv_provider,
            store: Default::default(),
            tracing_callbacks: Default::default(),
            _event_handlers: EventHandlerRegistry::default(),
            source_manager: Arc::new(source_manager),
        }
    }

    /// Register a trace handler for `event`
    pub fn register_trace_handler<F>(&mut self, event: TraceEvent, callback: F)
    where
        F: FnMut(RowIndex, TraceEvent) + 'static,
    {
        let key = match event {
            TraceEvent::AssertionFailed(None) => u32::MAX,
            ev => ev.into(),
        };
        self.tracing_callbacks.entry(key).or_default().push(Box::new(callback));
    }

    /// Load `forest` into the MAST store for this host
    pub fn load_mast_forest(&mut self, forest: Arc<MastForest>) {
        self.store.insert(forest);
    }
}

impl<S> BaseHost for DebuggerHost<S>
where
    S: SourceManager,
{
    fn get_label_and_source_file(
        &self,
        location: &Location,
    ) -> (SourceSpan, Option<Arc<SourceFile>>) {
        let maybe_file = self.source_manager.get_by_uri(location.uri());
        let span = self.source_manager.location_to_span(location.clone()).unwrap_or_default();
        (span, maybe_file)
    }

    fn on_trace(
        &mut self,
        process: &mut ProcessState,
        trace_id: u32,
    ) -> Result<(), TraceError> {
        let event = TraceEvent::from(trace_id);
        let clk = process.clk();
        if let Some(handlers) = self.tracing_callbacks.get_mut(&trace_id) {
            for handler in handlers.iter_mut() {
                handler(clk, event);
            }
        }
        Ok(())
    }
}

impl<S> SyncHost for DebuggerHost<S>
where
    S: SourceManager,
{
    fn get_mast_forest(&self, node_digest: &Word) -> Option<Arc<MastForest>> {
        self.store.get(node_digest)
    }

    fn on_event(
        &mut self,
        _process: &ProcessState,
    ) -> Result<Vec<miden_processor::AdviceMutation>, miden_processor::EventError> {
        Ok(Vec::new())
    }
}
