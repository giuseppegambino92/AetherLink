// Command palette overlay (spring entrance). Used by the Cmd+K Semantic
// Action Palette in sessions and by Quick Connect on the dashboard.
//
// Keys: ↑/↓ navigate · Enter run · Tab "type into field" mode (when the item
// accepts text) · Esc leaves field mode, then closes.
import { h, replace, debounce } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { openSheet } from './sheet.js';
import { t } from '../i18n.js';
import { describeError } from '../api.js';

/** Label with matched characters wrapped in <mark>. */
export function highlighted(label, indices = []) {
  const set = new Set(indices);
  const out = [];
  let run = '';
  let runMarked = false;
  const flush = () => {
    if (!run) return;
    out.push(runMarked ? h('mark', { text: run }) : document.createTextNode(run));
    run = '';
  };
  Array.from(String(label)).forEach((ch, i) => {
    const m = set.has(i);
    if (m !== runMarked) {
      flush();
      runMarked = m;
    }
    run += ch;
  });
  flush();
  return out;
}

/**
 * @param {{
 *   placeholderKey: string,
 *   search: (q: string) => Promise<Array<{id:any,label:string,matched?:number[],crumb?:string,glyph?:Node,meta?:Node,acceptsText?:boolean}>>,
 *   onRun: (item) => Promise<void>|void,
 *   onType?: (item, text: string) => Promise<void>|void,
 *   emptyKey?: string,
 *   footer?: Array<[string, string]>,   // [keycap, i18nKey]
 *   initialQuery?: string,
 * }} opts
 */
export function openPalette({ placeholderKey, search, onRun, onType, emptyKey = 'palette.noResults', footer = [], initialQuery = '' }) {
  const input = h('input', { class: 'palette-input', attrs: { spellcheck: 'false', autocomplete: 'off', 'aria-autocomplete': 'list', 'aria-controls': 'palette-results', role: 'combobox', 'aria-expanded': 'true' }, i18nAttr: { placeholder: placeholderKey, 'aria-label': placeholderKey } });
  const modeChip = h('span', { class: 'palette-mode', hidden: true });
  const results = h('div', { class: 'palette-results', attrs: { role: 'listbox', id: 'palette-results' } });
  const status = h('div', { class: 'palette-footer' }, footer.map(([cap, key]) => h('span', {}, h('span', { class: 'kbd', text: cap }), h('span', { i18n: key }))));
  const content = h('div', {}, h('div', { class: 'palette-input-row' }, icon('search', 18), modeChip, input), results, status);
  const sheet = openSheet({ content, className: 'palette', top: true });

  let items = [];
  let sel = 0;
  let seq = 0;
  let fieldItem = null;
  let savedQuery = '';

  function renderItems() {
    if (!items.length) {
      replace(results, h('div', { class: 'empty', style: { padding: '28px' } }, h('div', { class: 'empty-body', i18n: emptyKey })));
      return;
    }
    replace(
      results,
      items.map((it, i) =>
        h(
          'div',
          {
            class: 'palette-item',
            attrs: { role: 'option', 'aria-selected': String(i === sel), id: `pi-${i}` },
            on: {
              pointermove: () => i !== sel && select(i),
              click: () => run(it),
            },
          },
          h('div', { class: 'pi-glyph' }, it.glyph || icon('roleGeneric', 16)),
          h('div', { class: 'pi-main' }, h('div', { class: 'pi-label' }, highlighted(it.label, it.matched)), it.crumb && h('div', { class: 'pi-crumb', text: it.crumb })),
          it.meta,
          it.acceptsText && h('span', { class: 'kbd', text: '⇥', i18nAttr: { title: 'palette.tabToType' } }),
        ),
      ),
    );
    input.setAttribute('aria-activedescendant', `pi-${sel}`);
  }

  function select(i) {
    sel = Math.max(0, Math.min(items.length - 1, i));
    results.querySelectorAll('.palette-item').forEach((el, j) => el.setAttribute('aria-selected', String(j === sel)));
    const cur = results.children[sel];
    if (cur && cur.scrollIntoView) cur.scrollIntoView({ block: 'nearest' });
    input.setAttribute('aria-activedescendant', `pi-${sel}`);
  }

  const query = debounce(async () => {
    if (fieldItem) return;
    const my = ++seq;
    try {
      const res = await search(input.value);
      if (my !== seq) return;
      items = res || [];
      sel = 0;
      renderItems();
    } catch (e) {
      if (my !== seq) return;
      items = [];
      replace(results, h('div', { class: 'empty', style: { padding: '28px' } }, icon('alert', 18), h('div', { class: 'empty-body', text: describeError(e) })));
    }
  }, 60);

  async function run(it) {
    try {
      await onRun(it);
      sheet.close();
    } catch (e) {
      replace(status, h('span', { class: 'hint error', text: describeError(e) }));
    }
  }

  function enterFieldMode(it) {
    fieldItem = it;
    savedQuery = input.value;
    input.value = '';
    modeChip.hidden = false;
    modeChip.textContent = t('palette.typingInto', { label: it.label });
    input.setAttribute('placeholder', t('palette.typePlaceholder'));
  }

  function leaveFieldMode() {
    fieldItem = null;
    modeChip.hidden = true;
    input.value = savedQuery;
    input.setAttribute('placeholder', t(placeholderKey));
    query();
  }

  input.addEventListener('input', () => !fieldItem && query());
  input.addEventListener('keydown', async (e) => {
    if (fieldItem) {
      if (e.key === 'Enter') {
        e.preventDefault();
        try {
          await onType(fieldItem, input.value);
          sheet.close();
        } catch (err) {
          replace(status, h('span', { class: 'hint error', text: describeError(err) }));
        }
      } else if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        leaveFieldMode();
      }
      return;
    }
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      select(sel + 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      select(sel - 1);
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (items[sel]) run(items[sel]);
    } else if (e.key === 'Tab') {
      e.preventDefault();
      const it = items[sel];
      if (it && it.acceptsText && onType) enterFieldMode(it);
    }
  });

  input.value = initialQuery;
  requestAnimationFrame(() => input.focus());
  query();
  return sheet;
}
