// Compact settings sheet: language, device name, relay server, LAN visibility.
import { h } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { setLocale } from '../../shared/js/i18n.js';
import { api } from '../api.js';
import { openSheet, segmented, toggleSwitch, toastError, toastSuccess } from '../ui.js';

/** Persist the engine language choice and switch the UI ('auto' follows the OS). */
export async function applyLanguage(choice) {
  const code = choice === 'it' || choice === 'en' ? choice : (navigator.language || 'en').toLowerCase().startsWith('it') ? 'it' : 'en';
  await setLocale(code);
  api.uiSetLocale(code).catch(() => {});
}

export async function openSettings() {
  let settings;
  try {
    settings = await api.getSettings();
  } catch (e) {
    toastError(e);
    return;
  }
  const lang = segmented({
    labelKey: 'settings.language',
    value: settings.language || 'auto',
    options: [
      { value: 'auto', labelKey: 'settings.languageAuto' },
      { value: 'it', labelKey: 'settings.languageIt' },
      { value: 'en', labelKey: 'settings.languageEn' },
    ],
    onChange: async (v) => {
      try {
        settings = await api.setSettings({ language: v });
        await applyLanguage(v);
      } catch (e) {
        toastError(e);
      }
    },
  });
  const name = h('input', { class: 'input', attrs: { type: 'text', autocomplete: 'off', maxlength: '64' }, i18nAttr: { 'aria-label': 'settings.deviceName' } });
  name.value = settings.deviceName || '';
  const relay = h('input', { class: 'input mono', attrs: { type: 'text', autocomplete: 'off', autocapitalize: 'off', inputmode: 'url' }, i18nAttr: { placeholder: 'settings.relayPlaceholder', 'aria-label': 'settings.relay' } });
  relay.value = settings.relayServer || '';
  const lan = toggleSwitch({
    checked: settings.advertiseOnLan !== false,
    labelKey: 'settings.lan',
    onChange: (v) => api.setSettings({ advertiseOnLan: v }).catch(toastError),
  });

  async function save() {
    try {
      settings = await api.setSettings({ deviceName: name.value.trim() || settings.deviceName, relayServer: relay.value.trim() || null });
      toastSuccess('settings.saved');
      sheet.close();
    } catch (e) {
      toastError(e);
    }
  }

  const content = h(
    'div',
    { class: 'sheet-body-wrap' },
    h('h2', { class: 'sheet-title', id: 'settings-title', i18n: 'nav.settings' }),
    h('div', { class: 'field' }, h('span', { class: 'field-label', i18n: 'settings.language' }), lang),
    h('label', { class: 'field' }, h('span', { class: 'field-label', i18n: 'settings.deviceName' }), name),
    h('label', { class: 'field' }, h('span', { class: 'field-label', i18n: 'settings.relay' }), relay, h('span', { class: 'field-hint', i18n: 'settings.relayHint' })),
    h('div', { class: 'field row-between' }, h('span', { class: 'field-label', i18n: 'settings.lan' }), lan),
    h('div', { class: 'sheet-actions' }, h('button', { class: 'btn primary lg', attrs: { type: 'button' }, on: { click: save } }, icon('check', 18), h('span', { i18n: 'common.save' }))),
  );
  const sheet = openSheet({ content, labelledBy: 'settings-title' });
}
