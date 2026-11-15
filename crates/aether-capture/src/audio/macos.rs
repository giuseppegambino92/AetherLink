//! macOS system-audio capture: an audio-focused ScreenCaptureKit stream.
//!
//! ScreenCaptureKit only captures audio as part of a stream with a content filter, so the
//! stream targets the main display with a tiny video configuration (32×32 px, ≤ 1 fps; the
//! frames are discarded) and `capturesAudio = true`, `excludesCurrentProcessAudio = true`,
//! 48 kHz stereo. The `CMSampleBuffer` → interleaved `f32` conversion is the one used by the
//! screen capturer ([`crate::macos`]).
//!
//! Needs the Screen Recording (TCC) permission. Without it `start` returns
//! [`CaptureError::PermissionDenied`] *without* showing a prompt (the permission is checked
//! with `CGPreflightScreenCaptureAccess` before ScreenCaptureKit is touched).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_graphics::CGMainDisplayID;
use objc2_core_media::{CMTime, CMTimeFlags};
use objc2_core_video::kCVPixelFormatType_32BGRA;
use objc2_foundation::NSArray;
use objc2_screen_capture_kit::{SCContentFilter, SCStream, SCStreamConfiguration, SCStreamOutputType, SCWindow};

use super::{AudioCapturer, AudioFormat, AudioSink};
use crate::macos::{Shared, StreamOutput, shareable_content, wait_completion};
use crate::{CaptureError, CaptureEvent, Result};

/// Sample rate requested from ScreenCaptureKit.
pub const SCK_SAMPLE_RATE: u32 = 48_000;
/// Channel count requested from ScreenCaptureKit.
pub const SCK_CHANNELS: u8 = 2;
/// Side of the (discarded) video frames of the audio stream.
const VIDEO_SIDE: usize = 32;

pub fn create() -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(SckAudioCapturer::new()))
}

struct Running {
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _video_queue: DispatchRetained<DispatchQueue>,
    _audio_queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
}

/// ScreenCaptureKit system-audio loopback (everything the Mac plays, except our own process).
#[derive(Default)]
pub struct SckAudioCapturer {
    running: Option<Running>,
}

// SAFETY: SCStream and the output object are thread-safe Objective-C objects (SCStream's API is
// callable from any thread; the output's ivars are Sync); dispatch queues are thread-safe.
unsafe impl Send for SckAudioCapturer {}

impl SckAudioCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AudioCapturer for SckAudioCapturer {
    fn start(&mut self, mut sink: AudioSink) -> Result<AudioFormat> {
        if self.running.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        // Checks the permission first: never triggers the TCC prompt.
        let content = shareable_content()?;
        let displays = unsafe { content.displays() }.to_vec();
        let main = CGMainDisplayID();
        let display = displays
            .iter()
            .find(|d| unsafe { d.displayID() } == main)
            .or(displays.first())
            .ok_or_else(|| CaptureError::TargetNotFound("no display for the audio stream".into()))?;
        let display_id = unsafe { display.displayID() };
        let none: Retained<NSArray<SCWindow>> = NSArray::new();
        let filter =
            unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), display, &none) };

        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(VIDEO_SIDE);
            config.setHeight(VIDEO_SIDE);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 });
            config.setShowsCursor(false);
            config.setQueueDepth(3);
            config.setCapturesAudio(true);
            config.setSampleRate(SCK_SAMPLE_RATE as isize);
            config.setChannelCount(SCK_CHANNELS as isize);
            config.setExcludesCurrentProcessAudio(true);
        }

        let shared = Arc::new(Shared {
            sink: Mutex::new(Some(Box::new(move |ev| match ev {
                CaptureEvent::Audio(p) => sink(p),
                CaptureEvent::Error(e) => tracing::warn!("system audio capture: {e}"),
                CaptureEvent::Frame(_) => {} // the tiny video track only keeps SCK happy
            }))),
            display_id,
            base_pts_us: Mutex::new(None),
            audio_seq: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        });
        let output = StreamOutput::new(shared.clone());
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            )
        };
        // A screen output is registered too: without it SCK logs "stream output NOT found" for
        // every video frame it produces.
        let video_queue = DispatchQueue::new("dev.aetherlink.audio.video", None);
        let audio_queue = DispatchQueue::new("dev.aetherlink.audio.pcm", None);
        for (kind, queue) in [(SCStreamOutputType::Screen, &video_queue), (SCStreamOutputType::Audio, &audio_queue)] {
            unsafe { stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), kind, Some(queue)) }
                .map_err(|e| CaptureError::Os(format!("addStreamOutput({kind:?}): {}", e.localizedDescription())))?;
        }
        wait_completion("SCStream start (audio)", |b| unsafe { stream.startCaptureWithCompletionHandler(Some(b)) })?;
        tracing::debug!(display_id, "ScreenCaptureKit system-audio stream started");
        self.running =
            Some(Running { stream, output, _video_queue: video_queue, _audio_queue: audio_queue, shared });
        Ok(AudioFormat { sample_rate: SCK_SAMPLE_RATE, channels: SCK_CHANNELS })
    }

    fn stop(&mut self) {
        let Some(r) = self.running.take() else { return };
        r.shared.stopping.store(true, Ordering::SeqCst);
        if let Err(e) = wait_completion("SCStream stop (audio)", |b| unsafe { r.stream.stopCaptureWithCompletionHandler(Some(b)) })
        {
            tracing::warn!("stopping the system-audio stream: {e}");
        }
        for kind in [SCStreamOutputType::Screen, SCStreamOutputType::Audio] {
            let _ = unsafe { r.stream.removeStreamOutput_type_error(ProtocolObject::from_ref(&*r.output), kind) };
        }
        // Fence: once the sink is taken under its lock no callback can deliver anymore.
        let sink = r.shared.sink.lock().map(|mut g| g.take()).unwrap_or(None);
        drop(sink);
    }
}

impl Drop for SckAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn create_and_stop_without_starting() {
        // Never touches ScreenCaptureKit: no permission prompt.
        let mut c = create().unwrap();
        c.stop();
        c.stop();
    }

    #[test]
    fn start_without_permission_does_not_prompt() {
        if crate::permission_granted() {
            return; // covered by the ignored live test
        }
        let mut c = SckAudioCapturer::new();
        assert!(matches!(c.start(Box::new(|_| {})), Err(CaptureError::PermissionDenied)));
    }

    #[test]
    #[ignore = "needs Screen Recording permission (would trigger a TCC prompt)"]
    fn captures_system_audio_pcm() {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g2 = got.clone();
        let mut c = SckAudioCapturer::new();
        let fmt = c.start(Box::new(move |p| g2.lock().unwrap().push(p))).unwrap();
        assert_eq!(fmt, AudioFormat { sample_rate: 48_000, channels: 2 });
        std::thread::sleep(Duration::from_millis(1500));
        c.stop();
        let got = got.lock().unwrap();
        // SCK delivers buffers even for silence.
        assert!(!got.is_empty(), "no audio buffers delivered");
        let p = &got[0];
        assert_eq!(p.codec, crate::AudioCodec::PcmF32Le);
        assert_eq!(p.channels, 2);
        assert_eq!(p.data.len() % 8, 0);
    }
}
