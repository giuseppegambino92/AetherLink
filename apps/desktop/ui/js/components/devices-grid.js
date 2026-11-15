// Unattended (trusted) devices: cards with live miniature preview area,
// online dot and OS glyph.
import { h, replace } from '../lib/dom.js';
import { icon, osIcon } from '../lib/icons.js';
import { api, isNotWired } from '../api.js';
import { emptyState } from './controls.js';
import { springHover } from '../lib/spring.js';
import { formatIdSpaced, formatRelative, osKey, toEpochMs } from '../lib/format.js';
import { openSession } from '../sessions.js';
import { confirmSheet } from './sheet.js';
import { toastError, toastSuccess } from './toast.js';
import { onLocaleChange } from '../i18n.js';

export function devicesGrid({ limit = Infinity } = {}) {
  const el = h('div', { class: 'device-grid' });
  let devices = [];

  function thumbSrc(d) {
    const t = d.thumbnail || d.preview || null;
    if (!t) return null;
    if (typeof t === 'string') return t.startsWith('data:') || t.startsWith('blob:') || t.startsWith('asset:') || t.startsWith('http') ? t : `data:image/png;base64,${t}`;
    return null;
  }

  function deviceCard(d) {
    const online = d.online === true || d.status === 'online';
    const src = thumbSrc(d);
    const lastSeen = toEpochMs(d.lastSeen);
    const preview = h(
      'div',
      { class: `device-preview${online ? '' : ' offline'}` },
      src ? h('img', { attrs: { src, alt: '', draggable: 'false' } }) : icon('monitor', 34),
      h('div', { class: 'os-badge', i18nAttr: { title: `os.${osKey(d.os)}` } }, osIcon(osKey(d.os), 15)),
      h('span', { class: `chip status-badge ${online ? 'success' : ''}` }, h('span', { class: `dot ${online ? 'online' : ''}` }), h('span', { i18n: online ? 'devices.online' : 'devices.offline' })),
    );
    const name = d.name || d.deviceName || formatIdSpaced(d.deviceId);
    const c = h(
      'article',
      { class: 'card device-card' },
      preview,
      h(
        'div',
        { class: 'device-info' },
        h('div', { class: 'spacer', style: { minWidth: '0' } }, h('div', { class: 'device-name', text: name }), h('div', { class: 'device-id', text: formatIdSpaced(d.deviceId) })),
        !online && lastSeen && h('span', { class: 'hint', i18n: 'devices.lastSeen', i18nArgs: { when: formatRelative(lastSeen) } }),
      ),
      h(
        'div',
        { class: 'device-actions' },
        h('button', { class: 'btn primary sm', attrs: { disabled: !online }, on: { click: () => openSession({ target: String(d.deviceId), title: name }) } }, icon('arrowRight', 14), h('span', { i18n: 'common.connect' })),
        h(
          'button',
          {
            class: 'icon-btn sm bordered',
            i18nAttr: { title: 'devices.remove', 'aria-label': 'devices.remove' },
            on: { click: () => remove(d, name) },
          },
          icon('trash', 14),
        ),
      ),
    );
    springHover(c, { lift: 4, scale: 0.008 });
    return c;
  }

  async function remove(d, name) {
    const ok = await confirmSheet({ titleKey: 'devices.removeTitle', bodyKey: 'devices.removeBody', args: { name }, confirmKey: 'devices.remove', danger: true });
    if (!ok) return;
    try {
      const list = await api.removeTrustedDevice(d.deviceId);
      devices = Array.isArray(list) ? list : devices.filter((x) => x !== d);
      render();
      toastSuccess('devices.removed', { name });
    } catch (e) {
      toastError(e);
    }
  }

  function render() {
    if (!devices.length) {
      replace(el, h('div', { style: { gridColumn: '1 / -1' } }, emptyState({ iconName: 'devices', titleKey: 'devices.emptyTitle', bodyKey: 'devices.emptyBody' })));
      return;
    }
    replace(el, devices.slice(0, limit).map(deviceCard));
  }

  async function load() {
    replace(el, Array.from({ length: Math.min(limit, 3) }, () => h('div', { class: 'skeleton', style: { height: '230px', borderRadius: '20px' } })));
    try {
      const list = await api.listTrustedDevices();
      devices = Array.isArray(list) ? list : [];
      render();
    } catch (e) {
      replace(
        el,
        h(
          'div',
          { style: { gridColumn: '1 / -1' } },
          emptyState({ iconName: isNotWired(e) ? 'plug' : 'alert', titleKey: isNotWired(e) ? 'engine.notWiredTitle' : 'devices.unavailable', bodyKey: isNotWired(e) ? 'engine.notWiredBody' : 'errors.tryAgain' }),
        ),
      );
    }
  }

  load();
  // relative "last seen" labels are computed — refresh them on language change
  const off = onLocaleChange(() => devices.length && render());
  el.reload = load;
  el.dispose = off;
  return el;
}
