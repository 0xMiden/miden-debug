use std::collections::BTreeMap;

use miden_core::operations::{DebugVarInfo, DebugVarLocation};
use miden_processor::Felt;

/// A snapshot of a debug variable at a specific clock cycle.
#[derive(Debug, Clone)]
pub struct DebugVarSnapshot {
    /// The clock cycle when this variable info was recorded.
    pub cycle: usize,
    /// The debug variable information.
    pub info: DebugVarInfo,
}

/// Tracks debug variable information throughout program execution.
///
/// Instead of relying on host callbacks this tracker is fed debug variable info
/// directly from the MAST forest at each step.
pub struct DebugVarTracker {
    /// Current view of variables - maps variable name to most recent info.
    current_vars: BTreeMap<String, DebugVarSnapshot>,
}

impl Default for DebugVarTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugVarTracker {
    /// Create a new empty tracker.
    pub fn new() -> Self {
        Self {
            current_vars: BTreeMap::new(),
        }
    }

    /// Record debug variable info observed at the given cycle.
    ///
    /// This is called by the DebugExecutor after each step, passing any
    /// debug variable annotations found for the current operation in the
    /// MAST forest.
    pub fn record(&mut self, cycle: usize, var_info: &DebugVarInfo) {
        let snapshot = DebugVarSnapshot {
            cycle,
            info: var_info.clone(),
        };
        self.current_vars.insert(var_info.name().to_string(), snapshot);
    }

    /// Reset the tracker to the beginning of execution.
    pub fn reset(&mut self) {
        self.current_vars.clear();
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
/// The `get_local` closure receives the FMP offset (signed) and should compute
/// the actual address as `FMP + offset` to read the value.
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
        DebugVarLocation::Local(fmp_offset) => get_local(*fmp_offset),
        DebugVarLocation::Expression(_bytes) => {
            // TODO: Handle expression evaluation.
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracker_basic() {
        let mut tracker = DebugVarTracker::new();

        // Initially no variables
        assert_eq!(tracker.variable_count(), 0);

        // Record a variable at cycle 1
        let info = DebugVarInfo::new("x", DebugVarLocation::Stack(0));
        tracker.record(1, &info);
        assert_eq!(tracker.variable_count(), 1);
        assert!(tracker.get_variable("x").is_some());
        assert!(tracker.get_variable("y").is_none());

        // Record another variable at cycle 5
        let info = DebugVarInfo::new("y", DebugVarLocation::Stack(1));
        tracker.record(5, &info);
        assert_eq!(tracker.variable_count(), 2);
        assert!(tracker.get_variable("x").is_some());
        assert!(tracker.get_variable("y").is_some());
    }
}
