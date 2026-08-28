use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use miden_assembly::ast::{DebugFrameBase, DebugLocationExpression, DebugLocationExpressionOp};
use miden_assembly_syntax::ast::{DebugVarInfo, DebugVarLocation};
use miden_core::Felt;
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
                if is_debug_var_kill(info) {
                    self.current_vars.remove(info.name().as_ref());
                    continue;
                }
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

/// Snapshot transient debug locations at the decorator point.
///
/// Stack locations are only meaningful at the debug decorator itself. Keeping them live and
/// resolving them against a later VM stack can report unrelated values. Memory, local, and
/// frame-base declarations describe live storage and must be resolved against the current VM state
/// when the user inspects variables.
pub fn snapshot_transient_debug_values(infos: &mut [DebugVarInfo], stack: &[Felt]) {
    for info in infos {
        match info.value_location() {
            DebugVarLocation::Stack(position) => {
                let location = stack
                    .get(*position as usize)
                    .copied()
                    .map(DebugVarLocation::Const)
                    .unwrap_or(DebugVarLocation::Unavailable);
                info.set_value_location(location);
            }
            DebugVarLocation::Expression(expression) => {
                let operations = expression
                    .operations()
                    .iter()
                    .map(|operation| match operation {
                        DebugLocationExpressionOp::ReadStack(position) => stack
                            .get(*position as usize)
                            .map(|value| {
                                DebugLocationExpressionOp::ConstU64(value.as_canonical_u64())
                            })
                            .ok_or(()),
                        operation => Ok(*operation),
                    })
                    .collect::<Result<Vec<_>, _>>();
                let location = operations
                    .and_then(|ops| DebugLocationExpression::new(ops).map_err(|_| ()))
                    .map(DebugVarLocation::Expression)
                    .unwrap_or(DebugVarLocation::Unavailable);
                info.set_value_location(location);
            }
            _ => {}
        }
    }
}

fn is_debug_var_kill(info: &DebugVarInfo) -> bool {
    matches!(info.value_location(), DebugVarLocation::Unavailable)
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
        DebugVarLocation::ResolvedFrameBase { base, byte_offset } => {
            resolve_frame_base_value(*base, *byte_offset, &get_memory, &get_local)
        }
        DebugVarLocation::Expression(expression) => {
            resolve_expression_value(expression.operations(), stack, &get_memory, &get_local)
        }
        DebugVarLocation::Unavailable => None,
    }
}

fn resolve_expression_value(
    ops: &[DebugLocationExpressionOp],
    stack: &[Felt],
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<Felt> {
    let mut values = Vec::<Felt>::new();

    for op in ops {
        match op {
            DebugLocationExpressionOp::ReadStack(index) => {
                values.push(stack.get(*index as usize).copied()?);
            }
            DebugLocationExpressionOp::ReadMemory(index) => {
                values.push(get_memory(*index)?);
            }
            DebugLocationExpressionOp::ReadLocal(index) => {
                values.push(get_local(*index)?);
            }
            DebugLocationExpressionOp::ConstU64(value) => {
                values.push(Felt::new(*value).ok()?);
            }
            DebugLocationExpressionOp::ConstI64(value) => {
                values.push(Felt::new(*value as u64).ok()?);
            }
            DebugLocationExpressionOp::AddUnsigned(value) => {
                let lhs = values.pop()?;
                values.push(Felt::new(lhs.as_canonical_u64().wrapping_add(*value)).ok()?);
            }
            DebugLocationExpressionOp::Add => {
                let rhs = values.pop()?.as_canonical_u64();
                let lhs = values.pop()?.as_canonical_u64();
                values.push(Felt::new(lhs.wrapping_add(rhs)).expect("value exceeds field modulus"));
            }
            DebugLocationExpressionOp::Sub => {
                let rhs = values.pop()?.as_canonical_u64();
                let lhs = values.pop()?.as_canonical_u64();
                values.push(Felt::new(lhs.wrapping_sub(rhs)).expect("value exceeds field modulus"));
            }
            // TODO(pauls): This does not properly handle a read across element boundaries
            DebugLocationExpressionOp::DerefBytes => {
                let addr = u32::try_from(values.pop()?.as_canonical_u64() / 4).ok()?;
                values.push(get_memory(addr)?);
            }
            DebugLocationExpressionOp::FrameBaseAddress { base, byte_offset } => {
                values.push(
                    resolve_frame_base_address(*base, *byte_offset, get_memory, get_local)
                        .and_then(|addr| Felt::new(addr).ok())?,
                );
            }
        }
    }

    values.pop()
}

fn resolve_frame_base_value(
    base: DebugFrameBase,
    byte_offset: i64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<Felt> {
    let byte_address = resolve_frame_base_address(base, byte_offset, get_memory, get_local)?;
    resolve_byte_address(byte_address, get_memory)
}

fn resolve_frame_base_address(
    base: DebugFrameBase,
    byte_offset: i64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<u64> {
    let base = match base {
        DebugFrameBase::Local(offset) => get_local(offset)?,
        DebugFrameBase::Memory(address) => get_memory(address)?,
    };
    base.as_canonical_u64().checked_add_signed(byte_offset)
}

fn resolve_byte_address(
    byte_address: u64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<Felt> {
    if !byte_address.is_multiple_of(4) {
        return None;
    }
    let elem_addr = u32::try_from(byte_address / 4).ok()?;
    get_memory(elem_addr)
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
            &[Felt::new(42).expect("value exceeds field modulus")],
            |_| None,
            |_| None,
        );
        assert_eq!(value, Some(Felt::new(42).expect("value exceeds field modulus")));
    }

    #[test]
    fn snapshots_transient_stack_locations_as_constants() {
        let mut infos = vec![
            DebugVarInfo::new("a", DebugVarLocation::Stack(0)),
            DebugVarInfo::new("b", DebugVarLocation::Local(-1)),
            DebugVarInfo::new(
                "c",
                DebugVarLocation::Expression(
                    DebugLocationExpression::new(vec![
                        DebugLocationExpressionOp::ReadStack(0),
                        DebugLocationExpressionOp::AddUnsigned(3),
                    ])
                    .unwrap(),
                ),
            ),
        ];

        snapshot_transient_debug_values(
            &mut infos,
            &[Felt::new(7).expect("value exceeds field modulus")],
        );

        assert_eq!(
            infos[0].value_location(),
            &DebugVarLocation::Const(Felt::new(7).expect("value exceeds field modulus"))
        );
        assert_eq!(infos[1].value_location(), &DebugVarLocation::Local(-1));
        assert_eq!(
            infos[2].value_location(),
            &DebugVarLocation::Expression(
                DebugLocationExpression::new(vec![
                    DebugLocationExpressionOp::ConstU64(7),
                    DebugLocationExpressionOp::AddUnsigned(3)
                ])
                .unwrap()
            )
        );
    }

    #[test]
    fn resolves_explicit_frame_bases() {
        for (base, memory_base) in
            [(DebugFrameBase::Local(-7), false), (DebugFrameBase::Memory(9), true)]
        {
            let value = resolve_variable_value(
                &DebugVarLocation::ResolvedFrameBase {
                    base,
                    byte_offset: 28,
                },
                &[],
                |address| {
                    if memory_base && address == 9 {
                        Some(Felt::new(1_048_528).unwrap())
                    } else if address == 262_139 {
                        Some(Felt::new(13).unwrap())
                    } else {
                        None
                    }
                },
                |offset| (!memory_base && offset == -7).then_some(Felt::new(1_048_528).unwrap()),
            );
            assert_eq!(value, Some(Felt::new(13).expect("value exceeds field modulus")));
        }
    }

    #[test]
    fn resolves_structured_location_expressions() {
        let expression = DebugLocationExpression::new(vec![
            DebugLocationExpressionOp::FrameBaseAddress {
                base: DebugFrameBase::Local(-2),
                byte_offset: 4,
            },
            DebugLocationExpressionOp::AddUnsigned(8),
            DebugLocationExpressionOp::DerefBytes,
        ])
        .unwrap();
        let value = resolve_variable_value(
            &DebugVarLocation::Expression(expression),
            &[],
            |address| (address == 27).then_some(Felt::new(13).unwrap()),
            |offset| (offset == -2).then_some(Felt::new(96).unwrap()),
        );

        assert_eq!(value, Some(Felt::new(13).unwrap()));
    }

    #[test]
    fn rejects_invalid_location_expression_results() {
        for expression in [
            DebugLocationExpression::new(vec![DebugLocationExpressionOp::ConstI64(-1)]).unwrap(),
            DebugLocationExpression::new(vec![
                DebugLocationExpressionOp::ConstU64(u64::MAX),
                DebugLocationExpressionOp::ConstU64(1),
                DebugLocationExpressionOp::Add,
            ])
            .unwrap(),
        ] {
            assert_eq!(
                resolve_variable_value(
                    &DebugVarLocation::Expression(expression),
                    &[],
                    |_| None,
                    |_| None,
                ),
                None
            );
        }
    }

    #[test]
    fn debug_kill_removes_current_variable() {
        let events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());
        {
            let mut events = events.borrow_mut();
            events.insert(
                RowIndex::from(1),
                vec![DebugVarInfo::new(
                    "x",
                    DebugVarLocation::Const(Felt::new(1).expect("value exceeds field modulus")),
                )],
            );
            events.insert(
                RowIndex::from(2),
                vec![DebugVarInfo::new("x", DebugVarLocation::Unavailable)],
            );
        }

        let mut tracker = DebugVarTracker::new(events);
        tracker.update_to_cycle(RowIndex::from(1));
        assert!(tracker.get_variable("x").is_some());

        tracker.update_to_cycle(RowIndex::from(2));
        assert!(tracker.get_variable("x").is_none());
    }
}
