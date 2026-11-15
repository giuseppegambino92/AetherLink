// Remote file browser: the peer's VFS shares (no native mount on phones), browsed
// over the VFS RPC and downloaded into the app sandbox on tap.
import { h, replace } from '../../shared/js/lib/dom.js';
import { icon } from '../../shared/js/lib/icons.js';
import { formatBytes, formatRelative } from '../../shared/js/lib/format.js';
import { api, describeError } from '../api.js';
import { openSheet, listRow, iconButton, emptyState, toast } from '../ui.js';

export function openFileBrowser(sessionId) {
  const crumbs = h('div', { class: 'crumbs' });
  const list = h('div', { class: 'list' });
  const back = iconButton({ name: 'chevronRight', labelKey: 'files.up', className: 'icon-btn back', onClick: () => up() });
  const content = h('div', { class: 'sheet-body-wrap' }, h('div', { class: 'sheet-head' }, back, h('h2', { class: 'sheet-title', id: 'files-title', i18n: 'files.title' })), crumbs, list);
  openSheet({ content, full: true, labelledBy: 'files-title' });

  /** null = share list; otherwise {share, name, path} */
  let loc = null;

  function loading() {
    replace(list, h('div', { class: 'hint pad', i18n: 'common.loading' }));
  }

  async function showShares() {
    loc = null;
    back.hidden = true;
    replace(crumbs, h('span', { i18n: 'files.shares' }));
    loading();
    try {
      const shares = await api.vfsListShares(sessionId);
      if (!shares || !shares.length) return replace(list, emptyState({ iconName: 'folder', titleKey: 'files.noShares', bodyKey: 'files.noSharesBody' }));
      replace(
        list,
        shares.map((s) =>
          listRow({ glyph: icon('drive', 20), title: s.name, subKey: s.readOnly ? 'files.readOnly' : 'files.readWrite', trailing: icon('chevronRight', 16), onClick: () => openDir({ share: s.id, name: s.name, path: '/' }) }),
        ),
      );
    } catch (e) {
      replace(list, h('div', { class: 'hint pad', text: describeError(e) }));
    }
  }

  async function openDir(next) {
    loc = next;
    back.hidden = false;
    replace(crumbs, h('span', { class: 'crumb-share', text: next.name }), h('span', { class: 'mono crumb-path', text: next.path }));
    loading();
    try {
      const entries = await api.vfsListDir(sessionId, next.share, next.path);
      if (!entries.length) return replace(list, emptyState({ iconName: 'folder', titleKey: 'files.empty' }));
      replace(
        list,
        entries.map((e) =>
          listRow({
            glyph: icon(e.isDir ? 'folder' : 'textFormat', 20),
            title: e.name,
            sub: e.isDir ? formatRelative(e.modifiedMs) : `${formatBytes(e.size)} · ${formatRelative(e.modifiedMs)}`,
            trailing: e.isDir ? icon('chevronRight', 16) : icon('upload', 16, 'flip-y'),
            onClick: () => (e.isDir ? openDir({ ...next, path: e.path }) : download(e)),
          }),
        ),
      );
    } catch (e) {
      replace(list, h('div', { class: 'hint pad', text: describeError(e) }));
    }
  }

  async function download(entry) {
    toast({ key: 'files.downloading', args: { name: entry.name } });
    try {
      const res = await api.vfsDownload(sessionId, loc.share, entry.path);
      toast({ key: 'files.downloaded', args: { path: res.path || entry.name }, kind: 'success', timeout: 5000 });
    } catch (e) {
      toast({ message: describeError(e), kind: 'error' });
    }
  }

  function up() {
    if (!loc) return;
    if (loc.path === '/' || !loc.path) return showShares();
    const parent = loc.path.replace(/\/[^/]*\/?$/, '') || '/';
    openDir({ ...loc, path: parent });
  }

  showShares();
}
