// Shared observable state for the dashboard window: host status (polled) and
// the facts mirrored into the menu-bar status item.
import { api, on } from './api.js';

function createStore(initial) {
  let state = initial;
  const subs = new Set();
  return {
    get: () => state,
    set(next) {
      state = { ...state, ...next };
      for (const fn of subs) fn(state);
    },
    subscribe(fn) {
      subs.add(fn);
      fn(state);
      return () => subs.delete(fn);
    },
  };
}

// ---- host status (inbound sessions)
const host = createStore({ status: null, error: null });
let pollTimer = 0;

async function refreshHost() {
  try {
    host.set({ status: await api.hostStatus(), error: null });
  } catch (error) {
    host.set({ error });
  }
}

export const hostStore = {
  subscribe: host.subscribe,
  get: host.get,
  refresh: refreshHost,
  start(intervalMs = 5000) {
    if (pollTimer) return;
    refreshHost();
    pollTimer = setInterval(refreshHost, intervalMs);
    // engine pushes session-state for host sessions too; refresh eagerly
    on('session-state', () => refreshHost());
  },
};

// ---- tray mirror
const tray = createStore({ deviceId: null, activeConnections: 0, ghostMode: false });
let trayPushTimer = 0;

export const trayStore = {
  get: tray.get,
  subscribe: tray.subscribe,
  set(patch) {
    tray.set(patch);
    clearTimeout(trayPushTimer);
    trayPushTimer = setTimeout(() => api.uiTrayState(tray.get()).catch(() => {}), 50);
  },
};
