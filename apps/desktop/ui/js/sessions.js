// Dashboard-side session launcher + recent targets (UI-local convenience
// list; passwords are never stored).
import { api } from './api.js';
import { toastError } from './components/toast.js';

const RECENTS_KEY = 'aetherlink.recentTargets';
const MAX_RECENTS = 5;

export function recentTargets() {
  try {
    const v = JSON.parse(localStorage.getItem(RECENTS_KEY) || '[]');
    return Array.isArray(v) ? v.filter((r) => r && typeof r.target === 'string') : [];
  } catch {
    return [];
  }
}

function remember(target) {
  const list = [{ target, at: Date.now() }, ...recentTargets().filter((r) => r.target !== target)].slice(0, MAX_RECENTS);
  try {
    localStorage.setItem(RECENTS_KEY, JSON.stringify(list));
  } catch {
    /* storage unavailable */
  }
}

/** Open a session viewport window for `target` (9-digit ID or LAN peer id). */
export async function openSession({ target, password = null, title }) {
  remember(String(target));
  try {
    await api.uiOpenSessionWindow(String(target), password, title || String(target));
  } catch (e) {
    toastError(e);
  }
}
