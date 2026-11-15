// Incoming connection consent sheet (host side): accept / accept & trust / deny,
// auto-deny after the countdown.
import { h } from '../../shared/js/lib/dom.js';
import { icon, osIcon } from '../../shared/js/lib/icons.js';
import { formatFingerprint, formatIdSpaced, osKey } from '../../shared/js/lib/format.js';
import { t } from '../../shared/js/i18n.js';
import { api } from '../api.js';
import { openSheet, toastError, haptic } from '../ui.js';

const DEFAULT_TIMEOUT_S = 30;
const open = new Map();

/** payload: {requestId, deviceName, deviceId, fingerprint, os, timeoutSecs?} */
export function showConsent(req) {
  const id = String(req.requestId ?? req.id);
  if (open.has(id)) return;
  haptic(30);
  let remaining = Number(req.timeoutSecs) > 0 ? Number(req.timeoutSecs) : DEFAULT_TIMEOUT_S;
  const countdown = h('span', { class: 'hint' });
  const tick = () => (countdown.textContent = t('consent.autoDeny', { seconds: remaining }));
  tick();
  let sheet;
  const respond = async (decision) => {
    clearInterval(timer);
    open.delete(id);
    sheet.close();
    try {
      await api.respondIncoming(id, decision);
    } catch (e) {
      toastError(e);
    }
  };
  const name = req.deviceName || req.peerName || formatIdSpaced(req.deviceId);
  const content = h(
    'div',
    { class: 'sheet-body-wrap' },
    h('h2', { class: 'sheet-title', id: 'consent-title', i18n: 'consent.title', i18nArgs: { name } }),
    h('p', { class: 'sheet-text', i18n: 'consent.body' }),
    h(
      'div',
      { class: 'consent-peer' },
      h('span', { class: 'list-glyph' }, osIcon(osKey(req.os), 24)),
      h('div', { class: 'list-main' }, h('span', { class: 'list-title', text: name }), req.deviceId && h('span', { class: 'list-sub num', text: formatIdSpaced(req.deviceId) }), h('span', { class: 'fingerprint mono', text: formatFingerprint(req.fingerprint) })),
    ),
    h(
      'div',
      { class: 'sheet-actions' },
      h('button', { class: 'btn primary lg', attrs: { type: 'button' }, i18n: 'consent.accept', on: { click: () => respond('accept') } }),
      h('button', { class: 'btn lg', attrs: { type: 'button' }, on: { click: () => respond('acceptAndTrust') } }, icon('shieldCheck', 16), h('span', { i18n: 'consent.acceptTrust' })),
      h('button', { class: 'btn ghost lg', attrs: { type: 'button' }, i18n: 'consent.deny', on: { click: () => respond('deny') } }),
    ),
    h('div', { class: 'center' }, countdown),
  );
  sheet = openSheet({ content, labelledBy: 'consent-title', dismissible: false });
  open.set(id, sheet);
  const timer = setInterval(() => {
    remaining -= 1;
    tick();
    if (remaining <= 0) respond('deny');
  }, 1000);
}
