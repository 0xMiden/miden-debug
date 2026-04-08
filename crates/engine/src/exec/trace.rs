use std::collections::BTreeMap;

use miden_core::Word;
use miden_processor::{
    ContextId, FastProcessor, Felt, ProcessorState, StackInputs, StackOutputs, trace::RowIndex,
};
use smallvec::SmallVec;

use super::TraceEvent;
use crate::{debug::NativePtr, felt::FromMidenRepr};

/// A callback to be executed when a [TraceEvent] occurs
pub type TraceHandler = dyn FnMut(&ProcessorState<'_>, TraceEvent);

/// Occurs when an attempt to read memory of the VM fails
#[derive(Debug, thiserror::Error)]
pub enum MemoryReadError {
    #[error("attempted to read beyond end of linear memory")]
    OutOfBounds,
    #[error("unaligned reads are not supported yet")]
    UnalignedRead,
}

/// An [ExecutionTrace] represents a final state of a program that was executed.
///
/// It can be used to examine the program results, and the memory of the program at
/// any cycle up to the last cycle. It is typically used for those purposes once
/// execution of a program terminates.
pub struct ExecutionTrace {
    pub(super) root_context: ContextId,
    pub(super) last_cycle: RowIndex,
    pub(super) processor: FastProcessor,
    pub(super) outputs: StackOutputs,
    pub(super) printed_lines: BTreeMap<RowIndex, String>,
}

impl ExecutionTrace {
    /// Create an empty [ExecutionTrace] with no memory and no outputs.
    ///
    /// Used in DAP client mode where no local execution trace is available.
    pub fn empty() -> Self {
        Self {
            root_context: ContextId::root(),
            last_cycle: RowIndex::from(0u32),
            processor: FastProcessor::new(StackInputs::default()),
            outputs: StackOutputs::default(),
            printed_lines: BTreeMap::new(),
        }
    }

    /// Parse the program outputs on the operand stack as a value of type `T`
    pub fn parse_result<T>(&self) -> Option<T>
    where
        T: FromMidenRepr,
    {
        let size = <T as FromMidenRepr>::size_in_felts();
        let stack = self.outputs.get_num_elements(size);
        if stack.len() < size {
            return None;
        }
        let mut stack = stack.to_vec();
        stack.reverse();
        Some(<T as FromMidenRepr>::pop_from_stack(&mut stack))
    }

    /// Consume the [ExecutionTrace], extracting just the outputs on the operand stack
    #[inline]
    pub fn into_outputs(self) -> StackOutputs {
        self.outputs
    }

    /// Return a reference to the operand stack outputs
    #[inline]
    pub fn outputs(&self) -> &StackOutputs {
        &self.outputs
    }

    /// Return the lines printed via the `TRACE_PRINT_LN` event, keyed by the clock cycle they were
    /// emitted on.
    #[inline]
    pub fn printed_lines(&self) -> &BTreeMap<RowIndex, String> {
        &self.printed_lines
    }

    /// Read the word at the given Miden memory address
    pub fn read_memory_word(&self, addr: u32) -> Option<Word> {
        self.read_memory_word_in_context(addr, self.root_context, self.last_cycle)
    }

    /// Read the word at the given Miden memory address, under `ctx`, at cycle `clk`
    pub fn read_memory_word_in_context(
        &self,
        addr: u32,
        ctx: ContextId,
        clk: RowIndex,
    ) -> Option<Word> {
        const ZERO: Word = Word::new([Felt::ZERO; 4]);

        match self.processor.memory().read_word(ctx, Felt::new(addr as u64), clk) {
            Ok(word) => Some(word),
            Err(_) => Some(ZERO),
        }
    }

    /// Read the element at the given Miden memory address
    #[track_caller]
    pub fn read_memory_element(&self, addr: u32) -> Option<Felt> {
        self.processor
            .memory()
            .read_element(self.root_context, Felt::new(addr as u64))
            .ok()
    }

    /// Read the element at the given Miden memory address, under `ctx`, at cycle `clk`
    #[track_caller]
    pub fn read_memory_element_in_context(
        &self,
        addr: u32,
        ctx: ContextId,
        _clk: RowIndex,
    ) -> Option<Felt> {
        self.processor.memory().read_element(ctx, Felt::new(addr as u64)).ok()
    }

    /// Read a raw byte vector from `addr`, under `ctx`, at cycle `clk`, sufficient to hold a value
    /// of type `ty`
    pub fn read_bytes_for_type(
        &self,
        addr: NativePtr,
        ty: &miden_assembly_syntax::ast::types::Type,
        ctx: ContextId,
        clk: RowIndex,
    ) -> Result<Vec<u8>, MemoryReadError> {
        let size = ty.size_in_bytes();

        if addr.is_element_aligned() {
            read_memory_bytes(addr, size, |addr| {
                self.read_memory_element_in_context(addr, ctx, clk).unwrap_or_default()
            })
        } else {
            Err(MemoryReadError::UnalignedRead)
        }
    }

    /// Read a value of the given type, given an address in Rust's address space
    #[track_caller]
    pub fn read_from_rust_memory<T>(&self, addr: u32) -> Option<T>
    where
        T: core::any::Any + FromMidenRepr,
    {
        self.read_from_rust_memory_in_context(addr, self.root_context, self.last_cycle)
    }

    /// Read a value of the given type, given an address in Rust's address space, under `ctx`, at
    /// cycle `clk`
    #[track_caller]
    pub fn read_from_rust_memory_in_context<T>(
        &self,
        addr: u32,
        ctx: ContextId,
        clk: RowIndex,
    ) -> Option<T>
    where
        T: core::any::Any + FromMidenRepr,
    {
        let ptr = NativePtr::from_ptr(addr);
        assert_eq!(ptr.offset, 0, "support for unaligned reads is not yet implemented");
        let size = <T as FromMidenRepr>::size_in_felts();
        let mut felts = SmallVec::<[_; 4]>::with_capacity(size);
        for index in 0..(size as u32) {
            felts.push(self.read_memory_element_in_context(ptr.addr + index, ctx, clk)?);
        }
        Some(T::from_felts(&felts))
    }
}

pub(crate) fn felt_to_le_bytes(elem: Felt) -> [u8; 4] {
    ((elem.as_canonical_u64() & u32::MAX as u64) as u32).to_le_bytes()
}

/// Reads `size` bytes from memory, starting at `ptr`. Handles `ptr`'s offset.
///
/// The `read_elem` callback is used to fetch an element from an element address.
pub(crate) fn read_memory_bytes(
    ptr: NativePtr,
    size: usize,
    mut read_elem: impl FnMut(u32) -> Felt,
) -> Result<Vec<u8>, MemoryReadError> {
    if size == 0 {
        return Ok(Vec::new());
    }

    let start = usize::from(ptr.offset);
    let end = start.checked_add(size).ok_or(MemoryReadError::OutOfBounds)?;
    let num_elements = end.div_ceil(4);

    let mut bytes = Vec::with_capacity(num_elements.saturating_mul(4));
    for index in 0..num_elements {
        let index = u32::try_from(index).map_err(|_| MemoryReadError::OutOfBounds)?;
        let elem_addr = ptr.addr.checked_add(index).ok_or(MemoryReadError::OutOfBounds)?;
        bytes.extend(felt_to_le_bytes(read_elem(elem_addr)));
    }

    Ok(bytes[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly::DefaultSourceManager;
    use miden_assembly_syntax::ast::types::Type;
    use miden_processor::{ContextId, trace::RowIndex};

    use super::ExecutionTrace;
    use crate::{Executor, debug::NativePtr, exec::trace_event::TRACE_PRINT_LN, felt::ToMidenRepr};

    fn empty_trace() -> ExecutionTrace {
        ExecutionTrace {
            root_context: ContextId::root(),
            last_cycle: RowIndex::from(0_u32),
            processor: miden_processor::FastProcessor::new(miden_processor::StackInputs::default()),
            outputs: miden_processor::StackOutputs::default(),
            printed_lines: Default::default(),
        }
    }

    fn execute_trace(source: &str) -> ExecutionTrace {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(source)
            .unwrap();

        Executor::new(vec![]).capture_trace(&program, source_manager)
    }

    #[test]
    fn parse_result_reads_multi_felt_outputs_in_stack_order() {
        let outputs = 0x0807_0605_0403_0201_u64.to_felts();
        let trace = ExecutionTrace {
            outputs: miden_processor::StackOutputs::new(&outputs).unwrap(),
            ..empty_trace()
        };

        let result = trace.parse_result::<u64>().unwrap();

        assert_eq!(result, 0x0807_0605_0403_0201_u64);
    }

    #[test]
    fn trace_println_captures_byte_addressed_strings() {
        for offset in 0..4 {
            let base_elem = 278528 + offset;
            let second_elem = base_elem + 1;
            let byte_addr = base_elem * 4;

            let source = format!(
                r#"
begin
    # Store 'h' 'e' 'l' 'l' as little-endian bytes packed into felt at element address {base_elem}
    # (after memory reserved for the Rust stack).
    push.1819043176
    push.{base_elem}
    mem_store

    # Store the trailing 'o' byte in the next felt.
    push.111
    push.{second_elem}
    mem_store

    # TRACE_PRINT_LN expects [address, string_length] on the stack, so push the byte length first
    # and the byte address last.
    push.5
    push.{byte_addr}
    trace.{TRACE_PRINT_LN}

    # Drop the address and string length passed to the TRACE_PRINT_LN event.
    drop
    drop
end
"#,
            );
            let trace = execute_trace(&source);

            assert_eq!(trace.printed_lines().len(), 1);
            assert_eq!(trace.printed_lines().values().next().unwrap(), "hello");
        }
    }

    #[test]
    fn trace_println_captures_empty_strings() {
        for offset in 0..4 {
            let base_elem = 278528 + offset;
            let byte_addr = base_elem * 4;

            let source = format!(
                r#"
begin
    # No need to write string bytes to memory for an empty string, just put [address, string_length]
    # on the stack
    push.0
    push.{byte_addr}
    trace.{TRACE_PRINT_LN}

    # Drop the address and string length passed to the TRACE_PRINT_LN event.
    drop
    drop
end
"#,
            );
            let trace = execute_trace(&source);

            assert_eq!(trace.printed_lines().len(), 1);
            assert_eq!(trace.printed_lines().values().next().unwrap(), "");
        }
    }

    #[test]
    fn stepped_trace_println_preserves_lines_across_non_printing_steps() {
        let source = format!(
            r#"
begin
    # Store "hi" at element 278528
    push.26984
    push.278528
    mem_store

    # Print "hi"
    push.2
    push.1114112
    trace.{TRACE_PRINT_LN}
    drop
    drop

    # Normal instructions (no printing)
    push.1
    push.2
    add
    drop

    # Store "bye" at element 278529
    push.6650210
    push.278529
    mem_store

    # Print "bye"
    push.3
    push.1114116
    trace.{TRACE_PRINT_LN}
    drop
    drop

    # More normal instructions
    push.10
    push.20
    mul
    drop

    # Store "ok" at element 278530
    push.27503
    push.278530
    mem_store

    # Print "ok"
    push.2
    push.1114120
    trace.{TRACE_PRINT_LN}
    drop
    drop
end
"#
        );

        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(&source)
            .unwrap();

        let mut executor = Executor::new(vec![]).into_debug(&program, source_manager);

        let mut step_count = 0;
        let max_steps = 200;

        while !executor.stopped && step_count < max_steps {
            let before = executor.printed_lines.borrow().clone();

            executor.step().expect("step should not fail");

            let after = executor.printed_lines.borrow();

            for (key, value) in &before {
                assert!(
                    after.contains_key(key),
                    "step {step_count}: printed line at cycle {key:?} was lost after step",
                );
                assert_eq!(
                    after.get(key).unwrap(),
                    value,
                    "step {step_count}: printed line at cycle {key:?} changed value",
                );
            }

            if after.len() > before.len() {
                let new_keys: Vec<_> = after.keys().filter(|k| !before.contains_key(k)).collect();
                assert_eq!(
                    new_keys.len(),
                    1,
                    "step {step_count}: expected exactly one new printed line key, got {new_keys:?}",
                );
            }

            step_count += 1;
        }

        let final_lines = executor.printed_lines.borrow();
        assert_eq!(
            final_lines.len(),
            3,
            "expected 3 printed lines, got {}: {:?}",
            final_lines.len(),
            final_lines
        );

        let values: Vec<&String> = final_lines.values().collect();
        assert_eq!(values[0].as_str(), "hi");
        assert_eq!(values[1].as_str(), "bye");
        assert_eq!(values[2].as_str(), "ok");
    }

    #[test]
    fn read_bytes_for_type_preserves_little_endian_bytes() {
        let trace = execute_trace(
            r#"
begin
    push.4660
    push.8
    mem_store

    push.67305985
    push.12
    mem_store

    push.134678021
    push.13
    mem_store
end
"#,
        );
        let ctx = ContextId::root();

        let u16_bytes = trace
            .read_bytes_for_type(NativePtr::new(8, 0), &Type::U16, ctx, RowIndex::from(0_u32))
            .unwrap();
        let u64_bytes = trace
            .read_bytes_for_type(NativePtr::new(12, 0), &Type::U64, ctx, RowIndex::from(0_u32))
            .unwrap();

        assert_eq!(u16_bytes, vec![0x34, 0x12]);
        assert_eq!(u64_bytes, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
