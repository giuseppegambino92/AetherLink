// Connection handshake overlay with a spring-driven luminous ring.
// Each state advance "kicks" the ring; on success it blooms outward and fades.
import { h, replace } from '../lib/dom.js';
import { Spring, SPRINGS, springTo, reducedMotion } from '../lib/spring.js';

const STAGES = { connecting: 0.33, authenticating: 0.66, connected: 1 };

export function createHandshake({ root, peerName }) {
  const arc = h('svg:circle', {
    attrs: { cx: 60, cy: 60, r: 44, fill: 'none', stroke: 'url(#hsGrad)', 'stroke-width': 3, 'stroke-linecap': 'round', 'stroke-dasharray': '276.46', 'stroke-dashoffset': '276.46', transform: 'rotate(-90 60 60)' },
  });
  const orbit = h('svg:g', {}, h('svg:circle', { attrs: { cx: 60, cy: 16, r: 3.2, fill: '#e9f4ff' } }));
  const ring = h(
    'svg:svg',
    { class: 'handshake-ring', attrs: { viewBox: '0 0 120 120', 'aria-hidden': 'true' } },
    h('svg:defs', {}, h('svg:linearGradient', { attrs: { id: 'hsGrad', x1: '0', y1: '0', x2: '1', y2: '1' } }, h('svg:stop', { attrs: { offset: '0', 'stop-color': '#7de0d6' } }), h('svg:stop', { attrs: { offset: '0.5', 'stop-color': '#8ab8ff' } }), h('svg:stop', { attrs: { offset: '1', 'stop-color': '#a597ff' } }))),
    h('svg:circle', { attrs: { cx: 60, cy: 60, r: 44, fill: 'none', stroke: 'rgba(255,255,255,0.07)', 'stroke-width': 3 } }),
    h('svg:ellipse', { attrs: { cx: 60, cy: 60, rx: 30, ry: 15, fill: 'none', stroke: 'rgba(138,184,255,0.35)', 'stroke-width': 1.5, transform: 'rotate(-32 60 60)' } }),
    h('svg:ellipse', { attrs: { cx: 60, cy: 60, rx: 30, ry: 15, fill: 'none', stroke: 'rgba(125,224,214,0.35)', 'stroke-width': 1.5, transform: 'rotate(32 60 60)' } }),
    arc,
    orbit,
  );
  const title = h('div', { class: 'handshake-title' });
  const sub = h('div', { class: 'handshake-sub' });
  const actions = h('div', { class: 'row' });
  const el = h('div', { class: 'handshake' }, h('div', { class: 'handshake-inner' }, ring, title, sub, actions));
  root.appendChild(el);

  // progress spring for the arc; "kick" spring for scale
  const C = 276.46;
  const progress = new Spring({ ...SPRINGS.gentle, value: 0, onUpdate: (v) => arc.setAttribute('stroke-dashoffset', (C * (1 - v)).toFixed(2)) });
  const kick = new Spring({ ...SPRINGS.bouncy, value: 1, onUpdate: (v) => (ring.style.transform = `scale(${v.toFixed(4)})`) });

  // continuous orbit while waiting
  let raf = 0;
  const t0 = performance.now();
  const spin = (now) => {
    orbit.setAttribute('transform', `rotate(${(((now - t0) / 1000) * 140) % 360} 60 60)`);
    raf = requestAnimationFrame(spin);
  };
  if (!reducedMotion()) raf = requestAnimationFrame(spin);

  function setStage(stage, { titleKey, subKey, args, actionsNodes } = {}) {
    if (STAGES[stage] !== undefined) {
      progress.to(STAGES[stage]);
      kick.to(1, 3.2); // velocity impulse → springy pulse
    }
    replace(title, h('span', { i18n: titleKey || `session.state.${stage}`, i18nArgs: { name: peerName, ...(args || {}) } }));
    replace(sub, subKey ? h('span', { i18n: subKey, i18nArgs: { name: peerName, ...(args || {}) } }) : null);
    replace(actions, actionsNodes || null);
  }

  function showError(messageText, actionsNodes) {
    cancelAnimationFrame(raf);
    arc.setAttribute('stroke', '#ff8089');
    kick.to(1, -2.5);
    replace(title, h('span', { i18n: 'session.state.failed' }));
    replace(sub, h('span', { text: messageText }));
    replace(actions, actionsNodes || null);
    el.hidden = false;
    el.style.opacity = '1';
  }

  async function complete() {
    progress.to(1);
    cancelAnimationFrame(raf);
    await springTo({
      from: 0,
      to: 1,
      preset: SPRINGS.slow,
      onUpdate: (v) => {
        ring.style.transform = `scale(${(1 + v * 0.9).toFixed(4)})`;
        el.style.opacity = Math.max(0, 1 - v * 1.15).toFixed(3);
      },
    });
    el.hidden = true;
  }

  function reset() {
    arc.setAttribute('stroke', 'url(#hsGrad)');
    el.hidden = false;
    el.style.opacity = '1';
    ring.style.transform = 'scale(1)';
    progress.jump(0);
    if (!reducedMotion()) {
      cancelAnimationFrame(raf);
      raf = requestAnimationFrame(spin);
    }
  }

  return { el, setStage, showError, complete, reset };
}
