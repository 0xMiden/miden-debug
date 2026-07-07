//! Support utilities for working with [AdviceMutation]s, in particular cloning and recording
//! the mutations produced by event handlers so they can be replayed later.

use std::sync::{Arc, Mutex};

use miden_processor::advice::AdviceMutation;

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
/// This handle exists for executors that are consumed by execution and whose return type cannot
/// carry the log (see `DapExecutor::record_event_mutations`): obtain it before running, read it
/// once execution completes, and feed the recorded log into `DebuggerHost::set_event_replay`
/// (or `Executor::into_debug_with_replay`) to debug the same execution later without access to
/// the original host's event handlers. Hosts owned by the caller record internally instead; see
/// `DebuggerHost::with_event_advice_mutations_recording`.
#[derive(Clone, Default)]
pub struct EventMutationRecorder {
    log: Arc<Mutex<Vec<Vec<AdviceMutation>>>>,
}

impl core::fmt::Debug for EventMutationRecorder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventMutationRecorder").field("events", &self.len()).finish()
    }
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
