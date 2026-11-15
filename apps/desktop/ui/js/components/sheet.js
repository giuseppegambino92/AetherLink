// Modal sheet with spring entrance, scrim, Escape-to-dismiss and focus return.
import { h } from '../lib/dom.js';
import { springPresence, SPRINGS } from '../lib/spring.js';

/**
 * @param {{content: Node, className?: string, dismissible?: boolean, onDismiss?: Function, labelledBy?: string, top?: boolean}} opts
 * @returns {{el: HTMLElement, close: () => Promise<void>}}
 */
export function openSheet({ content, className = 'sheet', dismissible = true, onDismiss, labelledBy, top = false }) {
  const prevFocus = document.activeElement;
  const panel = h('div', { class: className, attrs: { role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': labelledBy } }, content);
  const overlay = h('div', { class: `overlay${top ? ' top' : ''}` }, panel);
  overlay.style.opacity = '0';
  let closed = false;

  const close = async () => {
    if (closed) return;
    closed = true;
    removeEventListener('keydown', onKey, true);
    await springPresence(panel, { show: false, preset: SPRINGS.snappy, scrim: overlay });
    overlay.remove();
    if (prevFocus && prevFocus.focus) prevFocus.focus();
  };
  const onKey = (e) => {
    if (e.key === 'Escape' && dismissible) {
      e.stopPropagation();
      e.preventDefault();
      close();
      onDismiss && onDismiss();
    }
  };
  overlay.addEventListener('pointerdown', (e) => {
    if (e.target === overlay && dismissible) {
      close();
      onDismiss && onDismiss();
    }
  });
  addEventListener('keydown', onKey, true);
  document.body.appendChild(overlay);
  springPresence(panel, { show: true, preset: SPRINGS.bouncy, scrim: overlay, distance: 26 });
  const focusable = panel.querySelector('[autofocus], input, button');
  if (focusable) requestAnimationFrame(() => focusable.focus());
  return { el: panel, overlay, close };
}

/** Simple confirm sheet; resolves true/false. Keys are i18n keys. */
export function confirmSheet({ titleKey, bodyKey, args, confirmKey, danger = false }) {
  return new Promise((resolve) => {
    let sheet;
    const done = (v) => {
      sheet.close();
      resolve(v);
    };
    const content = h(
      'div',
      {},
      h('h2', { class: 'sheet-title', id: 'confirm-title', i18n: titleKey, i18nArgs: args }),
      h('p', { class: 'sheet-body', i18n: bodyKey, i18nArgs: args }),
      h(
        'div',
        { class: 'sheet-actions inline' },
        h('button', { class: 'btn', i18n: 'common.cancel', on: { click: () => done(false) } }),
        h('button', { class: `btn ${danger ? 'danger' : 'primary'}`, i18n: confirmKey, attrs: { autofocus: true }, on: { click: () => done(true) } }),
      ),
    );
    sheet = openSheet({ content, labelledBy: 'confirm-title', onDismiss: () => resolve(false) });
  });
}
