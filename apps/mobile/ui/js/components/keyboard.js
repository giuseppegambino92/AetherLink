// Virtual keyboard bridge + modifier bar.
//
// A hidden <textarea> summons the OS keyboard. Plain typing is sent as Text events
// (IME/composition safe); Backspace / Enter and every modifier-bar key go out as HID
// Key press/release pairs. Sticky modifiers (Ctrl, Alt, Cmd, Shift) apply to the next
// key; a double tap locks them.
import { h } from '../../shared/js/lib/dom.js';
import { domCodeToHid, InputEvents } from '../../shared/js/lib/hid.js';
import { haptic } from '../ui.js';

const MOD_BITS = { ctrl: 0x01, shift: 0x02, alt: 0x04, meta: 0x08 };
const SENTINEL = '​​';

/** Character → DOM code (US layout) for modifier shortcuts like Ctrl+C. */
export function charToCode(ch) {
  if (/^[a-z]$/i.test(ch)) return `Key${ch.toUpperCase()}`;
  if (/^[0-9]$/.test(ch)) return `Digit${ch}`;
  return { ' ': 'Space', '-': 'Minus', '=': 'Equal', '[': 'BracketLeft', ']': 'BracketRight', '\\': 'Backslash', ';': 'Semicolon', "'": 'Quote', '`': 'Backquote', ',': 'Comma', '.': 'Period', '/': 'Slash' }[ch] || null;
}

/**
 * @param {{ root: HTMLElement, send: (ev:any) => void, androidKeys?: boolean }} opts
 */
export function createKeyboard({ root, send, androidKeys = false }) {
  const mods = { ctrl: 0, alt: 0, meta: 0, shift: 0 }; // 0 off · 1 one-shot · 2 locked
  const modBits = () => Object.entries(mods).reduce((m, [k, v]) => (v ? m | MOD_BITS[k] : m), 0);

  const field = h('textarea', {
    class: 'kbd-field',
    attrs: { autocapitalize: 'off', autocomplete: 'off', autocorrect: 'off', spellcheck: 'false', rows: '1', enterkeyhint: 'send' },
    i18nAttr: { 'aria-label': 'session.keyboard.field' },
  });
  field.value = SENTINEL;

  function tapKey(code, extra = 0) {
    const usage = domCodeToHid(code);
    if (usage === null) return;
    const m = modBits() | extra;
    send(InputEvents.key(usage, true, m));
    send(InputEvents.key(usage, false, m));
    consumeOneShot();
  }

  function consumeOneShot() {
    let changed = false;
    for (const k of Object.keys(mods)) if (mods[k] === 1) (mods[k] = 0), (changed = true);
    if (changed) renderMods();
  }

  function typeText(text) {
    if (!text) return;
    const m = modBits();
    if (m & (MOD_BITS.ctrl | MOD_BITS.alt | MOD_BITS.meta)) {
      // Shortcut: send each character as a HID key with the modifiers.
      for (const ch of text) {
        const code = charToCode(ch);
        if (code) tapKey(code, /[A-Z]/.test(ch) ? MOD_BITS.shift : 0);
      }
      return;
    }
    send(InputEvents.text(mods.shift ? text.toUpperCase() : text));
    consumeOneShot();
  }

  let composing = false;
  field.addEventListener('compositionstart', () => (composing = true));
  field.addEventListener('compositionend', (e) => {
    composing = false;
    typeText(e.data || '');
    field.value = SENTINEL;
  });
  field.addEventListener('beforeinput', (e) => {
    switch (e.inputType) {
      case 'insertText':
        if (composing) return;
        e.preventDefault();
        typeText(e.data || '');
        break;
      case 'insertLineBreak':
      case 'insertParagraph':
        e.preventDefault();
        tapKey('Enter');
        break;
      case 'deleteContentBackward':
        if (composing) return;
        e.preventDefault();
        tapKey('Backspace');
        break;
      case 'deleteContentForward':
        e.preventDefault();
        tapKey('Delete');
        break;
      default:
        break;
    }
  });
  // Fallback for keyboards that ignore preventDefault on beforeinput (some Android IMEs).
  field.addEventListener('input', () => {
    if (composing) return;
    const v = field.value;
    if (v.length < SENTINEL.length) tapKey('Backspace');
    else if (v.startsWith(SENTINEL) && v.length > SENTINEL.length) typeText(v.slice(SENTINEL.length));
    field.value = SENTINEL;
  });
  // Hardware keyboards (Bluetooth / iPad): special keys as HID.
  field.addEventListener('keydown', (e) => {
    const special = ['Enter', 'Tab', 'Escape', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown', 'Delete'];
    if (special.includes(e.key) || ((e.ctrlKey || e.metaKey || e.altKey) && e.code)) {
      e.preventDefault();
      const extra = (e.ctrlKey ? 1 : 0) | (e.shiftKey ? 2 : 0) | (e.altKey ? 4 : 0) | (e.metaKey ? 8 : 0);
      tapKey(e.code || e.key, extra);
    }
  });

  const modButtons = {};
  const modKey = (k, labelKey) => {
    const b = h('button', { class: 'key mod', attrs: { type: 'button', 'aria-pressed': 'false' }, i18n: labelKey });
    let lastTap = 0;
    b.addEventListener('pointerdown', (e) => {
      e.preventDefault(); // keep the OS keyboard up
      haptic(6);
      const now = performance.now();
      mods[k] = mods[k] ? (now - lastTap < 320 && mods[k] === 1 ? 2 : 0) : 1;
      lastTap = now;
      renderMods();
    });
    modButtons[k] = b;
    return b;
  };
  function renderMods() {
    for (const [k, b] of Object.entries(modButtons)) {
      b.setAttribute('aria-pressed', String(mods[k] > 0));
      b.classList.toggle('locked', mods[k] === 2);
    }
  }
  /** Key with a localized caption, or a symbol caption (`sym`) plus a localized aria-label. */
  const key = (labelKey, code, sym) => {
    const b = sym
      ? h('button', { class: 'key', attrs: { type: 'button' }, text: sym, i18nAttr: { 'aria-label': labelKey } })
      : h('button', { class: 'key', attrs: { type: 'button' }, i18n: labelKey });
    b.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      haptic(6);
      tapKey(code);
    });
    return b;
  };
  const sys = (labelKey, action, sym) => {
    const b = h('button', { class: 'key sys', attrs: { type: 'button' }, text: sym, i18nAttr: { 'aria-label': labelKey } });
    b.addEventListener('pointerdown', (e) => {
      e.preventDefault();
      haptic(6);
      send(InputEvents.system(action));
    });
    return b;
  };

  const fkeys = Array.from({ length: 12 }, (_, i) => {
    const b = h('button', { class: 'key', attrs: { type: 'button' }, text: `F${i + 1}` });
    b.addEventListener('pointerdown', (e) => (e.preventDefault(), haptic(6), tapKey(`F${i + 1}`)));
    return b;
  });

  const bar = h(
    'div',
    { class: 'modbar', hidden: true, attrs: { role: 'toolbar' }, i18nAttr: { 'aria-label': 'session.keyboard.bar' } },
    h(
      'div',
      { class: 'modbar-row' },
      key('keys.esc', 'Escape'),
      key('keys.tab', 'Tab'),
      modKey('ctrl', 'keys.ctrl'),
      modKey('alt', 'keys.alt'),
      modKey('meta', 'keys.cmd'),
      modKey('shift', 'keys.shift'),
      key('keys.left', 'ArrowLeft', '←'),
      key('keys.down', 'ArrowDown', '↓'),
      key('keys.up', 'ArrowUp', '↑'),
      key('keys.right', 'ArrowRight', '→'),
      key('keys.home', 'Home'),
      key('keys.end', 'End'),
      key('keys.pageUp', 'PageUp'),
      key('keys.pageDown', 'PageDown'),
      key('keys.delete', 'Delete'),
      ...fkeys,
      androidKeys && sys('keys.androidBack', 'AndroidBack', '◁'),
      androidKeys && sys('keys.androidHome', 'AndroidHome', '○'),
      androidKeys && sys('keys.androidRecents', 'AndroidRecents', '▢'),
    ),
    field,
  );
  root.appendChild(bar);
  let open = false;
  function show() {
    open = true;
    bar.hidden = false;
    field.value = SENTINEL;
    field.focus({ preventScroll: true });
  }
  function hide() {
    open = false;
    field.blur();
    bar.hidden = true;
    for (const k of Object.keys(mods)) mods[k] = 0;
    renderMods();
  }
  return { show, hide, toggle: () => (open ? hide() : show()), isOpen: () => open, el: bar };
}
