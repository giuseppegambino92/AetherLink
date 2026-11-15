// Quick Connect: 9-digit partner ID (auto-formatted XXX-XXX-XXX) + optional
// one-time password. Opens a session viewport window.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { card } from './controls.js';
import { formatIdDashed, formatIdSpaced, idDigits, isValidId } from '../lib/format.js';
import { openSession, recentTargets } from '../sessions.js';
import { t } from '../i18n.js';

export function quickConnectCard() {
  const idInput = h('input', {
    class: 'input xl',
    attrs: { inputmode: 'numeric', autocomplete: 'off', spellcheck: 'false', maxlength: '11', id: 'qc-id' },
    i18nAttr: { placeholder: 'dashboard.quickConnect.placeholder', 'aria-label': 'dashboard.quickConnect.idLabel' },
  });
  const pwInput = h('input', {
    class: 'input',
    attrs: { type: 'password', autocomplete: 'off', spellcheck: 'false', id: 'qc-pw' },
    i18nAttr: { placeholder: 'dashboard.quickConnect.passwordPlaceholder', 'aria-label': 'dashboard.quickConnect.passwordLabel' },
  });
  const hint = h('div', { class: 'hint', i18n: 'dashboard.quickConnect.hint' });
  const go = h('button', { class: 'btn primary lg', attrs: { disabled: true } }, h('span', { i18n: 'dashboard.quickConnect.connect' }), icon('arrowRight', 16));
  const recents = h('div', { class: 'recents' });

  const validate = () => {
    const ok = isValidId(idInput.value);
    go.disabled = !ok;
    const d = idDigits(idInput.value);
    const bad = d.length === 9 && !ok;
    idInput.classList.toggle('invalid', bad);
    const key = bad ? 'dashboard.quickConnect.invalid' : 'dashboard.quickConnect.hint';
    hint.dataset.i18n = key;
    hint.textContent = t(key);
    hint.classList.toggle('error', bad);
    return ok;
  };

  idInput.addEventListener('input', () => {
    // Re-format while keeping the caret after the same number of digits.
    const caret = idInput.selectionStart ?? idInput.value.length;
    const digitsBefore = idDigits(idInput.value.slice(0, caret)).length;
    const formatted = formatIdDashed(idInput.value);
    idInput.value = formatted;
    let pos = 0;
    let seen = 0;
    while (pos < formatted.length && seen < digitsBefore) {
      if (/\d/.test(formatted[pos])) seen++;
      pos++;
    }
    idInput.setSelectionRange(pos, pos);
    validate();
  });
  idInput.addEventListener('paste', (e) => {
    e.preventDefault();
    const text = (e.clipboardData || window.clipboardData).getData('text');
    idInput.value = formatIdDashed(text);
    validate();
  });

  const submit = () => {
    if (!validate()) return;
    const digits = idDigits(idInput.value);
    openSession({ target: digits, password: pwInput.value || null, title: formatIdSpaced(digits) });
    pwInput.value = '';
    renderRecents();
  };
  go.addEventListener('click', submit);
  for (const inp of [idInput, pwInput]) inp.addEventListener('keydown', (e) => e.key === 'Enter' && submit());

  function renderRecents() {
    const list = recentTargets();
    replace(
      recents,
      list.length ? h('span', { class: 'hint', style: { alignSelf: 'center', marginRight: '4px' }, i18n: 'dashboard.quickConnect.recent' }) : null,
      list.map((r) =>
        h('button', {
          class: 'recent-chip',
          text: formatIdSpaced(r.target),
          on: { click: () => ((idInput.value = formatIdDashed(r.target)), validate(), pwInput.focus()) },
        }),
      ),
    );
  }
  renderRecents();

  const el = card(
    { className: 'quick-connect span-5', titleKey: 'dashboard.quickConnect.title', subtitleKey: 'dashboard.quickConnect.subtitle', iconName: 'link', iconTone: 'violet' },
    h('div', { class: 'stack' }, idInput, h('div', { class: 'qc-row' }, pwInput, go), hint, recents),
  );
  el.focusInput = () => idInput.focus();
  return el;
}
