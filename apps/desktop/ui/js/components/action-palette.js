// Cmd+K Semantic Action Palette: fuzzy search over the remote UI's
// accessibility tree (`dom_search`). Enter presses the element; Tab switches
// to "type into field" mode for text-accepting roles (`dom_set_value`).
import { h } from '../lib/dom.js';
import { roleIcon } from '../lib/icons.js';
import { api } from '../api.js';
import { openPalette } from './palette.js';
import { t } from '../i18n.js';

const TEXT_ROLES = new Set(['TextField', 'TextArea', 'SearchField', 'ComboBox']);
let subscribed = new Set();

export async function openActionPalette(sessionId) {
  if (!subscribed.has(sessionId)) {
    subscribed.add(sessionId);
    api.domSubscribe(sessionId, true).catch(() => subscribed.delete(sessionId));
  }
  return openPalette({
    placeholderKey: 'palette.semanticPlaceholder',
    emptyKey: 'palette.noResults',
    footer: [
      ['↑↓', 'palette.navigate'],
      ['↩', 'palette.press'],
      ['⇥', 'palette.typeInto'],
      [t('keys.esc'), 'palette.close'],
    ],
    search: async (q) => {
      const hits = await api.domSearch(sessionId, q, 40);
      return (Array.isArray(hits) ? hits : []).map((hit) => ({
        id: hit.nodeId,
        label: hit.label || t('palette.unnamed'),
        matched: hit.matchedIndices || [],
        crumb: hit.breadcrumb || '',
        glyph: roleIcon(hit.role, 16),
        meta: h('span', { class: 'pi-role', i18n: `roles.${hit.role in ROLE_KEYS ? hit.role : 'Other'}` }),
        acceptsText: TEXT_ROLES.has(hit.role),
        role: hit.role,
      }));
    },
    onRun: (it) => api.domAction(sessionId, it.id, TEXT_ROLES.has(it.role) ? 'Focus' : 'Press'),
    onType: (it, text) => api.domSetValue(sessionId, it.id, text),
  });
}

// Roles that have a localized name in locales (`roles.*`).
const ROLE_KEYS = Object.fromEntries(
  [
    'Window', 'Button', 'Link', 'TextField', 'TextArea', 'SearchField', 'CheckBox', 'RadioButton', 'Switch', 'Slider', 'ComboBox', 'PopUpButton',
    'Menu', 'MenuItem', 'MenuBar', 'Tab', 'TabGroup', 'List', 'ListItem', 'Table', 'Row', 'Cell', 'Tree', 'TreeItem', 'Toolbar', 'Image',
    'StaticText', 'Heading', 'Group', 'ScrollArea', 'WebArea', 'Dialog', 'Other',
  ].map((r) => [r, true]),
);
