mod common;

use log::Level;
use miden_debug::TRACE_PRINT_LN;

#[test]
fn trace_println_logs_empty_strings() {
    common::init_test_debug_logger();
    for offset in 0..4 {
        let before = common::log_count();
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
        common::execute_trace(&source);
        common::assert_log_message(
            before,
            Level::Info,
            |message| message.is_empty(),
            "empty passed to println",
        );
    }
}
