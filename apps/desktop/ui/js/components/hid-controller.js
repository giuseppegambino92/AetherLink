// "Control iPhone / iPad": present this computer (or the ESP32-S3 dongle) to an iOS device as a
// Bluetooth/USB HID keyboard + pointer, then drive it from a pointer-locked trackpad surface.
// iOS cannot be controlled by software, so this is the only universal way in.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, describeError } from '../api.js';
import { card, segmented, emptyState } from './controls.js';
import { domCodeToHid, modifiersOf, mouseButtonName, InputEvents } from '../lib/hid.js';
import { toast, toastError } from './toast.js';

// Target screen in points (HID pointer motion is relative; this only scales the pointer estimate).
const TARGETS = { iphone: [393, 852], ipad: [820, 1180] };
const relative = (dx, dy) => ({ MouseMoveRelative: { dx, dy } });
/** Backend id → label key (`serial:<port>` is the ESP32-S3 dongle). */
const backendLabelKey = (id) => (id.startsWith('serial:') ? 'hid.backend.dongle' : id === 'linuxBlueZ' ? 'hid.backend.bluez' : 'hid.backend.mac');

export function hidControllerSection() {
  let backends = [];
  let selected = null;
  let target = 'iphone';
  let status = null;
  let paired = [];
  let busy = false;

  // ------------------------------------------------------------------ connection card
  const backendList = h('div', { class: 'hid-backends', attrs: { role: 'radiogroup' }, i18nAttr: { 'aria-label': 'hid.backend.label' } });
  const statusRow = h('div', { class: 'hid-status' });
  const startBtn = h('button', { class: 'btn primary', on: { click: () => toggle() } });
  const pairedList = h('div', { class: 'hid-paired' });
  const targetSeg = segmented({
    options: [
      { value: 'iphone', key: 'hid.target.iphone' },
      { value: 'ipad', key: 'hid.target.ipad' },
    ],
    value: target,
    labelKey: 'hid.target.label',
    onChange: (v) => (target = v),
  });

  const steps = h(
    'ol',
    { class: 'hid-steps' },
    h('li', { i18n: 'hid.steps.pair' }),
    h('li', { i18n: 'hid.steps.assistive' }),
    h('li', { i18n: 'hid.steps.start' }),
  );

  const connectCard = card(
    { className: 'span-5', titleKey: 'hid.connect.title', subtitleKey: 'hid.connect.subtitle', iconName: 'link', iconTone: 'accent' },
    h('div', { class: 'section-label', i18n: 'hid.backend.label' }),
    backendList,
    h('div', { class: 'row', style: { gap: '12px', marginTop: '14px' } }, targetSeg, h('div', { class: 'spacer' }), startBtn),
    statusRow,
    pairedList,
    h('div', { class: 'section-label', style: { marginTop: '16px' }, i18n: 'hid.steps.title' }),
    steps,
  );

  // ------------------------------------------------------------------ trackpad card
  const pad = h(
    'div',
    { class: 'hid-pad', attrs: { tabindex: '0', role: 'application' }, i18nAttr: { 'aria-label': 'hid.pad.aria' } },
    icon('crosshair', 28),
    h('div', { class: 'hid-pad-title', i18n: 'hid.pad.idle' }),
    h('div', { class: 'hid-pad-hint', i18n: 'hid.pad.hint' }),
  );
  const textInput = h('input', { class: 'input', attrs: { spellcheck: 'false', autocomplete: 'off' }, i18nAttr: { placeholder: 'hid.text.placeholder', 'aria-label': 'hid.text.label' } });
  const sendText = h('button', { class: 'btn', on: { click: () => typeText() } }, icon('keyboard', 14), h('span', { i18n: 'hid.text.send' }));
  textInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      typeText();
    }
  });
  const padCard = card(
    { className: 'span-7', titleKey: 'hid.pad.title', subtitleKey: 'hid.pad.subtitle', iconName: 'move', lift: false },
    pad,
    h('div', { class: 'row', style: { gap: '10px', marginTop: '12px' } }, h('div', { class: 'spacer' }, textInput), sendText),
  );

  // ------------------------------------------------------------------ rendering
  const connected = () => status && status.state === 'connected';
  const running = () => status && status.state !== 'stopped';

  function renderBackends() {
    if (!backends.length) {
      replace(backendList, emptyState({ iconName: 'plug', titleKey: 'hid.backend.none', bodyKey: 'hid.backend.noneBody' }));
      return;
    }
    replace(
      backendList,
      ...backends.map((b) =>
        h(
          'button',
          {
            class: `hid-backend ${selected === b.id ? 'selected' : ''} ${b.available ? '' : 'unavailable'}`,
            attrs: { role: 'radio', 'aria-checked': String(selected === b.id), disabled: running() ? true : undefined },
            on: { click: () => ((selected = b.id), renderBackends(), loadPaired()) },
          },
          icon(b.kind === 'dongle' ? 'plug' : 'wifi', 18),
          h(
            'div',
            { class: 'spacer' },
            h('div', { class: 'hid-backend-name', i18n: backendLabelKey(b.id) }),
            h('div', { class: 'hid-backend-detail', text: b.detail }),
          ),
          h('span', { class: `dot ${b.available ? 'online' : ''}` }),
        ),
      ),
    );
  }

  function renderStatus() {
    const st = status ? status.state : 'stopped';
    replace(
      statusRow,
      h('span', { class: `dot ${st === 'connected' ? 'online' : st === 'stopped' ? '' : 'busy'}` }),
      h('span', { i18n: `hid.state.${st}`, i18nArgs: { peer: status?.peer || '' } }),
      status?.pairingCode ? h('span', { class: 'chip accent mono', text: status.pairingCode }) : null,
      status?.lastError ? h('span', { class: 'hint danger', text: status.lastError }) : null,
    );
    replace(startBtn, icon(running() ? 'power' : 'play', 14), h('span', { i18n: running() ? 'hid.stop' : 'hid.start' }));
    startBtn.disabled = busy || (!running() && !backends.some((b) => b.id === selected && b.available));
    renderPad();
    renderPaired();
  }

  let padState = null;
  function renderPad() {
    const state = locked() ? 'captured' : connected() ? 'ready' : 'idle';
    pad.classList.toggle('captured', state === 'captured');
    pad.classList.toggle('ready', state !== 'idle');
    if (state === padState) return; // avoid re-rendering every status poll
    padState = state;
    replace(
      pad,
      icon(state === 'captured' ? 'move' : 'crosshair', 28),
      h('div', { class: 'hid-pad-title', i18n: `hid.pad.${state}` }),
      h('div', { class: 'hid-pad-hint', i18n: state === 'captured' ? 'hid.pad.release' : 'hid.pad.hint' }),
    );
  }

  function renderPaired() {
    if (!paired.length) {
      replace(pairedList);
      return;
    }
    replace(
      pairedList,
      h('div', { class: 'section-label', style: { marginTop: '14px' }, i18n: 'hid.paired.title' }),
      ...paired.map((d) =>
        h(
          'div',
          { class: 'hid-paired-row' },
          icon('osIos', 16),
          h('div', { class: 'spacer' }, h('div', { text: d.name || d.address }), h('div', { class: 'mono faint', text: d.address })),
          d.connected || (connected() && status.peer && status.peer.toLowerCase() === d.address.toLowerCase())
            ? h('span', { class: 'chip success', i18n: 'hid.paired.connected' })
            : h('button', { class: 'btn sm', attrs: { disabled: running() ? undefined : true }, on: { click: () => connectTo(d.address) } }, h('span', { i18n: 'hid.paired.connect' })),
        ),
      ),
    );
  }

  // ------------------------------------------------------------------ actions
  async function loadBackends() {
    try {
      backends = (await api.hidBackends()) || [];
      if (!selected) selected = (backends.find((b) => b.available) || backends[0] || {}).id || null;
    } catch (e) {
      backends = [];
      toastError(e);
    }
    renderBackends();
    renderStatus();
    loadPaired();
  }

  async function loadPaired() {
    if (!selected || selected.startsWith('serial:')) {
      paired = [];
      return renderPaired();
    }
    try {
      paired = (await api.hidPairedDevices(selected)) || [];
    } catch {
      paired = [];
    }
    renderPaired();
  }

  async function refreshStatus() {
    try {
      status = await api.hidStatus();
    } catch {
      status = null;
    }
    renderStatus();
  }

  async function toggle() {
    busy = true;
    renderStatus();
    try {
      if (running()) {
        await api.hidStop();
        status = null;
      } else {
        const [w, hgt] = TARGETS[target];
        status = await api.hidStart(selected, w, hgt);
        toast({ key: 'hid.startedToast', kind: 'success' });
      }
    } catch (e) {
      toast({ message: describeError(e), kind: 'error', timeout: 8000 });
    } finally {
      busy = false;
      renderBackends();
      renderStatus();
      renderPaired();
    }
  }

  async function connectTo(address) {
    try {
      await api.hidConnect(address);
      await refreshStatus();
    } catch (e) {
      toastError(e);
    }
  }

  function typeText() {
    const text = textInput.value;
    if (!text || !connected()) return;
    api.hidSendInput(InputEvents.text(text));
    textInput.value = '';
  }

  // ------------------------------------------------------------------ pointer-locked trackpad
  let acc = [0, 0];
  let raf = 0;
  const flush = () => {
    raf = 0;
    const [dx, dy] = acc;
    acc = [0, 0];
    if (dx || dy) api.hidSendInput(relative(dx, dy));
  };
  const locked = () => document.pointerLockElement === pad;
  const onMove = (e) => {
    if (!locked()) return;
    acc[0] += e.movementX;
    acc[1] += e.movementY;
    if (!raf) raf = requestAnimationFrame(flush);
  };
  const onButton = (pressed) => (e) => {
    if (!locked()) return;
    const b = mouseButtonName(e.button);
    if (b) api.hidSendInput(InputEvents.mouseButton(b, pressed));
    e.preventDefault();
  };
  const onWheel = (e) => {
    if (!locked()) return;
    e.preventDefault();
    api.hidSendInput(InputEvents.scroll(e.deltaX, e.deltaY, 'Pixel'));
  };
  const onKey = (pressed) => (e) => {
    if (!locked() || e.code === 'Escape') return;
    const code = domCodeToHid(e.code);
    if (code === null) return;
    e.preventDefault();
    api.hidSendInput(InputEvents.key(code, pressed, modifiersOf(e)));
  };
  const onLockChange = () => {
    renderPad();
    if (!locked() && connected()) api.hidSendInput(InputEvents.releaseAll());
  };
  pad.addEventListener('click', () => {
    if (!connected()) {
      toast({ key: 'hid.pad.needConnection' });
      return;
    }
    if (!locked()) pad.requestPointerLock();
  });
  pad.addEventListener('mousedown', onButton(true));
  pad.addEventListener('mouseup', onButton(false));
  pad.addEventListener('wheel', onWheel, { passive: false });
  pad.addEventListener('contextmenu', (e) => e.preventDefault());
  const keyDown = onKey(true);
  const keyUp = onKey(false);
  document.addEventListener('mousemove', onMove);
  document.addEventListener('keydown', keyDown);
  document.addEventListener('keyup', keyUp);
  document.addEventListener('pointerlockchange', onLockChange);

  loadBackends();
  refreshStatus();
  const timer = setInterval(refreshStatus, 1000);

  const root = h('div', { class: 'grid' }, connectCard, padCard);
  root.dispose = () => {
    clearInterval(timer);
    if (raf) cancelAnimationFrame(raf);
    if (locked()) document.exitPointerLock();
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('keydown', keyDown);
    document.removeEventListener('keyup', keyUp);
    document.removeEventListener('pointerlockchange', onLockChange);
  };
  return root;
}
