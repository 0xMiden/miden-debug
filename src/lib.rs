pub use miden_debug_engine::{debug, debug_types, events, exec, felt, processor};

mod config;
#[cfg(feature = "dap")]
mod dap_server;
#[cfg(feature = "flamegraph")]
pub mod flamegraph;
mod input;
mod linker;
mod package_registry;
mod program_loader;

#[cfg(any(feature = "tui", feature = "repl"))]
pub mod logger;
#[cfg(any(feature = "tui", feature = "repl"))]
mod ui;

#[cfg(feature = "repl")]
mod repl;

#[cfg(feature = "dap")]
pub use self::dap_server::run as run_dap_server;
#[cfg(feature = "repl")]
pub use self::repl::{
    run as run_repl, run_commands, run_with_log_level as run_repl_with_log_level,
};
#[cfg(feature = "tui")]
pub use self::ui::{
    DebugMode, State, run, run_replay_and_log_level, run_with_log_level, run_with_state,
    run_with_state_and_log_level,
};
pub use self::{
    config::{ColorChoice, DebuggerConfig},
    debug::*,
    exec::*,
    felt::{
        Felt, FromMidenRepr, RawFelt, ToMidenRepr, bytes_to_words, push_wasm_ty_to_operand_stack,
    },
    input::InputFile,
    linker::{LibraryKind, LinkLibrary},
};
