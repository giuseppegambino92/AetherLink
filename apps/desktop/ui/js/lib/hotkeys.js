// Accelerator helpers (Tauri global-shortcut syntax, e.g. "CmdOrCtrl+Shift+Period").
import { t } from '../i18n.js';

export const DEFAULT_HUD_HOTKEY = 'CmdOrCtrl+Shift+Period';
const KEY = 'aetherlink.hudHotkey';
const IS_MAC = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

export function loadHudHotkey() {
  try {
    return localStorage.getItem(KEY) || DEFAULT_HUD_HOTKEY;
  } catch {
    return DEFAULT_HUD_HOTKEY;
  }
}

export function saveHudHotkey(accel) {
  try {
    localStorage.setItem(KEY, accel);
  } catch {
    /* storage unavailable */
  }
}

const MODIFIER_CODES = new Set(['ShiftLeft', 'ShiftRight', 'ControlLeft', 'ControlRight', 'AltLeft', 'AltRight', 'MetaLeft', 'MetaRight', 'OSLeft', 'OSRight']);

/** KeyboardEvent → accelerator string, or null if it is not a valid combo. */
export function eventToAccelerator(e) {
  if (MODIFIER_CODES.has(e.code)) return null;
  const mods = [];
  if (e.metaKey) mods.push(IS_MAC ? 'CmdOrCtrl' : 'Super');
  if (e.ctrlKey) mods.push(IS_MAC ? 'Ctrl' : 'CmdOrCtrl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  if (!mods.length) return null;
  let key = e.code;
  if (/^Key[A-Z]$/.test(key)) key = key.slice(3);
  else if (/^Digit[0-9]$/.test(key)) key = key.slice(5);
  return [...mods, key].join('+');
}

// Keycap labels: glyphs on macOS, localized key names elsewhere.
const GLYPHS = {
  Period: '.',
  Comma: ',',
  Backquote: '`',
  Slash: '/',
  Backslash: '\\',
  Semicolon: ';',
  Quote: "'",
  BracketLeft: '[',
  BracketRight: ']',
  Minus: '-',
  Equal: '=',
  Enter: '↩',
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
};
const MAC_MODS = { CmdOrCtrl: '⌘', Cmd: '⌘', Command: '⌘', Super: '⌘', Ctrl: '⌃', Control: '⌃', Alt: '⌥', Option: '⌥', Shift: '⇧' };
const OTHER_MODS = { CmdOrCtrl: 'keys.ctrl', Cmd: 'keys.super', Command: 'keys.super', Super: 'keys.super', Ctrl: 'keys.ctrl', Control: 'keys.ctrl', Alt: 'keys.alt', Option: 'keys.alt', Shift: 'keys.shift' };

function keyLabel(k) {
  if (IS_MAC && MAC_MODS[k]) return MAC_MODS[k];
  if (!IS_MAC && OTHER_MODS[k]) return t(OTHER_MODS[k]);
  if (GLYPHS[k]) return GLYPHS[k];
  if (k === 'Space') return t('keys.space');
  if (k === 'Escape') return t('keys.esc');
  return k;
}

/** "CmdOrCtrl+Shift+Period" → ["⌘","⇧","."] */
export function acceleratorKeys(accel) {
  return String(accel).split('+').map(keyLabel);
}

/** Does a keydown match an accelerator (used for in-window shortcuts)? */
export function matchesAccelerator(e, accel) {
  const parts = String(accel).split('+');
  const key = parts.pop();
  const want = { meta: false, ctrl: false, alt: false, shift: false };
  for (const p of parts) {
    if (p === 'CmdOrCtrl') want[IS_MAC ? 'meta' : 'ctrl'] = true;
    else if (p === 'Cmd' || p === 'Command' || p === 'Super') want.meta = true;
    else if (p === 'Ctrl' || p === 'Control') want.ctrl = true;
    else if (p === 'Alt' || p === 'Option') want.alt = true;
    else if (p === 'Shift') want.shift = true;
  }
  if (e.metaKey !== want.meta || e.ctrlKey !== want.ctrl || e.altKey !== want.alt || e.shiftKey !== want.shift) return false;
  const code = /^[A-Z]$/.test(key) ? `Key${key}` : /^[0-9]$/.test(key) ? `Digit${key}` : key;
  return e.code === code;
}
