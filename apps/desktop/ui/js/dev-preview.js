// ============================================================================
//  DEV PREVIEW ADAPTER — browser-only design review. NOT USED INSIDE TAURI.
//  Everything returned here is SAMPLE DATA, clearly labelled on screen with a
//  persistent ribbon. api.js only imports this module when no Tauri marker is
//  present, and this module refuses to initialise if one is.
// ============================================================================
import { h } from './lib/dom.js';
import { icon } from './lib/icons.js';

function assertNotTauri() {
  const w = window;
  if (
    typeof w.__TAURI_INTERNALS__ !== 'undefined' ||
    typeof w.__TAURI__ !== 'undefined' ||
    location.protocol === 'tauri:' ||
    location.hostname === 'tauri.localhost'
  ) {
    throw new Error('dev-preview adapter must never run inside Tauri');
  }
}

const SAMPLE_PREFIX = 'sample';
const now = () => Date.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function randomPassword() {
  const alphabet = 'abcdefghjkmnpqrstuvwxyz23456789';
  let s = '';
  for (let i = 0; i < 8; i++) s += alphabet[Math.floor(Math.random() * alphabet.length)];
  return s;
}

export function createPreviewBackend() {
  assertNotTauri();
  const listeners = new Map();
  const emit = (event, payload) => {
    for (const cb of listeners.get(event) || []) cb(payload);
  };

  // ---- sample state
  let identity = {
    deviceId: 481203967,
    deviceName: 'Studio MacBook Pro',
    fingerprint: '4F2A-9C1B-77E0-D3A8',
    os: 'MacOs',
    password: randomPassword(),
    passwordExpiresAt: now() + 90_000,
    qrPayload: 'aetherlink://pair?id=481203967&fp=4F2A9C1B77E0D3A8&v=1',
  };
  let settings = {
    deviceName: 'Studio MacBook Pro',
    clipboardMode: 'Bidirectional',
    relayServer: 'relay.aetherlink.dev:21116',
    hudHotkey: 'CmdOrCtrl+Shift+Period',
    allowUnattended: true,
    requireConsent: true,
  };
  const permissions = { screenRecording: 'granted', accessibility: 'denied', inputMonitoring: 'granted' };
  let lanPeers = [
    { deviceId: 735902114, deviceName: 'Render Node 02', os: 'Linux', address: '192.168.1.42', port: 21118 },
    { deviceId: 268441930, deviceName: 'Design iPad', os: 'Ios', address: '192.168.1.57', port: 21118 },
    { deviceId: 902117345, deviceName: 'Reception PC', os: 'Windows', address: '192.168.1.80', port: 21118 },
  ];
  let trusted = [
    { deviceId: 735902114, name: 'Render Node 02', os: 'Linux', online: true, lastSeen: now() - 60_000, thumbnail: sampleThumb(0) },
    { deviceId: 551870023, name: 'Home Office iMac', os: 'MacOs', online: true, lastSeen: now() - 5_000, thumbnail: sampleThumb(1) },
    { deviceId: 902117345, name: 'Reception PC', os: 'Windows', online: true, lastSeen: now() - 300_000, thumbnail: null },
    { deviceId: 310998457, name: 'Lab Pixel 8', os: 'Android', online: false, lastSeen: now() - 86_400_000 * 2, thumbnail: null },
    { deviceId: 649120588, name: 'Build Server', os: 'Linux', online: false, lastSeen: now() - 3_600_000 * 5, thumbnail: null },
  ];
  let localShares = [
    { id: 1, name: 'Projects', path: '/Users/demo/Projects', readOnly: false },
    { id: 2, name: 'Screenshots', path: '/Users/demo/Desktop/Screenshots', readOnly: true },
  ];
  let mounts = [{ mountPoint: '/Volumes/AetherLink/Render Node 02 — assets', shareName: 'assets', peerName: 'Render Node 02', readOnly: false }];
  const forwarded = new Map();

  // periodic LAN churn so the radar animates
  setInterval(() => {
    if (lanPeers.length > 3) lanPeers = lanPeers.slice(0, 3);
    else lanPeers = [...lanPeers, { deviceId: 177300482, deviceName: 'Conference Room TV', os: 'Android', address: '192.168.1.90', port: 21118 }];
    emit('lan-peers-changed', lanPeers);
  }, 9000);

  if (new URLSearchParams(location.search).get('demo') === 'consent') {
    setTimeout(
      () =>
        emit('incoming-request', {
          requestId: 'req-1',
          deviceName: 'Giulia’s MacBook Air',
          deviceId: 573114806,
          fingerprint: 'A93C-11F0-5E2B-C8D4',
          os: 'MacOs',
          timeoutSecs: 30,
        }),
      900,
    );
  }

  // ---- synthetic session (tile patches over the frames channel)
  let session = null;

  const handlers = {
    get_identity: () => identity,
    rotate_password: () => (identity = { ...identity, password: randomPassword(), passwordExpiresAt: now() + 90_000 }),
    get_settings: () => settings,
    set_settings: ({ settings: s }) => (settings = { ...settings, ...s }),
    get_permissions: () => permissions,
    open_permission_settings: () => null,
    list_lan_peers: () => lanPeers,
    list_trusted_devices: () => trusted,
    remove_trusted_device: ({ deviceId }) => (trusted = trusted.filter((d) => String(d.deviceId) !== String(deviceId))),
    connect: async ({ target, frames }) => {
      const sessionId = `${SAMPLE_PREFIX}-session`;
      emit('session-state', { sessionId, state: 'connecting', peerName: 'Render Node 02' });
      await sleep(700);
      emit('session-state', { sessionId, state: 'authenticating', peerName: 'Render Node 02' });
      await sleep(600);
      session = startSyntheticSession(sessionId, frames, emit);
      emit('session-state', { sessionId, state: 'connected', peerName: 'Render Node 02', target });
      return { sessionId };
    },
    disconnect: ({ sessionId }) => {
      if (session) session.stop();
      session = null;
      emit('session-state', { sessionId, state: 'closed', reason: 'local' });
    },
    send_input: ({ event }) => {
      if (session && event && event.MouseMove) session.cursor(event.MouseMove.x, event.MouseMove.y);
    },
    select_display: ({ displayId }) => session && session.selectDisplay(displayId),
    set_ghost_mode: () => null,
    set_lossless_text: () => null,
    set_audio: () => null,
    set_audio_volume: () => null,
    list_audio_outputs: () => [
      { id: 'coreaudio:BuiltInSpeakerDevice', name: 'MacBook Pro Speakers', isDefault: true },
      { id: 'coreaudio:AirPodsPro', name: 'AirPods Pro', isDefault: false },
    ],
    set_audio_output: () => null,
    set_quality: () => null,
    set_virtual_display: () => null,
    local_ghost_off: () => null,
    hid_backends: () => [
      { id: 'macIoBluetooth', kind: 'bluetooth', available: true, detail: 'Bluetooth 5.3 controller' },
      { id: 'serial:/dev/cu.usbmodem1101', kind: 'dongle', available: true, detail: 'AetherLink HID dongle (ESP32-S3)' },
    ],
    hid_start: ({ backend }) => ({ backend, state: 'listening', peer: null, reportsSent: 0, reportsDropped: 0, pairingCode: null, lastError: null }),
    hid_stop: () => null,
    hid_status: () => ({ backend: 'macIoBluetooth', state: 'connected', peer: 'a4:83:e7:12:34:56', reportsSent: 1280, reportsDropped: 0, pairingCode: null, lastError: null }),
    hid_paired_devices: () => [{ address: 'a4:83:e7:12:34:56', name: 'iPhone di Giulia', connected: false }],
    hid_connect: () => null,
    hid_send_input: () => null,
    dom_subscribe: () => null,
    dom_search: ({ query, limit }) => fuzzySearch(query, limit),
    dom_action: () => null,
    dom_set_value: () => null,
    ports_scan: async () => {
      await sleep(500);
      return [
        { port: 22, protocol: 'Tcp', address: '0.0.0.0', pid: 401, process_name: 'sshd', service_hint: 'SSH' },
        { port: 3000, protocol: 'Tcp', address: '127.0.0.1', pid: 8812, process_name: 'node', service_hint: 'Node / Next.js' },
        { port: 5173, protocol: 'Tcp', address: '127.0.0.1', pid: 9120, process_name: 'node', service_hint: 'Vite' },
        { port: 5432, protocol: 'Tcp', address: '127.0.0.1', pid: 610, process_name: 'postgres', service_hint: 'PostgreSQL' },
        { port: 11434, protocol: 'Tcp', address: '127.0.0.1', pid: 7001, process_name: 'ollama', service_hint: 'Ollama' },
        { port: 41923, protocol: 'Tcp', address: '127.0.0.1', pid: 7333, process_name: 'code-helper', service_hint: null },
      ];
    },
    port_forward: ({ remotePort, localPort }) => {
      const p = localPort || (remotePort < 1024 ? remotePort + 10000 : remotePort);
      forwarded.set(remotePort, p);
      return { localAddr: `127.0.0.1:${p}` };
    },
    port_unforward: ({ remotePort }) => forwarded.delete(remotePort),
    terminal_open: ({ output }) => {
      const enc = new TextEncoder();
      const lines = [
        '\x1b[38;5;110mAetherLink remote shell\x1b[0m — sample output (dev preview)\r\n',
        '\x1b[2mLast login: Sat Nov 14 21:04:11 on ttys002\x1b[0m\r\n',
        '\x1b[38;5;116mrender@node02\x1b[0m \x1b[38;5;147m~/projects/aetherlink\x1b[0m $ ',
      ];
      setTimeout(() => lines.forEach((l) => output.onmessage(enc.encode(l).buffer)), 120);
      termEcho = (data) => {
        const text = data === '\r' ? '\r\n\x1b[38;5;116mrender@node02\x1b[0m \x1b[38;5;147m~/projects/aetherlink\x1b[0m $ ' : data;
        output.onmessage(enc.encode(text).buffer);
      };
      return { terminalId: `${SAMPLE_PREFIX}-tty`, cwd: '~/projects/aetherlink' };
    },
    terminal_input: ({ data }) => termEcho && termEcho(data),
    terminal_resize: () => null,
    terminal_close: () => (termEcho = null),
    vfs_list_shares: () => [
      { id: 1, name: 'assets', readOnly: false },
      { id: 2, name: 'renders', readOnly: true },
    ],
    vfs_mount: ({ shareId }) => {
      const mp = `/Volumes/AetherLink/Render Node 02 — share ${shareId}`;
      mounts = [...mounts, { mountPoint: mp, shareName: `share ${shareId}`, peerName: 'Render Node 02', readOnly: false }];
      return { mountPoint: mp };
    },
    vfs_unmount: ({ mountPoint }) => (mounts = mounts.filter((m) => m.mountPoint !== mountPoint)),
    list_mounts: () => mounts,
    list_local_shares: () => localShares,
    add_local_share: ({ path, readOnly }) => {
      const name = path.split('/').filter(Boolean).pop() || path;
      localShares = [...localShares, { id: Date.now() % 100000, name, path, readOnly }];
      return localShares;
    },
    remove_local_share: ({ shareId }) => (localShares = localShares.filter((s) => s.id !== shareId)),
    send_files: ({ sessionId, paths }) => {
      const transferId = `${SAMPLE_PREFIX}-xfer-${Date.now()}`;
      const total = 48_000_000 * paths.length;
      let done = 0;
      const tick = setInterval(() => {
        done = Math.min(total, done + total / 24);
        emit('transfer-progress', {
          transferId,
          sessionId,
          name: paths[0].split('/').pop(),
          bytesDone: done,
          bytesTotal: total,
          filesDone: Math.floor((done / total) * paths.length),
          filesTotal: paths.length,
          state: done >= total ? 'completed' : 'running',
        });
        if (done >= total) clearInterval(tick);
      }, 140);
      return { transferId };
    },
    dvr_index: () => {
      const end = 180_000_000;
      const keyframes = [];
      for (let t = 0; t <= end; t += 2_000_000) keyframes.push(t);
      return {
        start_us: 0,
        end_us: end,
        keyframes,
        marks: [
          { pts_us: 22_000_000, kind: 'Clipboard', label: 'Copied 2 lines' },
          { pts_us: 61_000_000, kind: 'Terminal', label: 'cargo build --release' },
          { pts_us: 94_000_000, kind: 'FileTransfer', label: 'scene.blend' },
          { pts_us: 131_000_000, kind: 'ResourceSpike', label: 'CPU 97%' },
          { pts_us: 162_000_000, kind: 'Alert', label: 'Disk almost full' },
        ],
      };
    },
    dvr_seek: () => null,
    dvr_live: () => null,
    dvr_ocr: () => 'Build finished in 2m 14s\nwarning: unused variable `frame`\n  --> src/render.rs:118:9\nArtifacts written to target/release/render-node',
    host_status: () => ({
      listening: true,
      sessions: [{ sessionId: `${SAMPLE_PREFIX}-host-1`, peerName: 'Giulia’s iPad', deviceId: 268441930, since: now() - 12 * 60_000, ghostMode: false }],
    }),
    kick_session: () => null,
    respond_incoming: () => null,
    ui_set_locale: () => null,
    ui_tray_state: () => null,
    ui_open_session_window: ({ target, title }) => {
      const url = `${location.pathname}?target=${encodeURIComponent(target)}&title=${encodeURIComponent(title)}#/session`;
      window.open(url, '_blank');
      return 'session-preview';
    },
    ui_take_pending_connect: () => {
      const q = new URLSearchParams(location.search);
      return { target: q.get('target') || '735902114', password: null, title: q.get('title') || 'Render Node 02' };
    },
    ui_copy_text: () => null,
    ui_set_hud_hotkey: () => null,
    ui_show_main: () => null,
  };
  let termEcho = null;

  // ---- ribbon
  const showRibbon = () =>
    document.body.appendChild(
      h('div', { class: 'dev-ribbon', attrs: { role: 'status' } }, h('span', { class: 'dot' }), icon('info', 14), h('span', { i18n: 'preview.ribbon' })),
    );
  if (document.body) showRibbon();
  else addEventListener('DOMContentLoaded', showRibbon);

  return {
    kind: 'preview',
    async invoke(cmd, args = {}) {
      const fn = handlers[cmd];
      if (!fn) throw 'engine-not-wired';
      await sleep(cmd === 'send_input' ? 0 : 60 + Math.random() * 120);
      return structuredCloneSafe(await fn(args));
    },
    channel(onmessage) {
      return { onmessage };
    },
    async listen(event, cb) {
      if (!listeners.has(event)) listeners.set(event, new Set());
      listeners.get(event).add(cb);
      return () => listeners.get(event).delete(cb);
    },
    async dragDrop(cb) {
      const over = (e) => {
        e.preventDefault();
        cb({ type: 'over', paths: [], position: { x: e.clientX * devicePixelRatio, y: e.clientY * devicePixelRatio } });
      };
      const enter = (e) => {
        e.preventDefault();
        cb({ type: 'enter', paths: [], position: { x: e.clientX * devicePixelRatio, y: e.clientY * devicePixelRatio } });
      };
      const leave = (e) => {
        if (e.relatedTarget === null) cb({ type: 'leave' });
      };
      const drop = (e) => {
        e.preventDefault();
        const paths = Array.from(e.dataTransfer.files || []).map((f) => `/preview/${f.name}`);
        cb({ type: 'drop', paths, position: { x: e.clientX * devicePixelRatio, y: e.clientY * devicePixelRatio } });
      };
      addEventListener('dragover', over);
      addEventListener('dragenter', enter);
      addEventListener('dragleave', leave);
      addEventListener('drop', drop);
      return () => {
        removeEventListener('dragover', over);
        removeEventListener('dragenter', enter);
        removeEventListener('dragleave', leave);
        removeEventListener('drop', drop);
      };
    },
    windowLabel: () => (location.hash.startsWith('#/session') ? 'session-preview' : 'main'),
    closeWindow: () => {
      window.close();
      location.href = `${location.pathname}#/home`;
    },
  };
}

function structuredCloneSafe(v) {
  if (v === undefined || v === null) return null;
  if (typeof v !== 'object') return v;
  return JSON.parse(JSON.stringify(v));
}

// ---------------------------------------------------------------- sample DOM search
const SAMPLE_NODES = [
  { node_id: 11, label: 'Render', role: 'Button', breadcrumb: 'Blender › Properties › Output' },
  { node_id: 12, label: 'Render Animation', role: 'MenuItem', breadcrumb: 'Blender › Menu bar › Render' },
  { node_id: 13, label: 'Output path', role: 'TextField', breadcrumb: 'Blender › Properties › Output' },
  { node_id: 14, label: 'Resolution X', role: 'TextField', breadcrumb: 'Blender › Properties › Format' },
  { node_id: 15, label: 'Frame Rate', role: 'PopUpButton', breadcrumb: 'Blender › Properties › Format' },
  { node_id: 16, label: 'Save As…', role: 'MenuItem', breadcrumb: 'Blender › Menu bar › File' },
  { node_id: 17, label: 'Denoise', role: 'CheckBox', breadcrumb: 'Blender › Properties › Sampling' },
  { node_id: 18, label: 'Search', role: 'SearchField', breadcrumb: 'Finder › Toolbar' },
  { node_id: 19, label: 'Share', role: 'Button', breadcrumb: 'Finder › Toolbar' },
  { node_id: 20, label: 'Documentation', role: 'Link', breadcrumb: 'Safari › docs.blender.org' },
  { node_id: 21, label: 'Viewport Shading', role: 'TabGroup', breadcrumb: 'Blender › 3D Viewport › Header' },
  { node_id: 22, label: 'Samples', role: 'Slider', breadcrumb: 'Blender › Properties › Sampling' },
];

function fuzzySearch(query, limit = 40) {
  const q = String(query || '').toLowerCase().trim();
  if (!q) return SAMPLE_NODES.slice(0, limit).map((n) => ({ ...n, score: 0, matched_indices: [] }));
  const out = [];
  for (const n of SAMPLE_NODES) {
    const label = n.label.toLowerCase();
    const idx = [];
    let j = 0;
    for (let i = 0; i < label.length && j < q.length; i++) if (label[i] === q[j]) idx.push(i), j++;
    if (j === q.length) out.push({ ...n, score: 1000 - (idx[idx.length - 1] - idx[0]) * 10 - idx[0], matched_indices: idx });
  }
  return out.sort((a, b) => b.score - a.score).slice(0, limit);
}

// ---------------------------------------------------------------- sample thumbnails
function sampleThumb(variant) {
  const c = document.createElement('canvas');
  c.width = 320;
  c.height = 200;
  paintDesktop(c.getContext('2d'), 320, 200, variant, 0);
  return c.toDataURL('image/png');
}

function paintDesktop(ctx, W, H, variant, tSec) {
  const g = ctx.createLinearGradient(0, 0, W, H);
  if (variant === 1) {
    g.addColorStop(0, '#1d2b4a');
    g.addColorStop(1, '#3b2a5c');
  } else {
    g.addColorStop(0, '#13202c');
    g.addColorStop(0.6, '#1b3440');
    g.addColorStop(1, '#2c2a4e');
  }
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, W, H);
  const s = W / 1600;
  ctx.fillStyle = 'rgba(10,12,18,0.55)';
  ctx.fillRect(0, 0, W, 26 * s);
  const win = (x, y, w, hh, title) => {
    ctx.fillStyle = 'rgba(28,32,42,0.96)';
    roundRect(ctx, x * s, y * s, w * s, hh * s, 14 * s);
    ctx.fill();
    ctx.fillStyle = 'rgba(255,255,255,0.06)';
    ctx.fillRect(x * s, y * s + 34 * s, w * s, 1);
    ['#ff6159', '#ffbd2e', '#28c941'].forEach((col, i) => {
      ctx.fillStyle = col;
      ctx.beginPath();
      ctx.arc((x + 20 + i * 20) * s, (y + 17) * s, 6 * s, 0, Math.PI * 2);
      ctx.fill();
    });
    ctx.fillStyle = 'rgba(255,255,255,0.55)';
    ctx.font = `${13 * s}px -apple-system, system-ui, sans-serif`;
    ctx.fillText(title, (x + 90) * s, (y + 22) * s);
  };
  win(120, 110, 860, 560, 'render.rs — aetherlink');
  ctx.font = `${15 * s}px "JetBrains Mono", ui-monospace, monospace`;
  const code = ['fn encode(frame: &CapturedFrame) -> Result<()> {', '    let tiles = scheduler.diff(frame)?;', '    for t in tiles.iter() {', '        sink.push(t.compress()?);', '    }', '    Ok(())', '}'];
  code.forEach((line, i) => {
    ctx.fillStyle = i % 3 === 0 ? 'rgba(165,151,255,0.9)' : 'rgba(220,230,245,0.78)';
    ctx.fillText(line, 160 * s, (190 + i * 30) * s);
  });
  win(1040, 180, 460, 340, 'Activity');
  for (let i = 0; i < 18; i++) {
    const hgt = 40 + 80 * Math.abs(Math.sin(i * 0.7 + tSec * 1.6));
    ctx.fillStyle = 'rgba(125,224,214,0.75)';
    ctx.fillRect((1070 + i * 23) * s, (480 - hgt) * s, 14 * s, hgt * s);
  }
  ctx.fillStyle = 'rgba(16,18,26,0.6)';
  roundRect(ctx, 560 * s, (H / s - 80) * s, 480 * s, 60 * s, 18 * s);
  ctx.fill();
  for (let i = 0; i < 7; i++) {
    ctx.fillStyle = ['#8ab8ff', '#7de0d6', '#a597ff', '#f2c46d', '#64d6a0', '#ff8089', '#c9d9f5'][i];
    roundRect(ctx, (585 + i * 64) * s, (H / s - 70) * s, 40 * s, 40 * s, 10 * s);
    ctx.fill();
  }
}

function roundRect(ctx, x, y, w, hh, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + hh, r);
  ctx.arcTo(x + w, y + hh, x, y + hh, r);
  ctx.arcTo(x, y + hh, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

// ---------------------------------------------------------------- synthetic frames channel
function startSyntheticSession(sessionId, frames, emit) {
  const displays = [
    { id: 1, name: 'Built-in Retina', bounds: { x: 0, y: 0, width: 1600, height: 1000 }, scale: 2, refresh_hz: 60, primary: true, is_virtual: false },
    { id: 2, name: 'Ultrawide 34"', bounds: { x: 1600, y: -40, width: 2560, height: 1080 }, scale: 1, refresh_hz: 100, primary: false, is_virtual: false },
  ];
  let active = displays[0];
  const off = document.createElement('canvas');
  const ctx = off.getContext('2d', { willReadFrequently: true });
  let stopped = false;
  let t0 = performance.now();

  const send = (u8) => frames.onmessage && frames.onmessage(u8.buffer);
  const sendJson = (kind, obj) => {
    const body = new TextEncoder().encode(JSON.stringify(obj));
    const msg = new Uint8Array(1 + body.length);
    msg[0] = kind;
    msg.set(body, 1);
    send(msg);
  };
  const sendTile = (x, y, w, hh) => {
    const img = ctx.getImageData(x, y, w, hh).data;
    const msg = new Uint8Array(1 + 16 + img.length);
    const dv = new DataView(msg.buffer);
    msg[0] = 3;
    dv.setUint32(1, x, true);
    dv.setUint32(5, y, true);
    dv.setUint32(9, w, true);
    dv.setUint32(13, hh, true);
    msg.set(img, 17);
    send(msg);
  };
  const configure = () => {
    off.width = active.bounds.width;
    off.height = active.bounds.height;
    sendJson(1, { display_id: active.id, codec: 'Tiles', chroma: 'Yuv444', width: off.width, height: off.height, fps: 30 });
    paintDesktop(ctx, off.width, off.height, active.id === 2 ? 1 : 0, 0);
    for (let y = 0; y < off.height; y += 256) for (let x = 0; x < off.width; x += 256) sendTile(x, y, Math.min(256, off.width - x), Math.min(256, off.height - y));
  };
  emit('session-displays', { sessionId, displays, activeDisplayId: active.id });
  configure();

  const loop = () => {
    if (stopped) return;
    const tSec = (performance.now() - t0) / 1000;
    paintDesktop(ctx, off.width, off.height, active.id === 2 ? 1 : 0, tSec);
    const s = off.width / 1600;
    sendTile(Math.floor(1040 * s), Math.floor(180 * s), Math.ceil(460 * s), Math.ceil(340 * s));
    setTimeout(loop, 1000 / 24);
  };
  loop();

  const telemetry = setInterval(() => {
    emit('session-telemetry', {
      sessionId,
      fps: 58 + Math.random() * 2,
      rtt_ms: 7 + Math.random() * 4,
      bitrate_kbps: 18_000 + Math.random() * 6_000,
      packet_loss: 0,
      decode_ms: 1.8 + Math.random(),
    });
  }, 1000);

  return {
    stop() {
      stopped = true;
      clearInterval(telemetry);
    },
    selectDisplay(id) {
      active = displays.find((d) => d.id === id) || active;
      emit('session-displays', { sessionId, displays, activeDisplayId: active.id });
      configure();
    },
    cursor(x, y) {
      sendJson(4, { display_id: active.id, x, y, visible: true, shape_id: 0 });
    },
  };
}
