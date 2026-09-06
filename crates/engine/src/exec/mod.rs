mod advice;
mod config;
#[cfg(feature = "dap")]
mod dap;
#[cfg(feature = "dap")]
mod dap_client;
#[cfg(feature = "dap")]
mod dap_types;
#[cfg(feature = "std")]
mod diagnostic;
#[cfg(feature = "std")]
pub mod event;
#[cfg(feature = "std")]
mod executor;
#[cfg(feature = "std")]
mod host;
mod query;
mod snapshot;
#[cfg(feature = "std")]
mod state;
mod trace;

#[cfg(feature = "dap")]
pub use self::dap::{DapConfig, DapExecutor};
#[cfg(feature = "dap")]
pub use self::dap_client::{DapClient, DapStopReason, SCOPE_MEMORY, SCOPE_STACK};
#[cfg(feature = "dap")]
pub use self::dap_types::{DapUiFrame, DapUiState};
#[cfg(feature = "std")]
pub use self::{
    advice::EventMutationRecorder,
    diagnostic::DiagnosticExecutor,
    event::Event,
    executor::Executor,
    host::DebuggerHost,
    snapshot::{
        MastForestRecorder, ReplaySnapshotError, ReplaySnapshotRecorder, ReplaySnapshotWrite,
        ReplaySnapshotWriteError,
    },
    state::DebugExecutor,
};
pub use self::{
    advice::{
        clone_advice_mutation, clone_advice_mutations, read_advice_mutation, read_event_log,
        write_advice_mutation, write_event_log,
    },
    config::ExecutionConfig,
    query::DebugQuery,
    snapshot::ReplaySnapshot,
    trace::ExecutionTrace,
};
