use std::sync::{Arc, Mutex};

use miden_processor::trace::RowIndex;

/// Can be used with `DebugExecutor::step_until` to break on trace events
#[derive(Default, Clone)]
pub struct TraceMonitor(Arc<Mutex<TraceMonitorInner>>);

#[derive(Default)]
struct TraceMonitorInner {
    events: Vec<(RowIndex, super::TraceEvent)>,
}

impl TraceMonitor {
    pub fn handle_event(&self, clock: RowIndex, event: super::TraceEvent) {
        let mut guard = self.0.lock().unwrap();
        guard.events.push((clock, event));
    }

    pub fn has_any_event_occurred_since(&self, since_clock: RowIndex) -> bool {
        let guard = self.0.lock().unwrap();
        guard.events.iter().any(|(cycle, _)| *cycle >= since_clock)
    }

    pub fn has_event_occurred_since<F>(&self, since_cycle: RowIndex, predicate: F) -> bool
    where
        F: Fn(super::TraceEvent) -> bool,
    {
        let guard = self.0.lock().unwrap();
        guard
            .events
            .iter()
            .any(move |(cycle, event)| *cycle >= since_cycle && predicate(*event))
    }
}
