mod common;

use std::sync::Arc;

use miden_assembly::DefaultSourceManager;
use miden_debug::event::PRINT_PANIC_MESSAGE_EVENT;

#[test]
fn panic_message_is_logged() {
    common::init_test_debug_logger();
    let source_manager = Arc::new(DefaultSourceManager::default());

    for offset in 0..4 {
        let base_elem = 278528 + offset;
        let second_elem = base_elem + 1;
        let byte_addr = base_elem * 4;

        let source = format!(
            r#"
begin
    # Print "oh no!".
    # Store 'o' 'h' ' ' 'n' as little-endian bytes packed into felt at element address {base_elem}
    # (after memory reserved for the Rust stack).
    push.1847617647
    push.{base_elem}
    mem_store

    # Store the trailing 'o' '!' bytes in the next felt.
    push.8559
    push.{second_elem}
    mem_store

    # The panic message event expects [address, string_length] on the stack, so push the byte
    # length first and the byte address last.
    push.6
    push.{byte_addr}
    emit.event("{PRINT_PANIC_MESSAGE_EVENT}")

    # Drop the address and string length passed to the panic message event.
    drop
    drop
end
"#,
        );

        common::execute_trace(&source, source_manager.clone());
        assert_println!(entry => entry.level == log::Level::Error && entry.message.contains("oh no!"));
        common::clear_logs();
    }
}
