//! Time-travel DVR index (exchanged between the core and UI, and persisted with exports).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimelineEventKind {
    Keyframe,
    Clipboard,
    Terminal,
    FileTransfer,
    /// CPU/RAM/network spike above threshold.
    ResourceSpike,
    /// A new dialog/alert window appeared (from the DOM stream).
    Alert,
    Input,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineMark {
    pub pts_us: u64,
    pub kind: TimelineEventKind,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeTravelIndex {
    /// Oldest seekable timestamp (a keyframe).
    pub start_us: u64,
    pub end_us: u64,
    pub keyframes: Vec<u64>,
    pub marks: Vec<TimelineMark>,
}
