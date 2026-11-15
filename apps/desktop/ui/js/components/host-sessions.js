// "Connected to this device": inbound host sessions with a kick button.
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, isNotWired } from '../api.js';
import { card, emptyState } from './controls.js';
import { confirmSheet } from './sheet.js';
import { toastError } from './toast.js';
import { formatIdSpaced, formatRelative, toEpochMs } from '../lib/format.js';
import { hostStore } from '../stores.js';

export function hostSessionsCard({ className = 'span-5' } = {}) {
  const list = h('div', { class: 'stack' });
  const el = card({ className, titleKey: 'dashboard.host.title', subtitleKey: 'dashboard.host.subtitle', iconName: 'activity', iconTone: 'aqua' }, list);

  const render = ({ status, error }) => {
    if (error) {
      replace(list, emptyState({ iconName: isNotWired(error) ? 'plug' : 'alert', titleKey: isNotWired(error) ? 'engine.notWiredTitle' : 'dashboard.host.unavailable', bodyKey: isNotWired(error) ? 'engine.notWiredBody' : 'errors.tryAgain' }));
      return;
    }
    if (!status) return;
    const sessions = Array.isArray(status.sessions) ? status.sessions : [];
    if (!sessions.length) {
      replace(list, emptyState({ iconName: 'shield', titleKey: 'dashboard.host.emptyTitle', bodyKey: status.listening === false ? 'dashboard.host.notListening' : 'dashboard.host.emptyBody' }));
      return;
    }
    replace(
      list,
      sessions.map((s) => {
        const since = toEpochMs(s.since);
        const name = s.peerName || formatIdSpaced(s.deviceId);
        return h(
          'div',
          { class: 'host-session' },
          h('span', { class: 'dot online' }),
          h(
            'div',
            { class: 'spacer', style: { minWidth: '0' } },
            h('div', { class: 'row-title', text: name }),
            h('div', { class: 'row-sub' }, since ? h('span', { i18n: 'dashboard.host.since', i18nArgs: { when: formatRelative(since) } }) : null),
          ),
          s.ghostMode && h('span', { class: 'chip accent' }, icon('ghost', 12), h('span', { i18n: 'session.ghost' })),
          h(
            'button',
            {
              class: 'btn sm danger',
              on: {
                click: async () => {
                  const ok = await confirmSheet({ titleKey: 'dashboard.host.kickTitle', bodyKey: 'dashboard.host.kickBody', args: { name }, confirmKey: 'dashboard.host.kick', danger: true });
                  if (!ok) return;
                  try {
                    await api.kickSession(s.sessionId);
                    hostStore.refresh();
                  } catch (e) {
                    toastError(e);
                  }
                },
              },
            },
            h('span', { i18n: 'dashboard.host.kick' }),
          ),
        );
      }),
    );
  };

  replace(list, h('div', { class: 'skeleton', style: { height: '64px' } }));
  el.dispose = hostStore.subscribe(render);
  return el;
}
