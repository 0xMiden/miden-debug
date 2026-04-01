use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use miden_core::{
    Felt,
    operations::{DebugVarInfo, DebugVarLocation},
};
use miden_processor::trace::RowIndex;

/// A snapshot of a debug variable at a specific clock cycle.
#[derive(Debug, Clone)]
pub struct DebugVarSnapshot {
    /// The clock cycle when this variable info was recorded.
    pub clk: RowIndex,
    /// The debug variable information.
    pub info: DebugVarInfo,
}

/// Tracks debug variable snapshots, mapping variable names to their most recent location info.
pub struct DebugVarTracker {
    /// All debug variable events recorded during execution, keyed by clock cycle.
    events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>>,
    /// Current view of variables - maps variable name to most recent info.
    current_vars: BTreeMap<String, DebugVarSnapshot>,
    /// The clock cycle up to which we've processed events.
    processed_up_to: RowIndex,
}

impl DebugVarTracker {
    /// Create a new tracker using the given shared event store.
    pub fn new(events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>>) -> Self {
        Self {
            events,
            current_vars: BTreeMap::new(),
            processed_up_to: RowIndex::from(0),
        }
    }

    /// Record debug variable events at the given clock cycle.
    pub fn record_events(&self, clk: RowIndex, infos: Vec<DebugVarInfo>) {
        if !infos.is_empty() {
            self.events.borrow_mut().entry(clk).or_default().extend(infos);
        }
    }

    /// Process all events up to and including `clk`, updating current variable state.
    pub fn update_to_cycle(&mut self, clk: RowIndex) {
        let events = self.events.borrow();

        // Process events from processed_up_to to clk
        for (event_clk, var_infos) in events.range(self.processed_up_to..=clk) {
            for info in var_infos {
                let snapshot = DebugVarSnapshot {
                    clk: *event_clk,
                    info: info.clone(),
                };
                self.current_vars.insert(info.name().to_string(), snapshot);
            }
        }

        self.processed_up_to = clk;
    }

    /// Reset the tracker to the beginning of execution.
    pub fn reset(&mut self) {
        self.current_vars.clear();
        self.processed_up_to = RowIndex::from(0);
    }

    /// Get all currently visible variables.
    pub fn current_variables(&self) -> impl Iterator<Item = &DebugVarSnapshot> {
        self.current_vars.values()
    }

    /// Get a specific variable by name.
    pub fn get_variable(&self, name: &str) -> Option<&DebugVarSnapshot> {
        self.current_vars.get(name)
    }

    /// Get the number of tracked variables.
    pub fn variable_count(&self) -> usize {
        self.current_vars.len()
    }

    /// Check if there are any tracked variables.
    pub fn has_variables(&self) -> bool {
        !self.current_vars.is_empty()
    }
}

/// Resolve a debug variable's value given its location and the current VM state.
pub fn resolve_variable_value(
    location: &DebugVarLocation,
    stack: &[Felt],
    get_memory: impl Fn(u32) -> Option<Felt>,
    get_local: impl Fn(i16) -> Option<Felt>,
) -> Option<Felt> {
    match location {
        DebugVarLocation::Stack(pos) => stack.get(*pos as usize).copied(),
        DebugVarLocation::Memory(addr) => get_memory(*addr),
        DebugVarLocation::Const(felt) => Some(*felt),
        DebugVarLocation::Local(offset) => get_local(*offset),
        DebugVarLocation::FrameBase {
            global_index,
            byte_offset,
        } => {
            // global_index was resolved to a Miden byte address during compilation.
            // Convert to element address (÷4) to read the stack pointer value.
            let sp_elem_addr = *global_index / 4;
            let base = get_memory(sp_elem_addr)?;
            // The stack pointer value is also a byte address; apply byte_offset,
            // then convert to element address to read the variable's value.
            let byte_addr = base.as_canonical_u64() as i64 + byte_offset;
            let elem_addr = (byte_addr / 4) as u32;
            get_memory(elem_addr)
        }
        DebugVarLocation::Expression(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracker_basic() {
        let events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());

        // Add some events
        {
            let mut events_mut = events.borrow_mut();
            events_mut.insert(
                RowIndex::from(1),
                vec![DebugVarInfo::new("x", DebugVarLocation::Stack(0))],
            );
            events_mut.insert(
                RowIndex::from(5),
                vec![DebugVarInfo::new("y", DebugVarLocation::Stack(1))],
            );
        }

        let mut tracker = DebugVarTracker::new(events);

        // Initially no variables
        assert_eq!(tracker.variable_count(), 0);

        // Process up to cycle 3
        tracker.update_to_cycle(RowIndex::from(3));
        assert_eq!(tracker.variable_count(), 1);
        assert!(tracker.get_variable("x").is_some());
        assert!(tracker.get_variable("y").is_none());

        // Process up to cycle 10
        tracker.update_to_cycle(RowIndex::from(10));
        assert_eq!(tracker.variable_count(), 2);
        assert!(tracker.get_variable("x").is_some());
        assert!(tracker.get_variable("y").is_some());

        // Verify resolve_variable_value resolves stack values
        let x_snapshot = tracker.get_variable("x").unwrap();
        let value = resolve_variable_value(
            x_snapshot.info.value_location(),
            &[Felt::new(42)],
            |_| None,
            |_| None,
        );
        assert_eq!(value, Some(Felt::new(42)));
    }
}
