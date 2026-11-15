//! WASAPI loopback capture of the default render endpoint ("what the host is playing").
//!
//! A worker thread (COM MTA) opens `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender,
//! eConsole)`, initialises a shared-mode `IAudioClient` on the endpoint's mix format with
//! `AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK` (event-driven loopback
//! needs Windows 10 1703+; older systems fall back to 10 ms polling), drains
//! `IAudioCaptureClient` packets and converts them (float32/64, 8/16/24/32-bit integer,
//! `WAVEFORMATEXTENSIBLE`) to interleaved `f32` little-endian → `AudioPacket { PcmF32Le }`.
//! `AUDCLNT_BUFFERFLAGS_SILENT` packets become zeros. Timestamps come from the packets' QPC
//! position (same clock as the video timestamps of `crate::windows`).
//!
//! When the default device changes or disappears (`AUDCLNT_E_DEVICE_INVALIDATED`) the stream
//! is re-opened on the new default endpoint; later packets carry the new rate / channel count.
//! Note that WASAPI loopback produces *no* packets while nothing is playing (the timestamps
//! then jump); consumers must not assume a continuous stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ::windows::Win32::Foundation::{CloseHandle, HANDLE};
use ::windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use ::windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use ::windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{AudioCapturer, AudioFormat, AudioSink};
use crate::windows::{os_err, qpc_100ns, rel_us};
use crate::windows_pure::{SampleFormat, sample_format, samples_to_f32le};
use crate::{AudioCodec, AudioPacket, CaptureError, Result};

/// Requested shared-mode buffer: 20 ms (event mode) / 100 ms (polling), in 100 ns units.
const EVENT_BUFFER_HNS: i64 = 200_000;
const POLL_BUFFER_HNS: i64 = 1_000_000;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

pub fn create() -> Result<Box<dyn AudioCapturer>> {
    Ok(Box::new(WasapiCapturer::with_epoch(None)))
}

/// WASAPI loopback capturer (see module docs).
pub struct WasapiCapturer {
    /// 100 ns QPC time pts are relative to; `None` = the moment `start` is called.
    epoch: Option<i64>,
    running: Option<(Arc<AtomicBool>, JoinHandle<()>)>,
}

impl WasapiCapturer {
    /// `epoch` (100 ns QPC units) aligns audio pts with a screen capture started at that time.
    pub(crate) fn with_epoch(epoch: Option<i64>) -> Self {
        Self { epoch, running: None }
    }
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Option<HANDLE>,
    fmt: SampleFormat,
    block_align: usize,
    channels: u8,
    rate: u32,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the client was started by `open`; the event handle is owned by us.
        unsafe {
            let _ = self.client.Stop();
            if let Some(e) = self.event.take() {
                let _ = CloseHandle(e);
            }
        }
    }
}

struct MixFormat(*mut WAVEFORMATEX);

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat with CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

fn open() -> Result<Stream> {
    // SAFETY: standard COM activation sequence on an MTA thread; every pointer passed is
    // valid for the call and the mix format is freed by `MixFormat`.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| os_err("MMDeviceEnumerator", e))?;
        let device =
            enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| os_err("GetDefaultAudioEndpoint", e))?;
        let activate = || -> Result<IAudioClient> {
            device.Activate::<IAudioClient>(CLSCTX_ALL, None).map_err(|e| os_err("IMMDevice::Activate(IAudioClient)", e))
        };
        let mut client = activate()?;
        let mix = MixFormat(client.GetMixFormat().map_err(|e| os_err("GetMixFormat", e))?);
        let wf = &*mix.0;
        let (ext_sub, ext_valid) = if wf.wFormatTag == WAVE_FORMAT_EXTENSIBLE && wf.cbSize >= 22 {
            let ext = &*(mix.0 as *const WAVEFORMATEXTENSIBLE);
            (Some(ext.SubFormat.data1), Some(ext.Samples.wValidBitsPerSample))
        } else {
            (None, None)
        };
        let fmt = sample_format(wf.wFormatTag, wf.wBitsPerSample, ext_sub, ext_valid)
            .ok_or(CaptureError::Unsupported("unsupported WASAPI mix format"))?;
        let channels = wf.nChannels.min(u8::MAX as u16) as u8;
        let rate = wf.nSamplesPerSec;
        let block_align = wf.nBlockAlign as usize;
        if channels == 0 || block_align != fmt.bytes() * wf.nChannels as usize {
            return Err(CaptureError::Unsupported("inconsistent WASAPI mix format"));
        }

        // Event-driven loopback first, 10 ms polling as a fallback (pre-1703 systems).
        let event = CreateEventW(None, false, false, None).ok();
        let mut used_event = None;
        if let Some(ev) = event {
            let r = client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    EVENT_BUFFER_HNS,
                    0,
                    mix.0,
                    None,
                )
                .and_then(|_| client.SetEventHandle(ev));
            match r {
                Ok(()) => used_event = Some(ev),
                Err(e) => {
                    tracing::debug!("event-driven loopback unavailable ({e}); polling");
                    let _ = CloseHandle(ev);
                    client = activate()?; // Initialize may only be called once per client
                }
            }
        }
        if used_event.is_none() {
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, POLL_BUFFER_HNS, 0, mix.0, None)
                .map_err(|e| os_err("IAudioClient::Initialize(loopback)", e))?;
        }
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| os_err("GetService(IAudioCaptureClient)", e))?;
        let stream = Stream { client, capture, event: used_event, fmt, block_align, channels, rate };
        stream.client.Start().map_err(|e| os_err("IAudioClient::Start", e))?;
        Ok(stream)
    }
}

enum Drain {
    Ok,
    /// Device gone / changed: re-open on the (new) default endpoint.
    Reopen(String),
}

fn drain(s: &Stream, epoch: i64, seq: &mut u64, sink: &mut AudioSink) -> Drain {
    loop {
        // SAFETY: GetBuffer/ReleaseBuffer are paired; the buffer is read only in between and
        // spans `frames × block_align` bytes.
        let res = unsafe {
            let n = match s.capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(e) => return reopen_reason(e),
            };
            if n == 0 {
                return Drain::Ok;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            if let Err(e) = s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) {
                return reopen_reason(e);
            }
            let samples = frames as usize * s.channels as usize;
            let mut out = Vec::with_capacity(samples * 4);
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                out.resize(samples * 4, 0);
            } else {
                let bytes = std::slice::from_raw_parts(data, frames as usize * s.block_align);
                samples_to_f32le(bytes, s.fmt, &mut out);
            }
            let _ = s.capture.ReleaseBuffer(frames);
            (out, qpc)
        };
        let (data, qpc) = res;
        if data.is_empty() {
            continue;
        }
        let t = if qpc != 0 { qpc as i64 } else { qpc_100ns() };
        let packet = AudioPacket {
            seq: *seq,
            pts_us: rel_us(t, epoch),
            codec: AudioCodec::PcmF32Le,
            sample_rate: s.rate,
            channels: s.channels,
            data,
        };
        *seq += 1;
        sink(packet);
    }
}

fn reopen_reason(e: ::windows::core::Error) -> Drain {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        Drain::Reopen("default render device invalidated".into())
    } else {
        Drain::Reopen(format!("WASAPI capture error: {e}"))
    }
}

fn worker(epoch: i64, stop: Arc<AtomicBool>, mut sink: AudioSink, first: Stream) {
    let mut stream = Some(first);
    let mut seq = 0u64;
    let mut backoff = Duration::from_millis(100);
    while !stop.load(Ordering::Acquire) {
        let Some(s) = stream.as_ref() else {
            std::thread::sleep(backoff);
            match open() {
                Ok(s) => {
                    tracing::info!(rate = s.rate, channels = s.channels, "WASAPI loopback re-opened");
                    stream = Some(s);
                    backoff = Duration::from_millis(100);
                }
                Err(e) => {
                    tracing::debug!("WASAPI re-open failed: {e}");
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                }
            }
            continue;
        };
        match s.event {
            // Timeout (nothing playing) still drains once in case a signal was missed.
            // SAFETY: event handle owned by the stream.
            Some(ev) => {
                let _ = unsafe { WaitForSingleObject(ev, 100) };
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
        if let Drain::Reopen(reason) = drain(s, epoch, &mut seq, &mut sink) {
            tracing::info!("{reason}; re-opening WASAPI loopback");
            stream = None;
        }
    }
}

impl AudioCapturer for WasapiCapturer {
    fn start(&mut self, sink: AudioSink) -> Result<AudioFormat> {
        if self.running.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let epoch = self.epoch.unwrap_or_else(qpc_100ns);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel::<Result<AudioFormat>>(1);
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("aether-wasapi-loopback".into())
            .spawn(move || {
                // SAFETY: balanced with CoUninitialize at the end of this thread.
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
                match open() {
                    Ok(s) => {
                        let _ = tx.send(Ok(AudioFormat { sample_rate: s.rate, channels: s.channels }));
                        worker(epoch, st, sink, s);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
                if com {
                    // SAFETY: matches the successful CoInitializeEx.
                    unsafe { CoUninitialize() };
                }
            })
            .map_err(|e| CaptureError::Os(format!("spawn WASAPI thread: {e}")))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(fmt)) => {
                self.running = Some((stop, thread));
                Ok(fmt)
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                Err(CaptureError::Timeout("WASAPI loopback start"))
            }
        }
    }

    fn stop(&mut self) {
        if let Some((stop, thread)) = self.running.take() {
            stop.store(true, Ordering::Release);
            let _ = thread.join();
        }
    }
}

impl Drop for WasapiCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}
