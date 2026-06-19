mod common;

use std::sync::Arc;

use miden_assembly::DefaultSourceManager;
use miden_debug::PRINTLN_EVENT;

#[test]
fn println_logs_empty_strings() {
    common::init_test_debug_logger();

    let source_manager = Arc::new(DefaultSourceManager::default());

    for offset in 0..4 {
        let byte_addr = (278528 + offset) * 4;

        let source = format!(
            r#"
begin
    # No need to write string bytes to memory for an empty string, just put [address, string_length]
    # on the stack
    push.0
    push.{byte_addr}
    emit.event("{PRINTLN_EVENT}")

    # Stack cleanup
    drop
    drop
end
"#,
        );
        common::execute_trace(&source, source_manager.clone());
        assert_println!(entry => entry.message.is_empty());
        common::clear_logs();
    }
}
