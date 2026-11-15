// Slide-down Quake terminal (xterm.js) attached to a remote PTY.
import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, describeError } from '../api.js';
import { iconButton } from './controls.js';
import { Spring, SPRINGS } from '../lib/spring.js';
import { t } from '../i18n.js';
import { debounce } from '../lib/dom.js';

let xtermLoaded = null;
function loadXterm() {
  if (!xtermLoaded) {
    const css = document.createElement('link');
    css.rel = 'stylesheet';
    css.href = new URL('../../vendor/xterm/xterm.css', import.meta.url).href;
    document.head.appendChild(css);
    xtermLoaded = Promise.all([import('../../vendor/xterm/xterm.mjs'), import('../../vendor/xterm/addon-fit.mjs')]).then(([x, f]) => ({ Terminal: x.Terminal, FitAddon: f.FitAddon }));
  }
  return xtermLoaded;
}

const THEME = {
  background: 'rgba(0,0,0,0)',
  foreground: '#dfe6f3',
  cursor: '#8ab8ff',
  cursorAccent: '#0b0e14',
  selectionBackground: 'rgba(138,184,255,0.28)',
  black: '#1a1f2b',
  red: '#ff8089',
  green: '#64d6a0',
  yellow: '#f2c46d',
  blue: '#8ab8ff',
  magenta: '#a597ff',
  cyan: '#7de0d6',
  white: '#dfe6f3',
  brightBlack: '#5b6478',
  brightRed: '#ff9ea5',
  brightGreen: '#8be5bb',
  brightYellow: '#f7d792',
  brightBlue: '#abcbff',
  brightMagenta: '#c1b7ff',
  brightCyan: '#a4ece5',
  brightWhite: '#ffffff',
};

export function createQuakeTerminal({ root, sessionId, onVisibility }) {
  const cwdChip = h('span', { class: 'cwd-chip', hidden: true });
  const status = h('span', { class: 'hint' });
  const host = h('div', { class: 'quake-term' });
  const el = h(
    'section',
    { class: 'quake', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'terminal.title' } },
    h(
      'div',
      { class: 'quake-bar' },
      icon('terminal', 16),
      h('span', { class: 'row-title', i18n: 'terminal.title' }),
      cwdChip,
      status,
      h('span', { class: 'spacer' }),
      h('span', { class: 'kbd', text: '⌃`' }),
      iconButton({ name: 'power', labelKey: 'terminal.end', onClick: () => end() }),
      iconButton({ name: 'chevronDown', labelKey: 'terminal.hide', onClick: () => hide() }),
    ),
    host,
  );
  root.appendChild(el);

  let term = null;
  let fit = null;
  let terminalId = null;
  let shown = false;
  const slide = new Spring({
    ...SPRINGS.snappy,
    value: 0,
    onUpdate: (p) => (el.style.transform = `translate3d(0, ${((p - 1) * 102).toFixed(3)}%, 0)`),
    onRest: (p) => {
      if (p <= 0.001) el.hidden = true;
    },
  });
  slide.jump(0);

  const pushResize = debounce(() => {
    if (term && terminalId) api.terminalResize(sessionId, terminalId, term.cols, term.rows).catch(() => {});
  }, 80);

  async function ensure() {
    if (term) return;
    const { Terminal, FitAddon } = await loadXterm();
    term = new Terminal({
      fontFamily: '"JetBrains Mono", "SF Mono", ui-monospace, Menlo, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      allowTransparency: true,
      theme: THEME,
      scrollback: 5000,
      macOptionIsMeta: true,
    });
    fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    fit.fit();
    new ResizeObserver(() => {
      if (!shown) return;
      fit.fit();
      pushResize();
    }).observe(host);
    term.onData((data) => terminalId && api.terminalInput(sessionId, terminalId, data));
    await connectPty();
  }

  async function connectPty() {
    status.textContent = t('terminal.connecting');
    try {
      const res = await api.terminalOpen(sessionId, term.cols, term.rows, (bytes) => term.write(bytes));
      terminalId = res.terminalId;
      status.textContent = '';
      if (res.cwd) {
        cwdChip.hidden = false;
        cwdChip.textContent = res.cwd;
        cwdChip.title = res.cwd;
      }
    } catch (e) {
      terminalId = null;
      status.textContent = describeError(e);
    }
  }

  async function show() {
    if (shown) return;
    shown = true;
    el.hidden = false;
    slide.to(1);
    onVisibility && onVisibility(true);
    await ensure();
    fit && fit.fit();
    if (!terminalId && term) await connectPty();
    term && term.focus();
  }

  function hide() {
    if (!shown) return;
    shown = false;
    slide.to(0);
    if (term) term.blur();
    onVisibility && onVisibility(false);
  }

  async function end() {
    if (terminalId) {
      try {
        await api.terminalClose(sessionId, terminalId);
      } catch {
        /* engine may already have closed it */
      }
    }
    terminalId = null;
    cwdChip.hidden = true;
    if (term) term.reset();
    hide();
  }

  return { show, hide, toggle: () => (shown ? hide() : show()), isOpen: () => shown, el, dispose: () => end() };
}
