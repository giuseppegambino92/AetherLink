//! Context-aware Quake terminal (remote PTY).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalMessage {
    /// Client → host. `cwd: None` asks the host to detect the working directory of
    /// the frontmost Finder/Explorer/IDE window.
    Open { cols: u16, rows: u16, cwd: Option<String>, shell: Option<String> },
    /// Host → client: PTY started; `cwd` is the directory actually used.
    Opened { cwd: String, shell: String },
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Exit { code: Option<i32> },
}
