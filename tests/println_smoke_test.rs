mod common;

use log::Level;
use miden_debug::TRACE_PRINT_LN;

#[test]
fn trace_println_logs_byte_addressed_strings() {
    for offset in 0..4 {
        let before = common::init_test_debug_logger();
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

        common::execute_trace(&source);
        common::assert_log_message(
            before,
            Level::Info,
            |message| message == "hello",
            "\"hello\" passed to println",
        );
    }
}
