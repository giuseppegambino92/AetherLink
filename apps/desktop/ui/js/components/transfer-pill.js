// Floating transfer pill + drop target. Dropping files on the viewport sends
// them with `send_files` using the real paths from Tauri's drag-drop event.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, on, onDragDrop } from '../api.js';
import { springPresence, SPRINGS } from '../lib/spring.js';
import { toastError } from './toast.js';
import { formatBytes } from '../lib/format.js';
import { t, tp } from '../i18n.js';

const R = 13;
const C = 2 * Math.PI * R;

export function createTransfers({ root, sessionId, peerName, isEnabled }) {
  const ringFg = h('svg:circle', { attrs: { cx: 16, cy: 16, r: R, fill: 'none', stroke: '#8ab8ff', 'stroke-width': 2.5, 'stroke-linecap': 'round', 'stroke-dasharray': C.toFixed(2), 'stroke-dashoffset': C.toFixed(2) } });
  const ringIcon = h('div', { class: 'icon' }, icon('upload', 14));
  const title = h('div', { class: 'row-title' });
  const sub = h('div', { class: 'mono' });
  const pill = h(
    'div',
    { class: 'transfer-pill', hidden: true, attrs: { role: 'status', 'aria-live': 'polite' } },
    h('div', { class: 'transfer-ring' }, h('svg:svg', { attrs: { width: 32, height: 32, viewBox: '0 0 32 32' } }, h('svg:circle', { attrs: { cx: 16, cy: 16, r: R, fill: 'none', stroke: 'rgba(255,255,255,0.1)', 'stroke-width': 2.5 } }), ringFg), ringIcon),
    h('div', { class: 'transfer-meta' }, title, sub),
  );
  const dropOverlay = h('div', { class: 'drop-overlay', hidden: true }, h('div', { class: 'row' }, icon('upload', 34), h('span', { i18n: 'transfer.dropTo', i18nArgs: { name: peerName } })));
  root.append(pill, dropOverlay);

  const transfers = new Map();
  let visible = false;
  let hideTimer = 0;

  function render() {
    const list = [...transfers.values()];
    if (!list.length) return;
    const total = list.reduce((a, x) => a + (Number(x.bytesTotal) || 0), 0);
    const done = list.reduce((a, x) => a + (Number(x.bytesDone) || 0), 0);
    const failed = list.some((x) => String(x.state).toLowerCase() === 'failed');
    const allDone = list.every((x) => ['completed', 'done', 'failed', 'cancelled'].includes(String(x.state).toLowerCase()));
    const f = total ? done / total : 0;
    ringFg.setAttribute('stroke-dashoffset', (C * (1 - f)).toFixed(2));
    ringFg.setAttribute('stroke', failed ? '#ff8089' : allDone ? '#64d6a0' : '#8ab8ff');
    replace(ringIcon, icon(failed ? 'alert' : allDone ? 'check' : 'upload', 14));
    const latest = list[list.length - 1];
    const files = list.reduce((a, x) => a + (Number(x.filesTotal) || 1), 0);
    title.textContent = failed ? t('transfer.failed') : allDone ? tp('transfer.sentCount', files) : latest.name || tp('transfer.sendingCount', files);
    sub.textContent = total ? t('transfer.progress', { done: formatBytes(done), total: formatBytes(total) }) : t('transfer.preparing');
    if (!visible) {
      visible = true;
      pill.hidden = false;
      springPresence(pill, { show: true, preset: SPRINGS.bouncy, distance: 20 });
    }
    clearTimeout(hideTimer);
    if (allDone) {
      hideTimer = setTimeout(async () => {
        visible = false;
        await springPresence(pill, { show: false, preset: SPRINGS.snappy, distance: 16 });
        if (!visible) pill.hidden = true;
        transfers.clear();
      }, failed ? 6000 : 2600);
    }
  }

  const offProgress = on('transfer-progress', (p) => {
    if (p.sessionId && String(p.sessionId) !== String(sessionId)) return;
    transfers.set(p.transferId, { ...(transfers.get(p.transferId) || {}), ...p });
    render();
  });

  const offDrop = onDragDrop(async (ev) => {
    if (!isEnabled()) return;
    if (ev.type === 'enter' || ev.type === 'over') dropOverlay.hidden = false;
    else if (ev.type === 'leave') dropOverlay.hidden = true;
    else if (ev.type === 'drop') {
      dropOverlay.hidden = true;
      const paths = ev.paths || [];
      if (!paths.length) return;
      try {
        const res = await api.sendFiles(sessionId, paths);
        const id = res.transferId;
        if (id && !transfers.has(id)) transfers.set(id, { transferId: id, name: paths[0].split(/[\\/]/).pop(), bytesDone: 0, bytesTotal: 0, filesTotal: paths.length, state: 'queued' });
        render();
      } catch (e) {
        toastError(e);
      }
    }
  });

  return { dispose: () => (offProgress(), offDrop()) };
}
