// Small form controls: switch, segmented control (spring-driven thumb), card.
import { h } from '../lib/dom.js';
import { Spring, SPRINGS, springHover } from '../lib/spring.js';
import { icon } from '../lib/icons.js';

/** Accessible switch with a spring-animated knob. */
export function toggleSwitch({ checked = false, labelKey, onChange }) {
  const el = h('button', {
    class: 'switch',
    attrs: { role: 'switch', 'aria-checked': String(checked), type: 'button' },
    i18nAttr: labelKey ? { 'aria-label': labelKey } : undefined,
  });
  const s = new Spring({ ...SPRINGS.snappy, value: checked ? 1 : 0, onUpdate: (v) => el.style.setProperty('--on', v.toFixed(4)) });
  el.style.setProperty('--on', checked ? '1' : '0');
  const set = (v, emit = true) => {
    checked = v;
    el.setAttribute('aria-checked', String(v));
    s.to(v ? 1 : 0);
    if (emit && onChange) onChange(v);
  };
  el.addEventListener('click', () => set(!checked));
  el.setChecked = (v) => set(v, false);
  return el;
}

/** Segmented control; options: [{value, key}] */
export function segmented({ options, value, labelKey, onChange }) {
  const thumb = h('span', { class: 'seg-thumb' });
  const buttons = options.map((o) =>
    h('button', {
      type: 'button',
      i18n: o.key,
      attrs: { role: 'radio', 'aria-checked': String(o.value === value) },
      dataset: { value: o.value },
      on: { click: () => select(o.value, true) },
    }),
  );
  const el = h('div', { class: 'segmented', attrs: { role: 'radiogroup' }, i18nAttr: labelKey ? { 'aria-label': labelKey } : undefined }, thumb, buttons);
  const x = new Spring({ ...SPRINGS.snappy, onUpdate: (v) => (thumb.style.transform = `translateX(${v}px)`) });
  const wSpring = new Spring({ ...SPRINGS.snappy, onUpdate: (v) => (thumb.style.width = `${v}px`) });
  let first = true;
  const layout = () => {
    const btn = buttons.find((b) => b.dataset.value === String(value));
    if (!btn || !btn.offsetWidth) return;
    if (first) {
      x.jump(btn.offsetLeft);
      wSpring.jump(btn.offsetWidth);
      first = false;
    } else {
      x.to(btn.offsetLeft);
      wSpring.to(btn.offsetWidth);
    }
  };
  function select(v, emit) {
    value = v;
    for (const b of buttons) b.setAttribute('aria-checked', String(b.dataset.value === String(v)));
    layout();
    if (emit && onChange) onChange(v);
  }
  // Re-measure when attached / when labels change length (locale switch).
  new ResizeObserver(layout).observe(el);
  el.setValue = (v) => select(v, false);
  return el;
}

/** Glass card with spring hover lift. */
export function card({ className = '', titleKey, subtitleKey, iconName, iconTone = '', actions = [], lift = true }, ...body) {
  const header =
    titleKey &&
    h(
      'div',
      { class: 'card-header' },
      iconName && h('div', { class: `card-icon ${iconTone}` }, icon(iconName, 18)),
      h('div', { class: 'spacer' }, h('h3', { class: 'card-title', i18n: titleKey }), subtitleKey && h('p', { class: 'card-subtitle', i18n: subtitleKey })),
      ...actions,
    );
  const el = h('section', { class: `card ${className}` }, header, ...body);
  if (lift) springHover(el, { lift: 3, scale: 0.004 });
  return el;
}

/** Empty / unavailable state block (i18n keys). */
export function emptyState({ iconName = 'info', titleKey, bodyKey, args, action }) {
  return h(
    'div',
    { class: 'empty' },
    h('div', { class: 'empty-glyph' }, icon(iconName, 22)),
    h('div', { class: 'empty-title', i18n: titleKey, i18nArgs: args }),
    bodyKey && h('div', { class: 'empty-body', i18n: bodyKey, i18nArgs: args }),
    action,
  );
}

/** Icon-only button with localized tooltip + aria-label. */
export function iconButton({ name, labelKey, onClick, className = 'icon-btn', size = 16, args }) {
  return h('button', { class: className, type: 'button', i18nAttr: { title: labelKey, 'aria-label': labelKey }, i18nArgs: args, on: { click: onClick } }, icon(name, size));
}
