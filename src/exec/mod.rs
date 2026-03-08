mod config;
#[cfg(feature = "dap")]
mod dap;
#[cfg(feature = "dap")]
mod dap_client;
mod diagnostic;
mod executor;
mod host;
mod state;
mod trace;
mod trace_event;
mod tx_executor;

pub use self::{
    config::ExecutionConfig,
    diagnostic::{DiagnosticExecutor, DiagnosticExecutorFactory},
    executor::Executor,
    host::DebuggerHost,
    state::DebugExecutor,
    trace::{ExecutionTrace, TraceHandler},
    trace_event::TraceEvent,
    tx_executor::{TransactionProgramExecutor, TransactionProgramExecutorFactory},
};

#[doc(hidden)]
pub use self::tx_executor::{ProgramExecutor, ProgramExecutorFactory};

#[cfg(feature = "dap")]
pub use self::dap::{DapConfig, DapExecutor, DapExecutorFactory};
#[cfg(feature = "dap")]
pub use self::dap_client::{DapClient, DapStopReason, SCOPE_MEMORY, SCOPE_STACK};
