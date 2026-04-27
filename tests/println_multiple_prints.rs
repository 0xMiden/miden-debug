mod common;

use log::Level;
use miden_debug::TRACE_PRINT_LN;

#[test]
fn stepped_trace_println_logs_across_non_printing_steps() {
    let before = common::init_test_debug_logger();
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

    let mut executor = common::execute_debug(&source);
    let mut hi_seen = false;
    let mut bye_seen = false;
    let mut ok_seen = false;
    let mut step_count = 0;
    let max_steps = 200;

    while !executor.stopped && step_count < max_steps {
        executor.step().expect("step should not fail");

        let logs = common::logs_since(before);
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
