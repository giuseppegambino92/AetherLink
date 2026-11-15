// LAN Radar: animated sweep with discovered peers as blips. Each blip's
// pulse is phase-locked to the sweep (animation-delay derived from its angle).
import { h, replace } from '../lib/dom.js';
import { osIcon } from '../lib/icons.js';
import { api, on, isNotWired } from '../api.js';
import { card, emptyState } from './controls.js';
import { formatIdSpaced, osKey } from '../lib/format.js';
import { openSession } from '../sessions.js';
import { tp, onLocaleChange } from '../i18n.js';

const SWEEP_S = 4;

function hash(str) {
  let x = 2166136261;
  for (const ch of String(str)) {
    x ^= ch.charCodeAt(0);
    x = Math.imul(x, 16777619);
  }
  return x >>> 0;
}

export function lanRadarCard({ className = 'span-5' } = {}) {
  const radar = h(
    'div',
    { class: 'radar', attrs: { role: 'img' }, i18nAttr: { 'aria-label': 'dashboard.radar.aria' } },
    h('div', { class: 'ring', style: { '--inset': '18%' } }),
    h('div', { class: 'ring', style: { '--inset': '34%' } }),
    h('div', { class: 'ring', style: { '--inset': '2%' } }),
    h('div', { class: 'axis h' }),
    h('div', { class: 'axis v' }),
    h('div', { class: 'sweep' }),
    h('div', { class: 'core' }),
  );
  const blips = h('div');
  radar.appendChild(blips);
  const list = h('div', { class: 'peer-list' });
  const count = h('span', { class: 'chip accent' });
  let peers = [];

  const highlight = (id, onOff) => {
    for (const el of radar.querySelectorAll(`[data-peer="${CSS.escape(String(id))}"]`)) el.classList.toggle('active', onOff);
    for (const el of list.querySelectorAll(`[data-peer="${CSS.escape(String(id))}"]`)) el.classList.toggle('active', onOff);
  };

  function render() {
    count.textContent = tp('dashboard.radar.found', peers.length);
    replace(
      blips,
      peers.map((p, i) => {
        const hsh = hash(p.deviceId ?? p.deviceName ?? i);
        const angle = hsh % 360;
        const radius = 22 + ((hsh >>> 9) % 60) * 0.42; // % of radar radius, 22..47
        const rad = ((angle - 90) * Math.PI) / 180;
        const x = 50 + Math.cos(rad) * radius;
        const y = 50 + Math.sin(rad) * radius;
        return h('button', {
          class: 'blip',
          dataset: { peer: String(p.deviceId) },
          style: { left: `${x}%`, top: `${y}%`, animationDelay: `${-((SWEEP_S * (360 - angle)) / 360).toFixed(2)}s` },
          attrs: { title: p.deviceName || '' },
          on: {
            click: () => connectTo(p),
            pointerenter: () => highlight(p.deviceId, true),
            pointerleave: () => highlight(p.deviceId, false),
          },
        });
      }),
    );
    if (!peers.length) {
      replace(list, emptyState({ iconName: 'wifi', titleKey: 'dashboard.radar.emptyTitle', bodyKey: 'dashboard.radar.emptyBody' }));
      return;
    }
    replace(
      list,
      peers.map((p) =>
        h(
          'div',
          {
            class: 'peer-card',
            dataset: { peer: String(p.deviceId) },
            on: { pointerenter: () => highlight(p.deviceId, true), pointerleave: () => highlight(p.deviceId, false) },
          },
          h('div', { class: 'row-glyph' }, osIcon(osKey(p.os), 16)),
          h('div', { class: 'row-main', style: { flex: '1', minWidth: '0' } }, h('div', { class: 'row-title', text: p.deviceName || formatIdSpaced(p.deviceId) }), h('div', { class: 'row-sub mono', text: [p.address, p.deviceId && formatIdSpaced(p.deviceId)].filter(Boolean).join(' · ') })),
          h('button', { class: 'btn sm', on: { click: () => connectTo(p) } }, h('span', { i18n: 'common.connect' })),
        ),
      ),
    );
  }

  function connectTo(p) {
    openSession({ target: String(p.deviceId ?? p.id ?? p.address), title: p.deviceName || formatIdSpaced(p.deviceId) });
  }

  const body = h('div', { class: 'radar-wrap' }, radar, list);
  const el = card({ className: `radar-card ${className}`, titleKey: 'dashboard.radar.title', subtitleKey: 'dashboard.radar.subtitle', iconName: 'radar', iconTone: 'aqua', actions: [count] }, body);

  api.listLanPeers().then(
    (p) => {
      peers = Array.isArray(p) ? p : [];
      render();
    },
    (e) => {
      count.hidden = true;
      replace(
        body,
        h(
          'div',
          { style: { gridColumn: '1 / -1' } },
          emptyState({ iconName: isNotWired(e) ? 'plug' : 'alert', titleKey: isNotWired(e) ? 'engine.notWiredTitle' : 'dashboard.radar.unavailable', bodyKey: isNotWired(e) ? 'engine.notWiredBody' : 'errors.tryAgain' }),
        ),
      );
    },
  );
  const off = on('lan-peers-changed', (p) => {
    peers = Array.isArray(p) ? p : Array.isArray(p?.peers) ? p.peers : peers;
    if (!body.contains(radar)) replace(body, radar, list);
    count.hidden = false;
    render();
  });
  const offLocale = onLocaleChange(() => (count.textContent = tp('dashboard.radar.found', peers.length)));
  el.dispose = () => (off(), offLocale());
  return el;
}

