// Boot: i18n → Tauri adapter → hash router with a bottom tab bar.
//   #/home        connect (ID, QR, LAN peers, trusted devices)
//   #/host        share this device
//   #/controller  Bluetooth HID controller (Android only)
//   #/session     full-screen viewport (no tab bar)
import { h, replace } from '../shared/js/lib/dom.js';
import { icon } from '../shared/js/lib/icons.js';
import { initI18n, apply, t, getLocale, setLocale } from '../shared/js/i18n.js';
import { initApi, api, on } from './api.js';
import { mountHome } from './views/home.js';
import { mountHost } from './views/host.js';
import { mountController } from './views/controller.js';
import { mountSession } from './views/session.js';
import { showConsent } from './components/consent.js';
import { haptic } from './ui.js';

let platform = { os: 'desktop', canHost: false, hostKind: 'none', canBeControlled: false, hidController: false };
let unmount = null;
const view = h('main', { class: 'view' });
// Created in boot() once the locale is loaded (i18n attributes are resolved at creation).
let tabs = null;

function tab(route, iconName, labelKey) {
  return h(
    'button',
    { class: 'tab', attrs: { type: 'button', role: 'tab' }, dataset: { route }, on: { click: () => (haptic(), (location.hash = `#/${route}`)) } },
    icon(iconName, 22),
    h('span', { i18n: labelKey }),
  );
}

function renderTabs() {
  replace(tabs, tab('home', 'link', 'nav.connect'), platform.canHost && tab('host', 'upload', 'nav.share'), platform.hidController && tab('controller', 'keyboard', 'nav.controller'));
}

function route() {
  const name = (location.hash.replace(/^#\/?/, '').split(/[?/]/)[0] || 'home').toLowerCase();
  if (unmount) unmount();
  unmount = null;
  replace(view);
  view.scrollTop = 0;
  const inSession = name === 'session';
  tabs.hidden = inSession;
  for (const b of tabs.children) b.setAttribute('aria-selected', String(b.dataset.route === name));
  switch (name) {
    case 'session':
      unmount = mountSession(view);
      break;
    case 'host':
      unmount = mountHost(view, platform);
      break;
    case 'controller':
      unmount = mountController(view, platform);
      break;
    default:
      unmount = mountHome(view);
  }
}

async function boot() {
  await initI18n();
  tabs = h('nav', { class: 'tabbar glass', attrs: { role: 'tablist' }, i18nAttr: { 'aria-label': 'nav.label' } });
  apply(document);
  await initApi();
  const root = document.getElementById('app');
  replace(root, view, tabs);
  try {
    platform = { ...platform, ...(await api.platform()) };
  } catch {
    /* desktop dev build without mobile commands */
  }
  document.documentElement.dataset.os = platform.os;
  // Engine language preference wins over the browser guess.
  try {
    const s = await api.getSettings();
    if ((s.language === 'it' || s.language === 'en') && s.language !== getLocale()) await setLocale(s.language);
  } catch {
    /* engine not started yet */
  }
  api.uiSetLocale(getLocale()).catch(() => {});
  on('locale-changed', (p) => p && p.locale && p.locale !== getLocale() && setLocale(p.locale));
  on('incoming-request', (req) => showConsent(req));
  renderTabs();
  addEventListener('hashchange', route);
  route();
}

boot().catch((e) => {
  console.error('[aetherlink] boot failed', e);
  const root = document.getElementById('app');
  if (!root) return;
  replace(root, h('div', { class: 'boot-error' }, h('h2', { text: t('errors.bootFailed') }), h('pre', { class: 'mono', text: String(e && e.stack ? e.stack : e) })));
});
