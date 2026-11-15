// The ONLY module that talks to Tauri (invoke / Channel / event.listen /
// drag-drop). Everything else imports `api` from here.
//
// Inside Tauri the real IPC adapter is used. When the page is opened in a
// plain browser over http(s) for design review, a clearly labelled
// DEV PREVIEW adapter (./dev-preview.js) serves sample data. The preview can
// never activate inside Tauri: any Tauri marker (globals, tauri: protocol,
// tauri.localhost host) forces the real adapter, and dev-preview.js refuses
// to load when those markers are present.
import { t } from './i18n.js';

const w = window;
export const IN_TAURI =
  typeof w.__TAURI_INTERNALS__ !== 'undefined' ||
  typeof w.__TAURI__ !== 'undefined' ||
  location.protocol === 'tauri:' ||
  location.hostname === 'tauri.localhost' ||
  /\bTauri\b/i.test(navigator.userAgent);

export const IS_PREVIEW = !IN_TAURI && (location.protocol === 'http:' || location.protocol === 'https:');

let backend = null;

function tauriBackend() {
  const T = w.__TAURI__;
  if (!T || !T.core) {
    throw new Error('Tauri globals missing: set app.withGlobalTauri = true in tauri.conf.json');
  }
  return {
    kind: 'tauri',
    invoke: (cmd, args) => T.core.invoke(cmd, args),
    channel: (onmessage) => {
      const ch = new T.core.Channel();
      ch.onmessage = onmessage;
      return ch;
    },
    listen: (event, cb) => T.event.listen(event, (e) => cb(e.payload)),
    dragDrop: (cb) => T.webview.getCurrentWebview().onDragDropEvent((e) => cb(e.payload)),
    windowLabel: () => T.window.getCurrentWindow().label,
    closeWindow: () => T.window.getCurrentWindow().close(),
  };
}

/** Must be awaited once before any API call. */
export async function initApi() {
  if (backend) return backend;
  if (IN_TAURI) {
    backend = tauriBackend();
    document.documentElement.classList.add('is-tauri');
  } else if (IS_PREVIEW) {
    const mod = await import('./dev-preview.js');
    backend = mod.createPreviewBackend();
  } else {
    throw new Error('AetherLink UI must run inside Tauri or be served over http(s) for preview');
  }
  return backend;
}

// ---------------------------------------------------------------- normalization

const camel = (k) => k.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());

/** Deep snake_case → camelCase key conversion (engine/protocol structs may use either). */
export function camelize(v) {
  if (Array.isArray(v)) return v.map(camelize);
  if (v && typeof v === 'object' && !(v instanceof ArrayBuffer) && !ArrayBuffer.isView(v)) {
    const out = {};
    for (const [k, val] of Object.entries(v)) out[camel(k)] = camelize(val);
    return out;
  }
  return v;
}

/** Channel payload → Uint8Array (raw ArrayBuffer, typed array or JSON number array). */
export function toBytes(data) {
  if (data instanceof Uint8Array) return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (Array.isArray(data)) return Uint8Array.from(data);
  return new Uint8Array(0);
}

export class ApiError extends Error {
  constructor(code, detail, command) {
    super(detail || code);
    this.code = code;
    this.detail = detail;
    this.command = command;
  }
}

const KNOWN_CODES = new Set([
  'engine-not-wired',
  'invalid-decision',
  'unsupported-locale',
  'not-found',
  'unauthorized',
  'timeout',
  'unsupported',
  'permission-denied',
  'offline',
]);

function toApiError(e, command) {
  if (e instanceof ApiError) return e;
  const raw = typeof e === 'string' ? e : e && typeof e === 'object' ? e.code || e.message || JSON.stringify(e) : String(e);
  const code = KNOWN_CODES.has(raw) ? raw : (typeof e === 'object' && e && KNOWN_CODES.has(e.code) ? e.code : 'backend');
  const detail = typeof e === 'object' && e && e.message ? e.message : raw;
  return new ApiError(code, detail, command);
}

/** Localized, user-facing message for an error thrown by `api.*`. */
export function describeError(err) {
  const e = toApiError(err);
  // Engine errors arrive as stable i18n keys (e.g. "errors.wrongPassword").
  const key = typeof e.detail === 'string' && /^errors\.[A-Za-z]+$/.test(e.detail) ? e.detail : null;
  if (key) {
    const msg = t(key);
    if (msg && msg !== key) return msg;
  }
  switch (e.code) {
    case 'engine-not-wired':
      return t('errors.engineNotWired');
    case 'invalid-decision':
      return t('errors.invalidDecision');
    case 'unsupported-locale':
      return t('errors.unsupportedLocale');
    case 'not-found':
      return t('errors.notFound');
    case 'unauthorized':
      return t('errors.unauthorized');
    case 'timeout':
      return t('errors.timeout');
    case 'unsupported':
      return t('errors.unsupported');
    case 'permission-denied':
      return t('errors.permissionDenied');
    case 'offline':
      return t('errors.offline');
    default:
      return t('errors.generic', { detail: e.detail || e.code });
  }
}

export const isNotWired = (err) => err && err.code === 'engine-not-wired';

async function call(command, args) {
  if (!backend) await initApi();
  try {
    return camelize(await backend.invoke(command, args));
  } catch (e) {
    throw toApiError(e, command);
  }
}

// ---------------------------------------------------------------- input hot path

let inputErrorLogged = false;
function fire(command, args) {
  backend.invoke(command, args).catch((e) => {
    if (!inputErrorLogged) {
      inputErrorLogged = true;
      console.warn(`[api] ${command} failed`, e);
    }
  });
}

// ---------------------------------------------------------------- public API

export const api = {
  // identity / settings
  getIdentity: () => call('get_identity'),
  rotatePassword: () => call('rotate_password'),
  getSettings: () => call('get_settings'),
  setSettings: (settings) => call('set_settings', { settings }),
  getPermissions: () => call('get_permissions'),
  openPermissionSettings: (kind) => call('open_permission_settings', { kind }),

  // discovery / trust
  listLanPeers: () => call('list_lan_peers'),
  listTrustedDevices: () => call('list_trusted_devices'),
  removeTrustedDevice: (deviceId) => call('remove_trusted_device', { deviceId: String(deviceId) }),

  // sessions
  /** Opens the binary frames channel and connects. `onFrame(Uint8Array)`. */
  async connect(target, password, onFrame) {
    if (!backend) await initApi();
    const frames = backend.channel((data) => onFrame(toBytes(data)));
    const res = await call('connect', { target, password: password || null, frames });
    return { ...res, frames };
  },
  disconnect: (sessionId) => call('disconnect', { sessionId }),
  /** Fire-and-forget; `event` is serde-JSON for aether_protocol::InputEvent. */
  sendInput: (sessionId, event) => fire('send_input', { sessionId, event }),
  selectDisplay: (sessionId, displayId) => call('select_display', { sessionId, displayId }),
  setGhostMode: (sessionId, enabled) => call('set_ghost_mode', { sessionId, enabled }),
  setLosslessText: (sessionId, enabled) => call('set_lossless_text', { sessionId, enabled }),
  setAudio: (sessionId, enabled) => call('set_audio', { sessionId, enabled }),
  /** Local gain 0–1; `muted` optional. The host keeps streaming. */
  setAudioVolume: (sessionId, volume, muted = null) => call('set_audio_volume', { sessionId, volume, muted }),
  /** `[{id, name, isDefault}]` of local output devices. */
  listAudioOutputs: () => call('list_audio_outputs'),
  /** `deviceId` from listAudioOutputs, or null for the system default. */
  setAudioOutput: (sessionId, deviceId) => call('set_audio_output', { sessionId, deviceId: deviceId || null }),
  setQuality: (sessionId, quality) => call('set_quality', { sessionId, quality }),
  /** Ask the host for a headless virtual display (`enabled=false` removes it). */
  setVirtualDisplay: (sessionId, enabled, width = null, height = null) => call('set_virtual_display', { sessionId, enabled, width, height }),
  /** Lift Ghost Mode on this machine (local escape hatch). */
  localGhostOff: () => call('local_ghost_off'),

  // control iPhone / iPad over Bluetooth HID or the ESP32-S3 dongle
  /** `[{id, kind: 'bluetooth'|'dongle', available, detail}]` */
  hidBackends: () => call('hid_backends'),
  hidStart: (backend, width = null, height = null) => call('hid_start', { backend, width, height }),
  hidStop: () => call('hid_stop'),
  /** `{backend, state, peer, reportsSent, reportsDropped, pairingCode, lastError}` or null. */
  hidStatus: () => call('hid_status'),
  hidPairedDevices: (backend) => call('hid_paired_devices', { backend }),
  hidConnect: (address) => call('hid_connect', { address }),
  /** Fire-and-forget; `event` is serde-JSON for aether_protocol::InputEvent. */
  hidSendInput: (event) => fire('hid_send_input', { event }),

  // semantic DOM
  domSubscribe: (sessionId, interactiveOnly = true) => call('dom_subscribe', { sessionId, interactiveOnly }),
  domSearch: (sessionId, query, limit = 40) => call('dom_search', { sessionId, query, limit }),
  domAction: (sessionId, nodeId, action = 'Press') => call('dom_action', { sessionId, nodeId, action }),
  domSetValue: (sessionId, nodeId, value) => call('dom_set_value', { sessionId, nodeId, value }),

  // ports
  portsScan: (sessionId) => call('ports_scan', { sessionId }),
  portForward: (sessionId, remotePort, localPort = null) => call('port_forward', { sessionId, remotePort, localPort }),
  portUnforward: (sessionId, remotePort) => call('port_unforward', { sessionId, remotePort }),

  // terminal
  async terminalOpen(sessionId, cols, rows, onOutput) {
    if (!backend) await initApi();
    const output = backend.channel((data) => onOutput(toBytes(data)));
    const res = await call('terminal_open', { sessionId, cols, rows, output });
    return { ...res, output };
  },
  terminalInput: (sessionId, terminalId, data) => fire('terminal_input', { sessionId, terminalId, data }),
  terminalResize: (sessionId, terminalId, cols, rows) => call('terminal_resize', { sessionId, terminalId, cols, rows }),
  terminalClose: (sessionId, terminalId) => call('terminal_close', { sessionId, terminalId }),

  // virtual drives
  vfsListShares: (sessionId) => call('vfs_list_shares', { sessionId }),
  vfsMount: (sessionId, shareId) => call('vfs_mount', { sessionId, shareId }),
  vfsUnmount: (mountPoint) => call('vfs_unmount', { mountPoint }),
  listMounts: () => call('list_mounts'),
  listLocalShares: () => call('list_local_shares'),
  addLocalShare: (path, readOnly) => call('add_local_share', { path, readOnly }),
  removeLocalShare: (shareId) => call('remove_local_share', { shareId }),

  // transfers
  sendFiles: (sessionId, paths) => call('send_files', { sessionId, paths }),

  // DVR
  dvrIndex: (sessionId) => call('dvr_index', { sessionId }),
  dvrSeek: (sessionId, ptsUs) => call('dvr_seek', { sessionId, ptsUs: Math.round(ptsUs) }),
  dvrLive: (sessionId) => call('dvr_live', { sessionId }),
  dvrOcr: (sessionId, ptsUs = null) => call('dvr_ocr', { sessionId, ptsUs: ptsUs === null ? null : Math.round(ptsUs) }),

  // host side
  hostStatus: () => call('host_status'),
  kickSession: (sessionId) => call('kick_session', { sessionId }),
  /** decision: 'accept' | 'acceptAndTrust' | 'deny' */
  respondIncoming: (requestId, decision) => call('respond_incoming', { requestId: String(requestId), decision }),

  // UI shell
  uiSetLocale: (locale) => call('ui_set_locale', { locale }),
  uiTrayState: (tray) => call('ui_tray_state', { tray }),
  uiOpenSessionWindow: (target, password, title) => call('ui_open_session_window', { target, password: password || null, title }),
  uiTakePendingConnect: () => call('ui_take_pending_connect'),
  uiSetHudHotkey: (accelerator) => call('ui_set_hud_hotkey', { accelerator }),
  uiShowMain: () => call('ui_show_main'),
  /** Close the current window (session viewport). */
  closeWindow: () => backend.closeWindow(),
  async copyText(text) {
    if (backend && backend.kind === 'tauri') return call('ui_copy_text', { text });
    await navigator.clipboard.writeText(text);
  },
};

/** Subscribe to a backend event; returns a synchronous unsubscribe function. */
export function on(event, cb) {
  let unlisten = null;
  let cancelled = false;
  Promise.resolve(backend || initApi())
    .then(() => backend.listen(event, (payload) => cb(camelize(payload))))
    .then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    })
    .catch((e) => console.warn(`[api] listen ${event} failed`, e));
  return () => {
    cancelled = true;
    if (unlisten) unlisten();
  };
}

/** Native file drag & drop with real filesystem paths. cb({type, paths, position}). */
export function onDragDrop(cb) {
  let unlisten = null;
  let cancelled = false;
  Promise.resolve(backend || initApi())
    .then(() => backend.dragDrop(cb))
    .then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    })
    .catch((e) => console.warn('[api] drag-drop listener failed', e));
  return () => {
    cancelled = true;
    if (unlisten) unlisten();
  };
}

export function windowLabel() {
  return backend ? backend.windowLabel() : 'main';
}

export const isPreview = () => backend && backend.kind === 'preview';
