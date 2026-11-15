// "Share this device": start/stop screen sharing (Android MediaProjection, iOS
// ReplayKit broadcast), live encoder status, connected viewers, and the
// permission onboarding needed to be controlled (Android) — or the explanation
// of why iOS is view-only.
import { h, replace } from '../../shared/js/lib/dom.js';
import { icon, osIcon } from '../../shared/js/lib/icons.js';
import { formatIdSpaced, formatBytes, formatNumber, formatRelative, toEpochMs, osKey } from '../../shared/js/lib/format.js';
import { t, tp } from '../../shared/js/i18n.js';
import { api, describeError } from '../api.js';
import { card, emptyState, iconButton, listRow, toggleSwitch, confirmSheet, toast, toastError, haptic } from '../ui.js';

const AUDIO_KEY = 'aetherlink.mobile.shareAudio';

export function mountHost(root, platform) {
  const timers = [];
  let status = null;
  let busy = false;
  let audio = true;
  try {
    audio = localStorage.getItem(AUDIO_KEY) !== '0';
  } catch {
    /* storage unavailable */
  }

  const page = h(
    'div',
    { class: 'page' },
    h('header', { class: 'page-head' }, h('span', { class: 'brand-mark' }, icon('upload', 22)), h('div', { class: 'page-head-text' }, h('h1', { class: 'page-title', i18n: 'host.title' }), h('p', { class: 'page-sub', i18n: 'host.subtitle' }))),
  );
  root.appendChild(page);

  if (!platform || !platform.canHost) {
    page.appendChild(card({ titleKey: 'host.unavailableTitle', iconName: 'info' }, h('p', { class: 'card-text', i18n: 'host.unavailableBody' })));
    return () => {};
  }
  const isIos = platform.os === 'ios';

  // ---------------------------------------------------------------- share control
  const orb = h('button', { class: 'share-orb', attrs: { type: 'button' }, on: { click: () => toggleShare() } }, icon('upload', 34));
  const stateTitle = h('div', { class: 'share-state' });
  const stateSub = h('div', { class: 'share-sub num' });
  const audioRow = isIos
    ? null
    : h(
        'div',
        { class: 'row-between' },
        h('span', { class: 'field-label', i18n: 'host.shareAudio' }),
        toggleSwitch({
          checked: audio,
          labelKey: 'host.shareAudio',
          onChange: (v) => {
            audio = v;
            try {
              localStorage.setItem(AUDIO_KEY, v ? '1' : '0');
            } catch {
              /* storage unavailable */
            }
          },
        }),
      );
  const shareCard = card({ titleKey: 'host.shareTitle', iconName: 'monitor' }, h('div', { class: 'share-hero' }, orb, stateTitle, stateSub), audioRow, isIos && h('p', { class: 'hint', i18n: 'host.iosPickerHint' }));

  // ---------------------------------------------------------------- how to connect
  const idLine = h('div', { class: 'my-id num small' });
  const pwLine = h('div', { class: 'pw num' });
  const credCard = card({ titleKey: 'host.credentialsTitle', iconName: 'key' }, h('p', { class: 'card-text', i18n: 'host.credentialsBody' }), h('div', { class: 'cred-grid' }, h('span', { class: 'field-label', i18n: 'host.id' }), idLine, h('span', { class: 'field-label', i18n: 'home.password' }), pwLine));
  async function loadIdentity() {
    try {
      const id = await api.getIdentity();
      idLine.textContent = formatIdSpaced(id.deviceId);
      pwLine.textContent = id.password;
    } catch (e) {
      idLine.textContent = describeError(e);
    }
  }

  // ---------------------------------------------------------------- viewers
  const viewersList = h('div', { class: 'list' });
  function renderViewers(sessions) {
    if (!sessions || !sessions.length) return replace(viewersList, emptyState({ iconName: 'user', titleKey: 'host.noViewers', bodyKey: 'host.noViewersBody' }));
    replace(
      viewersList,
      sessions.map((s) =>
        listRow({
          glyph: osIcon(osKey(s.os), 20),
          title: s.peerName || formatIdSpaced(s.deviceId),
          sub: `${formatIdSpaced(s.deviceId)} · ${t('host.since', { time: formatRelative(toEpochMs(s.sinceMs)) })}`,
          trailing: iconButton({
            name: 'power',
            labelKey: 'host.kick',
            onClick: async () => {
              const ok = await confirmSheet({ titleKey: 'host.kickTitle', bodyKey: 'host.kickBody', args: { name: s.peerName || formatIdSpaced(s.deviceId) }, confirmKey: 'host.kick', danger: true });
              if (ok) api.kickSession(s.sessionId).catch(toastError);
            },
          }),
        }),
      ),
    );
  }

  // ---------------------------------------------------------------- control onboarding
  const controlBody = h('div', { class: 'stack' });
  const controlCard = card({ titleKey: isIos ? 'host.iosControlTitle' : 'host.controlTitle', iconName: isIos ? 'lock' : 'move' }, controlBody);

  function step({ done, titleKey, bodyKey, actionKey, onAction, stateKey }) {
    return h(
      'div',
      { class: `step ${done ? 'done' : ''}` },
      h('span', { class: 'step-glyph' }, icon(done ? 'check' : 'info', 16)),
      h('div', { class: 'step-main' }, h('div', { class: 'step-title', i18n: titleKey }), h('div', { class: 'step-body', i18n: bodyKey }), stateKey && h('div', { class: 'step-state', i18n: stateKey })),
      actionKey && !done && h('button', { class: 'btn sm', attrs: { type: 'button' }, i18n: actionKey, on: { click: () => (haptic(), onAction()) } }),
    );
  }

  async function renderControl() {
    if (isIos) {
      replace(
        controlBody,
        h('p', { class: 'card-text', i18n: 'host.iosViewOnly' }),
        h('div', { class: 'option' }, icon('keyboard', 18), h('div', {}, h('div', { class: 'step-title', i18n: 'host.iosOptionHidTitle' }), h('div', { class: 'step-body', i18n: 'host.iosOptionHidBody' }))),
        h('div', { class: 'option' }, icon('plug', 18), h('div', {}, h('div', { class: 'step-title', i18n: 'host.iosOptionDongleTitle' }), h('div', { class: 'step-body', i18n: 'host.iosOptionDongleBody' }))),
      );
      return;
    }
    let st;
    try {
      st = await api.inputStatus();
    } catch (e) {
      replace(controlBody, h('p', { class: 'hint', text: describeError(e) }));
      return;
    }
    const backend = st.backend || 'none';
    replace(
      controlBody,
      h('div', { class: `backend-chip ${backend !== 'none' ? 'ok' : ''}` }, icon(backend !== 'none' ? 'shieldCheck' : 'alert', 16), h('span', { i18n: `host.backend.${['shizuku', 'accessibility'].includes(backend) ? backend : 'none'}` })),
      step({
        done: st.accessibility === 'enabled',
        titleKey: 'host.accessibilityTitle',
        bodyKey: 'host.accessibilityBody',
        actionKey: 'host.openSettings',
        onAction: () => api.openAccessibilitySettings().catch(toastError),
      }),
      step({
        done: st.shizuku === 'ready',
        titleKey: 'host.shizukuTitle',
        bodyKey: 'host.shizukuBody',
        stateKey: `host.shizukuState.${['ready', 'noPermission', 'unavailable'].includes(st.shizuku) ? st.shizuku : 'unavailable'}`,
        actionKey: st.shizuku === 'noPermission' ? 'host.grant' : null,
        onAction: () => api.requestShizuku().then(renderControl).catch(toastError),
      }),
      step({
        done: false,
        titleKey: 'host.permissionsTitle',
        bodyKey: 'host.permissionsBody',
        actionKey: 'host.grant',
        onAction: () => api.requestPermissions(['notifications', 'audio']).then(renderControl).catch(toastError),
      }),
    );
  }

  // ---------------------------------------------------------------- status
  function renderStatus() {
    const s = status || {};
    const sharing = !!s.sharing;
    orb.classList.toggle('on', sharing);
    orb.setAttribute('aria-pressed', String(sharing));
    orb.dataset.i18nAttr = `aria-label:${sharing ? 'host.stop' : 'host.start'}`;
    orb.setAttribute('aria-label', t(sharing ? 'host.stop' : 'host.start'));
    replace(stateTitle, h('span', { i18n: sharing ? 'host.sharing' : 'host.notSharing' }));
    if (sharing && s.width) {
      stateSub.textContent = [
        tp('host.viewers', s.viewers || 0),
        `${s.codec || ''} ${s.width}×${s.height} @ ${formatNumber(s.fps)} ${t('session.units.fps')}`,
        t('host.sent', { frames: formatNumber(s.frames), bytes: formatBytes(s.bytes) }),
      ].join(' · ');
    } else stateSub.textContent = t(sharing ? 'host.waitingEncoder' : isIos ? 'host.tapToBroadcast' : 'host.tapToShare');
    renderViewers(s.sessions);
  }

  async function poll() {
    try {
      status = await api.hostShareStatus();
      renderStatus();
    } catch (e) {
      stateSub.textContent = describeError(e);
    }
  }

  async function toggleShare() {
    if (busy) return;
    busy = true;
    haptic(14);
    try {
      if (status && status.sharing) await api.hostShareStop();
      else {
        const res = await api.hostShareStart(audio);
        if (res && res.picker) toast({ key: 'host.iosPickerHint', timeout: 6000 });
      }
    } catch (e) {
      toastError(e);
    } finally {
      busy = false;
      poll();
    }
  }

  page.append(shareCard, credCard, card({ titleKey: 'host.viewersTitle', iconName: 'user' }, viewersList), controlCard);
  renderStatus();
  poll();
  loadIdentity();
  renderControl();
  timers.push(setInterval(poll, 1000));
  timers.push(setInterval(loadIdentity, 15000));
  // Returning from Android Settings: refresh the checklist.
  const onVis = () => !document.hidden && (renderControl(), poll());
  document.addEventListener('visibilitychange', onVis);

  return () => {
    for (const id of timers) clearInterval(id);
    document.removeEventListener('visibilitychange', onVis);
  };
}
