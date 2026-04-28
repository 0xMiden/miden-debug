// Each test binary includes this module but only uses a subset of the helpers.
#![allow(dead_code)]

use std::sync::Arc;

use log::Level;
use miden_assembly::DefaultSourceManager;
use miden_debug::{Executor, logger::DebugLogger};

pub fn execute_trace(source: &str) -> miden_debug::ExecutionTrace {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(source)
        .unwrap();

    Executor::new(vec![]).capture_trace(&program, source_manager)
}

pub fn execute_debug(source: &str) -> miden_debug::DebugExecutor {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let program = miden_assembly::Assembler::new(source_manager.clone())
        .assemble_program(source)
        .unwrap();

    Executor::new(vec![]).into_debug(&program, source_manager)
}

/// Initializes the debug logger for tests and returns the current log count.
///
/// # Panics
///
/// Panics if called more than once per process, as integration tests should not share a
/// `DebugLogger`
pub fn init_test_debug_logger() -> usize {
    DebugLogger::init_for_tests().expect(
        "integration tests should run in different processes to isolate their `DebugLogger`",
    );
    DebugLogger::get().clone_captured().len()
}

/// Returns the current number of captured log entries.
pub fn log_count() -> usize {
    DebugLogger::get().log_count()
}

/// Returns all log entries captured since the given snapshot index.
pub fn logs_since(before: usize) -> Vec<miden_debug::logger::LogEntry> {
    DebugLogger::get().clone_captured().into_iter().skip(before).collect()
}

pub fn assert_log_message(
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
