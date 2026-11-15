// Boot: i18n → backend adapter → route to dashboard or session view.
import { initI18n, getLocale, setLocale, apply, onLocaleChange, t } from './i18n.js';
import { initApi, api, on, windowLabel } from './api.js';
import { mountDashboard } from './views/dashboard.js';
import { mountSession } from './views/session.js';
import { loadHudHotkey, DEFAULT_HUD_HOTKEY } from './lib/hotkeys.js';
import { trayStore } from './stores.js';

async function boot() {
  await initI18n();
  await initApi();
  apply(document);

  const root = document.getElementById('app');
  const label = windowLabel();
  const isSession = label.startsWith('session-') || location.hash.startsWith('#/session');

  // Language: tray → webviews (`locale-changed`), and webview → tray (ui_set_locale).
  on('locale-changed', (p) => p && p.locale && p.locale !== getLocale() && setLocale(p.locale));
  const syncTitle = () => {
    if (!isSession) document.title = t('app.name');
  };
  onLocaleChange(syncTitle);
  syncTitle();

  if (isSession) {
    await mountSession(root);
    return;
  }

  // Dashboard owns the shell-wide side effects (tray, hotkey, locale push).
  api.uiSetLocale(getLocale()).catch(() => {});
  onLocaleChange((code) => api.uiSetLocale(code).catch(() => {}));
  const hotkey = loadHudHotkey();
  if (hotkey !== DEFAULT_HUD_HOTKEY) api.uiSetHudHotkey(hotkey).catch(() => {});
  on('tray-ghost-mode', (p) => trayStore.set({ ghostMode: !!(p && p.enabled) }));
  mountDashboard(root);
}

boot().catch((e) => {
  console.error('[aetherlink] boot failed', e);
  const root = document.getElementById('app');
  if (root) {
    root.textContent = '';
    const title = document.createElement('h2');
    title.style.cssText = 'padding:24px 24px 0;font:600 18px var(--font-ui)';
    title.textContent = t('errors.bootFailed');
    root.appendChild(title);
    const pre = document.createElement('pre');
    pre.className = 'mono';
    pre.style.cssText = 'padding:24px;color:#ff8089;white-space:pre-wrap';
    pre.textContent = String(e && e.stack ? e.stack : e);
    root.appendChild(pre);
  }
});
