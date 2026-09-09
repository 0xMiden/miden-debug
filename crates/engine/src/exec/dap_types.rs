use alloc::{string::String, sync::Arc, vec::Vec};

use serde::{Deserialize, Serialize};

/// A bundled snapshot of the remote state needed by the TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapUiState {
    pub cycle: usize,
    pub current_stack: Vec<u64>,
    pub callstack: Vec<DapUiFrame>,
}

/// A single remote frame within a bundled UI-state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DapUiFrame {
    pub name: Arc<str>,
    pub source_path: Option<String>,
    pub line: i64,
    pub column: i64,
    #[serde(default)]
    pub inline: bool,
}
