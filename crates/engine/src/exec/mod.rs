mod advice;
mod config;
#[cfg(feature = "dap")]
mod dap;
#[cfg(feature = "dap")]
mod dap_client;
#[cfg(feature = "dap")]
mod dap_types;
mod diagnostic;
mod executor;
mod host;
mod query;
mod state;
mod trace;
mod trace_event;
mod trace_monitor;

#[cfg(feature = "dap")]
pub use self::dap::{DapConfig, DapExecutor};
#[cfg(feature = "dap")]
pub use self::dap_client::{DapClient, DapStopReason, SCOPE_MEMORY, SCOPE_STACK};
#[cfg(feature = "dap")]
pub use self::dap_types::{DapUiFrame, DapUiState};
pub use self::{
    advice::{EventMutationRecorder, clone_advice_mutation, clone_advice_mutations},
    config::ExecutionConfig,
    diagnostic::DiagnosticExecutor,
    executor::Executor,
    host::DebuggerHost,
    query::DebugQuery,
    state::DebugExecutor,
    trace::{ExecutionTrace, TraceHandler},
    trace_event::{TRACE_PRINT_LN, TraceEvent},
    trace_monitor::TraceMonitor,
};
