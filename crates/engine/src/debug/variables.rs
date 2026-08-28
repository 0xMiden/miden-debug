use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

use miden_assembly::ast::{DebugFrameBase, DebugLocationExpression, DebugLocationExpressionOp};
use miden_assembly_syntax::ast::{DebugVarInfo, DebugVarLocation, types::Type};
use miden_core::Felt;
use miden_processor::trace::RowIndex;

type DebugVarEvents = Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>>;
type CapturedDebugValues = Rc<RefCell<BTreeMap<(RowIndex, Arc<str>), Vec<Felt>>>>;

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
    events: DebugVarEvents,
    /// Canonical values frozen at transient debug decorators.
    captured_values: CapturedDebugValues,
    /// Current view of variables - maps variable name to most recent info.
    current_vars: BTreeMap<String, DebugVarSnapshot>,
    /// Canonical values captured for currently visible transient variables.
    current_captured_values: BTreeMap<String, Vec<Felt>>,
    /// The clock cycle up to which we've processed events.
    processed_up_to: RowIndex,
}

impl DebugVarTracker {
    /// Create a new tracker using the given shared event store.
    pub fn new(events: DebugVarEvents) -> Self {
        Self {
            events,
            captured_values: Rc::new(Default::default()),
            current_vars: BTreeMap::new(),
            current_captured_values: BTreeMap::new(),
            processed_up_to: RowIndex::from(0),
        }
    }

    /// Record debug variable events at the given clock cycle.
    pub fn record_events(&self, clk: RowIndex, infos: Vec<DebugVarInfo>) {
        if !infos.is_empty() {
            self.events.borrow_mut().entry(clk).or_default().extend(infos);
        }
    }

    /// Record debug variable events after freezing transient stack-backed values.
    pub fn record_events_with_stack(
        &self,
        clk: RowIndex,
        mut infos: Vec<DebugVarInfo>,
        stack: &[Felt],
    ) {
        let names = infos.iter().map(|info| info.name().clone()).collect::<Vec<_>>();
        let captured_values = snapshot_transient_debug_values(&mut infos, stack);
        {
            let mut stored_values = self.captured_values.borrow_mut();
            for name in names {
                stored_values.remove(&(clk, name));
            }
            stored_values
                .extend(captured_values.into_iter().map(|(name, values)| ((clk, name), values)));
        }
        self.record_events(clk, infos);
    }

    /// Process all events up to and including `clk`, updating current variable state.
    pub fn update_to_cycle(&mut self, clk: RowIndex) {
        let events = self.events.borrow();
        let captured_values = self.captured_values.borrow();

        // Process events from processed_up_to to clk
        for (event_clk, var_infos) in events.range(self.processed_up_to..=clk) {
            for info in var_infos {
                if is_debug_var_kill(info) {
                    self.current_vars.remove(info.name().as_ref());
                    self.current_captured_values.remove(info.name().as_ref());
                    continue;
                }
                let snapshot = DebugVarSnapshot {
                    clk: *event_clk,
                    info: info.clone(),
                };
                let name = info.name().to_string();
                match captured_values.get(&(*event_clk, info.name().clone())) {
                    Some(values) => {
                        self.current_captured_values.insert(name.clone(), values.clone());
                    }
                    None => {
                        self.current_captured_values.remove(&name);
                    }
                }
                self.current_vars.insert(name, snapshot);
            }
        }

        self.processed_up_to = clk;
    }

    /// Reset the tracker to the beginning of execution.
    pub fn reset(&mut self) {
        self.current_vars.clear();
        self.current_captured_values.clear();
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

    /// Returns canonical values captured at the variable's latest debug decorator.
    pub fn captured_values(&self, name: &str) -> Option<&[Felt]> {
        self.current_captured_values.get(name).map(Vec::as_slice)
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
pub fn snapshot_transient_debug_values(
    infos: &mut [DebugVarInfo],
    stack: &[Felt],
) -> BTreeMap<Arc<str>, Vec<Felt>> {
    let mut captured_values = BTreeMap::new();

    for info in infos {
        captured_values.remove(info.name().as_ref());
        match info.value_location() {
            DebugVarLocation::Stack(position) => {
                let count = info.ty().and_then(super::abi_types::value_felt_count).unwrap_or(1);
                let start = *position as usize;
                let values = start
                    .checked_add(count)
                    .and_then(|end| stack.get(start..end))
                    .map(<[Felt]>::to_vec);
                let location = values
                    .as_ref()
                    .and_then(|values| values.first().copied())
                    .or_else(|| (count == 0).then(|| Felt::from_u32(0)))
                    .map(DebugVarLocation::Const)
                    .unwrap_or(DebugVarLocation::Unavailable);
                if let Some(values) = values {
                    captured_values.insert(info.name().clone(), values);
                }
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

    captured_values
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
    resolve_variable_values(location, 1, stack, get_memory, get_local)?.pop()
}

/// Resolve one or more consecutive felts for a debug variable.
pub fn resolve_variable_values(
    location: &DebugVarLocation,
    count: usize,
    stack: &[Felt],
    get_memory: impl Fn(u32) -> Option<Felt>,
    get_local: impl Fn(i16) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    if count == 0 {
        return Some(Vec::new());
    }

    match location {
        DebugVarLocation::Stack(pos) => {
            let start = *pos as usize;
            let end = start.checked_add(count)?;
            Some(stack.get(start..end)?.to_vec())
        }
        DebugVarLocation::Memory(addr) => resolve_consecutive_memory(*addr, count, &get_memory),
        DebugVarLocation::Const(felt) => (count == 1).then_some(vec![*felt]),
        DebugVarLocation::Local(offset) => {
            let mut values = Vec::with_capacity(count);
            for index in 0..count {
                let index = i16::try_from(index).ok()?;
                values.push(get_local(offset.checked_add(index)?)?);
            }
            Some(values)
        }
        DebugVarLocation::ResolvedFrameBase { base, byte_offset } => {
            resolve_frame_base_values(*base, *byte_offset, count, &get_memory, &get_local)
        }
        DebugVarLocation::Expression(expression) => {
            match resolve_expression(expression.operations(), stack, &get_memory, &get_local)? {
                ResolvedExpression::Scalar(value) => {
                    (count == 1).then(|| integer_to_felt(value)).flatten().map(|value| vec![value])
                }
                ResolvedExpression::ByteAddress(address) => {
                    let element_address = u32::try_from(address / 4).ok()?;
                    resolve_consecutive_memory(element_address, count, &get_memory)
                }
            }
        }
        DebugVarLocation::Unavailable => None,
    }
}

/// Resolve a typed debug variable into the canonical ABI felts consumed by the typed decoder.
///
/// Simple Miden locations already contain canonical stack values. Frame-base locations and a
/// terminal [`DebugLocationExpressionOp::DerefBytes`] instead identify packed Rust memory, which
/// must be read at byte granularity and lifted according to `ty` before it can be decoded.
pub fn resolve_typed_variable_values(
    location: &DebugVarLocation,
    ty: &Type,
    count: usize,
    stack: &[Felt],
    get_memory: impl Fn(u32) -> Option<Felt>,
    get_local: impl Fn(i16) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    match location {
        DebugVarLocation::ResolvedFrameBase { base, byte_offset } => {
            let byte_address =
                resolve_frame_base_address(*base, *byte_offset, &get_memory, &get_local)?;
            resolve_typed_memory_value(ty, byte_address, count, &get_memory)
        }
        DebugVarLocation::Expression(expression) => {
            match resolve_expression(expression.operations(), stack, &get_memory, &get_local)? {
                ResolvedExpression::ByteAddress(byte_address) => {
                    resolve_typed_memory_value(ty, byte_address, count, &get_memory)
                }
                ResolvedExpression::Scalar(value) => {
                    (count == 1).then(|| integer_to_felt(value)).flatten().map(|value| vec![value])
                }
            }
        }
        _ => resolve_variable_values(location, count, stack, get_memory, get_local),
    }
}

fn resolve_consecutive_memory(
    start_addr: u32,
    count: usize,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let addr = start_addr.checked_add(u32::try_from(index).ok()?)?;
        values.push(get_memory(addr)?);
    }
    Some(values)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedExpression {
    Scalar(i128),
    ByteAddress(u64),
}

fn resolve_expression(
    ops: &[DebugLocationExpressionOp],
    stack: &[Felt],
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<ResolvedExpression> {
    let mut values = Vec::<i128>::new();

    for (index, op) in ops.iter().enumerate() {
        match op {
            DebugLocationExpressionOp::ReadStack(index) => {
                values.push(i128::from(stack.get(*index as usize)?.as_canonical_u64()));
            }
            DebugLocationExpressionOp::ReadMemory(index) => {
                values.push(i128::from(get_memory(*index)?.as_canonical_u64()));
            }
            DebugLocationExpressionOp::ReadLocal(index) => {
                values.push(i128::from(get_local(*index)?.as_canonical_u64()));
            }
            DebugLocationExpressionOp::ConstU64(value) => {
                values.push(i128::from(*value));
            }
            DebugLocationExpressionOp::ConstI64(value) => {
                values.push(i128::from(*value));
            }
            DebugLocationExpressionOp::AddUnsigned(value) => {
                let lhs = values.pop()?;
                values.push(lhs.checked_add(i128::from(*value))?);
            }
            DebugLocationExpressionOp::Add => {
                let rhs = values.pop()?;
                let lhs = values.pop()?;
                values.push(lhs.checked_add(rhs)?);
            }
            DebugLocationExpressionOp::Sub => {
                let rhs = values.pop()?;
                let lhs = values.pop()?;
                values.push(lhs.checked_sub(rhs)?);
            }
            DebugLocationExpressionOp::DerefBytes => {
                let byte_address = u64::try_from(values.pop()?).ok()?;
                if index + 1 == ops.len() {
                    return Some(ResolvedExpression::ByteAddress(byte_address));
                }
                let element_address = u32::try_from(byte_address / 4).ok()?;
                values.push(i128::from(get_memory(element_address)?.as_canonical_u64()));
            }
            DebugLocationExpressionOp::FrameBaseAddress { base, byte_offset } => {
                values.push(i128::from(resolve_frame_base_address(
                    *base,
                    *byte_offset,
                    get_memory,
                    get_local,
                )?));
            }
        }
    }

    values.pop().map(ResolvedExpression::Scalar)
}

fn resolve_frame_base_values(
    base: DebugFrameBase,
    byte_offset: i64,
    count: usize,
    get_memory: &impl Fn(u32) -> Option<Felt>,
    get_local: &impl Fn(i16) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    let byte_address = resolve_frame_base_address(base, byte_offset, get_memory, get_local)?;
    resolve_byte_address_values(byte_address, count, get_memory)
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

fn resolve_byte_address_values(
    byte_address: u64,
    count: usize,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    if !byte_address.is_multiple_of(4) {
        return None;
    }
    let element_address = u32::try_from(byte_address / 4).ok()?;
    resolve_consecutive_memory(element_address, count, get_memory)
}

fn integer_to_felt(value: i128) -> Option<Felt> {
    Felt::new(u64::try_from(value).ok()?).ok()
}

fn read_memory_bytes(
    byte_address: u64,
    size: usize,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<Vec<u8>> {
    if size == 0 {
        return Some(Vec::new());
    }

    let element_address = u32::try_from(byte_address / 4).ok()?;
    let byte_offset = usize::try_from(byte_address % 4).ok()?;
    let end = byte_offset.checked_add(size)?;
    let element_count = end.div_ceil(4);
    let mut bytes = Vec::with_capacity(element_count.checked_mul(4)?);

    for index in 0..element_count {
        let address = element_address.checked_add(u32::try_from(index).ok()?)?;
        let value = get_memory(address)?.as_canonical_u64() as u32;
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    Some(bytes.get(byte_offset..end)?.to_vec())
}

fn resolve_typed_memory_value(
    ty: &Type,
    byte_address: u64,
    expected_count: usize,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<Vec<Felt>> {
    let mut values = Vec::with_capacity(expected_count);
    append_typed_memory_value(ty, byte_address, get_memory, &mut values)?;
    (values.len() == expected_count).then_some(values)
}

fn append_typed_memory_value(
    ty: &Type,
    byte_address: u64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
    values: &mut Vec<Felt>,
) -> Option<()> {
    match ty {
        Type::Felt => {
            if !byte_address.is_multiple_of(4) {
                return None;
            }
            values.push(get_memory(u32::try_from(byte_address / 4).ok()?)?);
        }
        Type::I1 => {
            let byte = *read_memory_bytes(byte_address, 1, get_memory)?.first()?;
            values.push(Felt::from_u32(u32::from(byte & 1)));
        }
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::I128 => {
            let value = read_unsigned_integer(ty, byte_address, get_memory)?;
            let bit_width = u32::try_from(ty.size_in_bits()).ok()?;
            let shift = 128_u32.checked_sub(bit_width)?;
            let signed = ((value << shift) as i128) >> shift;
            let slot_bits = u32::try_from(ty.size_in_felts().checked_mul(32)?).ok()?;
            let canonical = (signed as u128) & low_bits_mask(slot_bits);
            append_u128_limbs(canonical, ty.size_in_felts(), values);
        }
        Type::U8 | Type::U16 | Type::U32 | Type::U64 | Type::U128 | Type::F64 => {
            let value = read_unsigned_integer(ty, byte_address, get_memory)?;
            append_u128_limbs(value, ty.size_in_felts(), values);
        }
        Type::Struct(struct_ty) => {
            for field in struct_ty.get().fields() {
                let field_address = byte_address.checked_add(u64::from(field.offset))?;
                append_typed_memory_value(&field.ty, field_address, get_memory, values)?;
            }
        }
        Type::Array(array_ty) => {
            let element_size = array_ty.ty.size_in_bytes();
            let alignment = array_ty.ty.min_alignment();
            let stride = align_up(element_size, alignment)?;
            for index in 0..array_ty.len {
                let offset = index.checked_mul(stride)?;
                let element_address = byte_address.checked_add(u64::try_from(offset).ok()?)?;
                append_typed_memory_value(&array_ty.ty, element_address, get_memory, values)?;
            }
        }
        Type::U256
        | Type::List(_)
        | Type::Ptr(_)
        | Type::Function(_)
        | Type::Enum(_)
        | Type::Unknown
        | Type::Never
        | Type::Variadic => return None,
    }

    Some(())
}

fn read_unsigned_integer(
    ty: &Type,
    byte_address: u64,
    get_memory: &impl Fn(u32) -> Option<Felt>,
) -> Option<u128> {
    let bytes = read_memory_bytes(byte_address, ty.size_in_bytes(), get_memory)?;
    let mut value = 0_u128;
    for (index, byte) in bytes.into_iter().enumerate() {
        value |= u128::from(byte) << index.checked_mul(8)?;
    }
    Some(value)
}

fn append_u128_limbs(value: u128, count: usize, values: &mut Vec<Felt>) {
    values.extend((0..count).map(|index| Felt::from_u32((value >> (index * 32)) as u32)));
}

fn low_bits_mask(bits: u32) -> u128 {
    if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    }
}

fn align_up(size: usize, alignment: usize) -> Option<usize> {
    if size == 0 {
        return Some(0);
    }
    size.checked_add(alignment.checked_sub(1)?)?
        .checked_div(alignment)?
        .checked_mul(alignment)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly_syntax::ast::types::{StructType, TypeRepr};

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
            DebugVarInfo::new(
                "missing",
                DebugVarLocation::Expression(
                    DebugLocationExpression::new(vec![DebugLocationExpressionOp::ReadStack(1)])
                        .unwrap(),
                ),
            ),
        ];

        let captured_values = snapshot_transient_debug_values(
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
        assert_eq!(infos[3].value_location(), &DebugVarLocation::Unavailable);
        assert_eq!(captured_values.get("a" as &str), Some(&vec![Felt::from_u32(7)]));
    }

    #[test]
    fn snapshots_all_felts_for_typed_stack_locations() {
        let events: Rc<RefCell<BTreeMap<RowIndex, Vec<DebugVarInfo>>>> =
            Rc::new(Default::default());
        let mut tracker = DebugVarTracker::new(events);
        let mut info = DebugVarInfo::new("wide", DebugVarLocation::Stack(0));
        info.set_ty(Type::U64, None);

        tracker.record_events_with_stack(
            RowIndex::from(1),
            vec![info],
            &[Felt::from_u32(7), Felt::from_u32(1)],
        );
        tracker.update_to_cycle(RowIndex::from(1));

        let snapshot = tracker.get_variable("wide").unwrap();
        assert_eq!(snapshot.info.value_location(), &DebugVarLocation::Const(Felt::from_u32(7)));
        assert_eq!(
            tracker.captured_values("wide"),
            Some([Felt::from_u32(7), Felt::from_u32(1)].as_slice())
        );
    }

    #[test]
    fn resolves_explicit_frame_bases() {
        let expected = Felt::new(4_294_967_303).unwrap();
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
                        Some(expected)
                    } else {
                        None
                    }
                },
                |offset| (!memory_base && offset == -7).then_some(Felt::new(1_048_528).unwrap()),
            );
            assert_eq!(value, Some(expected));
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
    fn resolves_untyped_byte_dereferences_as_memory_elements() {
        let expression = DebugLocationExpression::new(vec![
            DebugLocationExpressionOp::ConstU64(1),
            DebugLocationExpressionOp::DerefBytes,
        ])
        .unwrap();
        let expected = Felt::new(4_294_967_303).unwrap();
        let value = resolve_variable_value(
            &DebugVarLocation::Expression(expression),
            &[],
            |address| (address == 0).then_some(expected),
            |_| None,
        );

        assert_eq!(value, Some(expected));
    }

    #[test]
    fn preserves_whole_felts_after_nonterminal_byte_dereferences() {
        let expression = DebugLocationExpression::new(vec![
            DebugLocationExpressionOp::ConstU64(1),
            DebugLocationExpressionOp::DerefBytes,
            DebugLocationExpressionOp::AddUnsigned(1),
        ])
        .unwrap();
        let value = resolve_variable_value(
            &DebugVarLocation::Expression(expression),
            &[],
            |address| (address == 0).then(|| Felt::new(4_294_967_303).unwrap()),
            |_| None,
        );

        assert_eq!(value, Some(Felt::new(4_294_967_304).unwrap()));
    }

    #[test]
    fn resolves_wide_typed_values_from_unaligned_byte_addresses() {
        let expression = DebugLocationExpression::new(vec![
            DebugLocationExpressionOp::ConstU64(3),
            DebugLocationExpressionOp::DerefBytes,
        ])
        .unwrap();
        let values = resolve_typed_variable_values(
            &DebugVarLocation::Expression(expression),
            &Type::U64,
            2,
            &[],
            |address| match address {
                0 => Some(Felt::from_u32(0x3322_11aa)),
                1 => Some(Felt::from_u32(0x7766_5544)),
                2 => Some(Felt::from_u32(0xbbaa_9988)),
                _ => None,
            },
            |_| None,
        );

        assert_eq!(values, Some(vec![Felt::from_u32(0x6655_4433), Felt::from_u32(0xaa99_8877)]));
    }

    #[test]
    fn lifts_packed_struct_fields_into_canonical_abi_felts() {
        let packed = Type::from(StructType::new_with_repr(
            TypeRepr::packed(1),
            [(Arc::from("tiny"), Type::U8), (Arc::from("half"), Type::U16)],
        ));
        let values = resolve_typed_variable_values(
            &DebugVarLocation::ResolvedFrameBase {
                base: DebugFrameBase::Local(-1),
                byte_offset: 0,
            },
            &packed,
            2,
            &[],
            |address| (address == 0).then_some(Felt::from_u32(0x3322_11aa)),
            |offset| (offset == -1).then_some(Felt::from_u32(1)),
        );

        assert_eq!(values, Some(vec![Felt::from_u32(0x11), Felt::from_u32(0x3322)]));
        assert_eq!(
            crate::debug::format_value(&packed, |count| {
                resolve_typed_variable_values(
                    &DebugVarLocation::ResolvedFrameBase {
                        base: DebugFrameBase::Local(-1),
                        byte_offset: 0,
                    },
                    &packed,
                    count,
                    &[],
                    |address| (address == 0).then_some(Felt::from_u32(0x3322_11aa)),
                    |offset| (offset == -1).then_some(Felt::from_u32(1)),
                )
            })
            .as_deref(),
            Some("{ tiny: 17, half: 13090 }")
        );
    }

    #[test]
    fn sign_extends_typed_integers_to_their_canonical_slots() {
        let values = resolve_typed_variable_values(
            &DebugVarLocation::ResolvedFrameBase {
                base: DebugFrameBase::Local(-1),
                byte_offset: 0,
            },
            &Type::I8,
            1,
            &[],
            |address| (address == 0).then_some(Felt::from_u32(0x0000_ff00)),
            |offset| (offset == -1).then_some(Felt::from_u32(1)),
        );

        assert_eq!(values, Some(vec![Felt::from_u32(u32::MAX)]));
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
    fn resolves_consecutive_memory_values() {
        let values = resolve_variable_values(
            &DebugVarLocation::Memory(10),
            2,
            &[],
            |address| match address {
                10 => Some(Felt::new(1).unwrap()),
                11 => Some(Felt::new(2).unwrap()),
                _ => None,
            },
            |_| None,
        );

        assert_eq!(values, Some(vec![Felt::new(1).unwrap(), Felt::new(2).unwrap()]));
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
