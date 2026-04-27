pub use miden_debug_engine::{debug, exec, felt};

mod config;
mod input;
mod linker;

#[cfg(any(feature = "tui", feature = "repl"))]
pub mod logger;
#[cfg(any(feature = "tui", feature = "repl"))]
mod ui;

#[cfg(feature = "repl")]
mod repl;

#[cfg(feature = "repl")]
pub use self::repl::{run as run_repl, run_with_log_level as run_repl_with_log_level};
#[cfg(feature = "tui")]
pub use self::ui::{
    DebugMode, State, run, run_with_log_level, run_with_state, run_with_state_and_log_level,
};

pub use self::{
    config::{ColorChoice, DebuggerConfig},
    debug::*,
    exec::*,
    felt::{Felt, FromMidenRepr, ToMidenRepr, bytes_to_words, push_wasm_ty_to_operand_stack},
    input::InputFile,
    linker::{LibraryKind, LinkLibrary},
};
