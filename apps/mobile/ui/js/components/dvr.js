// Time-Travel scrubber (mobile): bottom glass panel with keyframe ticks, event
// marks, drag-to-scrub, pause, OCR and a LIVE snap-back pill.
import { h, replace, throttle } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { springPresence, SPRINGS } from '../../shared/js/lib/spring.js';
import { formatClock } from '../../shared/js/lib/format.js';
import { t } from '../../shared/js/i18n.js';
import { api, describeError } from '../api.js';
import { iconButton, openSheet, copyWithToast, toastError, haptic } from '../ui.js';

export function createDvr({ root, sessionId, onVisibility }) {
  const marksLayer = h('div', { class: 'tl-marks' });
  const played = h('div', { class: 'tl-played' });
  const playhead = h('div', { class: 'tl-head' });
  const timeline = h('div', { class: 'timeline', attrs: { role: 'slider', 'aria-valuemin': '0', 'aria-valuemax': '100' }, i18nAttr: { 'aria-label': 'dvr.timeline' } }, marksLayer, played, playhead);
  const time = h('span', { class: 'dvr-time num' });
  const livePill = h('button', { class: 'live-pill is-live', attrs: { type: 'button' }, on: { click: () => goLive() } });
  const playBtn = iconButton({ name: 'pause', labelKey: 'dvr.pause', onClick: () => (live ? pause() : goLive()) });
  const el = h(
    'section',
    { class: 'dvr glass', hidden: true, attrs: { role: 'region' }, i18nAttr: { 'aria-label': 'dvr.title' } },
    h(
      'div',
      { class: 'dvr-top' },
      icon('history', 16),
      h('span', { class: 'dvr-title', i18n: 'dvr.title' }),
      h('span', { class: 'spacer' }),
      playBtn,
      iconButton({ name: 'scanText', labelKey: 'dvr.ocr', onClick: () => ocr() }),
      livePill,
      iconButton({ name: 'close', labelKey: 'common.close', onClick: () => hide() }),
    ),
    timeline,
    time,
  );
  root.appendChild(el);

  let index = null;
  let live = true;
  let pos = 0;
  let shown = false;
  let poll = 0;
  const span = () => (index ? Math.max(1, index.endUs - index.startUs) : 1);
  const frac = (pts) => (index ? Math.min(1, Math.max(0, (pts - index.startUs) / span())) : 1);

  function renderMarks() {
    if (!index) return;
    const kfs = index.keyframes || [];
    const step = Math.max(1, Math.ceil(kfs.length / 80));
    replace(
      marksLayer,
      kfs.filter((_, i) => i % step === 0).map((k) => h('div', { class: 'tick', style: { left: `${(frac(k) * 100).toFixed(3)}%` } })),
      (index.marks || []).map((m) => h('div', { class: `mark k-${m.kind}`, style: { left: `${(frac(m.ptsUs) * 100).toFixed(3)}%` }, attrs: { title: [t(`dvr.kinds.${m.kind}`), m.label].filter(Boolean).join(' — ') } })),
    );
  }

  function renderHead() {
    const f = live ? 1 : frac(pos);
    playhead.style.left = `${(f * 100).toFixed(3)}%`;
    played.style.width = `${(f * 100).toFixed(3)}%`;
    timeline.setAttribute('aria-valuenow', String(Math.round(f * 100)));
    time.textContent = live ? t('dvr.windowLength', { duration: formatClock(span() / 1e6) }) : t('dvr.behindLive', { time: formatClock(index ? (index.endUs - pos) / 1e6 : 0) });
    livePill.classList.toggle('is-live', live);
    replace(livePill, h('span', { class: 'dot' }), h('span', { i18n: live ? 'dvr.live' : 'dvr.backToLive' }));
    replace(playBtn, icon(live ? 'pause' : 'play', 20));
    const k = live ? 'dvr.pause' : 'dvr.resume';
    playBtn.dataset.i18nAttr = `title:${k};aria-label:${k}`;
    playBtn.setAttribute('aria-label', t(k));
  }

  async function refresh() {
    try {
      index = await api.dvrIndex(sessionId);
      renderMarks();
      renderHead();
    } catch (e) {
      replace(marksLayer, h('div', { class: 'hint', text: describeError(e) }));
    }
  }

  const seek = throttle((pts) => api.dvrSeek(sessionId, pts).catch(toastError), 120);
  const ptsAt = (x) => {
    const r = timeline.getBoundingClientRect();
    return index ? index.startUs + Math.min(1, Math.max(0, (x - r.left) / r.width)) * span() : 0;
  };
  timeline.addEventListener('pointerdown', (e) => {
    if (!index) return;
    e.preventDefault();
    timeline.setPointerCapture(e.pointerId);
    haptic();
    live = false;
    const step = (ev) => {
      pos = ptsAt(ev.clientX);
      renderHead();
      seek(pos);
    };
    step(e);
    timeline.onpointermove = step;
    timeline.onpointerup = () => (timeline.onpointermove = null);
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
      let sheet;
      const content = h(
        'div',
        { class: 'sheet-body-wrap' },
        h('h2', { class: 'sheet-title', id: 'ocr-title', i18n: 'dvr.ocrTitle' }),
        h('div', { class: 'ocr-text', text: text || t('dvr.ocrEmpty') }),
        h(
          'div',
          { class: 'sheet-actions' },
          h('button', { class: 'btn lg primary', attrs: { type: 'button' }, on: { click: () => copyWithToast(String(text || '')) } }, icon('copy', 16), h('span', { i18n: 'common.copy' })),
          h('button', { class: 'btn lg ghost', attrs: { type: 'button' }, i18n: 'common.done', on: { click: () => sheet.close() } }),
        ),
      );
      sheet = openSheet({ content, labelledBy: 'ocr-title' });
    } catch (e) {
      toastError(e);
    }
  }

  async function show() {
    if (shown) return;
    shown = true;
    el.hidden = false;
    springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: 40 });
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
    await springPresence(el, { show: false, preset: SPRINGS.snappy, distance: 40 });
    if (!shown) el.hidden = true;
  }

  return { show, hide, toggle: () => (shown ? hide() : show()), isOpen: () => shown, el, dispose: () => clearInterval(poll) };
}
