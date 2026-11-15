//! System-audio streaming: host capture → Opus → `Datagram::Audio`, client jitter buffer →
//! Opus decode → `cpal` playback, plus a deterministic loopback self-test.
//!
//! Host ([`HostAudio`]): the capturer (`aether_capture::create_audio_capturer`, or the
//! [`crate::EngineConfig::audio_capturer`] factory) delivers PCM on its own thread; a bounded
//! queue (never blocks capture) feeds the `aether-audio-enc` thread, which resamples/remixes to
//! 48 kHz stereo, encodes 10 ms Opus frames and sends each as a QUIC datagram. Packets that do
//! not fit `max_datagram_size` (or when the peer disabled datagrams) go on a lazily opened uni
//! stream (`StreamKind::Audio`). Every second the encoder's expected-loss hint follows the QUIC
//! loss rate, which turns on Opus in-band FEC only on lossy links.
//!
//! Client ([`ClientAudio`]): packets → [`AudioReceiver`] (adaptive 20–60 ms jitter buffer,
//! reorder, late drop, FEC/PLC) → `aether-audio-out` thread → resample to the device rate →
//! lock-free ring (one device callback + 10 ms) → `cpal` output callback, which applies the
//! local gain / mute. Output device selectable at runtime. With
//! [`crate::EngineConfig::audio_null_output`] the frames are decoded on a wall-clock timer and
//! discarded (tests / headless).

use crate::{CoreError, Result};
use aether_capture::{AudioCapturer, AudioPacket};
use aether_codec::audio::{
    AudioEncoder, AudioEncoderConfig, AudioReceiver, AudioReceiverStats, FrameKind, JitterConfig, OPUS_SAMPLE_RATE,
    peak_frequency, pcm_to_f32le, sine_fit,
};
use aether_net::PeerConnection;
use aether_protocol::{AudioCodec, Datagram, StreamKind, decode_message, encode_message};
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Produces the host's system-audio capturer (overridable for deterministic test sources).
pub type AudioCapturerFactory = Arc<dyn Fn() -> aether_capture::Result<Box<dyn AudioCapturer>> + Send + Sync>;

/// Output channel count of the decoder (the wire format is 48 kHz stereo Opus).
const CHANNELS: u8 = 2;
const FRAME_US: u64 = 10_000;

// =============================================================================================
// Host
// =============================================================================================

#[derive(Debug, Default)]
struct HostCounters {
    packets: AtomicU64,
    bytes: AtomicU64,
    via_stream: AtomicU64,
    dropped_capture: AtomicU64,
}

/// A running host audio pipeline (capturer + encoder thread). Stop with [`HostAudio::stop`]
/// (blocking: it waits for the capturer to stop).
pub struct HostAudio {
    capturer: Box<dyn AudioCapturer>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<HostCounters>,
}

impl HostAudio {
    /// Start capturing and streaming to `conn`. Must be called inside a Tokio runtime (the
    /// stream fallback runs on it). Blocking (the OS capturer may take a moment to start).
    /// Errors are `(i18n key, detail)`.
    pub fn start(mut capturer: Box<dyn AudioCapturer>, conn: PeerConnection) -> std::result::Result<Self, (&'static str, String)> {
        let rt = tokio::runtime::Handle::try_current().map_err(|e| ("errors.audioUnavailable", e.to_string()))?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<AudioPacket>(64);
        let counters = Arc::new(HostCounters::default());
        let c2 = counters.clone();
        let format = capturer
            .start(Box::new(move |p| {
                if tx.try_send(p).is_err() {
                    c2.dropped_capture.fetch_add(1, Ordering::Relaxed);
                }
            }))
            .map_err(|e| match e {
                aether_capture::CaptureError::PermissionDenied => ("errors.audioPermission", e.to_string()),
                _ => ("errors.audioUnavailable", e.to_string()),
            })?;
        tracing::info!(rate = format.sample_rate, channels = format.channels, "host audio capture started");
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, c3) = (stop.clone(), counters.clone());
        let thread = std::thread::Builder::new()
            .name("aether-audio-enc".into())
            .spawn(move || encoder_thread(rx, conn, rt, stop2, c3))
            .map_err(|e| ("errors.audioUnavailable", e.to_string()));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                capturer.stop();
                return Err(e);
            }
        };
        Ok(Self { capturer, stop, thread: Some(thread), counters })
    }

    pub fn packets_sent(&self) -> u64 {
        self.counters.packets.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.capturer.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
            let c = &self.counters;
            tracing::info!(
                packets = c.packets.load(Ordering::Relaxed),
                bytes = c.bytes.load(Ordering::Relaxed),
                via_stream = c.via_stream.load(Ordering::Relaxed),
                dropped_capture = c.dropped_capture.load(Ordering::Relaxed),
                "host audio stopped"
            );
        }
    }
}

impl Drop for HostAudio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn encoder_thread(
    rx: std::sync::mpsc::Receiver<AudioPacket>,
    conn: PeerConnection,
    rt: tokio::runtime::Handle,
    stop: Arc<AtomicBool>,
    counters: Arc<HostCounters>,
) {
    let mut encoder = match AudioEncoder::new(AudioEncoderConfig::default()) {
        Ok(e) => e,
        Err(e) => return tracing::error!("opus encoder: {e}"),
    };
    // Reliable fallback for packets that cannot travel as datagrams.
    let mut fallback: Option<tokio::sync::mpsc::UnboundedSender<AudioPacket>> = None;
    let mut last_loss_check = Instant::now();
    let mut last_stats = conn.stats();
    while !stop.load(Ordering::Relaxed) {
        let pcm = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(p) => p,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let packets = match encoder.push_packet(&pcm) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("audio encode: {e}");
                continue;
            }
        };
        for p in packets {
            counters.packets.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
            let d = Datagram::Audio(p);
            let size = encode_message(&d).map(|b| b.len()).unwrap_or(usize::MAX);
            let fits = conn.max_datagram_size().is_some_and(|max| size <= max);
            if fits && conn.send_datagram(&d).is_ok() {
                continue;
            }
            let Datagram::Audio(p) = d else { unreachable!() };
            counters.via_stream.fetch_add(1, Ordering::Relaxed);
            let tx = fallback.get_or_insert_with(|| spawn_stream_fallback(&rt, conn.clone()));
            if tx.send(p).is_err() {
                return; // connection gone
            }
        }
        if last_loss_check.elapsed() >= Duration::from_secs(1) {
            last_loss_check = Instant::now();
            let s = conn.stats();
            let sent = s.sent_packets.saturating_sub(last_stats.sent_packets);
            let lost = s.lost_packets.saturating_sub(last_stats.lost_packets);
            last_stats = s;
            let pct = if sent == 0 { 0 } else { (lost * 100).div_ceil(sent).min(30) as u8 };
            if pct != encoder.config().expected_loss_percent {
                let _ = encoder.set_expected_loss(pct);
            }
        }
    }
}

fn spawn_stream_fallback(rt: &tokio::runtime::Handle, conn: PeerConnection) -> tokio::sync::mpsc::UnboundedSender<AudioPacket> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AudioPacket>();
    rt.spawn(async move {
        let mut send = match conn.open_uni(StreamKind::Audio).await {
            Ok(s) => s,
            Err(e) => return tracing::warn!("cannot open audio stream: {e}"),
        };
        send.set_priority(20);
        while let Some(p) = rx.recv().await {
            if send.send(&p).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });
    tx
}

// =============================================================================================
// Client
// =============================================================================================

/// An audio output device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Client-side audio statistics (also attached to `session-telemetry` as `audio`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientAudioStats {
    pub enabled: bool,
    pub muted: bool,
    pub volume: f32,
    pub output_device: Option<String>,
    pub packets_received: u64,
    pub bytes_received: u64,
    /// Device callbacks that found the ring buffer empty.
    pub output_underflows: u64,
    #[serde(flatten)]
    pub receiver: AudioReceiverStats,
}

struct PlaybackShared {
    receiver: Mutex<AudioReceiver>,
    epoch: Instant,
    enabled: AtomicBool,
    volume_bits: AtomicU32,
    muted: AtomicBool,
    packets: AtomicU64,
    bytes: AtomicU64,
    underflows: AtomicU64,
    device_name: Mutex<Option<String>>,
}

impl PlaybackShared {
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn gain(&self) -> f32 {
        if self.muted.load(Ordering::Relaxed) { 0.0 } else { f32::from_bits(self.volume_bits.load(Ordering::Relaxed)) }
    }

    fn pull(&self) -> (Vec<f32>, FrameKind) {
        let now = self.now_us();
        self.receiver.lock().pull(now)
    }
}

/// A running output thread; dropping it stops playback.
struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Client playback of the host's audio stream.
pub struct ClientAudio {
    shared: Arc<PlaybackShared>,
    output: Mutex<Option<Output>>,
    device: Mutex<Option<String>>,
    null_output: bool,
}

impl ClientAudio {
    pub(crate) fn new(null_output: bool) -> Self {
        let receiver = AudioReceiver::new(CHANNELS, JitterConfig::default()).expect("stereo opus decoder");
        Self {
            shared: Arc::new(PlaybackShared {
                receiver: Mutex::new(receiver),
                epoch: Instant::now(),
                enabled: AtomicBool::new(false),
                volume_bits: AtomicU32::new(1.0f32.to_bits()),
                muted: AtomicBool::new(false),
                packets: AtomicU64::new(0),
                bytes: AtomicU64::new(0),
                underflows: AtomicU64::new(0),
                device_name: Mutex::new(None),
            }),
            output: Mutex::new(None),
            device: Mutex::new(None),
            null_output,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// An audio packet from the network (datagram or fallback stream).
    pub(crate) fn on_packet(&self, p: AudioPacket) {
        if !self.is_enabled() {
            return;
        }
        let s = &self.shared;
        s.packets.fetch_add(1, Ordering::Relaxed);
        s.bytes.fetch_add(p.data.len() as u64, Ordering::Relaxed);
        let now = s.now_us();
        s.receiver.lock().push(p, now);
    }

    /// Open the output (selected device, else the system default) and accept packets.
    pub(crate) fn start(&self) -> Result<()> {
        let mut out = self.output.lock();
        if out.is_none() {
            let device = self.device.lock().clone();
            *out = Some(self.open(device.as_deref())?);
        }
        self.shared.enabled.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
        let out = self.output.lock().take();
        drop(out);
        self.shared.receiver.lock().reset();
    }

    fn open(&self, device: Option<&str>) -> Result<Output> {
        if self.null_output {
            *self.shared.device_name.lock() = None;
            return Ok(null_output(self.shared.clone()));
        }
        device_output(self.shared.clone(), device)
    }

    /// Local gain 0.0–1.0 (and optionally mute); the host keeps streaming.
    pub fn set_volume(&self, volume: f32, muted: Option<bool>) {
        let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
        self.shared.volume_bits.store(v.to_bits(), Ordering::Relaxed);
        if let Some(m) = muted {
            self.shared.muted.store(m, Ordering::Relaxed);
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Select the output device (`None` = system default); reopens it when playing.
    pub fn set_output(&self, device: Option<String>) -> Result<()> {
        let device = device.filter(|d| !d.trim().is_empty());
        let mut out = self.output.lock();
        if out.is_some() {
            // Close first: some backends refuse a second stream on the same device.
            *out = None;
            match self.open(device.as_deref()) {
                Ok(o) => *out = Some(o),
                Err(e) => {
                    // Keep playing on the previous device rather than going silent.
                    let prev = self.device.lock().clone();
                    *out = self.open(prev.as_deref()).ok();
                    return Err(e);
                }
            }
        }
        *self.device.lock() = device;
        Ok(())
    }

    pub fn stats(&self) -> ClientAudioStats {
        let s = &self.shared;
        ClientAudioStats {
            enabled: self.is_enabled(),
            muted: s.muted.load(Ordering::Relaxed),
            volume: f32::from_bits(s.volume_bits.load(Ordering::Relaxed)),
            output_device: s.device_name.lock().clone(),
            packets_received: s.packets.load(Ordering::Relaxed),
            bytes_received: s.bytes.load(Ordering::Relaxed),
            output_underflows: s.underflows.load(Ordering::Relaxed),
            receiver: s.receiver.lock().stats(),
        }
    }
}

impl Drop for ClientAudio {
    fn drop(&mut self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
        drop(self.output.lock().take());
    }
}

/// Decode on a wall-clock timer and discard (no device).
fn null_output(shared: Arc<PlaybackShared>) -> Output {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("aether-audio-null".into())
        .spawn(move || {
            let start = Instant::now();
            let mut frames = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                let due = start + Duration::from_micros(frames * FRAME_US);
                let now = Instant::now();
                if due > now {
                    std::thread::sleep((due - now).min(Duration::from_millis(5)));
                    continue;
                }
                let _ = shared.pull();
                frames += 1;
            }
        })
        .ok();
    Output { stop, thread }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
mod device {
    use super::*;
    use aether_codec::audio::{Resampler, remix};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    fn invalid(key: &str) -> CoreError {
        CoreError::Invalid(key.to_owned())
    }

    pub fn list() -> Result<Vec<AudioOutputInfo>> {
        let host = cpal::default_host();
        let default = host.default_output_device().and_then(|d| d.id().ok()).map(|id| id.to_string());
        let devices = host.output_devices().map_err(|e| {
            tracing::debug!("listing audio outputs: {e}");
            invalid("errors.audioOutputUnavailable")
        })?;
        let mut out: Vec<AudioOutputInfo> = devices
            .filter_map(|d| {
                let id = d.id().ok()?.to_string();
                let name = d.description().map(|x| x.name().to_owned()).unwrap_or_else(|_| id.clone());
                Some(AudioOutputInfo { is_default: default.as_deref() == Some(id.as_str()), id, name })
            })
            .collect();
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    fn find(id: Option<&str>) -> Result<cpal::Device> {
        let host = cpal::default_host();
        match id {
            None => host.default_output_device().ok_or_else(|| invalid("errors.audioOutputUnavailable")),
            Some(id) => {
                let parsed: cpal::DeviceId = id.parse().map_err(|_| invalid("errors.audioOutputNotFound"))?;
                host.device_by_id(&parsed).ok_or_else(|| invalid("errors.audioOutputNotFound"))
            }
        }
    }

    fn find_with_config(id: Option<&str>) -> Result<(cpal::Device, cpal::SupportedStreamConfig, Option<String>)> {
        let device = find(id)?;
        let supported = device.default_output_config().map_err(|e| {
            tracing::warn!("audio output config: {e}");
            invalid("errors.audioOutputUnavailable")
        })?;
        let name = device.description().map(|d| d.name().to_owned()).ok();
        Ok((device, supported, name))
    }

    pub fn open(shared: Arc<PlaybackShared>, id: Option<&str>) -> Result<Output> {
        let id = id.map(str::to_owned);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let thread = std::thread::Builder::new()
            .name("aether-audio-out".into())
            .spawn(move || {
                let (device, supported, name) = match find_with_config(id.as_deref()) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let config: cpal::StreamConfig = supported.config();
                let rate = config.sample_rate;
                let channels = config.channels.max(1) as usize;
                // Ring: 250 ms of device audio.
                let (producer, consumer) = rtrb::RingBuffer::<f32>::new(rate as usize / 4 * channels);
                let callback_frames = Arc::new(AtomicU64::new(rate as u64 / 100));
                let built = match supported.sample_format() {
                    SampleFormat::F32 => build::<f32>(&device, config, consumer, &shared, &callback_frames),
                    SampleFormat::I16 => build::<i16>(&device, config, consumer, &shared, &callback_frames),
                    SampleFormat::I32 => build::<i32>(&device, config, consumer, &shared, &callback_frames),
                    SampleFormat::U16 => build::<u16>(&device, config, consumer, &shared, &callback_frames),
                    SampleFormat::F64 => build::<f64>(&device, config, consumer, &shared, &callback_frames),
                    other => {
                        tracing::warn!("unsupported output sample format {other}");
                        Err(invalid("errors.audioOutputUnavailable"))
                    }
                };
                let stream = match built.and_then(|s| {
                    s.play().map_err(|e| {
                        tracing::warn!("audio output play: {e}");
                        invalid("errors.audioOutputUnavailable")
                    })?;
                    Ok(s)
                }) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                tracing::info!(device = ?name, rate, channels, "audio output started");
                *shared.device_name.lock() = name;
                let _ = ready_tx.send(Ok(()));
                feed(&shared, producer, rate, channels, &callback_frames, &stop2);
                drop(stream);
            })
            .map_err(|_| invalid("errors.audioOutputUnavailable"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Output { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(invalid("errors.audioOutputUnavailable"))
            }
        }
    }

    fn build<T: SizedSample + FromSample<f32> + Send + 'static>(
        device: &cpal::Device,
        config: cpal::StreamConfig,
        mut consumer: rtrb::Consumer<f32>,
        shared: &Arc<PlaybackShared>,
        callback_frames: &Arc<AtomicU64>,
    ) -> Result<cpal::Stream> {
        let channels = config.channels.max(1) as usize;
        let (shared, cb_frames) = (shared.clone(), callback_frames.clone());
        let mut started = false;
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    cb_frames.store((data.len() / channels) as u64, Ordering::Relaxed);
                    let gain = shared.gain();
                    let mut short = false;
                    for s in data.iter_mut() {
                        *s = T::from_sample(match consumer.pop() {
                            Ok(v) => v * gain,
                            Err(_) => {
                                short = true;
                                0.0
                            }
                        });
                    }
                    if short && started {
                        shared.underflows.fetch_add(1, Ordering::Relaxed);
                    }
                    started |= !short;
                },
                |e| tracing::warn!("audio output stream: {e}"),
                None,
            )
            .map_err(|e| {
                tracing::warn!("building audio output stream: {e}");
                invalid("errors.audioOutputUnavailable")
            })
    }

    /// Keeps the ring at ~one device callback + 10 ms: pulls 10 ms frames from the receiver,
    /// resamples 48 kHz → device rate and maps stereo to the device channel layout.
    fn feed(
        shared: &PlaybackShared,
        mut producer: rtrb::Producer<f32>,
        rate: u32,
        channels: usize,
        callback_frames: &AtomicU64,
        stop: &AtomicBool,
    ) {
        let mut resampler = Resampler::new(OPUS_SAMPLE_RATE, rate, CHANNELS).ok();
        let capacity = producer.buffer().capacity();
        while !stop.load(Ordering::Relaxed) {
            let target = ((callback_frames.load(Ordering::Relaxed) as usize + rate as usize / 100) * channels).min(capacity);
            while capacity - producer.slots() < target && !stop.load(Ordering::Relaxed) {
                let (pcm, _) = shared.pull();
                let pcm = match resampler.as_mut() {
                    Some(r) => r.process(&pcm),
                    None => pcm,
                };
                let pcm = remix(&pcm, CHANNELS, channels.min(255) as u8);
                // Never split a frame: a partial write would misalign the channel interleave.
                if producer.slots() < pcm.len() {
                    break;
                }
                for s in pcm {
                    let _ = producer.push(s);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod device {
    use super::*;

    pub fn list() -> Result<Vec<AudioOutputInfo>> {
        Err(CoreError::Unsupported("audio output on this platform"))
    }

    pub fn open(_shared: Arc<PlaybackShared>, _id: Option<&str>) -> Result<Output> {
        Err(CoreError::Unsupported("audio output on this platform"))
    }
}

fn device_output(shared: Arc<PlaybackShared>, id: Option<&str>) -> Result<Output> {
    device::open(shared, id)
}

/// Output devices of the default audio host, default first.
pub fn list_audio_outputs() -> Result<Vec<AudioOutputInfo>> {
    device::list()
}

// =============================================================================================
// Self-test
// =============================================================================================

/// Result of [`loopback_selftest`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioSelftestReport {
    pub codec: String,
    pub implementation: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_ms: u8,
    pub bitrate_bps: u32,
    pub duration_ms: u32,
    pub tone_hz: f64,
    pub packets: u64,
    /// Sum of Opus payloads.
    pub opus_bytes: u64,
    /// Sum of serialized `Datagram::Audio` sizes.
    pub datagram_bytes: u64,
    pub max_datagram_bytes: usize,
    pub avg_packet_bytes: f64,
    pub effective_kbps: f64,
    /// Every packet survived `Datagram::Audio` serialization unchanged.
    pub serialization_ok: bool,
    pub injected_lost: Vec<u64>,
    pub injected_reordered: Vec<u64>,
    pub decoded_frames: u64,
    pub fec_frames: u64,
    pub plc_frames: u64,
    pub stretch_frames: u64,
    pub late_packets: u64,
    pub concealed_slots: u64,
    pub target_delay_ms: f64,
    pub encode_us_per_frame: f64,
    pub decode_us_per_frame: f64,
    /// SNR of a fitted 1 kHz sine over the steady-state output, including the concealed frame.
    pub snr_db: f64,
    /// SNR over a stretch after the impairments (no concealment inside).
    pub snr_db_clean: f64,
    pub peak_hz: f64,
    pub peak_amplitude: f64,
    pub passed: bool,
}

/// Synthesizes a 1 kHz stereo sine (1 s, as 10 ms capture packets), runs Opus encode →
/// `Datagram::Audio` serialize/deserialize → jitter buffer (one packet lost, two reordered,
/// small deterministic jitter) → decode with FEC/PLC, and measures the result.
pub fn loopback_selftest() -> AudioSelftestReport {
    const TONE: f64 = 1000.0;
    const AMP: f32 = 0.5;
    const PACKETS: u64 = 100;
    let lost = vec![30u64];
    let reordered = vec![50u64, 51];
    let cfg = AudioEncoderConfig::default();
    let mut enc = AudioEncoder::new(cfg.clone()).expect("opus encoder");

    // Capture side: 10 ms PcmF32Le packets, as a capturer would deliver them.
    let mut opus = Vec::new();
    let t_enc = Instant::now();
    for i in 0..PACKETS {
        let pcm = aether_capture::audio::sine(TONE as f32, AMP, 48_000, 2, i * 480, 480);
        let p = AudioPacket {
            seq: i,
            pts_us: i * FRAME_US,
            codec: AudioCodec::PcmF32Le,
            sample_rate: 48_000,
            channels: 2,
            data: pcm_to_f32le(&pcm),
        };
        opus.extend(enc.push_packet(&p).expect("encode"));
    }
    let encode_us = t_enc.elapsed().as_secs_f64() * 1e6 / opus.len().max(1) as f64;

    // Wire: serialize as datagrams and back.
    let mut wire = Vec::new();
    let (mut datagram_bytes, mut max_datagram, mut opus_bytes, mut serialization_ok) = (0u64, 0usize, 0u64, true);
    for p in &opus {
        let bytes = encode_message(&Datagram::Audio(p.clone())).expect("serialize");
        datagram_bytes += bytes.len() as u64;
        max_datagram = max_datagram.max(bytes.len());
        opus_bytes += p.data.len() as u64;
        match decode_message::<Datagram>(&bytes) {
            Ok(Datagram::Audio(q)) => {
                serialization_ok &= q == *p;
                wire.push(q);
            }
            _ => serialization_ok = false,
        }
    }

    // Network: arrival = pts + 4 ms + 0–3 ms jitter; loss and a swapped pair.
    let mut arrivals: Vec<(u64, AudioPacket)> = wire
        .into_iter()
        .filter(|p| !lost.contains(&p.seq))
        .map(|p| (p.pts_us + 4_000 + (p.seq * 7 % 4) * 1_000, p))
        .collect();
    for (at, p) in arrivals.iter_mut() {
        if p.seq == reordered[0] {
            *at = (reordered[1] * FRAME_US) + 6_000; // arrives after its successor
        }
    }
    arrivals.sort_by_key(|a| a.0);

    // Receiver, pulled every 10 ms.
    let mut rx = AudioReceiver::new(2, JitterConfig::default()).expect("decoder");
    let mut out = Vec::new();
    let mut kinds = Vec::new();
    let mut i = 0;
    let mut t = 0u64;
    let t_dec = Instant::now();
    let mut pulls = 0u64;
    while t < (PACKETS + 20) * FRAME_US {
        while i < arrivals.len() && arrivals[i].0 <= t {
            let (at, p) = arrivals[i].clone();
            rx.push(p, at);
            i += 1;
        }
        let (pcm, kind) = rx.pull(t);
        pulls += 1;
        let st = rx.stats();
        if kind != FrameKind::Silence || !out.is_empty() {
            out.extend(pcm);
            kinds.push(kind);
        }
        if st.jitter.played + st.jitter.lost >= PACKETS {
            break;
        }
        t += FRAME_US;
    }
    let decode_us = t_dec.elapsed().as_secs_f64() * 1e6 / pulls.max(1) as f64;
    let s = rx.stats();

    // Analysis on channel 0, skipping 100 ms of codec warm-up and the final frame.
    let frame = 480 * 2;
    let steady = &out[(10 * frame).min(out.len())..out.len().saturating_sub(frame)];
    let fit = sine_fit(steady, 2, 0, 48_000, TONE);
    let clean_start = (60 * frame).min(out.len());
    let clean = &out[clean_start..(clean_start + 30 * frame).min(out.len())];
    let clean_fit = sine_fit(clean, 2, 0, 48_000, TONE);
    let peak = peak_frequency(steady, 2, 0, 48_000);

    let concealed = s.fec_frames + s.plc_frames;
    let passed = serialization_ok
        && s.jitter.lost == lost.len() as u64
        && concealed == lost.len() as u64
        && s.jitter.late == 0
        && (peak - TONE).abs() < 5.0
        && clean_fit.snr_db > 25.0
        && fit.snr_db > 15.0;
    AudioSelftestReport {
        codec: "opus".into(),
        implementation: aether_codec::audio::opus_version(),
        sample_rate: OPUS_SAMPLE_RATE,
        channels: cfg.channels,
        frame_ms: cfg.frame_ms,
        bitrate_bps: cfg.bitrate_bps,
        duration_ms: (PACKETS * FRAME_US / 1000) as u32,
        tone_hz: TONE,
        packets: opus.len() as u64,
        opus_bytes,
        datagram_bytes,
        max_datagram_bytes: max_datagram,
        avg_packet_bytes: opus_bytes as f64 / opus.len().max(1) as f64,
        effective_kbps: opus_bytes as f64 * 8.0 / (PACKETS * FRAME_US) as f64 * 1000.0,
        serialization_ok,
        injected_lost: lost,
        injected_reordered: reordered,
        decoded_frames: s.decoded,
        fec_frames: s.fec_frames,
        plc_frames: s.plc_frames,
        stretch_frames: s.stretch_frames,
        late_packets: s.jitter.late,
        concealed_slots: s.jitter.lost,
        target_delay_ms: s.jitter.target_delay_us as f64 / 1000.0,
        encode_us_per_frame: encode_us,
        decode_us_per_frame: decode_us,
        snr_db: round2(fit.snr_db),
        snr_db_clean: round2(clean_fit.snr_db),
        peak_hz: round2(peak),
        peak_amplitude: round2(fit.amplitude),
        passed,
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selftest_passes() {
        let r = loopback_selftest();
        println!("{}", serde_json::to_string_pretty(&r).unwrap());
        assert!(r.passed, "{r:#?}");
        assert_eq!(r.packets, 100);
        assert_eq!(r.concealed_slots, 1);
        assert_eq!(r.fec_frames + r.plc_frames, 1);
        assert!(r.max_datagram_bytes < 1200, "fits a datagram: {r:?}");
    }

    #[test]
    fn client_audio_null_output_plays_pushed_packets() {
        let audio = ClientAudio::new(true);
        audio.on_packet(AudioPacket { seq: 0, pts_us: 0, codec: AudioCodec::Opus, sample_rate: 48_000, channels: 2, data: vec![1] });
        assert_eq!(audio.stats().packets_received, 0, "ignored while disabled");
        audio.start().unwrap();
        let mut enc = AudioEncoder::new(AudioEncoderConfig::default()).unwrap();
        let pcm = aether_capture::audio::sine(440.0, 0.3, 48_000, 2, 0, 4800);
        for p in enc.push_pcm(&pcm).unwrap() {
            audio.on_packet(p);
        }
        std::thread::sleep(Duration::from_millis(200));
        let s = audio.stats();
        assert_eq!(s.packets_received, 10);
        assert!(s.receiver.decoded >= 5, "{s:?}");
        audio.set_volume(0.25, Some(true));
        let s = audio.stats();
        assert!(s.muted && (s.volume - 0.25).abs() < 1e-6);
        audio.stop();
        assert!(!audio.stats().enabled);
    }

    #[test]
    fn listing_outputs_does_not_panic() {
        // CI machines may have no audio device; only the shape is checked.
        if let Ok(list) = list_audio_outputs() {
            assert!(list.iter().filter(|d| d.is_default).count() <= 1);
            assert!(list.iter().all(|d| !d.id.is_empty()));
        }
    }

    #[test]
    #[ignore = "opens the default output device (muted); needs audio hardware"]
    fn plays_on_default_device() {
        let audio = ClientAudio::new(false);
        audio.set_volume(0.0, Some(true));
        audio.start().unwrap();
        let mut enc = AudioEncoder::new(AudioEncoderConfig::default()).unwrap();
        let pcm = aether_capture::audio::sine(660.0, 0.2, 48_000, 2, 0, 48_000);
        let start = Instant::now();
        for (i, p) in enc.push_pcm(&pcm).unwrap().into_iter().enumerate() {
            while start.elapsed() < Duration::from_millis(i as u64 * 10) {
                std::thread::sleep(Duration::from_millis(1));
            }
            audio.on_packet(p);
        }
        std::thread::sleep(Duration::from_millis(200));
        let s = audio.stats();
        audio.stop();
        assert!(s.receiver.decoded >= 90, "{s:?}");
        assert!(s.output_device.is_some());
    }
}
