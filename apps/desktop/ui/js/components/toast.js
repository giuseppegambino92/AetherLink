// Transient notifications. Messages are either i18n keys or pre-translated
// strings produced by describeError()/t().
import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { springPresence, SPRINGS } from '../lib/spring.js';
import { t } from '../i18n.js';
import { describeError } from '../api.js';

let host = null;

function ensureHost() {
  if (!host) {
    host = h('div', { class: 'toasts', attrs: { 'aria-live': 'polite', role: 'status' } });
    document.body.appendChild(host);
  }
  return host;
}

/** @param {{key?:string, args?:object, message?:string, kind?:'info'|'success'|'error', timeout?:number}} opts */
export function toast({ key, args, message, kind = 'info', timeout = 3600 }) {
  const glyph = kind === 'error' ? 'alert' : kind === 'success' ? 'check' : 'info';
  const label = key ? h('span', { i18n: key, i18nArgs: args }) : h('span', { text: message ?? '' });
  const el = h('div', { class: `toast ${kind}` }, icon(glyph, 16), label);
  ensureHost().appendChild(el);
  springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: 14 });
  setTimeout(async () => {
    await springPresence(el, { show: false, preset: SPRINGS.snappy, distance: 10 });
    el.remove();
  }, timeout);
}

export const toastError = (err) => toast({ message: describeError(err), kind: 'error', timeout: 5200 });
export const toastSuccess = (key, args) => toast({ key, args, kind: 'success' });
export const toastInfo = (key, args) => toast({ key, args, kind: 'info' });

/** Copy text through the shell clipboard and confirm with a toast. */
export async function copyWithToast(api, text, key = 'common.copied') {
  try {
    await api.copyText(text);
    toastSuccess(key);
  } catch (e) {
    toast({ message: t('errors.copyFailed'), kind: 'error' });
  }
}
