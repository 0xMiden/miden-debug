#[cfg(test)]
mod helpers {
    use std::sync::Arc;

    use log::Level;
    use miden_assembly::DefaultSourceManager;

    use crate::{Executor, logger::DebugLogger};

    pub(super) fn execute_trace(source: &str) -> crate::ExecutionTrace {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(source)
            .unwrap();

        Executor::new(vec![]).capture_trace(&program, source_manager)
    }

    pub(super) fn execute_debug(source: &str) -> crate::DebugExecutor {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = miden_assembly::Assembler::new(source_manager.clone())
            .assemble_program(source)
            .unwrap();

        Executor::new(vec![]).into_debug(&program, source_manager)
    }

    /// Initializes the debug logger for tests and returns the current log count.
    pub(super) fn init_test_debug_logger() -> usize {
        DebugLogger::init_for_tests();
        DebugLogger::get().peek_captured().len()
    }

    /// Returns all log entries captured since the given snapshot index.
    pub(super) fn logs_since(before: usize) -> Vec<crate::logger::LogEntry> {
        DebugLogger::get().peek_captured().into_iter().skip(before).collect()
    }

    pub(super) fn assert_log_message(
        before: usize,
        level: Level,
        predicate: impl Fn(&str) -> bool,
        description: &str,
    ) {
        let logs = logs_since(before);
        let matches = logs.iter().any(|entry| entry.level == level && predicate(&entry.message));
        let observed: Vec<String> = logs
            .into_iter()
            .map(|entry| format!("{}: {}", entry.level, entry.message))
            .collect();

        assert!(matches, "expected {description}; observed logs since snapshot: {observed:?}",);
    }
}

#[cfg(test)]
mod tests {
    use crate::TRACE_PRINT_LN;
    use log::Level;

    use super::helpers;

    #[test]
    fn trace_println_logs_byte_addressed_strings() {
        for offset in 0..4 {
            let before = helpers::init_test_debug_logger();
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

            helpers::execute_trace(&source);
            helpers::assert_log_message(
                before,
                Level::Info,
                |message| message == "hello",
                "\"hello\" passed to println",
            );
        }
    }

    #[test]
    fn trace_println_logs_empty_strings() {
        for offset in 0..4 {
            let before = helpers::init_test_debug_logger();
            let byte_addr = (278528 + offset) * 4;

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
            helpers::execute_trace(&source);
            helpers::assert_log_message(
                before,
                Level::Info,
                |message| message.is_empty(),
                "empty passed to println",
            );
        }
    }

    #[test]
    fn trace_println_invalid_utf8_logs_warning_and_continues_execution() {
        let before = helpers::init_test_debug_logger();
        let source = format!(
            r#"
begin
    # Store an invalid UTF-8 byte (0xFF) at element 278528
    push.255
    push.278528
    mem_store

    # Try to print it (length 1, byte address 1114112 = 278528*4)
    push.1
    push.1114112
    trace.{TRACE_PRINT_LN}
    drop
    drop

    # Store 42 at element 278529 to prove execution continued
    push.42
    push.278529
    mem_store
end
"#,
        );
        let trace = helpers::execute_trace(&source);

        assert_eq!(
            trace.read_memory_element(278529).map(|f| f.as_canonical_u64()),
            Some(42),
            "expected execution to continue and write 42 to memory",
        );
        helpers::assert_log_message(
            before,
            Level::Warn,
            |message| message.contains("invalid UTF-8"),
            "invalid UTF-8 passed to println should log warning",
        );
    }

    #[test]
    fn trace_println_uninitialized_memory_logs_warning_and_continues_execution() {
        let before = helpers::init_test_debug_logger();
        let source = format!(
            r#"
begin
    # Try to print a byte from uninitialized memory.
    push.1
    push.1114112
    trace.{TRACE_PRINT_LN}
    drop
    drop

    # Store 42 at element 278529 to prove execution continued.
    push.42
    push.278529
    mem_store
end
"#,
        );
        let trace = helpers::execute_trace(&source);

        assert_eq!(
            trace.read_memory_element(278529).map(|f| f.as_canonical_u64()),
            Some(42),
            "expected execution to continue and write 42 to memory",
        );
        helpers::assert_log_message(
            before,
            Level::Warn,
            |message| message.contains("memory is not initialized"),
            "printing string from uninitialized memory should log warning",
        );
    }

    #[test]
    fn trace_println_oversized_length_logs_warning_and_continues_execution() {
        let before = helpers::init_test_debug_logger();
        let source = format!(
            r#"
begin
    # Ask TRACE_PRINT_LN to read more than the maximum allowed byte length.
    push.524289
    push.1114112
    trace.{TRACE_PRINT_LN}
    drop
    drop

    # Store 42 at element 278529 to prove execution continued.
    push.42
    push.278529
    mem_store
end
"#,
        );
        let trace = helpers::execute_trace(&source);

        assert_eq!(
            trace.read_memory_element(278529).map(|f| f.as_canonical_u64()),
            Some(42),
            "expected execution to continue and write 42 to memory",
        );
        helpers::assert_log_message(
            before,
            Level::Warn,
            |message| message.contains("exceeds maximum"),
            "trying to print oversized string should log warning",
        );
    }

    #[test]
    fn stepped_trace_println_logs_across_non_printing_steps() {
        let before = helpers::init_test_debug_logger();
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
"#,
        );

        let mut executor = helpers::execute_debug(&source);
        let mut hi_seen = false;
        let mut bye_seen = false;
        let mut ok_seen = false;
        let mut step_count = 0;
        let max_steps = 200;

        while !executor.stopped && step_count < max_steps {
            executor.step().expect("step should not fail");

            let logs = helpers::logs_since(before);
            hi_seen |= logs.iter().any(|entry| entry.level == Level::Info && entry.message == "hi");
            bye_seen |=
                logs.iter().any(|entry| entry.level == Level::Info && entry.message == "bye");
            ok_seen |= logs.iter().any(|entry| entry.level == Level::Info && entry.message == "ok");

            if hi_seen {
                assert!(
                    logs.iter().any(|entry| entry.level == Level::Info && entry.message == "hi"),
                    "expected \"hi\" to remain visible in captured logs",
                );
            }
            if bye_seen {
                assert!(
                    logs.iter().any(|entry| entry.level == Level::Info && entry.message == "bye"),
                    "expected \"bye\" to remain visible in captured logs",
                );
            }
            if ok_seen {
                assert!(
                    logs.iter().any(|entry| entry.level == Level::Info && entry.message == "ok"),
                    "expected \"ok\" to remain visible in captured logs",
                );
            }

            step_count += 1;
        }

        assert!(executor.stopped, "expected execution to stop within {max_steps} steps");
        assert!(hi_seen, "expected to observe an info log for \"hi\"");
        assert!(bye_seen, "expected to observe an info log for \"bye\"");
        assert!(ok_seen, "expected to observe an info log for \"ok\"");
    }
}
