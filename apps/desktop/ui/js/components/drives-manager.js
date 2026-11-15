// Virtual Drives Manager: remote shares mounted on this Mac + local folders
// shared with peers (add by path or by dropping a folder).
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, isNotWired, onDragDrop } from '../api.js';
import { card, emptyState, iconButton, toggleSwitch } from './controls.js';
import { toastError, toastSuccess, copyWithToast } from './toast.js';

function unavailable(e, titleKey) {
  return emptyState({
    iconName: isNotWired(e) ? 'plug' : 'alert',
    titleKey: isNotWired(e) ? 'engine.notWiredTitle' : titleKey,
    bodyKey: isNotWired(e) ? 'engine.notWiredBody' : 'errors.tryAgain',
  });
}

export function mountsCard() {
  const list = h('div', { class: 'stack' });
  const el = card({ className: 'span-6', titleKey: 'drives.mounted.title', subtitleKey: 'drives.mounted.subtitle', iconName: 'drive', iconTone: 'violet', lift: false }, list);

  const render = (mounts) => {
    if (!mounts.length) {
      replace(list, emptyState({ iconName: 'drive', titleKey: 'drives.mounted.emptyTitle', bodyKey: 'drives.mounted.emptyBody' }));
      return;
    }
    replace(
      list,
      mounts.map((m) =>
        h(
          'div',
          { class: 'drive-tile' },
          h('div', { class: 'drive-glyph' }, icon('drive', 20)),
          h(
            'div',
            { class: 'row-main', style: { flex: '1', minWidth: '0' } },
            h('div', { class: 'row-title', text: m.shareName || m.name || m.mountPoint }),
            h('div', { class: 'row-sub', text: [m.peerName, m.mountPoint].filter(Boolean).join(' · ') }),
          ),
          m.readOnly && h('span', { class: 'chip', i18n: 'drives.readOnly' }),
          iconButton({ name: 'copy', labelKey: 'drives.copyPath', onClick: () => copyWithToast(api, m.mountPoint) }),
          h('button', { class: 'btn sm', on: { click: () => unmount(m) } }, h('span', { i18n: 'drives.unmount' })),
        ),
      ),
    );
  };

  async function unmount(m) {
    try {
      await api.vfsUnmount(m.mountPoint);
      toastSuccess('drives.unmounted', { name: m.shareName || m.mountPoint });
      load();
    } catch (e) {
      toastError(e);
    }
  }

  async function load() {
    try {
      const mounts = await api.listMounts();
      render(Array.isArray(mounts) ? mounts : []);
    } catch (e) {
      replace(list, unavailable(e, 'drives.mounted.unavailable'));
    }
  }
  load();
  return el;
}

export function localSharesCard() {
  const list = h('div', { class: 'list' });
  const pathInput = h('input', { class: 'input', attrs: { spellcheck: 'false', autocomplete: 'off' }, i18nAttr: { placeholder: 'drives.shares.pathPlaceholder', 'aria-label': 'drives.shares.pathLabel' } });
  let readOnly = true;
  const ro = toggleSwitch({ checked: true, labelKey: 'drives.shares.readOnly', onChange: (v) => (readOnly = v) });
  const addBtn = h('button', { class: 'btn primary' }, icon('plus', 14), h('span', { i18n: 'drives.shares.add' }));
  const drop = h('div', { class: 'dropzone' }, icon('folderPlus', 20), h('span', { i18n: 'drives.shares.drop' }));

  const el = card(
    { className: 'span-6', titleKey: 'drives.shares.title', subtitleKey: 'drives.shares.subtitle', iconName: 'folder', lift: false },
    list,
    h(
      'div',
      { class: 'stack', style: { marginTop: '16px' } },
      h('div', { class: 'row' }, pathInput, addBtn),
      h('label', { class: 'row hint' }, ro, h('span', { i18n: 'drives.shares.readOnly' })),
      drop,
    ),
  );

  const render = (shares) => {
    if (!shares.length) {
      replace(list, emptyState({ iconName: 'folder', titleKey: 'drives.shares.emptyTitle', bodyKey: 'drives.shares.emptyBody' }));
      return;
    }
    replace(
      list,
      shares.map((s) =>
        h(
          'div',
          { class: 'list-row' },
          h('div', { class: 'row-glyph' }, icon('folder', 16)),
          h('div', { class: 'row-main' }, h('div', { class: 'row-title', text: s.name || s.path }), h('div', { class: 'row-sub mono', text: s.path || '' })),
          h('span', { class: `chip ${s.readOnly ? '' : 'accent'}`, i18n: s.readOnly ? 'drives.readOnly' : 'drives.readWrite' }),
          iconButton({ name: 'trash', labelKey: 'drives.shares.remove', onClick: () => remove(s) }),
        ),
      ),
    );
  };

  async function add(path) {
    const p = String(path || '').trim();
    if (!p) return;
    try {
      const shares = await api.addLocalShare(p, readOnly);
      pathInput.value = '';
      toastSuccess('drives.shares.added', { name: p.split(/[\\/]/).filter(Boolean).pop() || p });
      if (Array.isArray(shares)) render(shares);
      else load();
    } catch (e) {
      toastError(e);
    }
  }

  async function remove(s) {
    try {
      const shares = await api.removeLocalShare(s.id);
      if (Array.isArray(shares)) render(shares);
      else load();
    } catch (e) {
      toastError(e);
    }
  }

  async function load() {
    try {
      const shares = await api.listLocalShares();
      render(Array.isArray(shares) ? shares : []);
    } catch (e) {
      replace(list, unavailable(e, 'drives.shares.unavailable'));
    }
  }

  addBtn.addEventListener('click', () => add(pathInput.value));
  pathInput.addEventListener('keydown', (e) => e.key === 'Enter' && add(pathInput.value));

  const inside = (pos) => {
    if (!pos) return false;
    const r = drop.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const x = pos.x / dpr;
    const y = pos.y / dpr;
    return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
  };
  const offDrop = onDragDrop((ev) => {
    if (!el.isConnected) return;
    if (ev.type === 'over' || ev.type === 'enter') drop.classList.toggle('over', inside(ev.position));
    else if (ev.type === 'leave') drop.classList.remove('over');
    else if (ev.type === 'drop') {
      const hit = inside(ev.position);
      drop.classList.remove('over');
      if (hit) for (const p of ev.paths || []) add(p);
    }
  });

  load();
  el.dispose = offDrop;
  return el;
}
