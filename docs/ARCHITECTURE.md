# AetherLink — Architecture & Crate Contracts

This document is the binding contract between crates. `aether-core` consumes the
APIs below; `apps/desktop` consumes `aether-core` through the Tauri command
contract at the bottom. Signatures may be *extended* (extra methods, extra
fields with defaults) but not changed without updating this file.

## Ground rules for every crate

* Rust 2024 edition, stable toolchain. `cargo clippy -p <crate> --all-targets` must be warning-free.
* Errors: one `thiserror` enum per crate (`<Crate>Error`), `pub type Result<T> = std::result::Result<T, <Crate>Error>`.
  Platform gaps return an explicit `Unsupported(&'static str)` variant — **never fake data**.
* Platform code lives in `#[cfg(target_os = "...")]` modules behind a portable trait + `create_*()` factory.
* Tests that need real hardware, OS permissions (Screen Recording, Accessibility) or root are `#[ignore = "reason"]`.
* No crate except `aether-net` depends on `quinn`. Stream-oriented crates are generic over
  `tokio::io::AsyncRead/AsyncWrite` and use `aether_protocol::{read_frame, write_frame}`.
* Wire types come from `aether-protocol` (frozen; see its rustdoc). Pixel format everywhere is **BGRA8**.

## Data flow (host → client)

```
ScreenCaptureKit/DXGI/PipeWire ──CapturedFrame (IOSurface/CVPixelBuffer | BGRA)──►
   HybridScheduler ──► VideoEncoder (VideoToolbox HEVC 4:4:4 / H.264) ──► VideoMessage::Frame
                   └─► TileEncoder (64px tiles, BLAKE3-hash diff, zstd) ──► VideoMessage::Tiles (refinement after 80 ms idle)
   ──► quinn uni stream (StreamKind::Video) ──► client: WebCodecs VideoDecoder (in webview) + tile patches ──► <canvas>
                                                     └─► aether-timetravel Blackbox (encrypted 180 s ring)
client input (DOM KeyboardEvent.code → HidUsage) ──► datagram/stream ──► aether-input InputInjector (CGEvent/SendInput/uinput)
```

## aether-net

```rust
pub struct Identity;                       // Ed25519 keypair
impl Identity {
    pub fn generate() -> Self;
    pub fn from_secret_bytes(b: [u8; 32]) -> Self;
    pub fn secret_bytes(&self) -> [u8; 32];
    pub fn public_key(&self) -> [u8; 32];
    pub fn device_id(&self) -> DeviceId;
    pub fn fingerprint(&self) -> KeyFingerprint;
    /// OS keychain (keyring crate) with 0600 file fallback in `dir`.
    pub fn load_or_create(service: &str, dir: &Path) -> Result<Self>;
}
pub struct TrustStore;                      // trusted peer keys + names, JSON-persisted
impl TrustStore { pub fn open(path: &Path) -> Result<Self>; pub fn is_trusted(&self, key: &[u8;32]) -> bool;
                  pub fn add(&mut self, peer: TrustedPeer) -> Result<()>; pub fn remove(&mut self, key: &[u8;32]) -> Result<()>;
                  pub fn list(&self) -> Vec<TrustedPeer>; }
pub enum ClientAuth { Trusted, Password(String) }
pub trait HostAuthPolicy: Send + Sync + 'static {
    fn is_trusted(&self, peer: &Hello) -> bool;
    fn password(&self) -> Option<String>;         // current rotating one-time password / pairing PIN
    /// Interactive consent; may await a UI prompt.
    fn consent(&self, peer: &Hello) -> BoxFuture<'static, bool>;
}
pub struct AetherEndpoint;
impl AetherEndpoint {
    pub fn bind(addr: SocketAddr, identity: Identity) -> Result<Self>;
    pub fn from_std_socket(sock: std::net::UdpSocket, identity: Identity) -> Result<Self>;
    pub fn local_addr(&self) -> Result<SocketAddr>;
    pub async fn connect(&self, addr: SocketAddr, hello: Hello, auth: ClientAuth) -> Result<Session>;
    pub async fn accept(&self, hello: Hello, policy: Arc<dyn HostAuthPolicy>) -> Option<Result<Session>>;
}
pub struct Session { pub conn: PeerConnection, pub control_tx: FramedSend, pub control_rx: FramedRecv, pub remote_hello: Hello }
#[derive(Clone)] pub struct PeerConnection;  // wraps quinn::Connection
impl PeerConnection {
    pub async fn open_bi(&self, kind: StreamKind) -> Result<(FramedSend, FramedRecv)>;
    pub async fn open_uni(&self, kind: StreamKind) -> Result<FramedSend>;
    pub async fn accept_stream(&self) -> Result<IncomingStream>;   // Bi { kind, send, recv } | Uni { kind, recv }
    pub fn send_datagram(&self, d: &Datagram) -> Result<()>;
    pub async fn recv_datagram(&self) -> Result<Datagram>;
    pub fn rtt(&self) -> Duration; pub fn remote_address(&self) -> SocketAddr; pub fn peer_public_key(&self) -> [u8;32];
    pub fn close(&self, reason: &str);
}
// FramedSend / FramedRecv implement AsyncWrite / AsyncRead (raw passthrough for tunnels) and
// `send<T: Serialize>(&mut self, &T)`, `recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>>`, `finish()`.

pub mod discovery { LanAdvertiser::start(&LanAdvertisement) -> Result<LanAdvertiser>; LanBrowser::start() -> Result<LanBrowser>;
                    LanBrowser::next_event().await -> Option<LanEvent>  /* Found(LanPeer) | Lost(DeviceId) */ }
pub mod stun     { pub async fn binding(sock: &tokio::net::UdpSocket, server: SocketAddr) -> Result<SocketAddr>; }
pub mod rendezvous { RendezvousClient (register / lookup / punch / relay allocation) — wire types in aether_protocol::rendezvous }
pub mod tunnel {
    pub async fn forward_local(conn: PeerConnection, bind: SocketAddr, remote_port: u16) -> Result<TunnelHandle>;
    pub async fn serve_tunnel(remote_port: u16, send: FramedSend, recv: FramedRecv, policy: &TunnelPolicy) -> Result<()>;
    pub fn scan_listening_ports() -> Vec<ListeningPort>;
}
pub async fn connect_to_device(endpoint, rendezvous, target: DeviceId, hello, auth) -> Result<Session>; // direct → hole-punch → relay
```

Security: TLS 1.3 (rustls) inside QUIC; each side's self-signed cert carries its Ed25519 identity key and the
verifiers accept any cert but **record** the peer key; trust is then enforced by the post-TLS handshake:
pinned key (`TrustedKey`) or SPAKE2 password bound to a TLS keying-material exporter (`Password`).
The relay only forwards UDP datagrams of the encrypted QUIC connection.

## aether-capture

```rust
pub struct CapturedFrame { pub display_id: u32, pub width: u32, pub height: u32, pub pts_us: u64,
                           pub dirty_rects: Option<Vec<Rect>>, /* from SCK/DXGI metadata when available */
                           pub pixels: FramePixels }
pub enum FramePixels { Bgra { stride: usize, data: Arc<[u8]> }, #[cfg(target_os="macos")] PixelBuffer(macos::PixelBuffer) }
impl CapturedFrame { pub fn to_bgra(&self) -> Result<BgraImage>; }        // locks + copies for native buffers
pub struct BgraImage { pub width: u32, pub height: u32, pub stride: usize, pub data: Vec<u8> }
pub struct CaptureConfig { pub fps: u16, pub show_cursor: bool, pub scale: f32, pub capture_audio: bool,
                           pub excluded_pids: Vec<u32> /* our own windows, e.g. the Ghost Mode curtain */ }
pub trait ScreenCapturer: Send {
    fn start(&mut self, target: CaptureTarget, cfg: CaptureConfig, sink: Box<dyn FnMut(CaptureEvent) + Send>) -> Result<()>;
    fn stop(&mut self);
}
pub enum CaptureEvent { Frame(CapturedFrame), Audio(AudioPacket), Error(CaptureError) }
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>>;
pub fn list_displays() -> Result<Vec<DisplayInfo>>;  pub fn list_windows() -> Result<Vec<WindowInfo>>;
pub fn permission_granted() -> bool;  pub fn request_permission() -> bool;
pub struct TestPatternCapturer;   // deterministic animated source for tests/benchmarks (not a mock of an OS)
```

## aether-codec

```rust
pub struct TileEncoder;  impl TileEncoder { pub fn new(tile: u16) -> Self;
    pub fn encode(&mut self, img: &BgraImage, display_id: u32, seq: u64, pts_us: u64, force_full: bool) -> Option<DirtyTileBatch>; }
pub struct Framebuffer;  impl Framebuffer { pub fn apply(&mut self, b: &DirtyTileBatch) -> Result<Vec<Rect>>;
    pub fn width/height/pixels(&self); pub fn patch_rgba(&self, r: Rect) -> Vec<u8>; }
pub struct EncoderConfig { pub codec: VideoCodec, pub chroma: ChromaFormat, pub width: u32, pub height: u32,
                           pub fps: u16, pub bitrate_kbps: u32, pub low_latency: bool }
pub trait VideoEncoder: Send { fn encode(&mut self, f: &CapturedFrame, force_keyframe: bool) -> Result<Option<EncodedFrame>>;
                               fn set_bitrate(&mut self, kbps: u32) -> Result<()>; fn config(&self) -> &EncoderConfig; }
pub trait VideoDecoder: Send { fn decode(&mut self, f: &VideoFrame) -> Result<Option<BgraImage>>; }
pub fn create_encoder(cfg: EncoderConfig) -> Result<Box<dyn VideoEncoder>>;  // VideoToolbox on Apple
pub fn create_decoder(codec: VideoCodec) -> Result<Box<dyn VideoDecoder>>;
pub fn encoder_capabilities() -> Vec<(VideoCodec, ChromaFormat)>;
pub struct HybridScheduler;  // motion tracking; emits lossless refinement tiles for regions static ≥ 80 ms
pub fn annexb_to_avcc / avcc_to_annexb helpers (needed by WebCodecs on the client)
```

## aether-input

```rust
pub trait InputInjector: Send { fn inject(&mut self, ev: &InputEvent) -> Result<()>; fn update_displays(&mut self, d: Vec<DisplayInfo>); }
pub fn create_injector(displays: Vec<DisplayInfo>) -> Result<Box<dyn InputInjector>>;
pub mod keymap { hid_to_macos_vk, hid_to_windows_vk/scancode, hid_to_linux_evdev }   // pure tables, tested on every OS
pub mod permissions { pub fn report() -> PermissionReport; pub fn open_settings(kind: PermissionKind) -> Result<()>;
                      pub fn request(kind) -> bool; }  // Accessibility, ScreenRecording, InputMonitoring, deep links
pub mod hid_report { BLE/USB HID report descriptor + InputEvent → boot keyboard / mouse reports }
pub mod ble_hid   { BLE HID peripheral (BlueZ on Linux); returns Unsupported where the OS forbids the HID GATT service }
```

## aether-dom

```rust
pub struct DomTree; impl DomTree { pub fn apply_snapshot(&mut self, s: DomSnapshot); pub fn apply_diff(&mut self, d: &DomTreeDiff) -> Result<()>;
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit>; pub fn node(&self, id: u64) -> Option<&DomNode>; pub fn revision(&self) -> u64; }
pub struct SearchHit { pub node_id: u64, pub score: u32, pub label: String, pub role: DomRole, pub breadcrumb: String, pub matched_indices: Vec<usize> }
pub fn diff(old: &DomSnapshot, new: &DomSnapshot) -> DomTreeDiff;
pub trait DomProvider: Send { fn snapshot(&mut self, max_depth: u16, interactive_only: bool) -> Result<DomSnapshot>;
    fn perform(&mut self, node: u64, action: DomAction) -> Result<()>; fn set_value(&mut self, node: u64, value: &str) -> Result<()>; }
pub fn create_provider() -> Result<Box<dyn DomProvider>>;          // AXUIElement / UIAutomation / AT-SPI
pub mod context { pub fn active_working_directory() -> Option<PathBuf>; } // Finder/Explorer/IDE cwd for the Quake terminal
```

## aether-vfs

```rust
pub struct ShareRegistry; pub struct VfsHost;                       // path-sandboxed request handler
impl VfsHost { pub fn new(reg: ShareRegistry) -> Self; pub fn handle(&self, r: VfsRequest) -> VfsResponse; }
pub async fn serve<R: AsyncRead+Unpin, W: AsyncWrite+Unpin>(host: Arc<VfsHost>, r: R, w: W) -> Result<()>;
pub struct VfsClient;  // pipelined RPC over (AsyncWrite, AsyncRead), + BLAKE3-verified LRU ChunkCache
pub async fn mount(client: Arc<VfsClient>, share: u32, mount_point: &Path) -> Result<MountHandle>; // NFSv3 loopback
pub mod transfer { pub async fn send_paths(...); pub async fn receive(...); }  // TransferMessage, BLAKE3-verified
```

## aether-timetravel

```rust
pub struct Blackbox; impl Blackbox {
    pub fn new(cfg: BlackboxConfig) -> Self;    // window: 180 s, max_bytes, random ChaCha20-Poly1305 key
    pub fn push_frame(&mut self, pts_us: u64, keyframe: bool, payload: &[u8]);
    pub fn push_mark(&mut self, mark: TimelineMark, payload: Option<&[u8]>);
    pub fn push_telemetry(&mut self, s: TelemetrySample);
    pub fn index(&self) -> TimeTravelIndex;
    pub fn seek(&self, pts_us: u64) -> Result<SeekPlan>;  // decrypted keyframe + following frames ≤ pts
}
```

## aether-usb

URB-level redirection: `UsbHost` (rusb/libusb) enumerates devices → `UsbDeviceInfo`, attaches and executes
`UrbSubmit` → `UrbComplete`. Client side exports via USB/IP to Linux `vhci-hcd`; macOS/Windows virtual host
controllers require signed drivers and return `Unsupported`.

## aether-core  →  desktop/mobile

`aether_core::Engine` owns identity, trust store, settings, endpoint, LAN discovery, host sessions and client
sessions. The Tauri app exposes it via commands (camelCase JSON):

| command | args | returns |
|---|---|---|
| `get_identity` | – | `{deviceId, deviceName, fingerprint, os, password, passwordExpiresAt, qrPayload}` |
| `rotate_password` | – | identity |
| `get_settings` / `set_settings` | `{settings}` | `Settings` |
| `get_permissions` | – | `{screenRecording, accessibility, inputMonitoring}` each `granted|denied|notApplicable` |
| `open_permission_settings` | `{kind}` | – |
| `list_lan_peers` | – | `[LanPeer]` (+ event `lan-peers-changed`) |
| `list_trusted_devices` / `remove_trusted_device` | `{deviceId}` | `[TrustedDevice]` |
| `connect` | `{target, password?, frames: Channel}` | `{sessionId}` |
| `disconnect` | `{sessionId}` | – |
| `send_input` | `{sessionId, event}` | – |
| `select_display`, `set_ghost_mode`, `set_lossless_text`, `set_audio`, `set_quality` | `{sessionId, …}` | – |
| `dom_subscribe` / `dom_search` / `dom_action` / `dom_set_value` | `{sessionId, …}` | `[SearchHit]` |
| `ports_scan` / `port_forward` / `port_unforward` | `{sessionId, remotePort, localPort?}` | `[ListeningPort]` / `{localAddr}` |
| `terminal_open` / `terminal_input` / `terminal_resize` / `terminal_close` | `{sessionId, …, output: Channel}` | `{terminalId, cwd}` |
| `vfs_list_shares` / `vfs_mount` / `vfs_unmount` / `list_mounts` | | `{mountPoint}` |
| `list_local_shares` / `add_local_share` / `remove_local_share` | `{path, readOnly}` | |
| `send_files` | `{sessionId, paths}` | `{transferId}` (+ event `transfer-progress`) |
| `dvr_index` / `dvr_seek` / `dvr_live` / `dvr_ocr` | `{sessionId, ptsUs}` | `TimeTravelIndex` / text |
| `host_status` / `kick_session` / `respond_incoming` | | (+ event `incoming-request`) |

Events: `session-state`, `session-telemetry`, `session-displays`, `dom-updated`, `transfer-progress`,
`incoming-request`, `lan-peers-changed`.

The `frames` channel carries binary messages `[u8 kind][payload]` produced by
`aether_core::ui_video::UiVideoAdapter` (also used to replay DVR history):

| kind | payload |
|------|---------|
| 1 config | JSON `{displayId, mode: "video"\|"tiles", codec (RFC 6381 string), description (base64 avcC/hvcC), width, height, chroma}` |
| 2 video | `[u8 keyframe][u64 LE pts_us]` + AVCC/HVCC length-prefixed access unit → WebCodecs `VideoDecoder` |
| 3 base tile | `[u32 LE x][y][w][h]` + RGBA8 (tiles-only mode) |
| 4 cursor | JSON `{x, y, visible}` (normalized) |
| 5 reset | – (seek / resync: clear picture and overlay) |
| 6 overlay tile | like 3, composited over video (hybrid lossless text refinement) |
| 7 invalidate | `[u32 n]` + n × `[x][y][w][h]`: drop overlay where the region changed again |

Engine errors cross the bridge as stable i18n keys (`errors.*`), IDs as strings.

## Phase 2 additions

### Audio
`Datagram::Audio(AudioPacket)` (appended variant) carries Opus at 48 kHz stereo, 10 ms frames, in-band FEC
(enabled when measured QUIC loss > 0); packets larger than `max_datagram_size` fall back to a
`StreamKind::Audio` stream. Host: `aether_capture::create_audio_capturer()` (SCK / WASAPI loopback /
PipeWire–Pulse monitor / mobile `push_pcm`) → `aether_codec::AudioEncoder` (+ `Resampler`). Client:
`JitterBuffer` (adaptive 20–60 ms, reorder, PLC/FEC) → `AudioDecoder` → `cpal` output (device selection,
volume/mute local). `aether_core::audio::loopback_selftest()` exercises the whole chain without devices.

### Platform hosts (mobile)
`aether_core::external::PlatformHost` lets Android (MediaProjection → MediaCodec, JNI) and iOS (ReplayKit
extension → VideoToolbox → App Group socket → Swift bridge) feed *already encoded* Annex-B frames into host
sessions instead of the desktop capture → encode pipeline. Input on Android goes through
`aether_input::android` → Kotlin (AccessibilityService `dispatchGesture` / Shizuku `injectInputEvent`).
iOS hosts are view-only by OS design.

### aether-display
* `ghost::enable/disable` — a black curtain on every monitor, kept out of AetherLink's own capture, plus
  the backlight dimmed to 0 after persisting the previous levels (`brightness-restore.json`, restored at
  engine start after a crash). Driven by `Engine::set_ghost_mode` on `ControlMessage::SetGhostMode`; the
  desktop shell registers the Ctrl+Opt+Cmd+Esc panic hotkey while active and falls back to a webview
  curtain only where native is unavailable (`ghost-mode` event `native: false`).
  `GhostStatus.capture_exclusion` says how the curtain is hidden from capture.
  * macOS: borderless `NSWindow` per screen at `CGShieldingWindowLevel()`, `sharingType = none`, all
    Spaces (`"sharingTypeNone"`).
  * Windows: dedicated UI thread + message loop; one `WS_POPUP` window per `EnumDisplayMonitors` monitor
    with `WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` (+ layered/`WS_EX_TRANSPARENT`, i.e.
    click-through so `SendInput` from the remote reaches the apps), re-raised above the taskbar every
    250 ms, rebuilt on `WM_DISPLAYCHANGE`. `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` hides it
    from DXGI duplication / WGC (`"excludeFromCapture"`); pre-2004 builds get `WDA_MONITOR`
    (`"monitor"`: captures show black there). `window_level`/`shielding_level` = z-order band (1 = topmost).
  * Linux X11: override-redirect black window per RandR monitor with an empty SHAPE input region
    (click-through for XTest), re-stacked on top by a keeper thread, ids published in the
    `_AETHERLINK_GHOST_CURTAINS` root property. A root-window grab would include it, so the AetherLink X11
    capturer switches to Composite (`RedirectSubwindows(Automatic)` + `NameWindowPixmap`) and composes
    frames from the windows below the curtain (`"x11Composite"`; shapes/translucency ignored). Third-party
    X11 capture tools still see the curtain. Wayland: `Unsupported` (needs a compositor protocol such as
    wlr-layer-shell, and portal captures include every surface) → webview fallback.
* `brightness` — same snapshot / restore-file contract everywhere (`read_all`, `set_all`, `restore`,
  `dim`, `restore_pending`): DisplayServices (macOS); WMI `WmiMonitorBrightness(Methods)` for panels +
  DDC/CI via dxva2 for external monitors (Windows); `/sys/class/backlight` read + systemd-logind
  `Session.SetBrightness` over the system bus via zbus, no root (Linux).

### Local OCR (`aether_core::ocr`, behind `dvr_ocr`)
macOS Vision; Windows `Windows.Media.Ocr` (requested → user-profile → en-US engine, `BitmapDecoder` from an
`InMemoryRandomAccessStream`, downscaled to `MaxImageDimension`); Linux pure-Rust `ocrs` with its two ONNX
models fetched once into `~/.cache/aetherlink/ocrs` and SHA-256-pinned (`AETHERLINK_OCRS_MODEL_DIR`,
`AETHERLINK_OCR_NO_DOWNLOAD=1`), falling back to the `tesseract` CLI (TSV) when offline or for non-Latin
scripts. Output is the same `OcrLine {text, confidence, bbox 0..1}` everywhere (no confidence from
Windows/ocrs → 1.0).
* `virtual_display::VirtualDisplay` — `CGVirtualDisplay` via the ObjC runtime. HiDPI modes are registered in
  pixels *and* points and need Retina-class density (≥ ~200 ppi) or the window server keeps them offline;
  1× fallback is automatic. Created on `ControlMessage::SetVirtualDisplay` (then `DisplaysChanged` + stream
  switch) and automatically when a host session starts with zero online displays.

### HID bridge ("Control iPhone / iPad")
`aether_core::hid_bridge` owns an `aether_input::bt_hid::HidPeripheral` on a worker thread
(IOBluetooth objects are not `Send`). Backends: `macIoBluetooth`, `linuxBlueZ`, `serial:<port>` (ESP32-S3
dongle, frame `A5 5A | ver | type | seq | len | payload | CRC-16/CCITT`, 921600 baud). Desktop commands:
`hid_backends`, `hid_start {backend, width?, height?}`, `hid_paired_devices`, `hid_connect {address}`,
`hid_send_input {event}`, `hid_status`, `hid_stop`.

### New desktop commands
`set_audio_volume`, `list_audio_outputs`, `set_audio_output`, `set_virtual_display {sessionId, enabled,
width?, height?}`, `local_ghost_off`, plus the HID commands above. Mobile adds `mobile_*` commands
(platform info, screen-share consent, QR scan, Bluetooth controller) in `apps/mobile/src-tauri/src/mobile.rs`
and reuses the desktop `backend.rs`/`commands.rs` via `#[path]`.

### Self-test
`aetherlink-desktop --selftest-live [--with-virtual-display] [--no-ghost]` prints a JSON report:
permissions (never requested), a 1 s capture probe (only if already permitted), a full localhost
host+client session (real screen when permitted, test pattern otherwise), Ghost Mode for 1 s with exact
backlight-restore verification, the audio loopback chain, and optionally a temporary virtual display.
Exit code 0 when every executed check passed.

## Security hardening (authorization, resources, durability)

Authentication (TLS 1.3 + SPAKE2 / pinned keys) says *who* is connected; these rules decide *what*
that session may do.

### Session permissions (least privilege)
`aether_protocol::SessionPermissions` lists every capability beyond viewing the screen: `input`,
`clipboard`, `file_transfer`, `drives`, `semantic_ui`, `audio`, `virtual_display`, `ghost_mode`,
`terminal`, `port_forwarding`, `usb`. The host keeps the policy in `Settings::permissions`
(defaults: remote assistance on; terminal, port forwarding, USB and Ghost Mode **off**) and enforces
it in `aether_core::host`:

* After `SessionReady` the host sends `ControlMessage::Permissions`; the client re-emits it as the
  `session-permissions` event and the HUD disables what is not granted.
* Every service stream (`Input`, `FileTransfer`, `Vfs`, `Clipboard`, `Dom`, `PortControl`,
  `PortTunnel`, `Terminal`, `Usb`) and every control request (`SetGhostMode`, `SetVirtualDisplay`,
  `SetClipboardMode`, `SetAudio`) and input datagram is checked; refusals answer
  `Error { code: "errors.permission.<capability>" }`. `semantic_ui` also requires `input`.
* Changes apply live: `Engine::set_settings` publishes them on a `watch` channel; running services
  that lose their permission are dropped (closing their streams), audio/clipboard/Ghost Mode are
  switched off, and the controller receives the new `Permissions`.
* Port forwarding uses an explicit allowlist (`tunnel_allowed_ports`; empty = no port). Port scans
  only advertise allowlisted ports.

### Owner verification for trusted devices
`HostAuthPolicy::verify_trusted` runs after a pinned key is recognised. With
`Settings::require_biometric` the host asks the person at the computer (`aether_core::local_auth`:
LocalAuthentication `DeviceOwnerAuthentication` on macOS, Windows Hello `UserConsentVerifier` on
Windows). It fails closed: errors, a 90 s timeout, a prompt already open for another peer, or a
platform without support all reject with `AuthRejectReason::OwnerVerificationFailed`, and the
password path is not offered as a fallback. The option cannot be enabled where verification is
unavailable (`errors.ownerVerificationUnavailable`).

### Resource limits
Per session, `StreamLimits` caps concurrently running services (96 in total; e.g. 4 terminals,
64 tunnels, 8 VFS streams, 4 USB, 4 incoming transfers); excess streams are refused with
`errors.tooManyStreams`. The terminal output path is a bounded channel (32 chunks of ≤ 16 KiB): when
the network is slower than the shell, the PTY reader blocks, the PTY buffer fills and the shell is
paused instead of output accumulating in memory.

### Ghost Mode state
`Engine::set_ghost_mode` returns a `GhostOutcome { requested, native_active, fallback_active,
effective }`. The app shell installs its webview curtain with `Engine::set_ghost_fallback`, which
reports whether every monitor was covered. Ghost Mode is only recorded as on (and shown as on) when a
curtain is really up; otherwise the controller gets `errors.ghostUnavailable` and its HUD button
resets.

### Durable state
Settings, shares and the trust store are written with `aether_net::write_private_file`: owner-only
(0600) temp file, `fsync`, rename, directory `fsync`. The Ghost Mode backlight restore file is written
the same way, so the levels to restore survive a crash or power loss while the screen is dark.

### Shared folders and file transfer
- **Path resolution without races (Unix).** `aether-vfs` resolves every request from a directory
  descriptor of the share root, one component at a time with `openat(O_NOFOLLOW | O_DIRECTORY)`, and
  performs the final operation relative to the resolved parent (`openat`, `mkdirat`, `unlinkat`,
  `renameat`, …). On Linux it uses `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` when the kernel
  offers it. Swapping a directory for a symlink after the check cannot redirect an operation outside
  the share; tests cover this race explicitly.
- **Windows.** Every directory between the share root and the target, and every directory *above*
  the root, is *pinned* for the duration of the operation. Pinning opens it without following
  reparse points and with a share mode lacking `FILE_SHARE_DELETE`; it requests
  `FILE_LIST_DIRECTORY`, or `FILE_TRAVERSE` for ancestors, because an open with attribute rights
  only is ignored by share-access checks. While pinned, no process can rename or delete these
  directories or replace them with a junction.
  - Junctions and symlinks met on the way are followed only while they stay inside the share.
    Absolute junction targets are compared with the root component by component, ASCII
    case-insensitively, so `share-evil` is not "inside" `share`.
  - Namespace operations never follow a final link: removing a junction removes the link, not the
    target's contents. Opened files are re-checked by handle (`GetFinalPathNameByHandleW`); if that
    fails, the operation is refused.
  - File names that Windows would alias or treat specially are refused: trailing dots or spaces,
    control characters, `:` (alternate data streams), and reserved device names including
    `COM¹`–`COM³`, `LPT¹`–`LPT³`, `CONIN$` and `CONOUT$`.
  - `.github/workflows/windows-vfs.yml` runs these tests on real Windows on NTFS, ReFS, FAT32 and
    exFAT volumes. On NTFS and ReFS a link that cannot be created fails the run
    (`AETHERLINK_REQUIRE_LINK_TESTS=1`) instead of skipping. Under Wine the junction, symlink and
    long-path tests are skipped with an explicit `SKIPPED` line, because Wine supports none of them
    faithfully.
- **Arithmetic.** Offer totals use `try_fold` + `checked_add`, and chunk bounds use `checked_add`, so
  hostile sizes and offsets are rejected instead of wrapping.
- **Staging.** An incoming transfer is written into a private (`0700`) staging directory with a random
  128-bit name inside the destination. Files are created with `create_new`, never truncating an
  existing file. Only a complete, verified transfer is published with a no-replace rename. On error
  or cancel the staging directory is removed and the destination is left untouched.
