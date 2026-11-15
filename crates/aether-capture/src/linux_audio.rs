//! Linux system-audio loopback: the monitor of the default sink as interleaved `f32` PCM
//! (48 kHz stereo; the sound server converts).
//!
//! * [`PipeWireAudioCapturer`] — native PipeWire capture stream with
//!   `stream.capture.sink = true` (the session manager links it to the default sink's monitor
//!   ports and follows default-sink changes).
//! * [`PulseAudioCapturer`] — `pa_simple` record stream on `@DEFAULT_MONITOR@`, for systems still
//!   running PulseAudio (also works against `pipewire-pulse`).
//! * [`LinuxAudioCapturer`] (what `create_audio_capturer` returns) tries PipeWire first, then
//!   PulseAudio; `AETHERLINK_AUDIO_BACKEND=pipewire|pulse` forces one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::audio::{AudioFormat as SpaAudioFormat, AudioInfoRaw};
use spa::pod::{Object, Pod, Value, serialize::PodSerializer};

use crate::audio::{AudioCapturer, AudioFormat, AudioSink};
use crate::{AudioCodec, AudioPacket, CaptureError, Result};

/// Sample rate requested from the sound server.
pub const AUDIO_RATE: u32 = 48_000;
/// Channel count requested from the sound server.
pub const AUDIO_CHANNELS: u8 = 2;
const FRAME_BYTES: usize = AUDIO_CHANNELS as usize * 4;

/// Shared sink slot: `stop` takes the sink out under the lock, so no packet is delivered
/// after `stop` returns even if a backend thread is still blocked in a read.
type SinkSlot = Arc<Mutex<Option<AudioSink>>>;

/// Builds packets with a sample-count based timeline (µs since the first packet).
struct Packetizer {
    seq: u64,
    frames: u64,
}

impl Packetizer {
    fn packet(&mut self, data: Vec<u8>) -> AudioPacket {
        let n = (data.len() / FRAME_BYTES) as u64;
        let p = AudioPacket {
            seq: self.seq,
            pts_us: self.frames * 1_000_000 / AUDIO_RATE as u64,
            codec: AudioCodec::PcmF32Le,
            sample_rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS,
            data,
        };
        self.seq += 1;
        self.frames += n;
        p
    }
}

fn deliver(slot: &SinkSlot, p: AudioPacket) -> bool {
    match slot.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        Some(s) => {
            s(p);
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------------------------
// PipeWire
// ---------------------------------------------------------------------------------------------

/// Default-sink monitor capture through a native PipeWire stream.
#[derive(Default)]
pub struct PipeWireAudioCapturer {
    slot: Option<SinkSlot>,
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireAudioCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

fn audio_format_pod() -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(SpaAudioFormat::F32LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS as u32);
    let mut pos = [0u32; spa::param::audio::MAX_CHANNELS];
    pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    let obj = Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(c, _)| c.into_inner())
        .unwrap_or_default()
}

impl AudioCapturer for PipeWireAudioCapturer {
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat> {
        if self.thread.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let slot: SinkSlot = Arc::new(Mutex::new(Some(sink)));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<pw::channel::Sender<()>>>();
        let thread_slot = slot.clone();
        let thread = std::thread::Builder::new()
            .name("aether-pipewire-audio".into())
            .spawn(move || {
                pw::init();
                let fail = |ctx: &str, e: pw::Error| {
                    if ctx == "connect" {
                        CaptureError::Unsupported("cannot connect to PipeWire (is the pipewire daemon running?)")
                    } else {
                        CaptureError::Os(format!("PipeWire {ctx}: {e}"))
                    }
                };
                let mainloop = match pw::main_loop::MainLoopRc::new(None) {
                    Ok(m) => m,
                    Err(e) => return drop(ready_tx.send(Err(fail("loop", e)))),
                };
                let context = match pw::context::ContextRc::new(&mainloop, None) {
                    Ok(c) => c,
                    Err(e) => return drop(ready_tx.send(Err(fail("context", e)))),
                };
                let core = match context.connect_rc(None) {
                    Ok(c) => c,
                    Err(e) => return drop(ready_tx.send(Err(fail("connect", e)))),
                };
                let (quit_tx, quit_rx) = pw::channel::channel::<()>();
                let ml = mainloop.clone();
                let _q = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());
                let stream = match pw::stream::StreamBox::new(
                    &core,
                    "aetherlink-system-audio",
                    properties! {
                        *pw::keys::MEDIA_TYPE => "Audio",
                        *pw::keys::MEDIA_CATEGORY => "Capture",
                        *pw::keys::MEDIA_ROLE => "Music",
                        *pw::keys::STREAM_CAPTURE_SINK => "true",
                        *pw::keys::NODE_NAME => "aetherlink-system-audio",
                        *pw::keys::NODE_LATENCY => "480/48000",
                    },
                ) {
                    Ok(s) => s,
                    Err(e) => return drop(ready_tx.send(Err(fail("stream", e)))),
                };
                let listener = stream
                    .add_local_listener_with_user_data(Packetizer { seq: 0, frames: 0 })
                    .process(move |s, pk| {
                        let Some(mut b) = s.dequeue_buffer() else { return };
                        let datas = b.datas_mut();
                        let Some(d) = datas.first_mut() else { return };
                        let (off, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                        let Some(bytes) = d.data() else { return };
                        let end = (off + size).min(bytes.len());
                        let usable = (end.saturating_sub(off) / FRAME_BYTES) * FRAME_BYTES;
                        if usable == 0 {
                            return;
                        }
                        let pkt = pk.packet(bytes[off..off + usable].to_vec());
                        deliver(&thread_slot, pkt);
                    })
                    .register();
                let _listener = match listener {
                    Ok(l) => l,
                    Err(e) => return drop(ready_tx.send(Err(fail("listener", e)))),
                };
                let fmt = audio_format_pod();
                let Some(pod) = Pod::from_bytes(&fmt) else {
                    return drop(ready_tx.send(Err(CaptureError::Os("audio format pod".into()))));
                };
                let mut pods = [pod];
                if let Err(e) = stream.connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut pods,
                ) {
                    return drop(ready_tx.send(Err(CaptureError::Os(format!("PipeWire audio connect: {e}")))));
                }
                if ready_tx.send(Ok(quit_tx)).is_ok() {
                    mainloop.run();
                }
                let _ = stream.disconnect();
            })
            .map_err(|e| CaptureError::Os(format!("spawn audio thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(q)) => {
                self.quit = Some(q);
                self.slot = Some(slot);
                self.thread = Some(thread);
                Ok(AudioFormat { sample_rate: AUDIO_RATE, channels: AUDIO_CHANNELS })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(CaptureError::Timeout("PipeWire audio stream")),
        }
    }

    fn stop(&mut self) {
        if let Some(s) = self.slot.take() {
            s.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PipeWireAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// PulseAudio
// ---------------------------------------------------------------------------------------------

/// Default-sink monitor capture through libpulse-simple (`@DEFAULT_MONITOR@`).
#[derive(Default)]
pub struct PulseAudioCapturer {
    slot: Option<SinkSlot>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PulseAudioCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AudioCapturer for PulseAudioCapturer {
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat> {
        use libpulse_binding::def::BufferAttr;
        use libpulse_binding::sample::{Format, Spec};
        use libpulse_binding::stream::Direction;
        use libpulse_simple_binding::Simple;

        if self.thread.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let slot: SinkSlot = Arc::new(Mutex::new(Some(sink)));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (thread_slot, thread_stop) = (slot.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("aether-pulse-audio".into())
            .spawn(move || {
                let spec = Spec { format: Format::F32le, channels: AUDIO_CHANNELS, rate: AUDIO_RATE };
                // 10 ms fragments.
                let frag = (AUDIO_RATE as usize / 100 * FRAME_BYTES) as u32;
                let attr = BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: frag };
                let pa = match Simple::new(None, "AetherLink", Direction::Record, Some("@DEFAULT_MONITOR@"), "system audio", &spec, None, Some(&attr)) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(CaptureError::Os(format!("PulseAudio @DEFAULT_MONITOR@: {e}"))));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                let mut pk = Packetizer { seq: 0, frames: 0 };
                let mut buf = vec![0u8; frag as usize];
                while !thread_stop.load(Ordering::Acquire) {
                    if let Err(e) = pa.read(&mut buf) {
                        tracing::warn!("PulseAudio read failed: {e}");
                        break;
                    }
                    if !deliver(&thread_slot, pk.packet(buf.clone())) {
                        break;
                    }
                }
            })
            .map_err(|e| CaptureError::Os(format!("spawn audio thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                self.slot = Some(slot);
                self.stop = stop;
                self.thread = Some(thread);
                Ok(AudioFormat { sample_rate: AUDIO_RATE, channels: AUDIO_CHANNELS })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(CaptureError::Timeout("PulseAudio record stream")),
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(s) = self.slot.take() {
            s.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
        if let Some(t) = self.thread.take() {
            // A suspended monitor can block `pa_simple_read` indefinitely; the sink is already
            // detached, so let the thread finish on its own instead of hanging the caller.
            if t.is_finished() {
                let _ = t.join();
            } else {
                std::thread::sleep(Duration::from_millis(30));
                if t.is_finished() {
                    let _ = t.join();
                }
            }
        }
    }
}

impl Drop for PulseAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------
// Automatic selection
// ---------------------------------------------------------------------------------------------

/// Which sound-server API delivers the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxAudioBackend {
    PipeWire,
    PulseAudio,
}

/// PipeWire first, PulseAudio as fallback (see module docs).
#[derive(Default)]
pub struct LinuxAudioCapturer {
    inner: Option<(LinuxAudioBackend, Box<dyn AudioCapturer>)>,
}

impl LinuxAudioCapturer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The backend of the running capture.
    pub fn active_backend(&self) -> Option<LinuxAudioBackend> {
        self.inner.as_ref().map(|(b, _)| *b)
    }
}

impl AudioCapturer for LinuxAudioCapturer {
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat> {
        if self.inner.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let order: &[LinuxAudioBackend] = match std::env::var("AETHERLINK_AUDIO_BACKEND").map(|v| v.to_ascii_lowercase()).as_deref() {
            Ok("pulse" | "pulseaudio") => &[LinuxAudioBackend::PulseAudio],
            Ok("pipewire") => &[LinuxAudioBackend::PipeWire],
            _ => &[LinuxAudioBackend::PipeWire, LinuxAudioBackend::PulseAudio],
        };
        // The sink is FnMut + Send but not Clone: share it between attempts.
        let shared: SinkSlot = Arc::new(Mutex::new(Some(sink)));
        let mut last = CaptureError::Unsupported("no Linux audio backend");
        for &b in order {
            let s = shared.clone();
            let forward: AudioSink = Box::new(move |p| {
                deliver(&s, p);
            });
            let mut cap: Box<dyn AudioCapturer> = match b {
                LinuxAudioBackend::PipeWire => Box::new(PipeWireAudioCapturer::new()),
                LinuxAudioBackend::PulseAudio => Box::new(PulseAudioCapturer::new()),
            };
            match cap.start(forward) {
                Ok(f) => {
                    tracing::info!(backend = ?b, "system audio capture started");
                    self.inner = Some((b, cap));
                    return Ok(f);
                }
                Err(e) => {
                    tracing::info!(backend = ?b, "audio backend unavailable: {e}");
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn stop(&mut self) {
        if let Some((_, mut c)) = self.inner.take() {
            c.stop();
        }
    }
}

impl Drop for LinuxAudioCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    //! Live tests need a sound server with a default sink (pipewire + wireplumber +
    //! pipewire-pulse and a null sink, see ci/linux-test.sh). A real tone is played with
    //! `paplay` and must show up in the captured monitor samples.
    use super::*;
    use std::process::Command;

    #[test]
    fn packetizer_timeline() {
        let mut p = Packetizer { seq: 0, frames: 0 };
        let a = p.packet(vec![0; 480 * FRAME_BYTES]);
        let b = p.packet(vec![0; 480 * FRAME_BYTES]);
        assert_eq!((a.seq, a.pts_us), (0, 0));
        assert_eq!((b.seq, b.pts_us), (1, 10_000));
        assert_eq!(b.codec, AudioCodec::PcmF32Le);
    }

    fn write_tone_wav(path: &std::path::Path) {
        let rate = 48_000u32;
        let n = rate as usize * 2; // 2 s
        let mut pcm = Vec::with_capacity(n * 4);
        for i in 0..n {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.5 * i16::MAX as f32) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        std::fs::write(path, wav).unwrap();
    }

    fn capture_tone(mut cap: Box<dyn AudioCapturer>, tag: &str) -> (usize, f32) {
        let (tx, rx) = mpsc::channel::<AudioPacket>();
        let fmt = cap
            .start(Box::new(move |p| {
                let _ = tx.send(p);
            }))
            .unwrap();
        assert_eq!(fmt, AudioFormat { sample_rate: 48_000, channels: 2 });
        let wav = std::env::temp_dir().join(format!("aether-tone-{tag}-{}.wav", std::process::id()));
        write_tone_wav(&wav);
        std::thread::sleep(Duration::from_millis(300));
        let mut player = Command::new("paplay").arg(&wav).spawn().expect("paplay (pulseaudio-utils)");
        let deadline = std::time::Instant::now() + Duration::from_millis(2500);
        let (mut packets, mut peak) = (0usize, 0f32);
        let mut last_seq = None;
        while std::time::Instant::now() < deadline {
            if let Ok(p) = rx.recv_timeout(Duration::from_millis(100)) {
                assert_eq!((p.sample_rate, p.channels, p.codec), (48_000, 2, AudioCodec::PcmF32Le));
                assert_eq!(p.data.len() % FRAME_BYTES, 0);
                if let Some(s) = last_seq {
                    assert_eq!(p.seq, s + 1);
                }
                last_seq = Some(p.seq);
                packets += 1;
                for s in p.data.as_chunks::<4>().0.iter() {
                    peak = peak.max(f32::from_le_bytes([s[0], s[1], s[2], s[3]]).abs());
                }
            }
        }
        cap.stop();
        let _ = player.wait();
        let _ = std::fs::remove_file(&wav);
        (packets, peak)
    }

    #[test]
    #[ignore = "needs pipewire + wireplumber + pipewire-pulse with a default sink (ci/linux-test.sh)"]
    fn pipewire_monitor_captures_played_tone() {
        let (packets, peak) = capture_tone(Box::new(PipeWireAudioCapturer::new()), "pw");
        println!("pipewire: {packets} packets, peak {peak}");
        assert!(packets > 20, "{packets} packets");
        assert!(peak > 0.2, "tone not captured (peak {peak})");
    }

    #[test]
    #[ignore = "needs pipewire + wireplumber + pipewire-pulse with a default sink (ci/linux-test.sh)"]
    fn pulse_monitor_captures_played_tone() {
        let (packets, peak) = capture_tone(Box::new(PulseAudioCapturer::new()), "pulse");
        println!("pulse: {packets} packets, peak {peak}");
        assert!(packets > 20, "{packets} packets");
        assert!(peak > 0.2, "tone not captured (peak {peak})");
    }

    #[test]
    #[ignore = "needs pipewire + wireplumber + pipewire-pulse with a default sink (ci/linux-test.sh)"]
    fn auto_backend_prefers_pipewire() {
        let mut c = LinuxAudioCapturer::new();
        c.start(Box::new(|_| {})).unwrap();
        assert_eq!(c.active_backend(), Some(LinuxAudioBackend::PipeWire));
        c.stop();
        assert_eq!(c.active_backend(), None);
    }
}
