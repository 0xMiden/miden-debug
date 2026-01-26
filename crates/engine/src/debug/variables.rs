use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt,
    rc::Rc,
};

use miden_core::Felt;
use miden_processor::RowIndex;

/// Location of a debug variable's value.
///
/// This is a stub type until miden-core provides DebugVarLocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugVarLocation {
    /// Variable is on the stack at the given position
    Stack(u16),
    /// Variable is in memory at the given address
    Memory(u32),
    /// Variable is a constant
    Const(Felt),
    /// Variable is a local at the given frame offset
    Local(u16),
    /// Variable location is computed via an expression
    Expression(Vec<u8>),
}

impl fmt::Display for DebugVarLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stack(pos) => write!(f, "stack[{pos}]"),
            Self::Memory(addr) => write!(f, "mem[{addr}]"),
            Self::Const(felt) => write!(f, "const({})", felt.as_int()),
            Self::Local(idx) => write!(f, "local[{idx}]"),
            Self::Expression(_) => write!(f, "expr(...)"),
        }
    }
}

/// Debug variable information.
///
/// This is a stub type until miden-core provides DebugVarInfo.
#[derive(Debug, Clone)]
pub struct DebugVarInfo {
    name: String,
    location: DebugVarLocation,
}

impl DebugVarInfo {
    /// Create a new debug variable info
    pub fn new(name: impl Into<String>, location: DebugVarLocation) -> Self {
        Self {
            name: name.into(),
            location,
        }
    }

    /// Get the variable name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the variable's value location
    pub fn value_location(&self) -> &DebugVarLocation {
        &self.location
    }
}

/// A snapshot of a debug variable at a specific clock cycle.
#[derive(Debug, Clone)]
pub struct DebugVarSnapshot {
    /// The clock cycle when this variable info was recorded.
    pub clk: RowIndex,
    /// The debug variable information.
    pub info: DebugVarInfo,
}

/// Tracks debug variable information throughout program execution.
///
/// This structure maintains a mapping from variable names to their most recent
/// location information at each clock cycle. It's designed to work with the
/// debugger to provide source-level variable inspection.
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

    /// Update the tracker state to reflect variables at the given clock cycle.
    ///
    /// This processes all events up to and including `clk`, updating the
    /// current variable state accordingly.
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
///
/// NOTE: This currently always returns None as it requires miden-core
/// for full debug variable support.
pub fn resolve_variable_value(
    _location: &DebugVarLocation,
    _stack: &[Felt],
    _get_memory: impl Fn(u32) -> Option<Felt>,
    _get_local: impl Fn(u16) -> Option<Felt>,
) -> Option<Felt> {
    // Variable value resolution requires miden-core
    // For now, always return None
    None
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

        // Verify resolve_variable_value returns None
        let x_snapshot = tracker.get_variable("x").unwrap();
        let value = resolve_variable_value(
            x_snapshot.info.value_location(),
            &[Felt::new(42)],
            |_| None,
            |_| None,
        );
        assert!(value.is_none(), "resolve_variable_value should return None for now");
    }
}
