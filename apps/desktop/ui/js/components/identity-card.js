// Local Identity Card: 9-digit ID, rotating one-time password with countdown
// ring, fingerprint and QR pairing code.
import { h, replace } from '../lib/dom.js';
import { icon, osIcon } from '../lib/icons.js';
import { api, isNotWired } from '../api.js';
import { card, emptyState, iconButton } from './controls.js';
import { copyWithToast, toastError } from './toast.js';
import { formatFingerprint, formatIdSpaced, idDigits, idGroups, osKey, toEpochMs } from '../lib/format.js';
import { renderQr } from './qr.js';
import { Spring, SPRINGS } from '../lib/spring.js';

const RING_R = 20;
const RING_C = 2 * Math.PI * RING_R;

export function identityCard({ onIdentity } = {}) {
  const body = h('div', { class: 'identity-body' });
  const el = card({ className: 'identity-card span-7', titleKey: 'dashboard.identity.title', subtitleKey: 'dashboard.identity.subtitle', iconName: 'shieldCheck' }, body);
  let identity = null;
  let fetchedAt = 0;
  let timer = 0;
  let masked = false;
  let refetching = false;

  const skeleton = () =>
    h(
      'div',
      { class: 'identity' },
      h('div', { class: 'stack' }, h('div', { class: 'skeleton', style: { width: '60%', height: '44px' } }), h('div', { class: 'skeleton', style: { width: '40%', height: '14px' } }), h('div', { class: 'skeleton', style: { width: '100%', height: '78px', marginTop: '16px' } })),
      h('div', { class: 'skeleton', style: { width: '172px', height: '196px' } }),
    );

  async function load() {
    replace(body, skeleton());
    try {
      setIdentity(await api.getIdentity());
    } catch (e) {
      showError(e);
    }
  }

  function showError(e) {
    clearInterval(timer);
    replace(
      body,
      emptyState({
        iconName: isNotWired(e) ? 'plug' : 'alert',
        titleKey: isNotWired(e) ? 'engine.notWiredTitle' : 'dashboard.identity.unavailable',
        bodyKey: isNotWired(e) ? 'engine.notWiredBody' : 'errors.tryAgain',
        action: h('button', { class: 'btn sm', on: { click: load } }, icon('refresh', 14), h('span', { i18n: 'common.retry' })),
      }),
    );
    onIdentity && onIdentity(null);
  }

  function setIdentity(id) {
    identity = id;
    fetchedAt = Date.now();
    refetching = false;
    render();
    onIdentity && onIdentity(id);
  }

  async function rotate() {
    try {
      setIdentity(await api.rotatePassword());
    } catch (e) {
      toastError(e);
    }
  }

  function render() {
    const id = identity;
    const digits = idDigits(id.deviceId);
    const ringFg = h('svg:circle', { class: 'ring-fg', attrs: { cx: 23, cy: 23, r: RING_R, fill: 'none', 'stroke-width': 3, 'stroke-linecap': 'round', 'stroke-dasharray': RING_C.toFixed(2), 'stroke-dashoffset': '0' } });
    const ringLabel = h('div', { class: 'ring-label' });
    const ring = h(
      'div',
      { class: 'otp-ring', i18nAttr: { 'aria-label': 'dashboard.identity.expiresIn' } },
      h(
        'svg:svg',
        { attrs: { width: 46, height: 46, viewBox: '0 0 46 46' } },
        h('svg:defs', {}, h('svg:linearGradient', { attrs: { id: 'ringGradient', x1: '0', y1: '0', x2: '1', y2: '1' } }, h('svg:stop', { attrs: { offset: '0', 'stop-color': '#7de0d6' } }), h('svg:stop', { attrs: { offset: '1', 'stop-color': '#8ab8ff' } }))),
        h('svg:circle', { class: 'ring-bg', attrs: { cx: 23, cy: 23, r: RING_R, fill: 'none', 'stroke-width': 3 } }),
        ringFg,
      ),
      ringLabel,
    );
    const pwd = h('div', { class: `otp-value${masked ? ' masked' : ''}`, text: id.password ? (masked ? '•'.repeat(String(id.password).length) : id.password) : '—' });
    const eye = iconButton({ name: masked ? 'eye' : 'eyeOff', labelKey: masked ? 'dashboard.identity.showPassword' : 'dashboard.identity.hidePassword', onClick: () => ((masked = !masked), render()) });

    const qrBox = h('div', { class: 'qr-tile' });
    if (id.qrPayload) {
      const canvas = renderQr(String(id.qrPayload), 148);
      if (canvas) qrBox.appendChild(canvas);
      else qrBox.appendChild(h('div', { class: 'qr-empty' }, icon('qr', 28)));
    } else {
      qrBox.appendChild(h('div', { class: 'qr-empty' }, icon('qr', 28)));
    }
    qrBox.appendChild(h('span', { class: 'hint', i18n: 'dashboard.identity.scanToPair' }));

    const digitsEl = h('div', { class: 'id-digits', attrs: { 'aria-live': 'polite' } }, idGroups(digits).map((g) => h('span', { class: 'grp', text: g })));
    // Spring "settle" when the ID first appears.
    const s = new Spring({ ...SPRINGS.bouncy, value: 0, onUpdate: (v) => ((digitsEl.style.opacity = Math.min(1, v).toFixed(3)), (digitsEl.style.transform = `translateY(${((1 - v) * 10).toFixed(2)}px)`)) });
    s.to(1);

    replace(
      body,
      h(
        'div',
        { class: 'identity' },
        h(
          'div',
          {},
          h('div', { class: 'section-label', i18n: 'dashboard.identity.yourId' }),
          h(
            'div',
            { class: 'id-number' },
            digitsEl,
            iconButton({ name: 'copy', labelKey: 'dashboard.identity.copyId', className: 'icon-btn bordered', onClick: () => copyWithToast(api, digits, 'dashboard.identity.idCopied') }),
          ),
          h(
            'div',
            { class: 'id-meta' },
            h('span', { class: 'row', style: { gap: '6px' } }, osIcon(osKey(id.os), 14), h('span', { text: id.deviceName || '' })),
            id.fingerprint && h('span', { class: 'row', style: { gap: '6px' }, i18nAttr: { title: 'dashboard.identity.fingerprint' } }, icon('fingerprint', 14), h('span', { class: 'mono', text: formatFingerprint(id.fingerprint) })),
          ),
          h(
            'div',
            { class: 'otp inset' },
            ring,
            h('div', { class: 'spacer' }, h('div', { class: 'section-label', i18n: 'dashboard.identity.oneTimePassword' }), pwd),
            eye,
            iconButton({ name: 'copy', labelKey: 'dashboard.identity.copyPassword', onClick: () => id.password && copyWithToast(api, String(id.password), 'dashboard.identity.passwordCopied') }),
            iconButton({ name: 'refresh', labelKey: 'dashboard.identity.regenerate', onClick: rotate }),
          ),
        ),
        qrBox,
      ),
    );

    clearInterval(timer);
    const tick = () => {
      const exp = toEpochMs(id.passwordExpiresAt);
      if (!exp) {
        ringFg.setAttribute('stroke-dashoffset', '0');
        ringLabel.textContent = '';
        return;
      }
      const remaining = Math.max(0, exp - Date.now());
      const period = Math.max(exp - fetchedAt, 1000);
      ringFg.setAttribute('stroke-dashoffset', (RING_C * (1 - remaining / period)).toFixed(2));
      ringLabel.textContent = String(Math.ceil(remaining / 1000));
      if (remaining <= 0 && !refetching) {
        refetching = true;
        api.getIdentity().then(setIdentity, (e) => (isNotWired(e) ? showError(e) : null));
      }
    };
    tick();
    timer = setInterval(tick, 1000);
  }

  load();
  el.reload = load;
  el.formattedId = () => (identity ? formatIdSpaced(identity.deviceId) : null);
  return el;
}
