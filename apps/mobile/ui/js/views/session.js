// Session viewport: full-screen remote picture (WebCodecs hardware decode via the
// shared FrameRenderer), touch input in trackpad or direct-touch mode, Spatial PiP
// radar, floating semantic search button, Time-Travel scrubber, slide-down terminal,
// virtual keyboard with modifier bar and the remote file browser.
import { h, replace } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { FrameRenderer } from '../../shared/js/lib/renderer.js';
import { InputEvents } from '../../shared/js/lib/hid.js';
import { formatNumber } from '../../shared/js/lib/format.js';
import { t } from '../../shared/js/i18n.js';
import { api, on, describeError } from '../api.js';
import { takePending, displayName } from '../sessions.js';
import { Touchpad, accelerate } from '../lib/touchpad.js';
import { createKeyboard } from '../components/keyboard.js';
import { createTerminal } from '../components/terminal.js';
import { createDvr } from '../components/dvr.js';
import { createPip } from '../components/pip.js';
import { openSemanticPalette } from '../components/palette.js';
import { openFileBrowser } from '../components/files.js';
import { iconButton, segmented, toast, toastError, openSheet, listRow, haptic } from '../ui.js';

const MODE_KEY = 'aetherlink.mobile.inputMode';

function loadMode() {
  try {
    return localStorage.getItem(MODE_KEY) === 'direct' ? 'direct' : 'trackpad';
  } catch {
    return 'trackpad';
  }
}

export function mountSession(root) {
  const pending = takePending();
  document.documentElement.classList.add('in-session');
  const canvas = h('canvas', { class: 'screen', attrs: { width: 16, height: 9 } });
  const cursorEl = h('div', { class: 'vcursor', hidden: true });
  const viewport = h('div', { class: 'viewport' }, canvas, cursorEl);
  const shell = h('div', { class: 'session' }, viewport);
  root.appendChild(shell);

  const name = displayName(pending);
  const statusEl = h('div', { class: 'handshake' });
  shell.appendChild(statusEl);

  const leave = () => {
    location.hash = '#/home';
  };

  if (!pending || !pending.target) {
    replace(statusEl, h('div', { class: 'handshake-card glass' }, h('p', { i18n: 'session.noPending' }), h('button', { class: 'btn primary lg', attrs: { type: 'button' }, i18n: 'common.back', on: { click: leave } })));
    return () => document.documentElement.classList.remove('in-session');
  }

  // ---------------------------------------------------------------- state
  let sessionId = null;
  let connected = false;
  let disposed = false;
  let mode = loadMode();
  let displays = [];
  let activeDisplay = null;
  const cursor = { x: 0.5, y: 0.5 };
  let pendingMove = null;
  let scrollAcc = null;
  let raf = 0;
  const send = (ev) => sessionId && connected && api.sendInput(sessionId, ev);
  let pip = null;

  const renderer = new FrameRenderer(viewport, canvas, {
    onConfig: (cfg) => {
      activeDisplay = cfg.displayId ?? activeDisplay;
      if (pip) pip.onConfig();
    },
    onFirstFrame: () => markConnected(),
    onDecoderError: (code, detail) => toast({ message: t(`session.decoder.${code}`, { detail: detail || '' }), kind: 'error', timeout: 6000 }),
    onView: (rect, z) => {
      if (pip) pip.onView(rect, z);
      placeCursor();
    },
  });
  pip = createPip({ root: shell, renderer, onSelectDisplay: (id) => selectDisplay(id) });

  // ---------------------------------------------------------------- input
  function flush() {
    raf = 0;
    if (pendingMove) {
      send(InputEvents.mouseMove(renderer.displayId, pendingMove.x, pendingMove.y));
      pendingMove = null;
    }
    if (scrollAcc && (scrollAcc.dx || scrollAcc.dy)) {
      send(InputEvents.scroll(scrollAcc.dx, scrollAcc.dy, 'Pixel', 'Change'));
      scrollAcc.dx = scrollAcc.dy = 0;
    }
  }
  const schedule = () => raf || (raf = requestAnimationFrame(flush));
  const moveTo = (x, y) => {
    cursor.x = Math.min(1, Math.max(0, x));
    cursor.y = Math.min(1, Math.max(0, y));
    pendingMove = { x: cursor.x, y: cursor.y };
    schedule();
    placeCursor();
  };

  function placeCursor() {
    cursorEl.hidden = mode !== 'trackpad' || !connected;
    if (cursorEl.hidden) return;
    const r = canvas.getBoundingClientRect();
    const vr = viewport.getBoundingClientRect();
    cursorEl.style.transform = `translate3d(${(r.left - vr.left + cursor.x * r.width).toFixed(1)}px, ${(r.top - vr.top + cursor.y * r.height).toFixed(1)}px, 0)`;
  }

  /** Keep the cursor visible while zoomed (trackpad mode pans with it). */
  function follow() {
    if (renderer.zoom <= 1.01) return;
    const v = renderer.visibleRect();
    const m = 0.12;
    if (cursor.x < v.x + v.w * m || cursor.x > v.x + v.w * (1 - m) || cursor.y < v.y + v.h * m || cursor.y > v.y + v.h * (1 - m)) renderer.centerOn(cursor.x, cursor.y);
  }

  const click = (button) => {
    flush();
    send(InputEvents.mouseButton(button, true));
    send(InputEvents.mouseButton(button, false));
    haptic(button === 'Right' ? 14 : 6);
  };

  let pinchZoom0 = 1;
  const pad = new Touchpad(viewport, {
    mode,
    isEnabled: () => connected && !terminal?.isOpen(),
    onMove: (dx, dy) => {
      const r = canvas.getBoundingClientRect();
      if (!r.width || !r.height) return;
      const [ax, ay] = accelerate(dx, dy);
      moveTo(cursor.x + ax / r.width, cursor.y + ay / r.height);
      follow();
    },
    onClick: (b) => click(b),
    onButton: (b, pressed) => {
      flush();
      send(InputEvents.mouseButton(b, pressed));
      if (pressed) haptic(18);
    },
    onAbsolute: (phase, x, y) => {
      const n = renderer.clientToNormalized(x, y);
      if (n) moveTo(n.x, n.y);
      if (phase === 'down' && !n) return;
      if (phase === 'down' || phase === 'up') {
        flush();
        send(InputEvents.mouseButton('Left', phase === 'down'));
      }
    },
    onLongPress: (x, y) => {
      const n = renderer.clientToNormalized(x, y);
      if (n) moveTo(n.x, n.y);
      click('Right');
    },
    onScroll: (dx, dy, phase) => {
      if (phase === 'Begin') {
        scrollAcc = { dx: 0, dy: 0 };
        send(InputEvents.scroll(0, 0, 'Pixel', 'Begin'));
      } else if (phase === 'End') {
        flush();
        send(InputEvents.scroll(0, 0, 'Pixel', 'End'));
        scrollAcc = null;
      } else if (scrollAcc) {
        scrollAcc.dx += dx * 1.5;
        scrollAcc.dy += dy * 1.5;
        schedule();
      }
    },
    onPinch: (phase, scale, cx, cy, panDx, panDy) => {
      if (phase === 'begin') pinchZoom0 = renderer.zoom;
      if (phase !== 'change') return;
      renderer.zoomTo(pinchZoom0 * scale, cx, cy);
      // Pan with the fingers' centroid while zoomed.
      const r = canvas.getBoundingClientRect();
      const sx = renderer._cxSpring;
      const sy = renderer._cySpring;
      if (r.width && sx && sy && (panDx || panDy)) renderer.centerOn(sx.target - panDx / r.width, sy.target - panDy / r.height, true);
    },
  });

  // ---------------------------------------------------------------- chrome
  const dot = h('span', { class: 'status-dot' });
  const telemetry = h('span', { class: 'telemetry num' });
  const modeCtl = segmented({
    labelKey: 'session.mode.label',
    value: mode,
    options: [
      { value: 'trackpad', labelKey: 'session.mode.trackpad', iconName: 'move' },
      { value: 'direct', labelKey: 'session.mode.direct', iconName: 'crosshair' },
    ],
    onChange: (m) => setMode(m),
  });
  function setMode(m) {
    mode = m;
    pad.setMode(m);
    try {
      localStorage.setItem(MODE_KEY, m);
    } catch {
      /* storage unavailable */
    }
    placeCursor();
    toast({ key: m === 'trackpad' ? 'session.mode.trackpadHint' : 'session.mode.directHint' });
  }

  const topBar = h(
    'header',
    { class: 'session-top glass' },
    iconButton({ name: 'close', labelKey: 'session.disconnect', onClick: () => disconnect() }),
    h('div', { class: 'session-title' }, dot, h('span', { class: 'session-name', text: name }), telemetry),
    iconButton({ name: 'eyeOff', labelKey: 'session.hideControls', onClick: () => setChrome(false) }),
  );

  const dock = h(
    'nav',
    { class: 'session-dock glass', attrs: { role: 'toolbar' }, i18nAttr: { 'aria-label': 'session.toolbar' } },
    modeCtl,
    h(
      'div',
      { class: 'dock-tools' },
      iconButton({ name: 'keyboard', labelKey: 'session.keyboard.toggle', onClick: () => keyboard.toggle() }),
      iconButton({ name: 'terminal', labelKey: 'session.terminal', onClick: () => terminal && terminal.toggle() }),
      iconButton({ name: 'folder', labelKey: 'session.files', onClick: () => sessionId && openFileBrowser(sessionId) }),
      iconButton({ name: 'history', labelKey: 'session.dvr', onClick: () => dvr && dvr.toggle() }),
      iconButton({ name: 'monitor', labelKey: 'session.displays', onClick: () => openDisplays() }),
      iconButton({ name: 'layers', labelKey: 'session.pip', onClick: () => pip.toggle() }),
    ),
  );
  const showChrome = h('button', { class: 'chrome-handle glass', hidden: true, attrs: { type: 'button' }, i18nAttr: { 'aria-label': 'session.showControls' }, on: { click: () => setChrome(true) } }, icon('eye', 18));
  const cmdk = h('button', { class: 'fab', attrs: { type: 'button' }, i18nAttr: { 'aria-label': 'session.search', title: 'session.search' }, on: { click: () => (haptic(), sessionId && openSemanticPalette(sessionId)) } }, icon('command', 22));
  shell.append(topBar, dock, showChrome, cmdk);

  function setChrome(v) {
    topBar.hidden = !v;
    dock.hidden = !v;
    showChrome.hidden = v;
  }

  const keyboard = createKeyboard({ root: shell, send, androidKeys: String(pending.os).toLowerCase().includes('android') });
  let terminal = null;
  let dvr = null;

  function openDisplays() {
    const content = h(
      'div',
      { class: 'sheet-body-wrap' },
      h('h2', { class: 'sheet-title', id: 'displays-title', i18n: 'session.displays' }),
      displays.length
        ? h(
            'div',
            { class: 'list' },
            displays.map((d, i) =>
              listRow({
                glyph: icon('monitor', 20),
                title: d.name || t('session.displayN', { n: i + 1 }),
                sub: `${d.bounds?.width ?? ''}×${d.bounds?.height ?? ''}`,
                trailing: d.id === activeDisplay ? icon('check', 18) : null,
                onClick: () => (sheet.close(), selectDisplay(d.id)),
              }),
            ),
          )
        : h('p', { class: 'hint pad', i18n: 'session.noDisplays' }),
    );
    const sheet = openSheet({ content, labelledBy: 'displays-title' });
  }

  async function selectDisplay(id) {
    try {
      await api.selectDisplay(sessionId, id);
      activeDisplay = id;
      pip.setDisplays(displays, id);
    } catch (e) {
      toastError(e);
    }
  }

  // ---------------------------------------------------------------- handshake
  function showStage(key, args) {
    replace(statusEl, h('div', { class: 'handshake-card glass' }, h('div', { class: 'ring' }), h('div', { class: 'handshake-title', i18n: key, i18nArgs: args })));
    statusEl.hidden = false;
  }

  function showError(message, needsPassword) {
    const pw = h('input', { class: 'input', attrs: { type: 'password', autocomplete: 'off', inputmode: 'text' }, i18nAttr: { placeholder: 'home.passwordPlaceholder', 'aria-label': 'home.password' } });
    replace(
      statusEl,
      h(
        'div',
        { class: 'handshake-card glass' },
        h('span', { class: 'handshake-glyph error' }, icon('alert', 26)),
        h('p', { class: 'handshake-msg', text: message }),
        needsPassword && pw,
        h(
          'div',
          { class: 'sheet-actions' },
          h('button', { class: 'btn primary lg', attrs: { type: 'button' }, on: { click: () => start(needsPassword ? pw.value : pending.password) } }, icon('refresh', 16), h('span', { i18n: 'common.retry' })),
          h('button', { class: 'btn ghost lg', attrs: { type: 'button' }, i18n: 'common.back', on: { click: leave } }),
        ),
      ),
    );
    statusEl.hidden = false;
  }

  function markConnected() {
    if (connected) return;
    connected = true;
    statusEl.hidden = true;
    dot.classList.add('on');
    placeCursor();
  }

  // ---------------------------------------------------------------- events
  const offs = [];
  const early = [];
  const mine = (p) => p && (p.sessionId === undefined || p.sessionId === null || String(p.sessionId) === String(sessionId));
  const scoped = (fn) => (p) => {
    if (sessionId === null) early.push(() => mine(p) && fn(p));
    else if (mine(p)) fn(p);
  };
  offs.push(
    on(
      'session-state',
      scoped((p) => {
        const st = String(p.state || '').toLowerCase();
        if (st === 'connecting' || st === 'authenticating') showStage(`session.state.${st}`, { name });
        else if (st === 'connected') {
          markConnected();
          if (p.errorCode) toast({ key: p.errorCode, kind: 'error', timeout: 9000 });
        } else if (st === 'reconnecting') {
          connected = false;
          dot.classList.remove('on');
          showStage('session.state.reconnecting', { name });
        } else if (st === 'closed' || st === 'failed') {
          connected = false;
          dot.classList.remove('on');
          const reason = p.errorCode ? t(p.errorCode) : p.reason;
          const wrongPw = p.errorCode === 'errors.wrongPassword' || p.errorCode === 'errors.unauthorized';
          showError(reason ? t('session.closedReason', { reason }) : t('session.closed'), wrongPw);
        }
      }),
    ),
    on(
      'session-telemetry',
      scoped((p) => {
        const tm = p.telemetry || p;
        const parts = [`${formatNumber(tm.fps ?? renderer.fps)} ${t('session.units.fps')}`];
        if (tm.rttMs !== undefined && tm.rttMs !== null) parts.push(`${formatNumber(tm.rttMs)} ${t('session.units.ms')}`);
        telemetry.textContent = parts.join(' · ');
      }),
    ),
    on(
      'session-displays',
      scoped((p) => {
        displays = Array.isArray(p.displays) ? p.displays : [];
        activeDisplay = p.activeDisplayId ?? p.active?.Display?.displayId ?? activeDisplay;
        pip.setDisplays(displays, activeDisplay);
      }),
    ),
  );

  // ---------------------------------------------------------------- connect / leave
  async function start(password) {
    sessionId = null;
    connected = false;
    early.length = 0;
    showStage('session.state.connecting', { name });
    try {
      const res = await api.connect(String(pending.target), password, (bytes) => renderer.handle(bytes));
      if (disposed) return api.disconnect(res.sessionId).catch(() => {});
      sessionId = res.sessionId;
      bindSession(sessionId);
      for (const replay of early.splice(0)) replay();
    } catch (e) {
      showError(describeError(e), e && (e.code === 'errors.wrongPassword' || e.code === 'errors.unauthorized'));
    }
  }

  let boundTo = null;
  function bindSession(id) {
    if (boundTo === id) return;
    if (terminal) terminal.el.remove();
    if (dvr) (dvr.dispose(), dvr.el.remove());
    boundTo = id;
    terminal = createTerminal({ root: shell, sessionId: id, onVisibility: (v) => v && keyboard.hide() });
    dvr = createDvr({ root: shell, sessionId: id, onVisibility: (v) => (cmdk.hidden = v) });
    api.domSubscribe(id, true).catch(() => {});
  }

  async function disconnect() {
    leave();
  }

  const onVisibility = () => document.hidden && sessionId && connected && api.sendInput(sessionId, InputEvents.releaseAll());
  document.addEventListener('visibilitychange', onVisibility);

  start(pending.password);

  // Unmount: called by the router when leaving #/session (back button, close).
  return () => {
    disposed = true;
    document.removeEventListener('visibilitychange', onVisibility);
    for (const off of offs) off();
    pad.dispose();
    keyboard.hide();
    if (terminal) terminal.dispose();
    if (dvr) dvr.dispose();
    renderer.dispose();
    if (sessionId) {
      api.sendInput(sessionId, InputEvents.releaseAll());
      api.disconnect(sessionId).catch(() => {});
    }
    document.documentElement.classList.remove('in-session');
  };
}
