// Settings & Permissions Health Check.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, isNotWired } from '../api.js';
import { card, emptyState, segmented, toggleSwitch } from './controls.js';
import { toastError, toastSuccess } from './toast.js';
import { getLocale, setLocale, t, tp, onLocaleChange } from '../i18n.js';
import { acceleratorKeys, eventToAccelerator, loadHudHotkey, saveHudHotkey } from '../lib/hotkeys.js';

const PERMS = [
  { kind: 'screenRecording', icon: 'monitor' },
  { kind: 'accessibility', icon: 'user' },
  { kind: 'inputMonitoring', icon: 'keyboard' },
];

function normStatus(v) {
  const s = String(v || '').toLowerCase().replace(/[^a-z]/g, '');
  if (s === 'granted' || s === 'true' || s === 'authorized') return 'granted';
  if (s === 'notapplicable' || s === 'na') return 'notApplicable';
  return 'denied';
}

export function permissionsCard() {
  const rows = h('div');
  const score = h('span', { class: 'chip' });
  const el = card({ className: 'span-12', titleKey: 'settings.permissions.title', subtitleKey: 'settings.permissions.subtitle', iconName: 'shield', iconTone: 'aqua', actions: [score], lift: false }, rows);
  let last = null;

  function render(p) {
    last = p;
    const statuses = PERMS.map((x) => normStatus(p[x.kind]));
    const applicable = statuses.filter((s) => s !== 'notApplicable');
    const granted = applicable.filter((s) => s === 'granted').length;
    const allOk = granted === applicable.length;
    score.hidden = false;
    score.className = `chip ${allOk ? 'success' : 'warning'}`;
    score.textContent = allOk ? t('settings.permissions.allGood') : tp('settings.permissions.missing', applicable.length - granted);
    replace(
      rows,
      PERMS.map((x, i) => {
        const st = statuses[i];
        const chipClass = st === 'granted' ? 'success' : st === 'notApplicable' ? '' : 'warning';
        return h(
          'div',
          { class: 'perm-row' },
          h('div', { class: 'row-glyph' }, icon(x.icon, 18)),
          h('div', { class: 'row-main', style: { flex: '1', minWidth: '0' } }, h('div', { class: 'row-title', i18n: `settings.permissions.${x.kind}.title` }), h('div', { class: 'row-sub', style: { whiteSpace: 'normal' }, i18n: `settings.permissions.${x.kind}.body` })),
          h('span', { class: `chip ${chipClass}` }, h('span', { class: `dot ${st === 'granted' ? 'online' : st === 'denied' ? 'busy' : ''}` }), h('span', { i18n: `settings.permissions.status.${st}` })),
          st !== 'notApplicable' &&
            h(
              'button',
              { class: `btn sm ${st === 'granted' ? 'ghost' : ''}`, on: { click: () => api.openPermissionSettings(x.kind).catch(toastError) } },
              h('span', { i18n: 'settings.permissions.openSettings' }),
              icon('external', 13),
            ),
        );
      }),
    );
  }

  async function load() {
    try {
      render(await api.getPermissions());
    } catch (e) {
      score.hidden = true;
      replace(rows, emptyState({ iconName: isNotWired(e) ? 'plug' : 'alert', titleKey: isNotWired(e) ? 'engine.notWiredTitle' : 'settings.permissions.unavailable', bodyKey: isNotWired(e) ? 'engine.notWiredBody' : 'errors.tryAgain' }));
    }
  }
  load();
  // Users grant permissions in System Settings and come back — re-check on focus.
  const onFocus = () => el.isConnected && load();
  addEventListener('focus', onFocus);
  const off = onLocaleChange(() => last && render(last));
  el.dispose = () => (removeEventListener('focus', onFocus), off());
  return el;
}

export function preferencesCard() {
  const body = h('div');
  const notice = h('div');
  const el = card({ className: 'span-12', titleKey: 'settings.preferences.title', subtitleKey: 'settings.preferences.subtitle', iconName: 'settings', lift: false }, notice, body);
  let settings = null;

  async function save(patch) {
    if (!settings) return;
    try {
      settings = await api.setSettings({ ...settings, ...patch });
      toastSuccess('settings.saved');
    } catch (e) {
      toastError(e);
    }
  }

  const row = (titleKey, bodyKey, control) =>
    h('div', { class: 'settings-row' }, h('div', {}, h('div', { class: 'row-title', i18n: titleKey }), bodyKey && h('div', { class: 'row-sub', style: { whiteSpace: 'normal' }, i18n: bodyKey })), control);

  function render() {
    const engineReady = !!settings;
    const lang = segmented({
      options: [
        { value: 'en', key: 'language.en' },
        { value: 'it', key: 'language.it' },
      ],
      value: getLocale(),
      labelKey: 'settings.language.title',
      // app.js mirrors every locale change to the tray (ui_set_locale).
      onChange: (v) => setLocale(v),
    });
    onLocaleChange((v) => lang.setValue(v));

    const clip = segmented({
      options: [
        { value: 'Disabled', key: 'settings.clipboard.disabled' },
        { value: 'PushOnly', key: 'settings.clipboard.pushOnly' },
        { value: 'Bidirectional', key: 'settings.clipboard.bidirectional' },
      ],
      value: settings?.clipboardMode || 'Bidirectional',
      labelKey: 'settings.clipboard.title',
      onChange: (v) => save({ clipboardMode: v }),
    });

    const relay = h('input', {
      class: 'input',
      style: { width: '280px' },
      attrs: { spellcheck: 'false', autocomplete: 'off', value: settings?.relayServer || '' },
      i18nAttr: { placeholder: 'settings.relay.placeholder', 'aria-label': 'settings.relay.title' },
    });
    relay.value = settings?.relayServer || '';
    relay.addEventListener('change', () => save({ relayServer: relay.value.trim() }));

    const unattended = toggleSwitch({ checked: settings ? settings.allowUnattended !== false : true, labelKey: 'settings.unattended.title', onChange: (v) => save({ allowUnattended: v }) });
    const consent = toggleSwitch({ checked: settings?.requireConsent === true, labelKey: 'settings.consent.title', onChange: (v) => save({ requireConsent: v }) });

    for (const c of [clip, relay, unattended, consent]) {
      if (!engineReady) {
        c.setAttribute('aria-disabled', 'true');
        c.style.pointerEvents = 'none';
        c.style.opacity = '0.45';
      }
    }

    replace(
      body,
      row('settings.language.title', 'settings.language.body', lang),
      row('settings.clipboard.title', 'settings.clipboard.body', clip),
      row('settings.relay.title', 'settings.relay.body', relay),
      row('settings.unattended.title', 'settings.unattended.body', unattended),
      row('settings.consent.title', 'settings.consent.body', consent),
    );
  }

  async function load() {
    try {
      settings = await api.getSettings();
      replace(notice);
    } catch (e) {
      settings = null;
      replace(
        notice,
        h('div', { class: 'banner' }, icon(isNotWired(e) ? 'plug' : 'alert', 16), h('span', { i18n: isNotWired(e) ? 'settings.engineSettingsUnavailable' : 'settings.loadFailed' })),
      );
    }
    render();
  }
  load();
  el.currentSettings = () => settings;
  return el;
}

export function hotkeysCard() {
  const body = h('div');
  const el = card({ className: 'span-12', titleKey: 'settings.hotkeys.title', subtitleKey: 'settings.hotkeys.subtitle', iconName: 'keyboard', iconTone: 'violet', lift: false }, body);

  const keycaps = (accel) => h('span', { class: 'kbd-group' }, acceleratorKeys(accel).map((k) => h('span', { class: 'kbd', text: k })));

  function render() {
    const current = loadHudHotkey();
    const capsHolder = h('span', {}, keycaps(current));
    const recordBtn = h('button', { class: 'btn sm' }, h('span', { i18n: 'settings.hotkeys.change' }));
    let recording = false;
    const stop = () => {
      recording = false;
      removeEventListener('keydown', onKey, true);
      recordBtn.querySelector('span').dataset.i18n = 'settings.hotkeys.change';
      recordBtn.querySelector('span').textContent = t('settings.hotkeys.change');
    };
    const onKey = async (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') return stop();
      const accel = eventToAccelerator(e);
      if (!accel) return; // need at least one modifier + a non-modifier key
      stop();
      try {
        await api.uiSetHudHotkey(accel);
        saveHudHotkey(accel);
        replace(capsHolder, keycaps(accel));
        toastSuccess('settings.hotkeys.updated');
      } catch (err) {
        toastError(err);
      }
    };
    recordBtn.addEventListener('click', () => {
      if (recording) return stop();
      recording = true;
      recordBtn.querySelector('span').dataset.i18n = 'settings.hotkeys.recording';
      recordBtn.querySelector('span').textContent = t('settings.hotkeys.recording');
      addEventListener('keydown', onKey, true);
    });

    const fixed = (titleKey, accel) => h('div', { class: 'settings-row' }, h('div', { class: 'row-title', i18n: titleKey }), keycaps(accel));
    replace(
      body,
      h('div', { class: 'settings-row' }, h('div', {}, h('div', { class: 'row-title', i18n: 'settings.hotkeys.hud' }), h('div', { class: 'row-sub', i18n: 'settings.hotkeys.hudBody' })), h('div', { class: 'row' }, capsHolder, recordBtn)),
      fixed('settings.hotkeys.palette', 'CmdOrCtrl+K'),
      fixed('settings.hotkeys.terminal', 'Ctrl+Backquote'),
      fixed('settings.hotkeys.quickConnect', 'CmdOrCtrl+K'),
    );
  }
  render();
  return el;
}
