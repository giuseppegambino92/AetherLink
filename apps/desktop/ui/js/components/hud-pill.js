// Collapsible floating pill HUD. Zero pixels when hidden (display:none);
// revealed by a 4px top-edge hot zone or the HUD hotkey. A spring drives the
// drop-in + horizontal "pill expand" (clip-path inset from the centre).
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { Spring, SPRINGS, springPresence } from '../lib/spring.js';
import { formatNumber } from '../lib/format.js';

const HIDE_DELAY_MS = 700;

/**
 * @param {{ root: HTMLElement, peerName: string, actions: Record<string, Function> }} opts
 *   actions: openDisplayMenu, toggleLossless, toggleAudio, openAudioMenu, openDrivesMenu, openPorts, toggleTerminal,
 *            openPalette, toggleDvr, toggleGhost, togglePip, disconnect
 */
export function createHud({ root, peerName, actions }) {
  const hotzone = h('div', { class: 'hud-hotzone', attrs: { 'aria-hidden': 'true' } });
  const btn = (name, labelKey, onClick, { pressed, text, cls = '' } = {}) => {
    const b = h(
      'button',
      {
        class: `hud-btn ${cls}`,
        type: 'button',
        i18nAttr: { title: labelKey, 'aria-label': labelKey },
        attrs: pressed === undefined ? {} : { 'aria-pressed': String(!!pressed) },
        on: { click: (e) => onClick(e, b) },
      },
      icon(name, 17),
      text && h('span', { i18n: text }),
    );
    return b;
  };

  const peer = h('div', { class: 'hud-peer' }, h('span', { class: 'dot online' }), h('span', { text: peerName }));
  const displayBtn = btn('monitor', 'session.hud.display', (e, b) => actions.openDisplayMenu(b));
  const losslessBtn = btn('textFormat', 'session.hud.lossless', () => actions.toggleLossless(), { pressed: false });
  // Speaker = mute / unmute (starts the stream on first use); chevron = volume + output popover.
  const audioBtn = btn('speakerOff', 'session.hud.audio', () => actions.toggleAudio(), { pressed: false });
  const audioMenuBtn = btn('chevronDown', 'session.hud.audioMenu', (e, b) => actions.openAudioMenu(b), { cls: 'hud-btn-caret' });
  audioMenuBtn.setAttribute('aria-haspopup', 'menu');
  const drivesBtn = btn('drive', 'session.hud.drives', (e, b) => actions.openDrivesMenu(b));
  const portsBtn = btn('plug', 'session.hud.ports', () => actions.openPorts());
  const termBtn = btn('terminal', 'session.hud.terminal', () => actions.toggleTerminal());
  const searchBtn = btn('search', 'session.hud.search', () => actions.openPalette());
  const dvrBtn = btn('history', 'session.hud.dvr', () => actions.toggleDvr(), { pressed: false });
  const ghostBtn = btn('ghost', 'session.hud.ghost', () => actions.toggleGhost(), { pressed: false });
  const pipBtn = btn('layers', 'session.hud.pip', () => actions.togglePip(), { pressed: false });
  const discBtn = btn('power', 'session.hud.disconnect', () => actions.disconnect(), { cls: 'danger' });

  const fps = h('span', { class: 't-val', text: '—' });
  const rtt = h('span', { class: 't-val', text: '—' });
  const rate = h('span', { class: 't-val', text: '—' });
  const telemetry = h(
    'div',
    { class: 'telemetry', i18nAttr: { title: 'session.hud.telemetry' } },
    h('span', {}, fps, h('span', { class: 't-unit', i18n: 'session.telemetry.fps' })),
    h('span', {}, rtt, h('span', { class: 't-unit', i18n: 'session.telemetry.ms' })),
    h('span', {}, rate, h('span', { class: 't-unit', i18n: 'session.telemetry.mbps' })),
  );

  const sep = () => h('span', { class: 'hud-sep' });
  const pill = h(
    'div',
    { class: 'hud collapsed', attrs: { role: 'toolbar' }, i18nAttr: { 'aria-label': 'session.hud.aria' } },
    peer,
    sep(),
    displayBtn,
    losslessBtn,
    h('span', { class: 'hud-group' }, audioBtn, audioMenuBtn),
    sep(),
    drivesBtn,
    portsBtn,
    termBtn,
    searchBtn,
    dvrBtn,
    ghostBtn,
    pipBtn,
    sep(),
    telemetry,
    sep(),
    discBtn,
  );
  root.append(hotzone, pill);

  // ---- reveal spring
  let shown = false;
  let pinned = false;
  let menuOpen = false;
  let hideTimer = 0;
  const spring = new Spring({
    ...SPRINGS.pill,
    onUpdate: (p) => {
      const c = Math.max(0, Math.min(1, p));
      pill.style.opacity = Math.min(1, c * 1.6).toFixed(3);
      pill.style.transform = `translate3d(-50%, ${((1 - p) * -36).toFixed(2)}px, 0) scale(${(0.92 + 0.08 * p).toFixed(4)})`;
      pill.style.clipPath = `inset(0 ${((1 - c) * 46).toFixed(2)}% 0 ${((1 - c) * 46).toFixed(2)}% round 999px)`;
    },
    onRest: (p) => {
      if (p <= 0.001) pill.classList.add('collapsed');
    },
  });
  spring.jump(0);
  pill.classList.add('collapsed');

  const show = (pin = false) => {
    clearTimeout(hideTimer);
    pinned = pinned || pin;
    if (shown) return;
    shown = true;
    pill.classList.remove('collapsed');
    spring.to(1, 4);
  };
  const hide = (force = false) => {
    if (!shown || (pinned && !force) || menuOpen) return;
    pinned = false;
    shown = false;
    spring.to(0);
  };
  const scheduleHide = () => {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => hide(), HIDE_DELAY_MS);
  };

  hotzone.addEventListener('pointerenter', () => show(false));
  pill.addEventListener('pointerenter', () => clearTimeout(hideTimer));
  pill.addEventListener('pointerleave', () => !pinned && scheduleHide());
  hotzone.addEventListener('pointerleave', (e) => {
    if (!pill.contains(e.relatedTarget)) scheduleHide();
  });

  // ---- menus anchored under a HUD button
  let menuEl = null;
  function openMenu(anchor, buildItems) {
    closeMenu();
    menuOpen = true;
    const menu = h('div', { class: 'hud-menu', attrs: { role: 'menu' } }, buildItems(closeMenu));
    root.appendChild(menu);
    const r = anchor.getBoundingClientRect();
    menu.style.left = `${Math.max(8, r.left + r.width / 2 - 110)}px`;
    menuEl = menu;
    const outside = (e) => {
      if (!menu.contains(e.target) && !anchor.contains(e.target)) closeMenu();
    };
    setTimeout(() => addEventListener('pointerdown', outside, true));
    menu._outside = outside;
    springPresence(menu, { show: true, preset: SPRINGS.snappy, distance: -8 });
  }
  function closeMenu() {
    if (!menuEl) return;
    removeEventListener('pointerdown', menuEl._outside, true);
    menuEl.remove();
    menuEl = null;
    menuOpen = false;
    if (!pill.matches(':hover')) scheduleHide();
  }

  return {
    el: pill,
    show,
    hide,
    toggle() {
      if (shown) hide(true);
      else show(true);
    },
    isShown: () => shown,
    openMenu,
    closeMenu,
    setPressed(name, v) {
      const map = { lossless: losslessBtn, audio: audioBtn, dvr: dvrBtn, ghost: ghostBtn, pip: pipBtn };
      const b = map[name];
      if (!b) return;
      b.setAttribute('aria-pressed', String(!!v));
      if (name === 'audio') replace(b, icon(v ? 'speaker' : 'speakerOff', 17));
    },
    /** Grey out what the host does not allow: {audio, drives, ports, terminal, search, ghost}. */
    setAllowed(allowed) {
      const map = { audio: [audioBtn, audioMenuBtn], drives: [drivesBtn], ports: [portsBtn], terminal: [termBtn], search: [searchBtn], ghost: [ghostBtn] };
      for (const [name, buttons] of Object.entries(map)) {
        if (!(name in allowed)) continue;
        for (const b of buttons) {
          b.disabled = !allowed[name];
          b.classList.toggle('denied', !allowed[name]);
        }
      }
    },
    /** Partial update: only the provided metrics change. */
    setTelemetry({ fps: f, rttMs, bitrateKbps }) {
      if (f !== undefined && f !== null) fps.textContent = formatNumber(f, 0);
      if (rttMs !== undefined && rttMs !== null) rtt.textContent = formatNumber(rttMs, rttMs < 10 ? 1 : 0);
      if (bitrateKbps !== undefined && bitrateKbps !== null) rate.textContent = formatNumber(bitrateKbps / 1000, 1);
    },
    setConnected(ok) {
      peer.querySelector('.dot').className = `dot ${ok ? 'online' : 'busy'}`;
    },
  };
}
