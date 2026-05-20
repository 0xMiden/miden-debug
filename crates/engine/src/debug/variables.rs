use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use miden_core::{
    Felt,
    operations::{DebugVarInfo, DebugVarLocation},
    serde::{ByteReader, Deserializable, SliceReader},
};
use miden_processor::trace::RowIndex;

const FRAME_BASE_LOCAL_MARKER: u32 = 1 << 31;
const DEBUG_VAR_KILL_SENTINEL: &[u8] = b"\0miden.debug.kill";

fn decode_frame_base_local_offset(encoded: u32) -> Option<i16> {
    if encoded & FRAME_BASE_LOCAL_MARKER == 0 {
        return None;
    }

    let low_bits = (encoded & 0xffff) as u16;
    Some(i16::from_le_bytes(low_bits.to_le_bytes()))
}

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
                    self.current_vars.remove(info.name());
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
        if let DebugVarLocation::Stack(pos) = info.value_location()
            && let Some(value) = stack.get(*pos as usize).copied()
        {
            info.set_value_location(DebugVarLocation::Const(value));
        }
    }
}

fn is_debug_var_kill(info: &DebugVarInfo) -> bool {
    matches!(
        info.value_location(),
        DebugVarLocation::Expression(expression) if expression == DEBUG_VAR_KILL_SENTINEL
    )
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
        } => resolve_frame_base_value(*global_index, *byte_offset, &get_memory, &get_local),
        DebugVarLocation::Expression(expression) => {
            resolve_expression_value(expression, stack, &get_memory, &get_local)
        }
    }
}

fn resolve_frame_base_value(
    global_index: u32,
    byte_offset: i64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<Felt> {
    if let Some(local_offset) = decode_frame_base_local_offset(global_index) {
        let base = get_local(local_offset)?;
        let byte_addr = base.as_canonical_u64() as i64 + byte_offset;
        let elem_addr = byte_addr / 4;
        let elem_addr = u32::try_from(elem_addr).ok()?;
        return get_memory(elem_addr);
    }

    // global_index was resolved to a Miden byte address during compilation.
    // Convert to element address (÷4) to read the stack pointer value.
    let sp_elem_addr = global_index / 4;
    let base = get_memory(sp_elem_addr)?;
    // The stack pointer value is also a byte address; apply byte_offset,
    // then convert to element address to read the variable's value.
    let byte_addr = base.as_canonical_u64() as i64 + byte_offset;
    let elem_addr = (byte_addr / 4) as u32;
    get_memory(elem_addr)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DebugExpressionOp {
    WasmLocal(u32),
    WasmGlobal(u32),
    WasmStack(u32),
    ConstU64(u64),
    ConstS64(i64),
    PlusUConst(u64),
    Minus,
    Plus,
    Deref,
    StackValue,
    Piece,
    BitPiece,
    FrameBase { global_index: u32, byte_offset: i64 },
    Address(u64),
    Unsupported,
}

fn resolve_expression_value(
    expression: &[u8],
    stack: &[Felt],
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<Felt> {
    let ops = read_expression(expression)?;
    let mut values = Vec::<Felt>::new();

    for op in ops {
        match op {
            DebugExpressionOp::WasmLocal(index) => {
                values.push(get_local(i16::try_from(index).ok()?)?);
            }
            DebugExpressionOp::WasmStack(index) => {
                values.push(stack.get(index as usize).copied()?);
            }
            DebugExpressionOp::ConstU64(value) => {
                values.push(Felt::new(value).expect("value exceeds field modulus"));
            }
            DebugExpressionOp::ConstS64(value) => {
                values.push(Felt::new(value as u64).expect("value exceeds field modulus"));
            }
            DebugExpressionOp::PlusUConst(value) => {
                let lhs = values.pop()?;
                values.push(
                    Felt::new(lhs.as_canonical_u64().wrapping_add(value))
                        .expect("value exceeds field modulus"),
                );
            }
            DebugExpressionOp::Minus => {
                let rhs = values.pop()?.as_canonical_u64();
                let lhs = values.pop()?.as_canonical_u64();
                values.push(Felt::new(lhs.wrapping_sub(rhs)).expect("value exceeds field modulus"));
            }
            DebugExpressionOp::Plus => {
                let rhs = values.pop()?.as_canonical_u64();
                let lhs = values.pop()?.as_canonical_u64();
                values.push(Felt::new(lhs.wrapping_add(rhs)).expect("value exceeds field modulus"));
            }
            DebugExpressionOp::Deref => {
                let addr = u32::try_from(values.pop()?.as_canonical_u64()).ok()?;
                values.push(get_memory(addr)?);
            }
            DebugExpressionOp::StackValue => {}
            DebugExpressionOp::FrameBase {
                global_index,
                byte_offset,
            } => {
                values.push(resolve_frame_base_value(
                    global_index,
                    byte_offset,
                    get_memory,
                    get_local,
                )?);
            }
            DebugExpressionOp::Address(address) => {
                values.push(Felt::new(address).expect("value exceeds field modulus"));
            }
            DebugExpressionOp::WasmGlobal(_)
            | DebugExpressionOp::Piece
            | DebugExpressionOp::BitPiece
            | DebugExpressionOp::Unsupported => return None,
        }
    }

    values.pop()
}

fn read_expression(expression: &[u8]) -> Option<Vec<DebugExpressionOp>> {
    let mut reader = SliceReader::new(expression);
    let len = usize::read_from(&mut reader).ok()?;
    let mut ops = Vec::with_capacity(len);
    for _ in 0..len {
        ops.push(read_expression_op(&mut reader)?);
    }
    Some(ops)
}

fn read_expression_op(reader: &mut SliceReader<'_>) -> Option<DebugExpressionOp> {
    Some(match reader.read_u8().ok()? {
        0 => DebugExpressionOp::WasmLocal(u32::read_from(reader).ok()?),
        1 => DebugExpressionOp::WasmGlobal(u32::read_from(reader).ok()?),
        2 => DebugExpressionOp::WasmStack(u32::read_from(reader).ok()?),
        3 => DebugExpressionOp::ConstU64(u64::read_from(reader).ok()?),
        4 => DebugExpressionOp::ConstS64(u64::read_from(reader).ok()? as i64),
        5 => DebugExpressionOp::PlusUConst(u64::read_from(reader).ok()?),
        6 => DebugExpressionOp::Minus,
        7 => DebugExpressionOp::Plus,
        8 => DebugExpressionOp::Deref,
        9 => DebugExpressionOp::StackValue,
        10 => {
            let _size = u64::read_from(reader).ok()?;
            DebugExpressionOp::Piece
        }
        11 => {
            let _size = u64::read_from(reader).ok()?;
            let _offset = u64::read_from(reader).ok()?;
            DebugExpressionOp::BitPiece
        }
        12 => {
            let global_index = u32::read_from(reader).ok()?;
            let byte_offset = u64::read_from(reader).ok()? as i64;
            DebugExpressionOp::FrameBase {
                global_index,
                byte_offset,
            }
        }
        13 => DebugExpressionOp::Address(u64::read_from(reader).ok()?),
        u8::MAX => {
            let len = usize::read_from(reader).ok()?;
            let _name = reader.read_slice(len).ok()?;
            DebugExpressionOp::Unsupported
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use miden_core::serde::ByteWriter;

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
    }

    #[test]
    fn resolves_local_frame_base_as_byte_address() {
        let encoded =
            FRAME_BASE_LOCAL_MARKER | u32::from(u16::from_le_bytes((-7i16).to_le_bytes()));

        let value = resolve_variable_value(
            &DebugVarLocation::FrameBase {
                global_index: encoded,
                byte_offset: 28,
            },
            &[],
            |addr| (addr == 262_139).then_some(Felt::new(13).expect("value exceeds field modulus")),
            |offset| {
                (offset == -7).then_some(Felt::new(1_048_528).expect("value exceeds field modulus"))
            },
        );

        assert_eq!(value, Some(Felt::new(13).expect("value exceeds field modulus")));
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
                vec![DebugVarInfo::new(
                    "x",
                    DebugVarLocation::Expression(DEBUG_VAR_KILL_SENTINEL.to_vec()),
                )],
            );
        }

        let mut tracker = DebugVarTracker::new(events);
        tracker.update_to_cycle(RowIndex::from(1));
        assert!(tracker.get_variable("x").is_some());

        tracker.update_to_cycle(RowIndex::from(2));
        assert!(tracker.get_variable("x").is_none());
    }

    #[test]
    fn resolves_const_stack_value_expression() {
        let expression =
            expression_bytes(&[TestExpressionOp::ConstU64(7), TestExpressionOp::StackValue]);

        let value = resolve_variable_value(
            &DebugVarLocation::Expression(expression),
            &[],
            |_| None,
            |_| None,
        );

        assert_eq!(value, Some(Felt::new(7).expect("value exceeds field modulus")));
    }

    #[test]
    fn resolves_local_stack_value_expression() {
        let expression =
            expression_bytes(&[TestExpressionOp::WasmLocal(0), TestExpressionOp::StackValue]);

        let value = resolve_variable_value(
            &DebugVarLocation::Expression(expression),
            &[],
            |_| None,
            |offset| (offset == 0).then_some(Felt::new(11).expect("value exceeds field modulus")),
        );

        assert_eq!(value, Some(Felt::new(11).expect("value exceeds field modulus")));
    }

    enum TestExpressionOp {
        WasmLocal(u32),
        ConstU64(u64),
        StackValue,
    }

    fn expression_bytes(ops: &[TestExpressionOp]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.write_usize(ops.len());
        for op in ops {
            match op {
                TestExpressionOp::WasmLocal(index) => {
                    bytes.write_u8(0);
                    bytes.write_u32(*index);
                }
                TestExpressionOp::ConstU64(value) => {
                    bytes.write_u8(3);
                    bytes.write_u64(*value);
                }
                TestExpressionOp::StackValue => {
                    bytes.write_u8(9);
                }
            }
        }
        bytes
    }
}
