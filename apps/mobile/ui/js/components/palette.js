// Semantic action palette (the mobile Cmd+K): fuzzy search over the remote UI's
// accessibility tree (`dom_search`). Tap a result to press it; text fields get an
// inline "type into" box (`dom_set_value`).
import { h, replace, debounce } from '../../shared/js/lib/dom.js';
import { icon, roleIcon } from '../../shared/js/lib/icons.js';
import { t } from '../../shared/js/i18n.js';
import { api, describeError } from '../api.js';
import { openSheet, toastError, toastSuccess, haptic } from '../ui.js';

const TEXT_ROLES = new Set(['TextField', 'TextArea', 'SearchField', 'ComboBox']);
const ROLES = new Set([
  'Window', 'Button', 'Link', 'TextField', 'TextArea', 'SearchField', 'CheckBox', 'RadioButton', 'Switch', 'Slider', 'ComboBox', 'PopUpButton',
  'Menu', 'MenuItem', 'MenuBar', 'Tab', 'TabGroup', 'List', 'ListItem', 'Table', 'Row', 'Cell', 'Tree', 'TreeItem', 'Toolbar', 'Image',
  'StaticText', 'Heading', 'Group', 'ScrollArea', 'WebArea', 'Dialog', 'Other',
]);
const subscribed = new Set();

/** Label with matched characters wrapped in <mark>. */
function highlighted(label, indices = []) {
  const set = new Set(indices);
  return Array.from(String(label)).map((ch, i) => (set.has(i) ? h('mark', { text: ch }) : document.createTextNode(ch)));
}

export function openSemanticPalette(sessionId) {
  if (!subscribed.has(sessionId)) {
    subscribed.add(sessionId);
    api.domSubscribe(sessionId, true).catch(() => subscribed.delete(sessionId));
  }
  const input = h('input', { class: 'input palette-input', attrs: { type: 'search', autocomplete: 'off', spellcheck: 'false', enterkeyhint: 'search' }, i18nAttr: { placeholder: 'palette.semanticPlaceholder', 'aria-label': 'palette.semanticPlaceholder' } });
  const results = h('div', { class: 'palette-results', attrs: { role: 'listbox' } });
  const content = h('div', { class: 'sheet-body-wrap' }, h('div', { class: 'palette-row' }, icon('search', 18), input), results);
  const sheet = openSheet({ content, full: true, className: 'palette' });
  let seq = 0;

  async function run(hit) {
    haptic();
    try {
      await api.domAction(sessionId, hit.nodeId, TEXT_ROLES.has(hit.role) ? 'Focus' : 'Press');
      if (!TEXT_ROLES.has(hit.role)) sheet.close();
    } catch (e) {
      toastError(e);
    }
  }

  function typeBox(hit) {
    const field = h('input', { class: 'input', attrs: { type: 'text', enterkeyhint: 'done' }, i18nAttr: { placeholder: 'palette.typeInto', 'aria-label': 'palette.typeInto' } });
    const submit = async () => {
      try {
        await api.domSetValue(sessionId, hit.nodeId, field.value);
        toastSuccess('palette.valueSet');
        sheet.close();
      } catch (e) {
        toastError(e);
      }
    };
    field.addEventListener('keydown', (e) => e.key === 'Enter' && submit());
    return h('div', { class: 'palette-type' }, field, h('button', { class: 'btn primary', attrs: { type: 'button' }, i18n: 'palette.set', on: { click: submit } }));
  }

  async function search() {
    const my = ++seq;
    const q = input.value.trim();
    let hits;
    try {
      hits = await api.domSearch(sessionId, q, 40);
    } catch (e) {
      if (my === seq) replace(results, h('div', { class: 'hint pad', text: describeError(e) }));
      return;
    }
    if (my !== seq) return;
    hits = Array.isArray(hits) ? hits : [];
    if (!hits.length) {
      replace(results, h('div', { class: 'hint pad', i18n: q ? 'palette.noResults' : 'palette.waitingTree' }));
      return;
    }
    replace(
      results,
      hits.map((hit) => {
        const role = ROLES.has(hit.role) ? hit.role : 'Other';
        const row = h(
          'div',
          { class: 'palette-item' },
          h(
            'button',
            { class: 'palette-hit', attrs: { type: 'button', role: 'option' }, on: { click: () => run(hit) } },
            h('span', { class: 'list-glyph' }, roleIcon(hit.role, 18)),
            h('span', { class: 'list-main' }, h('span', { class: 'list-title' }, ...(hit.label ? highlighted(hit.label, hit.matchedIndices) : [t('palette.unnamed')])), hit.breadcrumb && h('span', { class: 'list-sub', text: hit.breadcrumb })),
            h('span', { class: 'chip', i18n: `roles.${role}` }),
          ),
        );
        if (TEXT_ROLES.has(hit.role)) row.appendChild(typeBox(hit));
        return row;
      }),
    );
  }

  input.addEventListener('input', debounce(search, 90));
  search();
  setTimeout(() => input.focus(), 250);
  return sheet;
}
