// Mobile UI kit: toasts, bottom sheets, buttons, cards, switches, segmented controls.
// User-visible copy is always an i18n key (`i18n`, `labelKey`) or runtime data.
import { h } from '../shared/js/lib/dom.js';
import { icon } from '../shared/js/lib/icons.js';
import { springPresence, SPRINGS } from '../shared/js/lib/spring.js';
import { t } from '../shared/js/i18n.js';
import { api, describeError } from './api.js';

// ---------------------------------------------------------------- haptics
export function haptic(ms = 8) {
  try {
    if (navigator.vibrate) navigator.vibrate(ms);
  } catch {
    /* not available */
  }
}

// ---------------------------------------------------------------- toasts
let toastHost = null;

/** @param {{key?:string, args?:object, message?:string, kind?:'info'|'success'|'error', timeout?:number}} o */
export function toast({ key, args, message, kind = 'info', timeout = 3400 }) {
  if (!toastHost) {
    toastHost = h('div', { class: 'toasts', attrs: { 'aria-live': 'polite', role: 'status' } });
    document.body.appendChild(toastHost);
  }
  const glyph = kind === 'error' ? 'alert' : kind === 'success' ? 'check' : 'info';
  const label = key ? h('span', { i18n: key, i18nArgs: args }) : h('span', { text: message ?? '' });
  const el = h('div', { class: `toast ${kind}` }, icon(glyph, 16), label);
  toastHost.appendChild(el);
  springPresence(el, { show: true, preset: SPRINGS.bouncy, distance: -14 });
  setTimeout(async () => {
    await springPresence(el, { show: false, preset: SPRINGS.snappy, distance: -10 });
    el.remove();
  }, timeout);
}

export const toastError = (err) => toast({ message: describeError(err), kind: 'error', timeout: 5200 });
export const toastSuccess = (key, args) => toast({ key, args, kind: 'success' });

export async function copyWithToast(text, key = 'common.copied') {
  try {
    await api.copyText(String(text));
    toastSuccess(key);
  } catch {
    toast({ message: t('errors.copyFailed'), kind: 'error' });
  }
}

// ---------------------------------------------------------------- bottom sheet
/**
 * Bottom sheet with drag-to-dismiss handle.
 * @returns {{el: HTMLElement, overlay: HTMLElement, close: () => Promise<void>}}
 */
export function openSheet({ content, className = '', dismissible = true, onDismiss, labelledBy, full = false }) {
  const grab = h('div', { class: 'sheet-grab', attrs: { 'aria-hidden': 'true' } });
  const panel = h('div', { class: ['sheet', full && 'full', className], attrs: { role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': labelledBy } }, grab, content);
  const overlay = h('div', { class: 'overlay' }, panel);
  overlay.style.opacity = '0';
  let closed = false;
  const close = async () => {
    if (closed) return;
    closed = true;
    await springPresence(panel, { show: false, preset: SPRINGS.snappy, scrim: overlay, distance: 80 });
    overlay.remove();
  };
  const dismiss = () => {
    if (!dismissible) return;
    close();
    onDismiss && onDismiss();
  };
  overlay.addEventListener('pointerdown', (e) => e.target === overlay && dismiss());
  // drag the grab handle down to dismiss
  let y0 = null;
  grab.addEventListener('pointerdown', (e) => {
    y0 = e.clientY;
    grab.setPointerCapture(e.pointerId);
  });
  grab.addEventListener('pointermove', (e) => {
    if (y0 === null) return;
    const dy = Math.max(0, e.clientY - y0);
    panel.style.transform = `translate3d(0, ${dy}px, 0)`;
  });
  grab.addEventListener('pointerup', (e) => {
    if (y0 === null) return;
    const dy = e.clientY - y0;
    y0 = null;
    if (dy > 90) dismiss();
    else panel.style.transform = '';
  });
  document.body.appendChild(overlay);
  springPresence(panel, { show: true, preset: SPRINGS.bouncy, scrim: overlay, distance: 80 });
  return { el: panel, overlay, close };
}

/** Confirm sheet; resolves true/false. */
export function confirmSheet({ titleKey, bodyKey, args, confirmKey, danger = false }) {
  return new Promise((resolve) => {
    let sheet;
    const done = (v) => {
      sheet.close();
      resolve(v);
    };
    const content = h(
      'div',
      { class: 'sheet-body-wrap' },
      h('h2', { class: 'sheet-title', id: 'confirm-title', i18n: titleKey, i18nArgs: args }),
      bodyKey && h('p', { class: 'sheet-text', i18n: bodyKey, i18nArgs: args }),
      h(
        'div',
        { class: 'sheet-actions' },
        h('button', { class: `btn lg ${danger ? 'danger' : 'primary'}`, i18n: confirmKey, on: { click: () => done(true) } }),
        h('button', { class: 'btn lg ghost', i18n: 'common.cancel', on: { click: () => done(false) } }),
      ),
    );
    sheet = openSheet({ content, labelledBy: 'confirm-title', onDismiss: () => resolve(false) });
  });
}

// ---------------------------------------------------------------- controls
export function iconButton({ name, labelKey, onClick, className = 'icon-btn', size = 20, pressed }) {
  const b = h('button', { class: className, attrs: { type: 'button' }, i18nAttr: { 'aria-label': labelKey, title: labelKey }, on: { click: (e) => (haptic(), onClick && onClick(e)) } }, icon(name, size));
  if (pressed !== undefined) b.setAttribute('aria-pressed', String(!!pressed));
  return b;
}

export function button({ labelKey, args, iconName, onClick, className = 'btn' }) {
  return h('button', { class: className, attrs: { type: 'button' }, on: { click: (e) => (haptic(), onClick && onClick(e)) } }, iconName && icon(iconName, 18), h('span', { i18n: labelKey, i18nArgs: args }));
}

export function card({ titleKey, titleArgs, iconName, action, className = '' }, ...children) {
  return h(
    'section',
    { class: ['card', className] },
    titleKey &&
      h('header', { class: 'card-head' }, iconName && h('span', { class: 'card-glyph' }, icon(iconName, 18)), h('h2', { class: 'card-title', i18n: titleKey, i18nArgs: titleArgs }), h('span', { class: 'spacer' }), action || null),
    ...children,
  );
}

export function emptyState({ iconName = 'info', titleKey, bodyKey }) {
  return h('div', { class: 'empty' }, h('span', { class: 'empty-glyph' }, icon(iconName, 22)), h('div', { class: 'empty-title', i18n: titleKey }), bodyKey && h('div', { class: 'empty-body', i18n: bodyKey }));
}

/** iOS-style switch. `onChange(checked)` */
export function toggleSwitch({ checked = false, labelKey, onChange }) {
  const input = h('input', { attrs: { type: 'checkbox', role: 'switch' }, i18nAttr: { 'aria-label': labelKey } });
  input.checked = checked;
  input.addEventListener('change', () => (haptic(), onChange && onChange(input.checked)));
  return h('label', { class: 'switch' }, input, h('span', { class: 'switch-track' }, h('span', { class: 'switch-thumb' })));
}

/** Segmented control. options: [{value, labelKey, iconName?}] */
export function segmented({ options, value, onChange, labelKey }) {
  const el = h('div', { class: 'segmented', attrs: { role: 'radiogroup' }, i18nAttr: { 'aria-label': labelKey } });
  const render = (v) => {
    for (const b of el.children) b.setAttribute('aria-checked', String(b.dataset.value === String(v)));
  };
  for (const o of options) {
    el.appendChild(
      h(
        'button',
        { attrs: { type: 'button', role: 'radio' }, dataset: { value: String(o.value) }, on: { click: () => (haptic(), render(o.value), onChange && onChange(o.value)) } },
        o.iconName && icon(o.iconName, 16),
        h('span', { i18n: o.labelKey }),
      ),
    );
  }
  render(value);
  return Object.assign(el, { set: render });
}

/** A list row: glyph, title (data or key), subtitle, trailing node. */
export function listRow({ glyph, title, titleKey, sub, subKey, trailing, onClick }) {
  const tag = onClick ? 'button' : 'div';
  return h(
    tag,
    { class: 'list-row', attrs: onClick ? { type: 'button' } : {}, on: onClick ? { click: (e) => (haptic(), onClick(e)) } : {} },
    glyph && h('span', { class: 'list-glyph' }, glyph),
    h('span', { class: 'list-main' }, h('span', { class: 'list-title', text: title, i18n: titleKey }), (sub || subKey) && h('span', { class: 'list-sub', text: sub, i18n: subKey })),
    trailing || null,
  );
}
