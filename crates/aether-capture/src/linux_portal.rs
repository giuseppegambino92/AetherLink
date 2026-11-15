//! xdg-desktop-portal ScreenCast session (`ashpd` on zbus) feeding [`PipeWireCapturer`].
//!
//! The portal handshake runs on a dedicated thread driven by zbus's own executor, so callers
//! may live inside or outside an async runtime. That thread keeps the D-Bus
//! connection and the portal session alive until the capture stops, then closes the session.
//!
//! Restore tokens (`persist_mode = 2`, "until explicitly revoked") are stored per source kind
//! in `$XDG_STATE_HOME/aetherlink/` (default `~/.local/state/aetherlink/`) so the picker
//! dialog is shown only the first time.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::enumflags2::BitFlags;

use super::PipeWireCapturer;
use crate::{CaptureConfig, CaptureError, CaptureSink, CaptureTarget, DisplayInfo, Rect, Result, ScreenCapturer};

/// How long `start` waits for the portal (which may be showing its picker to the user).
const PORTAL_TIMEOUT: Duration = Duration::from_secs(120);

/// Kind of source requested from the portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Monitor,
    Window,
}

impl SourceKind {
    fn token_name(self) -> &'static str {
        match self {
            SourceKind::Monitor => "screencast-monitor",
            SourceKind::Window => "screencast-window",
        }
    }
}

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|d| d.join("aetherlink"))
}

/// Where the ScreenCast restore token for monitor (`window = false`) or window captures is
/// stored. Delete the file to make the portal ask again.
pub fn portal_restore_token_path(window: bool) -> Option<PathBuf> {
    let kind = if window { SourceKind::Window } else { SourceKind::Monitor };
    state_dir().map(|d| d.join(format!("{}.token", kind.token_name())))
}

fn read_token(kind: SourceKind) -> Option<String> {
    let p = portal_restore_token_path(kind == SourceKind::Window)?;
    let t = std::fs::read_to_string(p).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn write_token(kind: SourceKind, token: &str) {
    let Some(p) = portal_restore_token_path(kind == SourceKind::Window) else { return };
    let res = (|| -> std::io::Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("tmp");
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(token.as_bytes())?;
        }
        std::fs::rename(&tmp, &p)
    })();
    if let Err(e) = res {
        tracing::warn!("cannot persist the ScreenCast restore token at {}: {e}", p.display());
    }
}

fn map_portal_error(e: ashpd::Error) -> CaptureError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => CaptureError::PermissionDenied,
        ashpd::Error::PortalNotFound(_) => {
            CaptureError::Unsupported("xdg-desktop-portal ScreenCast interface not available (install a portal backend)")
        }
        e => CaptureError::Os(format!("ScreenCast portal: {e}")),
    }
}

/// One stream granted by the portal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GrantedStream {
    pub node_id: u32,
    /// Logical position / size in the compositor's desktop space, when reported.
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// A live portal session (D-Bus thread) plus the PipeWire remote it opened.
pub(crate) struct PortalSession {
    pub fd: Option<OwnedFd>,
    pub streams: Vec<GrantedStream>,
    /// Cursor is delivered as `SPA_META_Cursor` (must be composited by us).
    pub cursor_metadata: bool,
    stop: Option<futures::channel::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PortalSession {
    fn open(kind: SourceKind, show_cursor: bool) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (stop_tx, stop_rx) = futures::channel::oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("aether-screencast-portal".into())
            .spawn(move || {
                ashpd::zbus::block_on(async move {
                    match handshake(kind, show_cursor).await {
                        Ok((proxy, session, granted)) => {
                            let _ = tx.send(Ok(granted));
                            // Keep the session open until the capturer stops (or is dropped).
                            let _ = stop_rx.await;
                            if let Err(e) = session.close().await {
                                tracing::debug!("closing the ScreenCast session: {e}");
                            }
                            drop(proxy);
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                        }
                    }
                });
            })
            .map_err(|e| CaptureError::Os(format!("spawn portal thread: {e}")))?;
        let (fd, streams, cursor_metadata) = match rx.recv_timeout(PORTAL_TIMEOUT) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            // The thread finishes on its own once the portal answers (stop_tx is dropped).
            Err(_) => return Err(CaptureError::Timeout("the ScreenCast portal (picker dialog)")),
        };
        Ok(Self { fd: Some(fd), streams, cursor_metadata, stop: Some(stop_tx), thread: Some(thread) })
    }

    fn close(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        self.close();
    }
}

type Granted = (OwnedFd, Vec<GrantedStream>, bool);

async fn handshake(
    kind: SourceKind,
    show_cursor: bool,
) -> Result<(Screencast, ashpd::desktop::Session<Screencast>, Granted)> {
    let proxy = Screencast::new().await.map_err(map_portal_error)?;
    let modes = proxy.available_cursor_modes().await.unwrap_or_else(|_| BitFlags::empty());
    let cursor = if !show_cursor {
        CursorMode::Hidden
    } else if modes.contains(CursorMode::Embedded) {
        CursorMode::Embedded
    } else if modes.contains(CursorMode::Metadata) {
        CursorMode::Metadata
    } else {
        CursorMode::Hidden
    };
    let sources: BitFlags<SourceType> = match kind {
        SourceKind::Monitor => BitFlags::from_flag(SourceType::Monitor),
        SourceKind::Window => BitFlags::from_flag(SourceType::Window),
    };
    let session = proxy.create_session(Default::default()).await.map_err(map_portal_error)?;
    let token = read_token(kind);
    let mut opts = SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(sources)
        .set_multiple(kind == SourceKind::Monitor)
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    if let Some(t) = token.as_deref() {
        opts = opts.set_restore_token(t);
    }
    proxy.select_sources(&session, opts).await.map_err(map_portal_error)?.response().map_err(map_portal_error)?;
    let started = proxy.start(&session, None, Default::default()).await.map_err(map_portal_error)?.response().map_err(map_portal_error)?;
    if let Some(t) = started.restore_token()
        && token.as_deref() != Some(t)
    {
        write_token(kind, t);
    }
    let streams: Vec<GrantedStream> = started
        .streams()
        .iter()
        .map(|s| GrantedStream { node_id: s.pipe_wire_node_id(), position: s.position(), size: s.size() })
        .collect();
    if streams.is_empty() {
        return Err(CaptureError::TargetNotFound("the ScreenCast portal granted no streams".into()));
    }
    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await.map_err(map_portal_error)?;
    Ok((proxy, session, (fd, streams, cursor == CursorMode::Metadata)))
}

/// Granted monitor streams as displays: id = 1-based order of the portal's answer (stable
/// across sessions restored with the same token), bounds in the compositor's logical space.
fn streams_to_displays(streams: &[GrantedStream]) -> Vec<DisplayInfo> {
    streams
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (x, y) = s.position.unwrap_or((0, 0));
            let (w, h) = s.size.unwrap_or((0, 0));
            DisplayInfo {
                id: i as u32 + 1,
                name: format!("Portal monitor {}", i + 1),
                bounds: Rect::new(x, y, w.max(0) as u32, h.max(0) as u32),
                scale: 1.0,
                refresh_hz: 60.0,
                primary: i == 0,
                is_virtual: false,
            }
        })
        .collect()
}

/// ScreenCast-portal capturer (Wayland and portal-enabled X11 desktops).
///
/// `start` blocks while the portal shows its picker (first run only, thanks to the restore
/// token; up to 120 s). For [`CaptureTarget::Display`] the granted monitor with the same id
/// (see [`crate::list_displays`]) is used, else the first granted one; for
/// [`CaptureTarget::Window`] the user picks the window in the portal dialog.
#[derive(Default)]
pub struct PortalCapturer {
    session: Option<PortalSession>,
    stream: Option<PipeWireCapturer>,
}

impl PortalCapturer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ScreenCapturer for PortalCapturer {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, sink: CaptureSink) -> Result<()> {
        if self.session.is_some() {
            return Err(CaptureError::AlreadyRunning);
        }
        let kind = match target {
            CaptureTarget::Display { .. } => SourceKind::Monitor,
            CaptureTarget::Window { .. } => SourceKind::Window,
        };
        let mut session = PortalSession::open(kind, cfg.show_cursor)?;
        let (display_id, node_id) = match target {
            CaptureTarget::Display { display_id } => {
                let displays = streams_to_displays(&session.streams);
                super::remember_portal_displays(displays);
                let idx = (display_id as usize).checked_sub(1).filter(|i| *i < session.streams.len()).unwrap_or_else(|| {
                    tracing::info!(display_id, "display not among the portal-granted monitors; using the first one");
                    0
                });
                (idx as u32 + 1, session.streams[idx].node_id)
            }
            CaptureTarget::Window { .. } => (0, session.streams[0].node_id),
        };
        let fd = session.fd.take().ok_or(CaptureError::Os("PipeWire remote already consumed".into()))?;
        let mut pw = PipeWireCapturer::with_remote(fd, node_id, display_id);
        pw.set_composite_metadata_cursor(session.cursor_metadata && cfg.show_cursor);
        pw.start(target, cfg, sink)?;
        self.session = Some(session);
        self.stream = Some(pw);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut s) = self.stream.take() {
            s.stop();
        }
        if let Some(mut s) = self.session.take() {
            s.close();
        }
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_paths_follow_xdg_state_home() {
        let p = portal_restore_token_path(false).unwrap();
        assert!(p.ends_with("aetherlink/screencast-monitor.token"), "{p:?}");
        let w = portal_restore_token_path(true).unwrap();
        assert!(w.ends_with("aetherlink/screencast-window.token"), "{w:?}");
    }

    #[test]
    fn granted_streams_become_displays() {
        let d = streams_to_displays(&[
            GrantedStream { node_id: 40, position: Some((0, 0)), size: Some((1920, 1080)) },
            GrantedStream { node_id: 41, position: Some((1920, 0)), size: Some((1280, 1024)) },
        ]);
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].id, 2);
        assert_eq!(d[1].bounds, Rect::new(1920, 0, 1280, 1024));
        assert!(d[0].primary && !d[1].primary);
    }
}
