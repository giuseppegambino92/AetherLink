//! Platform notes for the capture backends (Windows and Linux: implemented).
//!
//! Each section describes what a backend does and what it has (not) been verified on. OSes
//! without a backend return [`CaptureError::Unsupported`](crate::CaptureError::Unsupported)
//! from [`create_capturer`](crate::create_capturer), [`list_displays`](crate::list_displays)
//! and [`list_windows`](crate::list_windows).
//!
//! ## Windows (implemented — `crate::windows`, `crate::audio::windows`)
//!
//! Cross-compiled and clippy-clean for `x86_64-pc-windows-{gnu,msvc}`; the pure helpers
//! (`windows_pure`: rects, cursor compositing, downscale, rotation, PCM conversion) are unit
//! tested on every OS. The OS-API paths have **not** been run on real Windows hardware yet.
//!
//! *Displays: DXGI Desktop Duplication.*
//! 1. `CreateDXGIFactory1` → `IDXGIAdapter1` / `IDXGIOutput` attached to the desktop.
//!    `DisplayInfo::id` is `n` of the output's `\\.\DISPLAYn` device name (stable, matches
//!    Display Settings); bounds = `DesktopCoordinates` (physical px; the process is made
//!    per-monitor-v2 DPI aware), scale = `GetDpiForMonitor(MDT_EFFECTIVE_DPI) / 96`,
//!    refresh = `EnumDisplaySettingsW(ENUM_CURRENT_SETTINGS).dmDisplayFrequency`.
//! 2. `D3D11CreateDevice` on the output's adapter, `IDXGIOutput5::DuplicateOutput1(BGRA)`
//!    (fallback `IDXGIOutput1::DuplicateOutput`).
//! 3. Capture thread: frame pacing to `fps`, `AcquireNextFrame`; pointer-only updates re-emit
//!    the cached image with the cursor moved, idle frames are skipped. `GetFrameMoveRects`
//!    (source + destination) and `GetFrameDirtyRects` → `dirty_rects`; `CopyResource` into a
//!    `D3D11_USAGE_STAGING` texture, `Map`, only the dirty regions are copied into a
//!    persistent CPU image → `FramePixels::Bgra`.
//! 4. Cursor: the duplication never contains it; the shape from `GetFramePointerShape`
//!    (monochrome / color / masked-color) is composited when `show_cursor`, and its old and
//!    new rects are added to `dirty_rects` (`CaptureEvent` has no separate cursor channel).
//! 5. `DXGI_ERROR_ACCESS_LOST` & co. (desktop switch, UAC, mode change): the duplication is
//!    re-created after `OpenInputDesktop` + `SetThreadDesktop`, with backoff; outages longer
//!    than 3 s are reported once via `CaptureEvent::Error`. Capturing the Winlogon / UAC
//!    secure desktop requires running as LocalSystem in the user's session (`aether-winsvc`).
//! 6. Rotated outputs are rotated on the CPU (dirty rects reported as unknown); `scale < 1`
//!    is an area-average CPU downscale to even dimensions.
//! 7. Excluded windows (Ghost Mode curtain): `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
//!    on our own window — set by the window owner; `excluded_pids` is not used on Windows.
//!
//! *Windows and fallback: Windows.Graphics.Capture.* `CaptureTarget::Window { window_id }`
//! (`window_id` = `HWND`) and displays whose duplication fails with `DXGI_ERROR_UNSUPPORTED`
//! use `IGraphicsCaptureItemInterop::CreateForWindow/CreateForMonitor`, a free-threaded
//! `Direct3D11CaptureFramePool` (its `FrameArrived` only wakes the capture thread), pool
//! re-creation on `ContentSize` change, cursor via `IsCursorCaptureEnabled`, no yellow border
//! where allowed, WGC dirty regions on Windows 11 24H2+. `list_windows` = `EnumWindows`
//! filtered to visible, uncloaked, unowned, non-tool-window titled windows with
//! `DWMWA_EXTENDED_FRAME_BOUNDS`.
//!
//! *Audio: WASAPI loopback* on the default render endpoint, event-driven (polling fallback),
//! any shared-mode mix format → interleaved `f32` (`AudioCodec::PcmF32Le`), re-opened when
//! the default device changes; timestamps share the video's QPC clock.
//!
//! ## Linux (implemented — `crate::linux`, `crate::audio::linux`)
//!
//! Built and tested natively on arm64 Linux (Docker, `ci/linux-test.sh`).
//!
//! * *xdg-desktop-portal ScreenCast + PipeWire* (`linux::PortalCapturer`, Wayland and portal
//!   X11 desktops): `CreateSession → SelectSources(monitor | window, cursor embedded or
//!   metadata, persist_mode = 2 + stored restore token) → Start → OpenPipeWireRemote`, then
//!   `linux::PipeWireCapturer`: BGRx/BGRA/RGBx/RGBA, linear DMA-BUF preferred (mmap +
//!   `DMA_BUF_IOCTL_SYNC`) with MemFd/MemPtr fallback, `SPA_META_VideoDamage` →
//!   `dirty_rects`, `SPA_META_VideoCrop`, metadata cursor composited. The PipeWire stream
//!   path is tested against a real `Video/Source` node; the portal handshake itself needs a
//!   compositor portal backend and is **not** exercised in CI.
//! * *X11* (`linux::X11Capturer`): MIT-SHM (`ShmAttachFd` memfd, SysV fallback, plain
//!   `GetImage` for remote servers) per RandR 1.5 monitor or window rectangle, XFixes cursor,
//!   tile-diff dirty rects with idle-frame skipping; tested against Xvfb.
//! * `list_displays`: RandR on X11; on Wayland only the monitors granted by the last portal
//!   session (or the Xwayland RandR view). `list_windows`: X11 only (`_NET_CLIENT_LIST`).
//! * Audio: default-sink monitor through PipeWire (`stream.capture.sink`) or PulseAudio
//!   (`@DEFAULT_MONITOR@`), f32 48 kHz stereo; both tested with `pipewire-pulse` + null sink.
//! * Not possible: excluding our own windows (`excluded_pids`); DMA-BUFs with tiled GPU
//!   modifiers (would need an EGL/Vulkan import) are not negotiated.
#![allow(dead_code)]

/// Message returned by the portable entry points on platforms without a backend.
pub(crate) const UNSUPPORTED_MSG: &str =
    "screen capture backend not implemented for this OS (see aether_capture::platform_notes)";
