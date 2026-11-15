// Bluetooth controller (Android): the phone registers as a HID keyboard + mouse
// (BluetoothHidDevice, report descriptor from aether-input) and becomes a trackpad
// and keyboard for an iPhone, iPad or Mac. Same gesture engine as the session view.
import { h, replace } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { InputEvents } from '../../shared/js/lib/hid.js';
import { t } from '../../shared/js/i18n.js';
import { api, describeError } from '../api.js';
import { Touchpad, accelerate } from '../lib/touchpad.js';
import { createKeyboard } from '../components/keyboard.js';
import { card, listRow, emptyState, toastError, haptic } from '../ui.js';

const LINE_PX = 28;

export function mountController(root, platform) {
  const page = h(
    'div',
    { class: 'page controller' },
    h('header', { class: 'page-head' }, h('span', { class: 'brand-mark' }, icon('keyboard', 22)), h('div', { class: 'page-head-text' }, h('h1', { class: 'page-title', i18n: 'controller.title' }), h('p', { class: 'page-sub', i18n: 'controller.subtitle' }))),
  );
  root.appendChild(page);
  if (!platform || !platform.hidController) {
    page.appendChild(card({ titleKey: 'controller.unavailableTitle', iconName: 'info' }, h('p', { class: 'card-text', i18n: 'controller.unavailableBody' })));
    return () => {};
  }

  const timers = [];
  let st = null;

  // ---------------------------------------------------------------- pairing
  const stateLine = h('div', { class: 'share-state' });
  const hostLine = h('div', { class: 'share-sub' });
  const toggleBtn = h('button', { class: 'btn primary lg grow', attrs: { type: 'button' }, on: { click: () => toggle() } });
  const bonded = h('div', { class: 'list' });
  const pairCard = card({ titleKey: 'controller.pairTitle', iconName: 'link' }, stateLine, hostLine, h('div', { class: 'btn-row' }, toggleBtn), h('p', { class: 'hint', i18n: 'controller.pairHint' }), h('div', { class: 'section-label', i18n: 'controller.bonded' }), bonded);

  function render() {
    const s = st || {};
    if (s.supported === false) {
      replace(stateLine, h('span', { i18n: 'controller.unsupported' }));
      toggleBtn.hidden = true;
      return;
    }
    replace(stateLine, h('span', { i18n: s.connected ? 'controller.connected' : s.registered ? 'controller.discoverable' : 'controller.off' }));
    hostLine.textContent = s.connected && s.host ? t('controller.connectedTo', { name: s.host.name || s.host.address }) : '';
    replace(toggleBtn, icon(s.registered ? 'power' : 'play', 18), h('span', { i18n: s.registered ? 'controller.stop' : 'controller.start' }));
    const list = Array.isArray(s.bonded) ? s.bonded : [];
    if (!list.length) replace(bonded, emptyState({ iconName: 'link', titleKey: 'controller.noBonded', bodyKey: 'controller.noBondedBody' }));
    else
      replace(
        bonded,
        list.map((d) =>
          listRow({
            glyph: icon('monitor', 20),
            title: d.name || d.address,
            sub: d.address,
            trailing: s.host && s.host.address === d.address && s.connected ? icon('check', 18) : icon('chevronRight', 16),
            onClick: () => api.hidConnect(d.address).then(refresh).catch(toastError),
          }),
        ),
      );
  }

  async function refresh() {
    try {
      st = await api.hidStatus();
      render();
    } catch (e) {
      stateLine.textContent = describeError(e);
    }
  }

  async function toggle() {
    haptic(14);
    try {
      if (st && st.registered) await api.hidStop();
      else {
        await api.requestPermissions(['bluetooth']).catch(() => {});
        await api.hidStart();
      }
    } catch (e) {
      toastError(e);
    }
    refresh();
  }

  // ---------------------------------------------------------------- trackpad + keys
  const surface = h('div', { class: 'hid-pad glass', attrs: { role: 'application' }, i18nAttr: { 'aria-label': 'controller.trackpad' } }, h('span', { class: 'hid-pad-hint', i18n: 'controller.trackpadHint' }));
  let move = null;
  let raf = 0;
  let scrollY = 0;
  let scrollX = 0;
  const send = (ev) => api.hidSend(ev);
  const flush = () => {
    raf = 0;
    if (move) {
      send({ MouseMoveRelative: { dx: Math.round(move.dx), dy: Math.round(move.dy) } });
      move = null;
    }
  };
  const pad = new Touchpad(surface, {
    mode: 'trackpad',
    onMove: (dx, dy) => {
      const [ax, ay] = accelerate(dx, dy, 1.8);
      move = move ? { dx: move.dx + ax, dy: move.dy + ay } : { dx: ax, dy: ay };
      if (!raf) raf = requestAnimationFrame(flush);
    },
    onClick: (b) => {
      flush();
      send(InputEvents.mouseButton(b, true));
      send(InputEvents.mouseButton(b, false));
      haptic(b === 'Right' ? 14 : 6);
    },
    onButton: (b, pressed) => {
      flush();
      send(InputEvents.mouseButton(b, pressed));
      if (pressed) haptic(18);
    },
    onScroll: (dx, dy, phase) => {
      if (phase !== 'Change') return;
      scrollX += dx;
      scrollY += dy;
      const lx = Math.trunc(scrollX / LINE_PX);
      const ly = Math.trunc(scrollY / LINE_PX);
      if (lx || ly) {
        scrollX -= lx * LINE_PX;
        scrollY -= ly * LINE_PX;
        send(InputEvents.scroll(lx, ly, 'Line', null));
      }
    },
  });

  const keysBar = h('div', { class: 'hid-keys' });
  const keyboard = createKeyboard({ root: keysBar, send });
  const buttons = h(
    'div',
    { class: 'hid-buttons' },
    h('button', { class: 'btn lg grow', attrs: { type: 'button' }, i18n: 'controller.leftClick', on: { click: () => (send(InputEvents.mouseButton('Left', true)), send(InputEvents.mouseButton('Left', false))) } }),
    h('button', { class: 'btn lg', attrs: { type: 'button' }, i18nAttr: { 'aria-label': 'session.keyboard.toggle' }, on: { click: () => keyboard.toggle() } }, icon('keyboard', 20)),
    h('button', { class: 'btn lg grow', attrs: { type: 'button' }, i18n: 'controller.rightClick', on: { click: () => (send(InputEvents.mouseButton('Right', true)), send(InputEvents.mouseButton('Right', false))) } }),
  );
  const padCard = card({ titleKey: 'controller.trackpad', iconName: 'move' }, surface, buttons, keysBar, h('p', { class: 'hint', i18n: 'controller.iosHint' }));

  page.append(pairCard, padCard);
  render();
  refresh();
  timers.push(setInterval(refresh, 2000));

  return () => {
    for (const id of timers) clearInterval(id);
    pad.dispose();
    keyboard.hide();
  };
}
