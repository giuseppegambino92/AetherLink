// Incoming connection consent sheet (host side): peer name, fingerprint,
// accept / accept & trust / deny, with an auto-deny countdown.
import { h } from '../lib/dom.js';
import { icon, osIcon } from '../lib/icons.js';
import { api } from '../api.js';
import { openSheet } from './sheet.js';
import { toastError } from './toast.js';
import { formatFingerprint, formatIdSpaced, osKey } from '../lib/format.js';
import { t } from '../i18n.js';

const DEFAULT_TIMEOUT_S = 30;
const open = new Map();

/** payload: {requestId, deviceName, deviceId, fingerprint, os, timeoutSecs?} */
export function showConsentSheet(req) {
  const id = String(req.requestId ?? req.id);
  if (open.has(id)) return;
  let remaining = Number(req.timeoutSecs) > 0 ? Number(req.timeoutSecs) : DEFAULT_TIMEOUT_S;
  const countdown = h('span', { class: 'hint' });
  const renderCountdown = () => (countdown.textContent = t('consent.autoDeny', { seconds: remaining }));
  renderCountdown();

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
    {},
    h('div', { class: 'row', style: { gap: '10px', marginBottom: '6px' } }, h('span', { class: 'chip accent' }, icon('shieldCheck', 13), h('span', { i18n: 'consent.badge' }))),
    h('h2', { class: 'sheet-title', id: 'consent-title', i18n: 'consent.title', i18nArgs: { name } }),
    h('p', { class: 'sheet-body', i18n: 'consent.body' }),
    h(
      'div',
      { class: 'consent-peer inset' },
      h('div', { class: 'consent-avatar' }, osIcon(osKey(req.os), 24)),
      h(
        'div',
        { class: 'spacer', style: { minWidth: '0' } },
        h('div', { class: 'row-title', text: name }),
        req.deviceId && h('div', { class: 'row-sub mono', text: formatIdSpaced(req.deviceId) }),
        h('div', { class: 'row', style: { gap: '6px', marginTop: '8px' } }, icon('fingerprint', 14), h('span', { class: 'fingerprint', text: formatFingerprint(req.fingerprint) || '—' })),
      ),
    ),
    h('p', { class: 'hint', i18n: 'consent.verifyHint' }),
    h(
      'div',
      { class: 'sheet-actions' },
      h('button', { class: 'btn primary lg', attrs: { autofocus: true }, on: { click: () => respond('accept') } }, h('span', { i18n: 'consent.accept' })),
      h('button', { class: 'btn lg', on: { click: () => respond('acceptAndTrust') } }, icon('shieldCheck', 16), h('span', { i18n: 'consent.acceptTrust' })),
      h('button', { class: 'btn ghost lg', on: { click: () => respond('deny') } }, h('span', { i18n: 'consent.deny' })),
    ),
    h('div', { class: 'row', style: { justifyContent: 'center', marginTop: '12px' } }, countdown),
  );
  sheet = openSheet({ content, labelledBy: 'consent-title', dismissible: false });
  open.set(id, sheet);
  const timer = setInterval(() => {
    remaining -= 1;
    renderCountdown();
    if (remaining <= 0) respond('deny');
  }, 1000);
}

/** Engine may cancel a pending request (peer gave up / answered elsewhere). */
export function dismissConsent(requestId) {
  const s = open.get(String(requestId));
  if (s) {
    s.close();
    open.delete(String(requestId));
  }
}
