pub use miden_debug_engine::{debug, exec, felt};

mod config;
mod input;
mod linker;

#[cfg(any(feature = "tui", feature = "repl"))]
mod logger;
#[cfg(any(feature = "tui", feature = "repl"))]
pub(crate) mod ui;

#[cfg(feature = "repl")]
mod repl;

#[cfg(feature = "repl")]
pub use self::repl::run as run_repl;
#[cfg(feature = "tui")]
pub use self::ui::{DebugMode, State, run, run_with_state};
pub use self::{
    config::{ColorChoice, DebuggerConfig},
    debug::*,
    exec::*,
    felt::{Felt, FromMidenRepr, ToMidenRepr, bytes_to_words, push_wasm_ty_to_operand_stack},
    input::InputFile,
    linker::{LibraryKind, LinkLibrary},
};
