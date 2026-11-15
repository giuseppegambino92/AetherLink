//! System-audio capture backend (Linux): the default sink's monitor through PipeWire, with a
//! PulseAudio fallback. Implementation and the backend-specific capturers live in
//! [`crate::linux`] (`LinuxAudioCapturer`, `PipeWireAudioCapturer`, `PulseAudioCapturer`).

use super::AudioCapturer;
use crate::Result;

pub fn create() -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(crate::linux::LinuxAudioCapturer::new()))
}
