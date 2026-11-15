// Port Forwarding Drawer: scan the remote host's listening ports and toggle
// a local tunnel per port with one click; shows the local address to copy.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, isNotWired, describeError } from '../api.js';
import { emptyState, iconButton, toggleSwitch } from './controls.js';
import { springPresence, SPRINGS } from '../lib/spring.js';
import { copyWithToast, toastError } from './toast.js';
import { t } from '../i18n.js';

export function createPortsDrawer({ root, sessionId }) {
  const forwarded = new Map(); // remotePort -> localAddr
  const body = h('div', { class: 'drawer-body' });
  const scanBtn = h('button', { class: 'btn sm', on: { click: () => scan() } }, icon('refresh', 14), h('span', { i18n: 'ports.scan' }));
  const el = h(
    'aside',
    { class: 'drawer', attrs: { role: 'dialog', 'aria-labelledby': 'ports-title' }, hidden: true },
    h(
      'div',
      { class: 'drawer-header' },
      h('div', { class: 'card-icon aqua' }, icon('plug', 18)),
      h('div', { class: 'spacer' }, h('h3', { class: 'card-title', id: 'ports-title', i18n: 'ports.title' }), h('p', { class: 'card-subtitle', i18n: 'ports.subtitle' })),
      scanBtn,
      iconButton({ name: 'close', labelKey: 'common.close', onClick: () => close() }),
    ),
    body,
  );
  root.appendChild(el);
  el.addEventListener('keydown', (e) => e.key === 'Escape' && (e.stopPropagation(), close()));
  let open = false;
  let ports = [];

  function row(p) {
    const port = Number(p.port);
    const local = forwarded.get(port);
    const sw = toggleSwitch({
      checked: !!local,
      labelKey: 'ports.forwardToggle',
      onChange: async (on) => {
        try {
          if (on) {
            const res = await api.portForward(sessionId, port, null);
            forwarded.set(port, res.localAddr || res.local_addr || '');
          } else {
            await api.portUnforward(sessionId, port);
            forwarded.delete(port);
          }
        } catch (e) {
          sw.setChecked(!on);
          toastError(e);
        }
        render();
      },
    });
    const hint = p.serviceHint || p.processName || null;
    return h(
      'div',
      { class: 'port-row' },
      h('div', { class: 'port-num', text: String(port) }),
      h(
        'div',
        { style: { minWidth: '0' } },
        hint ? h('div', { class: 'row-title', text: hint }) : h('div', { class: 'row-title faint', i18n: 'ports.unknownService' }),
        h('div', { class: 'row-sub mono', text: [p.processName, p.pid && t('ports.pid', { pid: p.pid }), p.address].filter(Boolean).join(' · ') }),
      ),
      sw,
      local &&
        h(
          'div',
          { class: 'port-local' },
          icon('link', 13),
          h('span', { class: 'spacer', text: local }),
          iconButton({ name: 'copy', className: 'icon-btn sm', size: 13, labelKey: 'ports.copyAddress', onClick: () => copyWithToast(api, local) }),
        ),
    );
  }

  function render() {
    if (!ports.length) {
      replace(body, emptyState({ iconName: 'plug', titleKey: 'ports.emptyTitle', bodyKey: 'ports.emptyBody' }));
      return;
    }
    replace(body, ports.map(row));
  }

  async function scan() {
    replace(body, ...Array.from({ length: 4 }, () => h('div', { class: 'skeleton', style: { height: '48px', margin: '10px 0' } })));
    scanBtn.disabled = true;
    try {
      const list = await api.portsScan(sessionId);
      ports = (Array.isArray(list) ? list : []).sort((a, b) => a.port - b.port);
      render();
    } catch (e) {
      replace(
        body,
        isNotWired(e)
          ? emptyState({ iconName: 'plug', titleKey: 'engine.notWiredTitle', bodyKey: 'engine.notWiredBody' })
          : h('div', { class: 'empty' }, icon('alert', 18), h('div', { class: 'empty-body', text: describeError(e) })),
      );
    } finally {
      scanBtn.disabled = false;
    }
  }

  async function show() {
    if (open) return;
    open = true;
    el.hidden = false;
    springPresence(el, { show: true, axis: 'x', distance: 40, preset: SPRINGS.snappy });
    scan();
  }
  async function close() {
    if (!open) return;
    open = false;
    await springPresence(el, { show: false, axis: 'x', distance: 40, preset: SPRINGS.snappy });
    if (!open) el.hidden = true;
  }
  return { show, close, toggle: () => (open ? close() : show()), isOpen: () => open, el };
}
