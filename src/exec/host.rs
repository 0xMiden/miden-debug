use std::{collections::BTreeMap, num::NonZeroU32, sync::Arc};

use miden_assembly::SourceManager;
use miden_core::Word;
use miden_debug_types::{Location, SourceFile, SourceSpan};
use miden_processor::{
    FutureMaybeSend, Host, MastForestStore, MemMastForestStore, ProcessorState, TraceError,
    advice::AdviceMutation, event::EventError, mast::MastForest, trace::RowIndex,
};

use super::{TraceEvent, TraceHandler};

/// This is an implementation of [Host] which is essentially [miden_processor::DefaultHost],
/// but extended with additional functionality for debugging, in particular it manages trace
/// events that record the entry or exit of a procedure call frame.
pub struct DebuggerHost<S: SourceManager + ?Sized> {
    store: MemMastForestStore,
    tracing_callbacks: BTreeMap<u32, Vec<Box<TraceHandler>>>,
    on_assert_failed: Option<Box<TraceHandler>>,
    source_manager: Arc<S>,
}
impl<S> DebuggerHost<S>
where
    S: SourceManager + ?Sized,
{
    /// Construct a new instance of [DebuggerHost] with the given source manager.
    pub fn new(source_manager: Arc<S>) -> Self {
        Self {
            store: Default::default(),
            tracing_callbacks: Default::default(),
            on_assert_failed: None,
            source_manager,
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

    /// Register a handler to be called when an assertion in the VM fails
    pub fn register_assert_failed_tracer<F>(&mut self, callback: F)
    where
        F: FnMut(RowIndex, TraceEvent) + 'static,
    {
        self.on_assert_failed = Some(Box::new(callback));
    }

    /// Invoke the assert-failed handler, if registered.
    ///
    /// This is called externally when `step()` returns an assertion error, since
    /// `on_assert_failed` no longer exists on the Host trait in 0.21.
    pub fn handle_assert_failed(&mut self, clk: RowIndex, err_code: Option<NonZeroU32>) {
        if let Some(handler) = self.on_assert_failed.as_mut() {
            handler(clk, TraceEvent::AssertionFailed(err_code));
        }
    }

    /// Load `forest` into the MAST store for this host
    pub fn load_mast_forest(&mut self, forest: Arc<MastForest>) {
        self.store.insert(forest);
    }
}

impl<S> Host for DebuggerHost<S>
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

    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        std::future::ready(self.store.get(node_digest))
    }

    fn on_event(
        &mut self,
        _process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn on_trace(&mut self, process: &ProcessorState<'_>, trace_id: u32) -> Result<(), TraceError> {
        let event = TraceEvent::from(trace_id);
        let clk = process.clock();
        if let Some(handlers) = self.tracing_callbacks.get_mut(&trace_id) {
            for handler in handlers.iter_mut() {
                handler(clk, event);
            }
        }
        Ok(())
    }
}
