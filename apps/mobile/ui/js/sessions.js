// Hands connect parameters from the home screen to the session route (in memory:
// passwords never go into the URL).
import { formatIdSpaced, idDigits } from '../shared/js/lib/format.js';

let pending = null;

/** @param {{target:string, password?:string|null, name?:string, os?:string}} p */
export function openSession(p) {
  const target = String(p.target || '').trim();
  pending = { target: /^[\d\s-]+$/.test(target) ? idDigits(target) : target, password: p.password || null, name: p.name || '', os: p.os || '' };
  location.hash = '#/session';
}

export function takePending() {
  const p = pending;
  pending = null;
  return p;
}

export const displayName = (p) => (p && (p.name || (/^\d{9}$/.test(p.target) ? formatIdSpaced(p.target) : p.target))) || '';

/** Parse an `aetherlink://connect?id=…&pk=…&port=…` QR payload (or a bare 9-digit ID). */
export function parseConnectPayload(text) {
  const s = String(text || '').trim();
  if (/^\d[\d\s-]{8,12}$/.test(s) && idDigits(s).length === 9) return { id: idDigits(s) };
  let url;
  try {
    url = new URL(s);
  } catch {
    return null;
  }
  if (url.protocol !== 'aetherlink:') return null;
  // `aetherlink://connect?…` parses with host "connect" (or pathname "//connect").
  const action = (url.host || url.pathname.replace(/^\/+/, '')).toLowerCase();
  if (action !== 'connect') return null;
  const id = idDigits(url.searchParams.get('id') || '');
  if (id.length !== 9) return null;
  const port = Number(url.searchParams.get('port')) || null;
  const pk = (url.searchParams.get('pk') || '').toLowerCase();
  return { id, port, pk: /^[0-9a-f]{64}$/.test(pk) ? pk : null, password: url.searchParams.get('pw') || null };
}
