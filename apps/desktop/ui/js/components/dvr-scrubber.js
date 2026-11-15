// Time-Travel DVR scrubber: keyframe ticks, event marks, pause, scrub, OCR
// and a "LIVE" snap-back pill.
import { h, replace, throttle } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, describeError } from '../api.js';
import { iconButton } from './controls.js';
import { springPresence, SPRINGS } from '../lib/spring.js';
import { openSheet } from './sheet.js';
import { copyWithToast, toastError } from './toast.js';
import { formatClock } from '../lib/format.js';
import { t } from '../i18n.js';

export function createDvr({ root, sessionId, onVisibility }) {
  const timeline = h('div', { class: 'timeline', attrs: { role: 'slider', tabindex: '0', 'aria-valuemin': '0', 'aria-valuemax': '100' }, i18nAttr: { 'aria-label': 'dvr.timeline' } });
  const played = h('div', { class: 'played' });
  const playhead = h('div', { class: 'playhead' });
  const marksLayer = h('div');
  timeline.append(marksLayer, played, playhead);
  const time = h('span', { class: 'dvr-time' });
  const liveDot = h('span', { class: 'dot' });
  const livePill = h('button', { class: 'live-pill is-live', on: { click: () => goLive() } }, liveDot, h('span', { i18n: 'dvr.live' }));
  const playBtn = iconButton({ name: 'pause', labelKey: 'dvr.pause', onClick: () => (live ? pause() : goLive()) });
  const el = h(
    'div',
    { class: 'dvr', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'dvr.title' } },
    h(
      'div',
      { class: 'dvr-top' },
      icon('history', 16),
      h('span', { class: 'row-title', i18n: 'dvr.title' }),
      time,
      h('span', { class: 'spacer' }),
      playBtn,
      h('button', { class: 'btn sm', on: { click: () => ocr() } }, icon('scanText', 14), h('span', { i18n: 'dvr.ocr' })),
      livePill,
      iconButton({ name: 'close', labelKey: 'common.close', onClick: () => hide() }),
    ),
    timeline,
  );
  el.dataset.baseTransform = 'translateX(-50%)';
  el.style.transform = 'translateX(-50%)';
  root.appendChild(el);
  el.addEventListener('keydown', (e) => e.key === 'Escape' && (e.stopPropagation(), hide()));

  let index = null;
  let live = true;
  let pos = 0; // pts_us when not live
  let shown = false;
  let poll = 0;

  const span = () => (index ? Math.max(1, index.endUs - index.startUs) : 1);
  const frac = (pts) => (index ? Math.min(1, Math.max(0, (pts - index.startUs) / span())) : 1);

  function renderMarks() {
    if (!index) return;
    const kfs = index.keyframes || [];
    // thin out ticks so dense GOPs stay readable
    const step = Math.max(1, Math.ceil(kfs.length / 160));
    const ticks = kfs.filter((_, i) => i % step === 0).map((k) => h('div', { class: 'tick', style: { left: `${(frac(k) * 100).toFixed(3)}%` } }));
    const marks = (index.marks || []).map((m) =>
      h('div', {
        class: `mark k-${m.kind}`,
        style: { left: `${(frac(m.ptsUs) * 100).toFixed(3)}%` },
        attrs: { title: [t(`dvr.kinds.${m.kind}`), m.label].filter(Boolean).join(' — ') },
      }),
    );
    replace(marksLayer, ticks, marks);
  }

  function renderHead() {
    const f = live ? 1 : frac(pos);
    playhead.style.left = `${(f * 100).toFixed(3)}%`;
    played.style.width = `${(f * 100).toFixed(3)}%`;
    timeline.setAttribute('aria-valuenow', String(Math.round(f * 100)));
    const behind = live || !index ? 0 : (index.endUs - pos) / 1e6;
    time.textContent = live ? t('dvr.windowLength', { duration: formatClock(span() / 1e6) }) : t('dvr.behindLive', { time: formatClock(behind) });
    livePill.classList.toggle('is-live', live);
    replace(livePill, liveDot, h('span', { i18n: live ? 'dvr.live' : 'dvr.backToLive' }));
    replace(playBtn, icon(live ? 'pause' : 'play', 16));
    playBtn.setAttribute('title', t(live ? 'dvr.pause' : 'dvr.resume'));
    playBtn.setAttribute('aria-label', t(live ? 'dvr.pause' : 'dvr.resume'));
    playBtn.dataset.i18nAttr = `title:${live ? 'dvr.pause' : 'dvr.resume'};aria-label:${live ? 'dvr.pause' : 'dvr.resume'}`;
  }

  async function refresh() {
    try {
      index = await api.dvrIndex(sessionId);
      renderMarks();
      renderHead();
    } catch (e) {
      replace(marksLayer, h('div', { class: 'hint', style: { padding: '12px' }, text: describeError(e) }));
    }
  }

  const seek = throttle((pts) => api.dvrSeek(sessionId, pts).catch(toastError), 120);

  function ptsAt(clientX) {
    const r = timeline.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (clientX - r.left) / r.width));
    return index ? index.startUs + f * span() : 0;
  }

  timeline.addEventListener('pointerdown', (e) => {
    if (!index) return;
    timeline.setPointerCapture(e.pointerId);
    live = false;
    pos = ptsAt(e.clientX);
    renderHead();
    seek(pos);
    const move = (ev) => {
      pos = ptsAt(ev.clientX);
      renderHead();
      seek(pos);
    };
    const up = () => {
      timeline.removeEventListener('pointermove', move);
      timeline.removeEventListener('pointerup', up);
    };
    timeline.addEventListener('pointermove', move);
    timeline.addEventListener('pointerup', up);
  });
  timeline.addEventListener('keydown', (e) => {
    if (!index) return;
    if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
      e.preventDefault();
      e.stopPropagation();
      live = false;
      const base = pos || index.endUs;
      pos = Math.min(index.endUs, Math.max(index.startUs, base + (e.key === 'ArrowLeft' ? -2e6 : 2e6)));
      renderHead();
      seek(pos);
    }
  });

  async function pause() {
    if (!index) await refresh();
    if (!index) return;
    live = false;
    pos = index.endUs;
    renderHead();
    api.dvrSeek(sessionId, pos).catch(toastError);
  }

  async function goLive() {
    live = true;
    pos = 0;
    renderHead();
    try {
      await api.dvrLive(sessionId);
    } catch (e) {
      toastError(e);
    }
  }

  async function ocr() {
    try {
      const text = await api.dvrOcr(sessionId, live ? null : pos);
      const content = h(
        'div',
        {},
        h('h2', { class: 'sheet-title', id: 'ocr-title', i18n: 'dvr.ocrTitle' }),
        h('div', { class: 'ocr-text inset', text: text || t('dvr.ocrEmpty') }),
        h('div', { class: 'sheet-actions inline' }, h('button', { class: 'btn', on: { click: () => copyWithToast(api, String(text || '')) } }, icon('copy', 14), h('span', { i18n: 'common.copy' })), h('button', { class: 'btn primary', on: { click: () => sheet.close() } }, h('span', { i18n: 'common.done' }))),
      );
      const sheet = openSheet({ content, labelledBy: 'ocr-title' });
    } catch (e) {
      toastError(e);
    }
  }

  async function show() {
    if (shown) return;
    shown = true;
    el.hidden = false;
    springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: 30 });
    onVisibility && onVisibility(true);
    renderHead();
    await refresh();
    poll = setInterval(() => live && refresh(), 3000);
  }

  async function hide() {
    if (!shown) return;
    shown = false;
    clearInterval(poll);
    onVisibility && onVisibility(false);
    if (!live) goLive();
    await springPresence(el, { show: false, preset: SPRINGS.snappy, distance: 30 });
    if (!shown) el.hidden = true;
  }

  return { show, hide, toggle: () => (shown ? hide() : show()), isOpen: () => shown, el };
}
