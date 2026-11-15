// Home: my ID + one-time password (+ QR), connect by ID / QR scan, LAN peers,
// trusted devices.
import { h, replace } from '../../shared/js/lib/dom.js';
import { icon, osIcon, brandMark } from '../../shared/js/lib/icons.js';
import { formatIdSpaced, formatIdDashed, idDigits, isValidId, osKey, formatClock, toEpochMs, formatFingerprint } from '../../shared/js/lib/format.js';
import { renderQr } from '../../shared/js/components/qr.js';
import { t, tp } from '../../shared/js/i18n.js';
import { api, on, describeError } from '../api.js';
import { openSession, parseConnectPayload } from '../sessions.js';
import { card, emptyState, iconButton, listRow, openSheet, confirmSheet, copyWithToast, toast, toastError, toastSuccess, haptic } from '../ui.js';
import { openSettings } from './settings.js';

export function mountHome(root) {
  const offs = [];
  const timers = [];

  // ---------------------------------------------------------------- identity
  const idEl = h('div', { class: 'my-id num' });
  const pwEl = h('span', { class: 'pw num' });
  const pwTimer = h('span', { class: 'pw-timer num' });
  const nameEl = h('div', { class: 'my-name' });
  let identity = null;
  let pwVisible = false;
  const eyeBtn = iconButton({ name: 'eye', labelKey: 'home.showPassword', onClick: () => ((pwVisible = !pwVisible), renderIdentity()) });

  function renderIdentity() {
    if (!identity) return;
    idEl.textContent = formatIdSpaced(identity.deviceId);
    nameEl.textContent = identity.deviceName || '';
    pwEl.textContent = pwVisible ? identity.password : '••••••';
    replace(eyeBtn, icon(pwVisible ? 'eyeOff' : 'eye', 20));
    tickPw();
  }
  function tickPw() {
    const exp = identity && toEpochMs(identity.passwordExpiresAt);
    pwTimer.textContent = exp ? t('home.passwordRotates', { time: formatClock((exp - Date.now()) / 1000) }) : '';
    if (exp && exp - Date.now() < 0) loadIdentity();
  }
  timers.push(setInterval(tickPw, 1000));

  async function loadIdentity() {
    try {
      identity = await api.getIdentity();
      renderIdentity();
    } catch (e) {
      replace(identityCard, h('p', { class: 'hint pad', text: describeError(e) }));
    }
  }

  function showQr() {
    if (!identity) return;
    const qr = renderQr(identity.qrPayload, 220);
    const content = h(
      'div',
      { class: 'sheet-body-wrap center' },
      h('h2', { class: 'sheet-title', id: 'qr-title', i18n: 'home.qrTitle' }),
      h('p', { class: 'sheet-text', i18n: 'home.qrBody' }),
      h('div', { class: 'qr-tile' }, qr || h('span', { class: 'hint', i18n: 'home.qrTooLong' })),
      h('div', { class: 'my-id num small', text: formatIdSpaced(identity.deviceId) }),
      h('div', { class: 'fingerprint mono', text: formatFingerprint(identity.fingerprint) }),
    );
    openSheet({ content, labelledBy: 'qr-title' });
  }

  const identityCard = card(
    { titleKey: 'home.myDevice', iconName: 'shieldCheck', action: iconButton({ name: 'qr', labelKey: 'home.showQr', onClick: showQr }) },
    nameEl,
    h('button', { class: 'id-button', attrs: { type: 'button' }, i18nAttr: { 'aria-label': 'home.copyId' }, on: { click: () => identity && copyWithToast(identity.deviceId, 'home.idCopied') } }, idEl, icon('copy', 16)),
    h(
      'div',
      { class: 'pw-row' },
      h('span', { class: 'pw-label', i18n: 'home.password' }),
      pwEl,
      h('span', { class: 'spacer' }),
      eyeBtn,
      iconButton({ name: 'copy', labelKey: 'home.copyPassword', onClick: () => identity && copyWithToast(identity.password, 'home.passwordCopied') }),
      iconButton({
        name: 'refresh',
        labelKey: 'home.rotatePassword',
        onClick: async () => {
          try {
            identity = await api.rotatePassword();
            renderIdentity();
            toastSuccess('home.passwordRotated');
          } catch (e) {
            toastError(e);
          }
        },
      }),
    ),
    pwTimer,
  );

  // ---------------------------------------------------------------- connect
  const idInput = h('input', { class: 'input id-input num', attrs: { inputmode: 'numeric', autocomplete: 'off', maxlength: '11', enterkeyhint: 'next' }, i18nAttr: { placeholder: 'home.idPlaceholder', 'aria-label': 'home.remoteId' } });
  const pwInput = h('input', { class: 'input', attrs: { type: 'password', autocomplete: 'off', enterkeyhint: 'go' }, i18nAttr: { placeholder: 'home.passwordPlaceholder', 'aria-label': 'home.password' } });
  idInput.addEventListener('input', () => {
    const v = idInput.value;
    if (/^[\d\s-]*$/.test(v)) idInput.value = formatIdDashed(v);
  });
  idInput.addEventListener('keydown', (e) => e.key === 'Enter' && pwInput.focus());
  pwInput.addEventListener('keydown', (e) => e.key === 'Enter' && connect());

  function connect() {
    const raw = idInput.value.trim();
    const target = /^[\d\s-]+$/.test(raw) ? idDigits(raw) : raw;
    if (!(isValidId(target) || target.includes(':'))) {
      toast({ key: 'errors.invalidTarget', kind: 'error' });
      return;
    }
    haptic();
    openSession({ target, password: pwInput.value });
  }

  async function scan() {
    try {
      const text = await api.scanQr();
      const p = parseConnectPayload(text);
      if (!p) {
        toast({ key: 'home.qrInvalid', kind: 'error' });
        return;
      }
      haptic(20);
      idInput.value = formatIdDashed(p.id);
      if (p.password) return openSession({ target: p.id, password: p.password });
      toast({ key: 'home.qrScanned' });
      pwInput.focus();
    } catch (e) {
      toastError(e);
    }
  }

  const connectCard = card(
    { titleKey: 'home.connectTitle', iconName: 'link' },
    h('label', { class: 'field' }, h('span', { class: 'field-label', i18n: 'home.remoteId' }), idInput),
    h('label', { class: 'field' }, h('span', { class: 'field-label', i18n: 'home.password' }), pwInput),
    h(
      'div',
      { class: 'btn-row' },
      h('button', { class: 'btn primary lg grow', attrs: { type: 'button' }, on: { click: connect } }, icon('arrowRight', 18), h('span', { i18n: 'home.connect' })),
      h('button', { class: 'btn lg', attrs: { type: 'button' }, on: { click: scan } }, icon('qr', 18), h('span', { i18n: 'home.scanQr' })),
    ),
  );

  // ---------------------------------------------------------------- LAN peers
  const lanList = h('div', { class: 'list' });
  const lanCount = h('span', { class: 'chip' });
  function renderLan(peers) {
    lanCount.textContent = tp('home.lanCount', peers.length);
    if (!peers.length) return replace(lanList, emptyState({ iconName: 'radar', titleKey: 'home.lanEmpty', bodyKey: 'home.lanEmptyBody' }));
    replace(
      lanList,
      peers.map((p) =>
        listRow({
          glyph: osIcon(osKey(p.os), 20),
          title: p.name || formatIdSpaced(p.deviceId),
          sub: `${formatIdSpaced(p.deviceId)} · ${t(`os.${osKey(p.os)}`)}`,
          trailing: p.trusted ? h('span', { class: 'chip accent', i18n: 'home.trusted' }) : icon('chevronRight', 16),
          onClick: () => openSession({ target: p.deviceId, name: p.name, os: osKey(p.os) }),
        }),
      ),
    );
  }
  async function loadLan() {
    try {
      renderLan(await api.listLanPeers());
    } catch (e) {
      replace(lanList, h('p', { class: 'hint pad', text: describeError(e) }));
    }
  }
  offs.push(on('lan-peers-changed', (peers) => Array.isArray(peers) && renderLan(peers)));

  // ---------------------------------------------------------------- trusted
  const trustedList = h('div', { class: 'list' });
  async function loadTrusted() {
    try {
      const list = await api.listTrustedDevices();
      if (!list.length) return replace(trustedList, emptyState({ iconName: 'shield', titleKey: 'home.trustedEmpty', bodyKey: 'home.trustedEmptyBody' }));
      replace(
        trustedList,
        list.map((d) =>
          listRow({
            glyph: h('span', { class: `presence ${d.online ? 'on' : ''}` }, icon('monitor', 20)),
            title: d.name || formatIdSpaced(d.deviceId),
            sub: `${formatIdSpaced(d.deviceId)} · ${t(d.online ? 'home.online' : 'home.offline')}`,
            trailing: iconButton({
              name: 'trash',
              labelKey: 'home.forget',
              onClick: async (e) => {
                e.stopPropagation();
                const ok = await confirmSheet({ titleKey: 'home.forgetTitle', bodyKey: 'home.forgetBody', args: { name: d.name || formatIdSpaced(d.deviceId) }, confirmKey: 'home.forget', danger: true });
                if (!ok) return;
                try {
                  await api.removeTrustedDevice(d.deviceId);
                  loadTrusted();
                } catch (err) {
                  toastError(err);
                }
              },
            }),
            onClick: () => openSession({ target: d.deviceId, name: d.name }),
          }),
        ),
      );
    } catch (e) {
      replace(trustedList, h('p', { class: 'hint pad', text: describeError(e) }));
    }
  }

  // ---------------------------------------------------------------- layout
  const page = h(
    'div',
    { class: 'page' },
    h(
      'header',
      { class: 'page-head' },
      h('span', { class: 'brand-mark' }, brandMark(24)),
      h('div', { class: 'page-head-text' }, h('h1', { class: 'page-title', i18n: 'app.name' }), h('p', { class: 'page-sub', i18n: 'home.subtitle' })),
      iconButton({ name: 'settings', labelKey: 'nav.settings', onClick: () => openSettings() }),
    ),
    identityCard,
    connectCard,
    card({ titleKey: 'home.lanTitle', iconName: 'wifi', action: lanCount }, lanList),
    card({ titleKey: 'home.trustedTitle', iconName: 'shieldCheck' }, trustedList),
  );
  root.appendChild(page);

  loadIdentity();
  loadLan();
  loadTrusted();
  offs.push(on('settings-changed', () => loadIdentity()));

  return () => {
    for (const off of offs) off();
    for (const id of timers) clearInterval(id);
  };
}
