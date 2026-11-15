//! JNI bridge to the Kotlin side (`dev.aetherlink.mobile.NativeBridge`).
//!
//! Kotlin → Rust (hot path, `external fun` on the `NativeBridge` object):
//! encoded H.265/H.264 access units and PCM from `ScreenShareService`, display metrics.
//!
//! Rust → Kotlin (methods on the same object, via a global reference):
//! `requestKeyframe`, `viewersChanged`, `inputReady`, `inject`, `hidSendReport`.

use aether_core::mobile::{MobileEncoderControl, MobileHost};
use aether_input::android::{AndroidInput, AndroidInputSink, TouchState};
use aether_protocol::VideoCodec;
use jni::objects::{GlobalRef, JByteBuffer, JObject, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jlong};
use jni::{JNIEnv, JavaVM};
use std::sync::{Arc, OnceLock};

static VM: OnceLock<JavaVM> = OnceLock::new();
static BRIDGE: OnceLock<GlobalRef> = OnceLock::new();
static HOST: OnceLock<MobileHost> = OnceLock::new();

pub fn host() -> Option<MobileHost> {
    HOST.get().cloned()
}

/// Register the platform host and the input sink (Rust side; Kotlin attaches via `nativeInit`).
pub fn install(_app: &tauri::AppHandle) {
    let host = HOST.get_or_init(|| MobileHost::install(Arc::new(JniEncoderControl), "Android"));
    let _ = host;
    aether_input::android::set_sink(Some(Arc::new(JniInput)));
}

/// Run `f` with an attached JNIEnv and the bridge object (no-op before `nativeInit`).
fn with_bridge<T>(f: impl FnOnce(&mut JNIEnv<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Option<T> {
    let vm = VM.get()?;
    let bridge = BRIDGE.get()?;
    let mut env = vm.attach_current_thread_permanently().ok()?;
    match f(&mut env, bridge.as_obj()) {
        Ok(v) => Some(v),
        Err(e) => {
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            tracing::warn!("JNI call failed: {e}");
            None
        }
    }
}

struct JniEncoderControl;

impl MobileEncoderControl for JniEncoderControl {
    fn request_keyframe(&self) {
        with_bridge(|env, b| env.call_method(b, "requestKeyframe", "()V", &[]).map(|_| ()));
    }

    fn viewers_changed(&self, viewers: usize) {
        with_bridge(|env, b| env.call_method(b, "viewersChanged", "(I)V", &[JValue::Int(viewers as jint)]).map(|_| ()));
    }
}

struct JniInput;

/// Wire encoding of [`AndroidInput`] for `NativeBridge.inject(kind, x, y, a, b, code, meta, text)`.
fn encode(cmd: &AndroidInput) -> (jint, f32, f32, f32, f32, jint, jint, Option<&str>) {
    match cmd {
        AndroidInput::PointerDown { x, y } => (0, *x, *y, 0.0, 0.0, 0, 0, None),
        AndroidInput::PointerMove { x, y } => (1, *x, *y, 0.0, 0.0, 0, 0, None),
        AndroidInput::PointerUp { x, y } => (2, *x, *y, 0.0, 0.0, 0, 0, None),
        AndroidInput::LongPress { x, y } => (3, *x, *y, 0.0, 0.0, 0, 0, None),
        AndroidInput::Scroll { x, y, dx, dy } => (4, *x, *y, *dx, *dy, 0, 0, None),
        AndroidInput::Pinch { x, y, scale } => (5, *x, *y, *scale, 0.0, 0, 0, None),
        AndroidInput::Touch { id, x, y, state } => {
            let s = match state {
                TouchState::Down => 0,
                TouchState::Move => 1,
                TouchState::Up => 2,
                TouchState::Cancel => 3,
            };
            (6, *x, *y, 0.0, 0.0, *id as jint, s, None)
        }
        AndroidInput::Key { keycode, down, meta_state } => (7, 0.0, 0.0, if *down { 1.0 } else { 0.0 }, 0.0, *keycode, *meta_state, None),
        AndroidInput::Text { text } => (8, 0.0, 0.0, 0.0, 0.0, 0, 0, Some(text.as_str())),
        AndroidInput::Global(g) => (9, 0.0, 0.0, 0.0, 0.0, g.android_constant(), 0, None),
        AndroidInput::ReleaseAll => (10, 0.0, 0.0, 0.0, 0.0, 0, 0, None),
    }
}

impl AndroidInputSink for JniInput {
    fn ready(&self) -> bool {
        static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
        let ok = with_bridge(|env, b| env.call_method(b, "inputReady", "()Z", &[])?.z()).unwrap_or(false);
        // Log transitions only (view-only sessions ask on every event).
        if LAST.swap(ok as u8, std::sync::atomic::Ordering::Relaxed) != ok as u8 {
            tracing::info!(ready = ok, "remote input backend");
        }
        ok
    }

    fn send(&self, cmd: AndroidInput) -> Result<(), String> {
        let (kind, x, y, a, bb, code, meta, text) = encode(&cmd);
        let ok = with_bridge(|env, b| {
            let jtext = match text {
                Some(t) => JObject::from(env.new_string(t)?),
                None => JObject::null(),
            };
            env.call_method(
                b,
                "inject",
                "(IFFFFIILjava/lang/String;)Z",
                &[
                    JValue::Int(kind),
                    JValue::Float(x),
                    JValue::Float(y),
                    JValue::Float(a),
                    JValue::Float(bb),
                    JValue::Int(code),
                    JValue::Int(meta),
                    JValue::Object(&jtext),
                ],
            )?
            .z()
        });
        match ok {
            Some(true) => Ok(()),
            Some(false) => {
                tracing::warn!(kind, "input backend rejected the event");
                Err("input backend rejected the event".into())
            }
            None => Err("input backend not attached".into()),
        }
    }
}

/// Send one HID input report through `BluetoothHidDevice.sendReport`.
pub fn hid_send_report(id: u8, data: &[u8]) -> Result<(), String> {
    let ok = with_bridge(|env, b| {
        let arr = env.byte_array_from_slice(data)?;
        env.call_method(b, "hidSendReport", "(I[B)Z", &[JValue::Int(id as jint), JValue::Object(&arr)])?.z()
    });
    match ok {
        Some(true) => Ok(()),
        _ => Err("errors.hidNotConnected".into()),
    }
}

/// Bytes `[offset, offset + len)` of a direct `ByteBuffer`.
fn direct_slice<'a>(env: &JNIEnv<'_>, buf: &JByteBuffer<'_>, offset: jint, len: jint) -> Option<&'a [u8]> {
    let ptr = env.get_direct_buffer_address(buf).ok()?;
    let cap = env.get_direct_buffer_capacity(buf).ok()?;
    let (off, len) = (usize::try_from(offset).ok()?, usize::try_from(len).ok()?);
    if ptr.is_null() || off.checked_add(len)? > cap {
        return None;
    }
    // SAFETY: the range was bounds-checked against the buffer capacity; Kotlin keeps the
    // buffer alive (and unmodified) for the duration of the native call.
    Some(unsafe { std::slice::from_raw_parts(ptr.add(off), len) })
}

// ---------------------------------------------------------------- Kotlin → Rust

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeInit<'l>(env: JNIEnv<'l>, this: JObject<'l>) {
    if let Ok(vm) = env.get_java_vm() {
        let _ = VM.set(vm);
    }
    if let Ok(g) = env.new_global_ref(&this) {
        let _ = BRIDGE.set(g);
    }
    tracing::info!("NativeBridge attached");
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeSetDisplay<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    width: jint,
    height: jint,
    scale: jfloat,
    refresh_hz: jfloat,
) {
    if let Some(h) = HOST.get() {
        h.set_display(width.max(1) as u32, height.max(1) as u32, scale, refresh_hz);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeVideoConfig<'l>(
    _env: JNIEnv<'l>,
    _this: JObject<'l>,
    hevc: jboolean,
    width: jint,
    height: jint,
    fps: jint,
) {
    if let Some(h) = HOST.get() {
        let codec = if hevc != 0 { VideoCodec::Hevc } else { VideoCodec::H264 };
        h.push_config(codec, width.max(1) as u32, height.max(1) as u32, fps.clamp(1, 240) as u16);
        tracing::info!(?codec, width, height, fps, "platform encoder configured");
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeVideoCodecConfig<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
    buf: JByteBuffer<'l>,
    offset: jint,
    len: jint,
) {
    if let (Some(h), Some(data)) = (HOST.get(), direct_slice(&env, &buf, offset, len)) {
        h.push_codec_config(data);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeVideoFrame<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
    buf: JByteBuffer<'l>,
    offset: jint,
    len: jint,
    pts_us: jlong,
    keyframe: jboolean,
) {
    if let (Some(h), Some(data)) = (HOST.get(), direct_slice(&env, &buf, offset, len)) {
        h.push_frame(data, pts_us.max(0) as u64, keyframe != 0);
    }
}

/// PCM from `AudioPlaybackCapture`: 16-bit (`is_float == false`) or 32-bit float, interleaved.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeAudio<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
    buf: JByteBuffer<'l>,
    len: jint,
    sample_rate: jint,
    channels: jint,
    is_float: jboolean,
    pts_us: jlong,
) {
    let Some(bytes) = direct_slice(&env, &buf, 0, len) else { return };
    let (rate, ch, pts) = (sample_rate.max(1) as u32, channels.clamp(1, 8) as u8, pts_us.max(0) as u64);
    if is_float != 0 {
        let f: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        aether_capture::audio::mobile::push_pcm(&f, rate, ch, pts);
    } else {
        let s: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
        aether_capture::audio::mobile::push_pcm_i16(&s, rate, ch, pts);
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeAudioWanted<'l>(_env: JNIEnv<'l>, _this: JObject<'l>) -> jboolean {
    (aether_capture::audio::mobile::active_capturers() > 0) as jboolean
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeHostStopped<'l>(_env: JNIEnv<'l>, _this: JObject<'l>) {
    if let Some(h) = HOST.get() {
        h.push_stopped();
    }
    aether_capture::audio::mobile::platform_stopped();
    tracing::info!("screen sharing stopped");
}

/// `Build.MODEL` etc.: replaces a generic default device name (`localhost`, empty).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeSetDefaultDeviceName<'l>(
    mut env: JNIEnv<'l>,
    _this: JObject<'l>,
    name: JString<'l>,
) {
    let Ok(name) = env.get_string(&name).map(String::from) else { return };
    crate::mobile::apply_default_device_name(name);
}

/// HID report descriptor (single source of truth: `aether_input::hid_report::REPORT_DESCRIPTOR`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_aetherlink_mobile_NativeBridge_nativeHidDescriptor<'l>(
    env: JNIEnv<'l>,
    _this: JObject<'l>,
) -> jni::sys::jbyteArray {
    env.byte_array_from_slice(aether_input::hid_report::REPORT_DESCRIPTOR)
        .map(|a| a.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
