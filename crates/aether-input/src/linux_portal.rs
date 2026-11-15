//! Wayland injector through `org.freedesktop.portal.RemoteDesktop` (no root, no uinput).
//!
//! Session setup (on a dedicated D-Bus thread driven by zbus's executor):
//! `CreateSession → SelectDevices(keyboard | pointer, persist_mode = until revoked,
//! restore token) → ScreenCast.SelectSources(monitors, multiple)` on the same session (absolute
//! pointer motion is expressed relative to a granted screencast stream) `→ Start`. The first
//! start shows the compositor's consent dialog; the restore token is stored in
//! `$XDG_STATE_HOME/aetherlink/remote-desktop.token` so later sessions start unattended.
//!
//! Events are forwarded fire-and-forget over a channel to the D-Bus thread; a portal error is
//! reported by the next `inject` call. Keys use `NotifyKeyboardKeycode` with evdev codes,
//! text uses `NotifyKeyboardKeysym` (any Unicode character), buttons are evdev `BTN_*`
//! codes, smooth scrolling uses `NotifyPointerAxis` (+ `finish` on gesture end), line
//! scrolling `NotifyPointerAxisDiscrete`.
//!
//! Display mapping: a [`DisplayInfo`] is matched to the granted stream with the same logical
//! position (else by order); normalized coordinates are multiplied by the stream size.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use aether_protocol::{DisplayInfo, Gesture, GesturePhase, HidUsage, InputEvent, Modifiers, MouseButton, ScrollUnit, SystemAction};
use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::enumflags2::BitFlags;
use futures::StreamExt;
use futures::channel::mpsc as fmpsc;

use crate::state::{Accumulator, ModifierState, button_bit, plan_key};
use crate::{InputError, InputInjector, Result, keymap};

const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;

fn evdev_button(b: MouseButton) -> i32 {
    match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

/// Where the RemoteDesktop restore token is stored.
pub fn restore_token_path() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("aetherlink").join("remote-desktop.token"))
}

fn read_token() -> Option<String> {
    let t = std::fs::read_to_string(restore_token_path()?).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn write_token(token: &str) {
    let Some(p) = restore_token_path() else { return };
    let res = (|| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(token.as_bytes())?;
        drop(f);
        std::fs::rename(&tmp, &p)
    })();
    if let Err(e) = res {
        tracing::warn!("cannot persist the RemoteDesktop restore token at {}: {e}", p.display());
    }
}

/// A screencast stream granted with the RemoteDesktop session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortalStream {
    pub node_id: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

#[derive(Debug)]
enum Cmd {
    Key(i32, bool),
    Keysym(i32, bool),
    Button(i32, bool),
    MotionAbs(u32, f64, f64),
    Motion(f64, f64),
    Axis(f64, f64, bool),
    AxisDiscrete(Axis, i32),
}

fn map_err(e: ashpd::Error) -> InputError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            InputError::PermissionDenied("remote control was declined in the RemoteDesktop portal dialog")
        }
        ashpd::Error::PortalNotFound(_) => {
            InputError::Unsupported("xdg-desktop-portal RemoteDesktop interface not available")
        }
        e => InputError::Os(format!("RemoteDesktop portal: {e}")),
    }
}

async fn open_session() -> Result<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>, Vec<PortalStream>)> {
    let rd = RemoteDesktop::new().await.map_err(map_err)?;
    let session = rd.create_session(Default::default()).await.map_err(map_err)?;
    let token = read_token();
    let mut opts = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    if let Some(t) = token.as_deref() {
        opts = opts.set_restore_token(t);
    }
    rd.select_devices(&session, opts).await.map_err(map_err)?.response().map_err(map_err)?;
    // Absolute pointer motion needs screencast streams in the same session.
    let sc = Screencast::with_connection(rd.connection().clone()).await.map_err(map_err)?;
    sc.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_sources(BitFlags::from_flag(SourceType::Monitor))
            .set_multiple(true)
            .set_cursor_mode(CursorMode::Hidden),
    )
    .await
    .map_err(map_err)?
    .response()
    .map_err(map_err)?;
    let started = rd.start(&session, None, Default::default()).await.map_err(map_err)?.response().map_err(map_err)?;
    if !started.devices().contains(DeviceType::Pointer) && !started.devices().contains(DeviceType::Keyboard) {
        return Err(InputError::PermissionDenied("the RemoteDesktop portal granted no input devices"));
    }
    if let Some(t) = started.restore_token()
        && token.as_deref() != Some(t)
    {
        write_token(t);
    }
    let streams = started
        .streams()
        .iter()
        .map(|s| PortalStream { node_id: s.pipe_wire_node_id(), position: s.position(), size: s.size() })
        .collect();
    Ok((rd, session, streams))
}

async fn dispatch(rd: &RemoteDesktop, s: &ashpd::desktop::Session<RemoteDesktop>, cmd: Cmd) -> ashpd::Result<()> {
    let st = |p: bool| if p { KeyState::Pressed } else { KeyState::Released };
    match cmd {
        Cmd::Key(code, p) => rd.notify_keyboard_keycode(s, code, st(p), Default::default()).await,
        Cmd::Keysym(sym, p) => rd.notify_keyboard_keysym(s, sym, st(p), Default::default()).await,
        Cmd::Button(b, p) => rd.notify_pointer_button(s, b, st(p), Default::default()).await,
        Cmd::MotionAbs(stream, x, y) => rd.notify_pointer_motion_absolute(s, stream, x, y, Default::default()).await,
        Cmd::Motion(dx, dy) => rd.notify_pointer_motion(s, dx, dy, Default::default()).await,
        Cmd::Axis(dx, dy, finish) => {
            rd.notify_pointer_axis(s, dx, dy, NotifyPointerAxisOptions::default().set_finish(finish)).await
        }
        Cmd::AxisDiscrete(axis, steps) => rd.notify_pointer_axis_discrete(s, axis, steps, Default::default()).await,
    }
}

/// RemoteDesktop-portal injector.
pub struct PortalInjector {
    tx: Option<fmpsc::UnboundedSender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    error: Arc<Mutex<Option<String>>>,
    streams: Vec<PortalStream>,
    displays: Vec<DisplayInfo>,
    mods: ModifierState,
    keys_down: Vec<u16>,
    buttons: u8,
    detent_v: Accumulator,
    detent_h: Accumulator,
}

impl PortalInjector {
    /// Opens the RemoteDesktop session (may show the portal's consent dialog; up to 120 s).
    pub fn new(displays: Vec<DisplayInfo>) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Vec<PortalStream>>>();
        let (tx, mut rx) = fmpsc::unbounded::<Cmd>();
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let err_slot = error.clone();
        let thread = std::thread::Builder::new()
            .name("aether-remote-desktop".into())
            .spawn(move || {
                ashpd::zbus::block_on(async move {
                    let (rd, session, streams) = match open_session().await {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    if ready_tx.send(Ok(streams)).is_err() {
                        let _ = session.close().await;
                        return;
                    }
                    while let Some(cmd) = rx.next().await {
                        if let Err(e) = dispatch(&rd, &session, cmd).await {
                            *err_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                        }
                    }
                    let _ = session.close().await;
                });
            })
            .map_err(InputError::Io)?;
        let streams = match ready_rx.recv_timeout(PORTAL_TIMEOUT) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => return Err(InputError::Os("timed out waiting for the RemoteDesktop portal".into())),
        };
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
            error,
            streams,
            displays,
            mods: ModifierState::default(),
            keys_down: Vec::new(),
            buttons: 0,
            detent_v: Accumulator::default(),
            detent_h: Accumulator::default(),
        })
    }

    /// Streams granted with the session (logical monitor geometry).
    pub fn streams(&self) -> &[PortalStream] {
        &self.streams
    }

    fn send(&self, c: Cmd) -> Result<()> {
        if let Some(e) = self.error.lock().unwrap_or_else(|p| p.into_inner()).take() {
            return Err(InputError::Os(format!("RemoteDesktop portal: {e}")));
        }
        self.tx
            .as_ref()
            .and_then(|t| t.unbounded_send(c).ok())
            .ok_or(InputError::Os("the RemoteDesktop session has ended".into()))
    }

    fn stream_for(&self, display_id: u32) -> Result<PortalStream> {
        let idx = self.displays.iter().position(|d| d.id == display_id).ok_or(InputError::UnknownDisplay(display_id))?;
        let d = &self.displays[idx];
        self.streams
            .iter()
            .find(|s| s.position == Some((d.bounds.x, d.bounds.y)))
            .or_else(|| self.streams.get(idx))
            .or_else(|| self.streams.first())
            .copied()
            .ok_or(InputError::Unsupported("the RemoteDesktop session has no screencast stream for absolute pointer motion"))
    }

    fn key(&mut self, code: HidUsage, pressed: bool, modifiers: Modifiers) -> Result<()> {
        keymap::hid_to_linux_evdev(code)
            .ok_or_else(|| InputError::InvalidArgument(format!("HID usage {:#04x} has no evdev code", code.0)))?;
        for s in plan_key(&mut self.mods, code, pressed, modifiers) {
            if !s.code.is_modifier() {
                self.keys_down.retain(|k| *k != s.code.0);
                if s.pressed {
                    self.keys_down.push(s.code.0);
                }
            }
            if let Some(k) = keymap::hid_to_linux_evdev(s.code) {
                self.send(Cmd::Key(k as i32, s.pressed))?;
            }
        }
        Ok(())
    }

    fn combo(&mut self, mods: &[HidUsage], key: HidUsage) -> Result<()> {
        let code = |u: HidUsage| keymap::hid_to_linux_evdev(u).map(|c| c as i32).unwrap_or(0);
        let added: Vec<HidUsage> = mods.iter().copied().filter(|m| !self.mods.is_down(*m)).collect();
        for m in &added {
            self.send(Cmd::Key(code(*m), true))?;
        }
        self.send(Cmd::Key(code(key), true))?;
        self.send(Cmd::Key(code(key), false))?;
        for m in added.iter().rev() {
            self.send(Cmd::Key(code(*m), false))?;
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        for k in std::mem::take(&mut self.keys_down) {
            if let Some(c) = keymap::hid_to_linux_evdev(HidUsage(k)) {
                self.send(Cmd::Key(c as i32, false))?;
            }
        }
        for m in self.mods.pressed() {
            if let Some(c) = keymap::hid_to_linux_evdev(m) {
                self.send(Cmd::Key(c as i32, false))?;
            }
        }
        self.mods = ModifierState::default();
        for b in [MouseButton::Left, MouseButton::Right, MouseButton::Middle, MouseButton::Back, MouseButton::Forward] {
            if self.buttons & button_bit(b) != 0 {
                self.send(Cmd::Button(evdev_button(b), false))?;
            }
        }
        self.buttons = 0;
        Ok(())
    }
}

impl Drop for PortalInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        self.tx.take(); // closes the channel → the D-Bus thread closes the session
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl InputInjector for PortalInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match ev {
            InputEvent::MouseMove { display_id, x, y } => {
                let s = self.stream_for(*display_id)?;
                let (w, h) = s.size.unwrap_or_else(|| {
                    let d = self.displays.iter().find(|d| d.id == *display_id);
                    d.map(|d| (d.bounds.width as i32, d.bounds.height as i32)).unwrap_or((1, 1))
                });
                let nx = if x.is_finite() { x.clamp(0.0, 1.0) as f64 } else { 0.0 };
                let ny = if y.is_finite() { y.clamp(0.0, 1.0) as f64 } else { 0.0 };
                let px = (nx * w as f64).min((w - 1).max(0) as f64);
                let py = (ny * h as f64).min((h - 1).max(0) as f64);
                self.send(Cmd::MotionAbs(s.node_id, px, py))
            }
            InputEvent::MouseMoveRelative { dx, dy } => self.send(Cmd::Motion(*dx as f64, *dy as f64)),
            InputEvent::MouseButton { button, pressed } => {
                let bit = button_bit(*button);
                if *pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                self.send(Cmd::Button(evdev_button(*button), *pressed))
            }
            InputEvent::Scroll { dx, dy, unit, phase } => match (unit, phase) {
                (_, Some(GesturePhase::Cancel)) => {
                    self.detent_v.reset();
                    self.detent_h.reset();
                    self.send(Cmd::Axis(0.0, 0.0, true))
                }
                (ScrollUnit::Line, _) => {
                    let v = self.detent_v.take(*dy as f64) as i32;
                    let h = self.detent_h.take(*dx as f64) as i32;
                    if v != 0 {
                        self.send(Cmd::AxisDiscrete(Axis::Vertical, v))?;
                    }
                    if h != 0 {
                        self.send(Cmd::AxisDiscrete(Axis::Horizontal, h))?;
                    }
                    Ok(())
                }
                (ScrollUnit::Pixel, p) => {
                    let finish = matches!(p, Some(GesturePhase::End));
                    if *dx == 0.0 && *dy == 0.0 && !finish {
                        return Ok(());
                    }
                    self.send(Cmd::Axis(*dx as f64, *dy as f64, finish))
                }
            },
            InputEvent::Key { code, pressed, modifiers } => self.key(*code, *pressed, *modifiers),
            InputEvent::Text { text } => {
                for c in text.chars() {
                    let Some(sym) = crate::linux_backend::char_to_keysym(c) else { continue };
                    self.send(Cmd::Keysym(sym as i32, true))?;
                    self.send(Cmd::Keysym(sym as i32, false))?;
                }
                Ok(())
            }
            InputEvent::Gesture { gesture: Gesture::SwipeNavigate { dx, dy }, phase } => {
                if *phase != GesturePhase::End || dx.abs() <= dy.abs() {
                    return Ok(());
                }
                let key = if *dx > 0.0 { HidUsage::ARROW_LEFT } else { HidUsage::ARROW_RIGHT };
                self.combo(&[HidUsage::ALT_LEFT], key)
            }
            InputEvent::Gesture { .. } => Err(InputError::Unsupported("pinch/rotate gestures are not exposed by the RemoteDesktop portal")),
            InputEvent::Touch { .. } => Err(InputError::Unsupported("touch injection through the portal is not implemented")),
            InputEvent::System(a) => match a {
                SystemAction::Screenshot => self.combo(&[], HidUsage::PRINT_SCREEN),
                SystemAction::LockScreen => self.combo(&[HidUsage::META_LEFT], HidUsage::L),
                SystemAction::ShowDesktop => self.combo(&[HidUsage::META_LEFT], HidUsage::D),
                SystemAction::MissionControl => {
                    let k = keymap::hid_to_linux_evdev(HidUsage::META_LEFT).unwrap_or(125) as i32;
                    self.send(Cmd::Key(k, true))?;
                    self.send(Cmd::Key(k, false))
                }
                SystemAction::CtrlAltDel => self.combo(&[HidUsage::CONTROL_LEFT, HidUsage::ALT_LEFT], HidUsage::DELETE),
                _ => Err(InputError::Unsupported("Android navigation actions are not available on Linux")),
            },
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn update_displays(&mut self, d: Vec<DisplayInfo>) {
        self.displays = d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_path_is_under_state_dir() {
        assert!(restore_token_path().unwrap().ends_with("aetherlink/remote-desktop.token"));
        assert_eq!(evdev_button(MouseButton::Back), BTN_SIDE);
    }
}
