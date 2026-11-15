//! # aether-core
//!
//! The engine facade used by the desktop and mobile apps. It owns identity,
//! trust, settings and every live session, and glues the engine crates
//! together (capture → codec → net on the host, net → UI frame channel on the
//! client).

pub mod audio;
pub mod client;
pub mod clipboard;
pub mod engine;
pub mod events;
pub mod external;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod hid_bridge;
pub mod host;
pub mod error;
pub mod media;
pub mod mobile;
pub mod mobile_vfs;
#[cfg(target_os = "ios")]
#[path = "mobile_sysinfo.rs"]
pub(crate) mod sysinfo;
#[cfg(not(target_os = "ios"))]
pub(crate) use ::sysinfo;
pub mod ocr;
pub mod password;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod selftest;
pub mod settings;
pub mod streams;
pub mod telemetry;
pub mod terminal;
pub mod ui_video;

pub use engine::{Engine, EngineConfig};
pub use error::{CoreError, Result};
pub use events::{ByteSink, EventSink};
