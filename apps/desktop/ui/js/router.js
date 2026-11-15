// Hash router. Dashboard sections live under #/home, #/devices, #/drives,
// #/iphone, #/settings. Session windows (label `session-*`) always render the session view.

export const SECTIONS = ['home', 'devices', 'drives', 'iphone', 'settings'];

export function parseRoute(hash = location.hash) {
  const clean = hash.replace(/^#\/?/, '');
  const [path, query = ''] = clean.split('?');
  const name = path.split('/')[0] || 'home';
  return { name, params: Object.fromEntries(new URLSearchParams(query)) };
}

export function navigate(name) {
  const target = `#/${name}`;
  if (location.hash !== target) location.hash = target;
}

export function onRoute(cb) {
  const fire = () => cb(parseRoute());
  addEventListener('hashchange', fire);
  return () => removeEventListener('hashchange', fire);
}
