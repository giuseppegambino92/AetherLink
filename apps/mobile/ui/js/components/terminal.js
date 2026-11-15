// Slide-down terminal (xterm.js) attached to a remote PTY. Typing goes through
// xterm's own hidden textarea, so the OS keyboard works as on desktop.
import { h, debounce } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { Spring, SPRINGS } from '../../shared/js/lib/spring.js';
import { t } from '../../shared/js/i18n.js';
import { api, describeError } from '../api.js';
import { iconButton } from '../ui.js';

let xtermLoaded = null;
function loadXterm() {
  if (!xtermLoaded) {
    const css = document.createElement('link');
    css.rel = 'stylesheet';
    css.href = new URL('../../shared/vendor/xterm/xterm.css', import.meta.url).href;
    document.head.appendChild(css);
    xtermLoaded = Promise.all([import('../../shared/vendor/xterm/xterm.mjs'), import('../../shared/vendor/xterm/addon-fit.mjs')]).then(([x, f]) => ({ Terminal: x.Terminal, FitAddon: f.FitAddon }));
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

export function createTerminal({ root, sessionId, onVisibility }) {
  const cwd = h('span', { class: 'cwd-chip', hidden: true });
  const status = h('span', { class: 'hint' });
  const host = h('div', { class: 'term-host' });
  const ctrlKeys = h(
    'div',
    { class: 'term-keys' },
    ...[
      ['keys.esc', '\x1b'],
      ['keys.tab', '\t'],
      ['terminal.ctrlC', '\x03'],
      ['terminal.ctrlD', '\x04'],
      ['terminal.ctrlZ', '\x1a'],
      ['keys.up', '\x1b[A', '↑'],
      ['keys.down', '\x1b[B', '↓'],
    ].map(([key, seq, sym]) =>
      h('button', {
        class: 'key',
        attrs: { type: 'button' },
        text: sym,
        i18n: sym ? undefined : key,
        i18nAttr: sym ? { 'aria-label': key } : undefined,
        on: { pointerdown: (e) => (e.preventDefault(), terminalId && api.terminalInput(sessionId, terminalId, seq)) },
      }),
    ),
  );
  const el = h(
    'section',
    { class: 'term glass', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'terminal.title' } },
    h('div', { class: 'term-bar' }, icon('terminal', 16), h('span', { class: 'term-title', i18n: 'terminal.title' }), cwd, status, h('span', { class: 'spacer' }), iconButton({ name: 'power', labelKey: 'terminal.end', onClick: () => end() }), iconButton({ name: 'chevronDown', labelKey: 'terminal.hide', onClick: () => hide() })),
    host,
    ctrlKeys,
  );
  root.appendChild(el);

  let term = null;
  let fit = null;
  let terminalId = null;
  let shown = false;
  const slide = new Spring({
    ...SPRINGS.snappy,
    value: 0,
    onUpdate: (p) => (el.style.transform = `translate3d(0, ${((p - 1) * 104).toFixed(3)}%, 0)`),
    onRest: (p) => p <= 0.001 && (el.hidden = true),
  });
  slide.jump(0);

  const pushResize = debounce(() => term && terminalId && api.terminalResize(sessionId, terminalId, term.cols, term.rows).catch(() => {}), 100);

  async function ensure() {
    if (term) return;
    const { Terminal, FitAddon } = await loadXterm();
    term = new Terminal({ fontFamily: '"JetBrains Mono", "SF Mono", ui-monospace, Menlo, monospace', fontSize: 12, lineHeight: 1.2, cursorBlink: true, allowTransparency: true, theme: THEME, scrollback: 3000 });
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
        cwd.hidden = false;
        cwd.textContent = res.cwd;
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
    term && term.blur();
    onVisibility && onVisibility(false);
  }

  async function end() {
    if (terminalId) await api.terminalClose(sessionId, terminalId).catch(() => {});
    terminalId = null;
    cwd.hidden = true;
    term && term.reset();
    hide();
  }

  return { show, hide, toggle: () => (shown ? hide() : show()), isOpen: () => shown, el, dispose: () => end() };
}
