// Main dashboard window: sidebar navigation + sections.
import { h, replace } from '../lib/dom.js';
import { icon, brandMark } from '../lib/icons.js';
import { api, on, isNotWired } from '../api.js';
import { navigate, onRoute, parseRoute, SECTIONS } from '../router.js';
import { identityCard } from '../components/identity-card.js';
import { quickConnectCard } from '../components/quick-connect.js';
import { lanRadarCard } from '../components/lan-radar.js';
import { devicesGrid } from '../components/devices-grid.js';
import { hostSessionsCard } from '../components/host-sessions.js';
import { hidControllerSection } from '../components/hid-controller.js';
import { mountsCard, localSharesCard } from '../components/drives-manager.js';
import { permissionsCard, preferencesCard, hotkeysCard } from '../components/settings-panel.js';
import { showConsentSheet, dismissConsent } from '../components/consent-sheet.js';
import { openPalette } from '../components/palette.js';
import { hostStore, trayStore } from '../stores.js';
import { openSession, recentTargets } from '../sessions.js';
import { formatIdSpaced, idDigits, isValidId, osKey } from '../lib/format.js';
import { osIcon } from '../lib/icons.js';
import { t, tp, onLocaleChange } from '../i18n.js';
import { toastError } from '../components/toast.js';

const NAV = [
  { name: 'home', icon: 'home', key: 'nav.home' },
  { name: 'devices', icon: 'devices', key: 'nav.devices' },
  { name: 'drives', icon: 'drive', key: 'nav.drives' },
  { name: 'iphone', icon: 'osIos', key: 'nav.iphone' },
  { name: 'settings', icon: 'settings', key: 'nav.settings' },
];

export function mountDashboard(root) {
  const navButtons = NAV.map((n) =>
    h('button', { class: 'nav-item', dataset: { route: n.name }, on: { click: () => navigate(n.name) } }, icon(n.icon, 18), h('span', { i18n: n.key })),
  );
  const engineChip = h('div', { class: 'chip' });
  const connChip = h('div', { class: 'chip' });
  const sidebar = h(
    'aside',
    { class: 'sidebar', attrs: { 'data-tauri-drag-region': true } },
    h('div', { class: 'brand', attrs: { 'data-tauri-drag-region': true } }, h('div', { class: 'brand-mark' }, brandMark(20)), h('div', {}, h('div', { class: 'brand-name', i18n: 'app.name' }), h('div', { class: 'brand-sub', i18n: 'app.tagline' }))),
    h('nav', { class: 'nav', i18nAttr: { 'aria-label': 'nav.aria' } }, navButtons),
    h('div', { class: 'sidebar-footer' }, connChip, engineChip),
  );
  const content = h('main', { class: 'content', attrs: { tabindex: '-1' } });
  const titlebar = h(
    'div',
    { class: 'titlebar', attrs: { 'data-tauri-drag-region': true } },
    h('button', { class: 'btn sm ghost', on: { click: () => openQuickConnectPalette() }, i18nAttr: { title: 'dashboard.palette.open' } }, icon('search', 14), h('span', { i18n: 'dashboard.palette.button' }), h('span', { class: 'kbd', text: '⌘K' })),
  );
  replace(root, h('div', { class: 'app-shell' }, sidebar, content), titlebar);

  // ---- footer status chips
  const renderStatus = ({ status, error }) => {
    if (error) {
      engineChip.className = `chip ${isNotWired(error) ? 'warning' : 'danger'}`;
      replace(engineChip, h('span', { class: 'dot busy' }), h('span', { i18n: isNotWired(error) ? 'engine.chipNotWired' : 'engine.chipError' }));
      connChip.hidden = true;
      trayStore.set({ activeConnections: 0 });
      return;
    }
    if (!status) {
      replace(engineChip, h('span', { class: 'dot' }), h('span', { i18n: 'engine.chipStarting' }));
      return;
    }
    engineChip.className = `chip ${status.listening === false ? '' : 'success'}`;
    replace(engineChip, h('span', { class: `dot ${status.listening === false ? '' : 'online'}` }), h('span', { i18n: status.listening === false ? 'engine.chipPaused' : 'engine.chipReady' }));
    const n = Array.isArray(status.sessions) ? status.sessions.length : 0;
    connChip.hidden = n === 0;
    connChip.className = 'chip accent';
    connChip.textContent = tp('dashboard.host.activeCount', n);
    trayStore.set({ activeConnections: n });
  };
  hostStore.subscribe(renderStatus);
  onLocaleChange(() => renderStatus(hostStore.get()));
  hostStore.start();

  // ---- sections
  let disposers = [];
  const track = (el) => {
    if (el && el.dispose) disposers.push(el.dispose);
    return el;
  };

  const header = (titleKey, subtitleKey, ...extra) =>
    h('header', { class: 'page-header' }, h('div', {}, h('h1', { class: 'page-title', i18n: titleKey }), h('p', { class: 'page-subtitle', i18n: subtitleKey })), ...extra);

  let quick = null;
  const sections = {
    home() {
      quick = quickConnectCard();
      const devicesPreview = h(
        'section',
        { class: 'span-12' },
        h('div', { class: 'row', style: { margin: '8px 4px 14px' } }, h('div', { class: 'section-label spacer', i18n: 'dashboard.devices.title' }), h('button', { class: 'btn sm ghost', on: { click: () => navigate('devices') } }, h('span', { i18n: 'common.seeAll' }), icon('chevronRight', 14))),
        track(devicesGrid({ limit: 4 })),
      );
      return [
        header('dashboard.home.title', 'dashboard.home.subtitle'),
        h(
          'div',
          { class: 'grid' },
          track(identityCard({ onIdentity: (id) => trayStore.set({ deviceId: id ? formatIdSpaced(id.deviceId) : null }) })),
          quick,
          track(lanRadarCard({ className: 'span-7' })),
          track(hostSessionsCard({ className: 'span-5' })),
          devicesPreview,
        ),
      ];
    },
    devices() {
      return [
        header('devices.title', 'devices.subtitle', h('button', { class: 'btn', on: { click: () => openQuickConnectPalette() } }, icon('plus', 14), h('span', { i18n: 'devices.connectNew' }))),
        h('div', { class: 'grid' }, h('div', { class: 'span-12' }, track(devicesGrid())), track(lanRadarCard({ className: 'span-12' }))),
      ];
    },
    drives() {
      return [header('drives.title', 'drives.subtitle'), h('div', { class: 'grid' }, track(mountsCard()), track(localSharesCard()))];
    },
    iphone() {
      return [header('hid.title', 'hid.subtitle'), track(hidControllerSection())];
    },
    settings() {
      return [header('settings.title', 'settings.subtitle'), h('div', { class: 'grid' }, track(permissionsCard()), track(preferencesCard()), track(hotkeysCard()))];
    },
  };

  const show = ({ name }) => {
    const section = SECTIONS.includes(name) ? name : 'home';
    for (const d of disposers) {
      try {
        d();
      } catch {
        /* ignore */
      }
    }
    disposers = [];
    quick = null;
    for (const b of navButtons) {
      if (b.dataset.route === section) b.setAttribute('aria-current', 'page');
      else b.removeAttribute('aria-current');
    }
    replace(content, ...sections[section]());
    content.scrollTop = 0;
  };
  onRoute(show);
  show(parseRoute());

  // ---- quick-connect palette (⌘K / tray)
  async function openQuickConnectPalette() {
    const [trusted, lan] = await Promise.all([api.listTrustedDevices().catch(() => []), api.listLanPeers().catch(() => [])]);
    const candidates = [
      ...(Array.isArray(trusted) ? trusted : []).map((d) => ({ id: `t-${d.deviceId}`, target: String(d.deviceId), label: d.name || d.deviceName || formatIdSpaced(d.deviceId), crumbKey: 'dashboard.palette.trusted', os: d.os })),
      ...(Array.isArray(lan) ? lan : []).map((p) => ({ id: `l-${p.deviceId}`, target: String(p.deviceId), label: p.deviceName || formatIdSpaced(p.deviceId), crumbKey: 'dashboard.palette.lan', os: p.os })),
      ...recentTargets().map((r) => ({ id: `r-${r.target}`, target: r.target, label: formatIdSpaced(r.target), crumbKey: 'dashboard.palette.recent', os: null })),
    ];
    openPalette({
      placeholderKey: 'dashboard.palette.placeholder',
      emptyKey: 'dashboard.palette.empty',
      footer: [
        ['↑↓', 'palette.navigate'],
        ['↩', 'palette.connect'],
        [t('keys.esc'), 'palette.close'],
      ],
      search: async (q) => {
        const query = q.trim().toLowerCase();
        const digits = idDigits(q);
        const out = [];
        if (isValidId(digits)) out.push({ id: 'direct', target: digits, label: t('dashboard.palette.connectTo', { id: formatIdSpaced(digits) }), crumb: t('dashboard.palette.directHint'), glyph: icon('arrowRight', 16) });
        for (const c of candidates) {
          const hay = `${c.label} ${c.target}`.toLowerCase();
          let j = 0;
          const matched = [];
          const label = c.label.toLowerCase();
          for (let i = 0; i < label.length && j < query.length; i++) if (label[i] === query[j]) matched.push(i), j++;
          if (!query || j === query.length || hay.includes(query.replace(/\s|-/g, ''))) {
            out.push({ ...c, matched: j === query.length ? matched : [], crumb: `${t(c.crumbKey)} · ${formatIdSpaced(c.target)}`, glyph: c.os ? osIcon(osKey(c.os), 16) : icon('history', 16) });
          }
        }
        return out.slice(0, 30);
      },
      onRun: (it) => openSession({ target: it.target, title: it.id === 'direct' ? formatIdSpaced(it.target) : it.label }),
    });
  }

  addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.code === 'KeyK') {
      e.preventDefault();
      openQuickConnectPalette().catch(toastError);
    }
  });
  on('open-quick-connect', () => openQuickConnectPalette().catch(toastError));

  // ---- host consent requests
  on('incoming-request', (req) => {
    if (req && (req.cancelled || req.state === 'cancelled')) dismissConsent(req.requestId);
    else showConsentSheet(req);
  });
}
