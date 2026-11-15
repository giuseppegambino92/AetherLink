// Spatial PiP radar (mobile): mini-map of every remote display with the visible
// viewport rectangle. Appears automatically for multi-monitor hosts, ultrawide
// displays (≥ 2.1:1, unreadable at phone width) or when zoomed in. Tap another
// display to switch; tap / drag inside the active one to pan there.
import { h, replace } from '../../shared/js/lib/dom.js';
import { springPresence, SPRINGS } from '../../shared/js/lib/spring.js';
import { formatNumber } from '../../shared/js/lib/format.js';
import { iconButton } from '../ui.js';

const MAP_W = 150;
const MAP_H = 84;

export function createPip({ root, renderer, onSelectDisplay }) {
  const map = h('div', { class: 'pip-map', style: { width: `${MAP_W}px`, height: `${MAP_H}px` } });
  const zoomVal = h('span', { class: 'pip-zoom num' });
  const el = h(
    'div',
    { class: 'pip glass', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'pip.title' } },
    map,
    h('div', { class: 'pip-bar' }, zoomVal, h('span', { class: 'spacer' }), iconButton({ name: 'maximize', labelKey: 'pip.fit', className: 'icon-btn sm', size: 16, onClick: () => renderer.resetView() })),
  );
  root.appendChild(el);

  let displays = [];
  let activeId = null;
  let viewRect = { x: 0, y: 0, w: 1, h: 1 };
  let zoom = 1;
  let manual = null;
  let shown = false;
  let geom = null;
  const viewEl = h('div', { class: 'pip-view' });

  function layoutMap() {
    let list = displays;
    if (!list.length) {
      list = [{ id: renderer.displayId, bounds: { x: 0, y: 0, width: renderer.width || 16, height: renderer.height || 9 }, synthetic: true }];
    }
    const minX = Math.min(...list.map((d) => d.bounds.x));
    const minY = Math.min(...list.map((d) => d.bounds.y));
    const maxX = Math.max(...list.map((d) => d.bounds.x + d.bounds.width));
    const maxY = Math.max(...list.map((d) => d.bounds.y + d.bounds.height));
    const s = Math.min((MAP_W - 4) / (maxX - minX || 1), (MAP_H - 4) / (maxY - minY || 1));
    geom = { s, ox: (MAP_W - (maxX - minX) * s) / 2, oy: (MAP_H - (maxY - minY) * s) / 2, minX, minY, list };
    replace(
      map,
      list.map((d, i) => {
        const b = d.bounds;
        const active = d.id === (activeId ?? list[0].id);
        const btn = h('button', {
          class: `pip-display${active ? ' active' : ''}`,
          text: String(i + 1),
          attrs: { type: 'button' },
          style: { left: `${geom.ox + (b.x - minX) * s}px`, top: `${geom.oy + (b.y - minY) * s}px`, width: `${b.width * s}px`, height: `${b.height * s}px` },
        });
        btn.addEventListener('pointerdown', (e) => {
          e.preventDefault();
          if (!active) return onSelectDisplay && onSelectDisplay(d.id);
          const pan = (ev) => {
            const r = btn.getBoundingClientRect();
            renderer.centerOn((ev.clientX - r.left) / r.width, (ev.clientY - r.top) / r.height);
          };
          if (zoom <= 1.01) {
            const r = btn.getBoundingClientRect();
            renderer.zoomTo(2.5);
            renderer.centerOn((e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height);
          } else pan(e);
          btn.setPointerCapture(e.pointerId);
          btn.onpointermove = pan;
          btn.onpointerup = () => (btn.onpointermove = null);
        });
        return btn;
      }),
      viewEl,
    );
    placeView();
  }

  function placeView() {
    if (!geom) return;
    const d = geom.list.find((x) => x.id === activeId) || geom.list[0];
    const b = d.bounds;
    const { s, ox, oy, minX, minY } = geom;
    viewEl.style.left = `${ox + (b.x - minX + viewRect.x * b.width) * s}px`;
    viewEl.style.top = `${oy + (b.y - minY + viewRect.y * b.height) * s}px`;
    viewEl.style.width = `${viewRect.w * b.width * s}px`;
    viewEl.style.height = `${viewRect.h * b.height * s}px`;
    zoomVal.textContent = `${formatNumber(zoom * 100, 0)}%`;
  }

  function wanted() {
    if (manual !== null) return manual;
    const ultrawide = (renderer.width || 16) / Math.max(1, renderer.height || 9) >= 2.1;
    return displays.length > 1 || ultrawide || zoom > 1.01;
  }

  function sync() {
    const want = wanted();
    if (want && !shown) {
      shown = true;
      el.hidden = false;
      layoutMap();
      springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: 14 });
    } else if (!want && shown) {
      shown = false;
      springPresence(el, { show: false, preset: SPRINGS.snappy, distance: 10 }).then(() => !shown && (el.hidden = true));
    } else if (shown) placeView();
  }

  return {
    el,
    setDisplays(list, active) {
      displays = (list || []).map((d) => ({ ...d, bounds: d.bounds || { x: 0, y: 0, width: d.width || 1, height: d.height || 1 } }));
      activeId = active ?? (displays.find((d) => d.primary) || displays[0] || {}).id ?? null;
      if (shown) layoutMap();
      sync();
    },
    onConfig() {
      if (shown) layoutMap();
      sync();
    },
    onView(rect, z) {
      viewRect = rect;
      zoom = z;
      sync();
    },
    toggle() {
      manual = !shown;
      sync();
      return manual;
    },
  };
}
