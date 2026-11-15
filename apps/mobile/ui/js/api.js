// The ONLY mobile module that talks to Tauri (invoke / Channel / events).
// Same engine command surface as the desktop app (docs/ARCHITECTURE.md) plus the
// `mobile_*` commands of apps/mobile/src-tauri/src/mobile.rs and the barcode plugin.
import { t } from '../shared/js/i18n.js';

const w = window;
export const IN_TAURI =
  typeof w.__TAURI_INTERNALS__ !== 'undefined' ||
  typeof w.__TAURI__ !== 'undefined' ||
  location.protocol === 'tauri:' ||
  location.hostname === 'tauri.localhost';

let backend = null;

function tauriBackend() {
  const T = w.__TAURI__;
  if (!T || !T.core) throw new Error('Tauri globals missing: set app.withGlobalTauri = true');
  return {
    kind: 'tauri',
    invoke: (cmd, args) => T.core.invoke(cmd, args),
    channel: (onmessage) => {
      const ch = new T.core.Channel();
      ch.onmessage = onmessage;
      return ch;
    },
    listen: (event, cb) => T.event.listen(event, (e) => cb(e.payload)),
  };
}

/** Must be awaited once before any API call. */
export async function initApi() {
  if (backend) return backend;
  if (!IN_TAURI) throw new Error('not-tauri');
  backend = tauriBackend();
  document.documentElement.classList.add('is-tauri');
  return backend;
}

// ---------------------------------------------------------------- normalization

const camel = (k) => k.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());

/** Deep snake_case → camelCase key conversion. */
export function camelize(v) {
  if (Array.isArray(v)) return v.map(camelize);
  if (v && typeof v === 'object' && !(v instanceof ArrayBuffer) && !ArrayBuffer.isView(v)) {
    const out = {};
    for (const [k, val] of Object.entries(v)) out[camel(k)] = camelize(val);
    return out;
  }
  return v;
}

/** Channel payload → Uint8Array. */
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

function toApiError(e, command) {
  if (e instanceof ApiError) return e;
  const raw = typeof e === 'string' ? e : e && typeof e === 'object' ? e.code || e.message || JSON.stringify(e) : String(e);
  // Engine + mobile bridge errors are stable i18n keys ("errors.wrongPassword").
  const m = /errors\.[A-Za-z]+/.exec(String(raw));
  if (m) return new ApiError(m[0], m[0], command);
  if (raw === 'engine-not-wired') return new ApiError('errors.engineNotWired', raw, command);
  if (raw === 'invalid-decision') return new ApiError('errors.invalidDecision', raw, command);
  if (raw === 'unsupported-locale') return new ApiError('errors.unsupportedLocale', raw, command);
  return new ApiError('backend', raw, command);
}

/** Localized, user-facing message for an error thrown by `api.*`. */
export function describeError(err) {
  const e = toApiError(err);
  if (/^errors\.[A-Za-z]+$/.test(e.code)) {
    const msg = t(e.code);
    if (msg && msg !== e.code) return msg;
  }
  return t('errors.generic', { detail: e.detail || e.code });
}

export const isNotWired = (err) => err && toApiError(err).code === 'errors.engineNotWired';

async function call(command, args) {
  if (!backend) await initApi();
  try {
    return camelize(await backend.invoke(command, args));
  } catch (e) {
    throw toApiError(e, command);
  }
}

let fireErrorLogged = false;
function fire(command, args) {
  if (!backend) return;
  backend.invoke(command, args).catch((e) => {
    if (!fireErrorLogged) {
      fireErrorLogged = true;
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
  setAudio: (sessionId, enabled) => call('set_audio', { sessionId, enabled }),
  setQuality: (sessionId, quality) => call('set_quality', { sessionId, quality }),

  // semantic DOM
  domSubscribe: (sessionId, interactiveOnly = true) => call('dom_subscribe', { sessionId, interactiveOnly }),
  domSearch: (sessionId, query, limit = 40) => call('dom_search', { sessionId, query, limit }),
  domAction: (sessionId, nodeId, action = 'Press') => call('dom_action', { sessionId, nodeId, action }),
  domSetValue: (sessionId, nodeId, value) => call('dom_set_value', { sessionId, nodeId, value }),

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

  // remote files (no native mount on phones)
  vfsListShares: (sessionId) => call('vfs_list_shares', { sessionId }),
  vfsListDir: (sessionId, shareId, path) => call('mobile_vfs_list_dir', { sessionId, shareId, path }),
  vfsDownload: (sessionId, shareId, path) => call('mobile_vfs_download', { sessionId, shareId, path }),

  // DVR
  dvrIndex: (sessionId) => call('dvr_index', { sessionId }),
  dvrSeek: (sessionId, ptsUs) => call('dvr_seek', { sessionId, ptsUs: Math.round(ptsUs) }),
  dvrLive: (sessionId) => call('dvr_live', { sessionId }),
  dvrOcr: (sessionId, ptsUs = null) => call('dvr_ocr', { sessionId, ptsUs: ptsUs === null ? null : Math.round(ptsUs) }),

  // host side (engine)
  hostStatus: () => call('host_status'),
  kickSession: (sessionId) => call('kick_session', { sessionId: String(sessionId) }),
  /** decision: 'accept' | 'acceptAndTrust' | 'deny' */
  respondIncoming: (requestId, decision) => call('respond_incoming', { requestId: String(requestId), decision }),

  // UI shell
  uiSetLocale: (locale) => call('ui_set_locale', { locale }),
  copyText: (text) => call('ui_copy_text', { text }),

  // mobile platform
  platform: () => call('mobile_platform'),
  hostShareStatus: () => call('mobile_host_status'),
  hostShareStart: (audio = true) => call('mobile_host_start', { audio }),
  hostShareStop: () => call('mobile_host_stop'),
  inputStatus: () => call('mobile_input_status'),
  openAccessibilitySettings: () => call('mobile_open_accessibility_settings'),
  requestShizuku: () => call('mobile_request_shizuku'),
  /** kinds: any of 'notifications' | 'audio' | 'bluetooth' */
  requestPermissions: (kinds) => call('mobile_request_permissions', { kinds }),

  // Bluetooth HID controller (Android)
  hidStatus: () => call('mobile_hid_status'),
  hidStart: () => call('mobile_hid_start'),
  hidStop: () => call('mobile_hid_stop'),
  hidConnect: (address) => call('mobile_hid_connect', { address }),
  hidSend: (event) => fire('mobile_hid_send', { event }),

  // Camera QR: iOS uses the app's native AVFoundation scanner (`mobile_scan_qr`, answers
  // `errors.unsupported` elsewhere); Android uses tauri-plugin-barcode-scanner.
  async scanQr() {
    const native = await call('mobile_scan_qr').catch((e) => {
      if (e && e.code === 'errors.unsupported') return null;
      throw e;
    });
    if (native) return typeof native.content === 'string' ? native.content : '';
    const perm = await call('plugin:barcode-scanner|request_permissions').catch(() => null);
    if (perm === 'denied' || (perm && perm.camera === 'denied')) throw new ApiError('errors.cameraDenied', 'errors.cameraDenied');
    const res = await call('plugin:barcode-scanner|scan', { windowed: false, formats: ['QR_CODE'] });
    return res && typeof res.content === 'string' ? res.content : '';
  },
  cancelScan: () => call('plugin:barcode-scanner|cancel').catch(() => {}),
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
