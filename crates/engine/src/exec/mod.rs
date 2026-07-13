mod advice;
mod config;
#[cfg(feature = "dap")]
mod dap;
#[cfg(feature = "dap")]
mod dap_client;
#[cfg(feature = "dap")]
mod dap_types;
mod diagnostic;
pub mod event;
mod executor;
mod host;
mod query;
mod snapshot;
mod state;
mod trace;

#[cfg(feature = "dap")]
pub use self::dap::{DapConfig, DapExecutor};
#[cfg(feature = "dap")]
pub use self::dap_client::{DapClient, DapStopReason, SCOPE_MEMORY, SCOPE_STACK};
#[cfg(feature = "dap")]
pub use self::dap_types::{DapUiFrame, DapUiState};
pub use self::{
    advice::{
        EventMutationRecorder, clone_advice_mutation, clone_advice_mutations, read_advice_mutation,
        read_event_log, write_advice_mutation, write_event_log,
    },
    config::ExecutionConfig,
    diagnostic::DiagnosticExecutor,
    event::Event,
    executor::Executor,
    host::DebuggerHost,
    query::DebugQuery,
    snapshot::{
        MastForestRecorder, ReplaySnapshot, ReplaySnapshotError, ReplaySnapshotRecorder,
        ReplaySnapshotWrite, ReplaySnapshotWriteError,
    },
    state::DebugExecutor,
    trace::ExecutionTrace,
};
