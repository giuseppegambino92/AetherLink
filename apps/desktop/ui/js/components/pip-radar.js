// Spatial PiP radar: mini-map of all remote displays with the current
// viewport rectangle. Click a display to switch, click inside the active one
// to jump there; zoom controls and cursor-follow toggle.
import { h, replace } from '../lib/dom.js';
import { iconButton } from './controls.js';
import { springPresence, SPRINGS } from '../lib/spring.js';
import { formatNumber } from '../lib/format.js';

const MAP_W = 220;
const MAP_H = 120;

export function createPip({ root, renderer, onSelectDisplay, onFollowChange }) {
  const map = h('div', { class: 'pip-map' });
  const zoomVal = h('span', { class: 'zoom-val' });
  let follow = false;
  const followBtn = iconButton({
    name: 'crosshair',
    labelKey: 'pip.follow',
    className: 'icon-btn sm',
    onClick: () => {
      follow = !follow;
      followBtn.setAttribute('aria-pressed', String(follow));
      followBtn.style.color = follow ? 'var(--accent)' : '';
      onFollowChange && onFollowChange(follow);
    },
  });
  followBtn.setAttribute('aria-pressed', 'false');
  const el = h(
    'div',
    { class: 'pip', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'pip.title' } },
    map,
    h(
      'div',
      { class: 'pip-bar' },
      iconButton({ name: 'zoomOut', labelKey: 'pip.zoomOut', className: 'icon-btn sm', onClick: () => renderer.zoomBy(1 / 1.5) }),
      zoomVal,
      iconButton({ name: 'zoomIn', labelKey: 'pip.zoomIn', className: 'icon-btn sm', onClick: () => renderer.zoomBy(1.5) }),
      iconButton({ name: 'maximize', labelKey: 'pip.fit', className: 'icon-btn sm', onClick: () => renderer.resetView() }),
      h('span', { class: 'spacer' }),
      followBtn,
    ),
  );
  root.appendChild(el);

  let displays = [];
  let activeId = null;
  let viewRect = { x: 0, y: 0, w: 1, h: 1 };
  let zoom = 1;
  let manual = null; // user toggled visibility explicitly
  let shown = false;
  let geom = null;
  const viewEl = h('div', { class: 'pip-view' });

  function layoutMap() {
    if (!displays.length) {
      // single display without metadata: show its aspect from the renderer
      const W = renderer.width || 16;
      const H = renderer.height || 9;
      displays = [{ id: renderer.displayId, bounds: { x: 0, y: 0, width: W, height: H }, synthetic: true }];
      activeId = renderer.displayId;
    }
    const minX = Math.min(...displays.map((d) => d.bounds.x));
    const minY = Math.min(...displays.map((d) => d.bounds.y));
    const maxX = Math.max(...displays.map((d) => d.bounds.x + d.bounds.width));
    const maxY = Math.max(...displays.map((d) => d.bounds.y + d.bounds.height));
    const s = Math.min((MAP_W - 4) / (maxX - minX || 1), (MAP_H - 4) / (maxY - minY || 1));
    const ox = (MAP_W - (maxX - minX) * s) / 2;
    const oy = (MAP_H - (maxY - minY) * s) / 2;
    geom = { s, ox, oy, minX, minY };
    replace(
      map,
      displays.map((d, i) => {
        const b = d.bounds;
        const active = d.id === activeId;
        return h('button', {
          class: `pip-display${active ? ' active' : ''}`,
          text: String(i + 1),
          attrs: { title: d.name || '' },
          style: { left: `${ox + (b.x - minX) * s}px`, top: `${oy + (b.y - minY) * s}px`, width: `${b.width * s}px`, height: `${b.height * s}px` },
          on: {
            click: (e) => {
              if (!active) return onSelectDisplay && onSelectDisplay(d.id);
              const r = e.currentTarget.getBoundingClientRect();
              renderer.centerOn((e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height);
              if (zoom <= 1.01) renderer.zoomTo(2);
            },
          },
        });
      }),
      viewEl,
    );
    placeView();
  }

  function placeView() {
    const d = displays.find((x) => x.id === activeId) || displays[0];
    if (!d || !geom) return;
    const b = d.bounds;
    const { s, ox, oy, minX, minY } = geom;
    viewEl.style.left = `${ox + (b.x - minX + viewRect.x * b.width) * s}px`;
    viewEl.style.top = `${oy + (b.y - minY + viewRect.y * b.height) * s}px`;
    viewEl.style.width = `${viewRect.w * b.width * s}px`;
    viewEl.style.height = `${viewRect.h * b.height * s}px`;
    zoomVal.textContent = `${formatNumber(zoom * 100, 0)}%`;
  }

  function autoVisible() {
    if (manual !== null) return manual;
    const real = displays.filter((d) => !d.synthetic);
    const ultrawide = renderer.width / Math.max(1, renderer.height) >= 2.1;
    return real.length > 1 || ultrawide || zoom > 1.01;
  }

  function sync() {
    const want = autoVisible();
    if (want && !shown) {
      shown = true;
      el.hidden = false;
      layoutMap();
      springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: 16 });
    } else if (!want && shown) {
      shown = false;
      springPresence(el, { show: false, preset: SPRINGS.snappy, distance: 12 }).then(() => !shown && (el.hidden = true));
    }
  }

  return {
    el,
    setDisplays(list, active) {
      displays = (list || []).map((d) => ({ ...d, bounds: d.bounds || { x: 0, y: 0, width: d.width || 1, height: d.height || 1 } }));
      activeId = active ?? (displays.find((d) => d.primary) || displays[0] || {}).id;
      if (shown) layoutMap();
      sync();
    },
    onView(rect, z) {
      viewRect = rect;
      zoom = z;
      if (shown) placeView();
      sync();
    },
    toggle() {
      manual = !shown;
      sync();
      return manual;
    },
    isFollowing: () => follow,
  };
}
