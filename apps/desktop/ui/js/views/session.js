// Active Session Viewport (session-* windows): full-bleed canvas renderer,
// input capture and the floating instruments (HUD, palette, ports drawer,
// Quake terminal, DVR, transfers, PiP radar).
import { h, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { api, on, describeError, isPreview } from '../api.js';
import { FrameRenderer } from '../lib/renderer.js';
import { InputCapture } from '../lib/input.js';
import { createHud } from '../components/hud-pill.js';
import { createHandshake } from '../components/handshake.js';
import { createPortsDrawer } from '../components/ports-drawer.js';
import { createQuakeTerminal } from '../components/quake-terminal.js';
import { createDvr } from '../components/dvr-scrubber.js';
import { createTransfers } from '../components/transfer-pill.js';
import { createPip } from '../components/pip-radar.js';
import { openActionPalette } from '../components/action-palette.js';
import { toggleSwitch } from '../components/controls.js';
import { toast, toastError, toastSuccess } from '../components/toast.js';
import { loadHudHotkey, matchesAccelerator } from '../lib/hotkeys.js';
import { formatIdSpaced } from '../lib/format.js';
import { t } from '../i18n.js';

export async function mountSession(root) {
  document.documentElement.classList.add('is-session');
  const canvas = h('canvas', { class: 'screen', attrs: { width: 16, height: 9 } });
  const viewport = h('div', { class: 'viewport local-cursor' }, canvas);
  const shell = h('div', { class: 'session' }, viewport);
  root.appendChild(shell);

  const pending = await api.uiTakePendingConnect().catch(() => null);
  const peerName = pending?.title || (pending?.target ? formatIdSpaced(pending.target) : '');
  document.title = peerName ? t('session.windowTitle', { name: peerName }) : t('app.name');

  const handshake = createHandshake({ root: shell, peerName });
  if (!pending || !pending.target) {
    handshake.showError(t('session.noPending'), [h('button', { class: 'btn', on: { click: () => api.closeWindow() } }, h('span', { i18n: 'common.close' }))]);
    return;
  }

  // ---------------------------------------------------------------- state
  let sessionId = null;
  let connected = false;
  let ghost = false;
  // Granted by the host (`session-permissions`); until it arrives assume the defaults.
  let permissions = null;
  let lossless = true;
  let displays = [];
  let activeDisplay = null;
  let overlays = 0; // palette / sheets that need the keyboard
  let lastPointer = null;

  const renderer = new FrameRenderer(viewport, canvas, {
    onConfig: (cfg) => {
      activeDisplay = cfg.displayId ?? activeDisplay;
      if (!displays.length) pip.setDisplays([], activeDisplay);
    },
    onFirstFrame: () => markConnected(),
    onDecoderError: (code, detail) => toast({ message: t(`session.decoder.${code}`, { detail: detail || '' }), kind: 'error', timeout: 6000 }),
    onView: (rect, z) => pip && pip.onView(rect, z),
    onCursor: (c) => {
      if (pip && pip.isFollowing() && renderer.zoom > 1.01 && c && c.visible !== false) renderer.centerOn(c.x, c.y);
    },
  });

  const keyboardFree = () => overlays === 0 && !(terminal && terminal.isOpen()) && !document.querySelector('.overlay');
  const input = new InputCapture({
    surface: viewport,
    renderer,
    // View-only sessions: the host drops input anyway; don't send it.
    send: (ev) => sessionId && connected && permissions?.input !== false && api.sendInput(sessionId, ev),
    isEnabled: () => connected && keyboardFree() && !document.activeElement?.closest?.('.drawer, .dvr, .hud, .quake, input, textarea'),
    onPointer: (n) => {
      lastPointer = n;
      if (pip.isFollowing() && renderer.zoom > 1.01) renderer.centerOn(n.x, n.y);
    },
    intercept: (e) => {
      // Local shortcuts (everything else goes to the remote machine).
      if ((e.metaKey || e.ctrlKey) && !e.altKey && e.code === 'KeyK') {
        e.preventDefault();
        if (permissions?.semanticUi !== false && permissions?.input !== false) openPalette();
        return true;
      }
      if (e.ctrlKey && e.code === 'Backquote') {
        e.preventDefault();
        if (terminal && permissions?.terminal === true) terminal.toggle();
        return true;
      }
      // In Tauri the HUD hotkey is a global shortcut (see shell.rs); the
      // browser preview has none, so match it locally there.
      if (isPreview() && matchesAccelerator(e, loadHudHotkey())) {
        e.preventDefault();
        hud.toggle();
        return true;
      }
      if (e.key === 'Escape' && hud.isShown() && !keyboardFree()) return false;
      return false;
    },
  });

  // ---------------------------------------------------------------- instruments
  // Session-bound instruments are created once the engine returns a session id.
  let ports = null;
  let terminal = null;
  let dvr = null;
  let transfers = null;
  const pip = createPip({
    root: shell,
    renderer,
    onSelectDisplay: (id) => selectDisplay(id),
    onFollowChange: (f) => f && lastPointer && renderer.zoom > 1.01 && renderer.centerOn(lastPointer.x, lastPointer.y),
  });

  async function openPalette() {
    if (!sessionId) return;
    input.releaseAll();
    overlays++;
    const sheet = await openActionPalette(sessionId);
    const obs = new MutationObserver(() => {
      if (!sheet.overlay.isConnected) {
        overlays = Math.max(0, overlays - 1);
        obs.disconnect();
      }
    });
    obs.observe(document.body, { childList: true });
  }

  // Headless virtual displays on the host (Ghost Mode without a monitor, ultrawide workspaces).
  const VIRTUAL_PRESETS = [
    { key: 'session.virtual.fhd', w: 1920, h: 1080 },
    { key: 'session.virtual.qhd', w: 2560, h: 1440 },
    { key: 'session.virtual.ultrawide', w: 5120, h: 1440 },
  ];

  async function addVirtualDisplay(p) {
    try {
      await api.setVirtualDisplay(sessionId, true, p.w, p.h);
      toast({ key: 'session.virtual.creating' });
    } catch (e) {
      toastError(e);
    }
  }

  async function removeVirtualDisplay() {
    try {
      await api.setVirtualDisplay(sessionId, false);
    } catch (e) {
      toastError(e);
    }
  }

  async function selectDisplay(id) {
    try {
      await api.selectDisplay(sessionId, id);
      activeDisplay = id;
      pip.setDisplays(displays, id);
    } catch (e) {
      toastError(e);
    }
  }

  const hud = createHud({
    root: shell,
    peerName,
    actions: {
      openDisplayMenu: (anchor) =>
        hud.openMenu(anchor, (close) => [
          h('div', { class: 'menu-label', i18n: 'session.hud.display' }),
          ...(displays.length
            ? displays.map((d, i) =>
                h(
                  'button',
                  { attrs: { role: 'menuitemradio', 'aria-checked': String(d.id === activeDisplay) }, on: { click: () => (close(), selectDisplay(d.id)) } },
                  icon('monitor', 15),
                  h('span', { class: 'spacer', text: d.isVirtual ? t('session.virtual.name', { w: d.bounds?.width ?? '', h: d.bounds?.height ?? '' }) : d.name || t('session.displayN', { n: i + 1 }) }),
                  h('span', { class: 'mono faint', text: `${d.bounds?.width ?? ''}×${d.bounds?.height ?? ''}` }),
                ),
              )
            : [h('div', { class: 'hint', style: { padding: '8px 10px' }, i18n: 'session.noDisplays' })]),
          h('div', { class: 'menu-sep' }),
          h('div', { class: 'menu-label', i18n: 'session.virtual.title' }),
          ...VIRTUAL_PRESETS.map((p) =>
            h(
              'button',
              { attrs: { role: 'menuitem' }, on: { click: () => (close(), addVirtualDisplay(p)) } },
              icon('layers', 15),
              h('span', { class: 'spacer', i18n: p.key }),
              h('span', { class: 'mono faint', text: `${p.w}×${p.h}` }),
            ),
          ),
          displays.some((d) => d.isVirtual)
            ? h('button', { attrs: { role: 'menuitem' }, on: { click: () => (close(), removeVirtualDisplay()) } }, icon('trash', 15), h('span', { class: 'spacer', i18n: 'session.virtual.remove' }))
            : null,
        ]),
      toggleLossless: async () => {
        try {
          await api.setLosslessText(sessionId, !lossless);
          lossless = !lossless;
          hud.setPressed('lossless', lossless);
          toast({ key: lossless ? 'session.losslessOn' : 'session.losslessOff' });
        } catch (e) {
          toastError(e);
        }
      },
      // Speaker button: first use starts the stream, then it mutes / unmutes locally.
      toggleAudio: async () => {
        if (!sessionId) return;
        try {
          if (!audio.enabled) {
            audio.muted = false;
            await api.setAudioVolume(sessionId, audio.volume, false);
            await setAudioEnabled(true);
          } else {
            audio.muted = !audio.muted;
            await api.setAudioVolume(sessionId, audio.volume, audio.muted);
            saveAudio();
            reflectAudio();
          }
          toast({ key: audio.muted ? 'session.audio.mutedToast' : 'session.audio.onToast' });
        } catch (e) {
          toastError(e);
        }
      },
      openAudioMenu: (anchor) => openAudioMenu(anchor),
      openDrivesMenu: async (anchor) => {
        let shares = null;
        let err = null;
        try {
          shares = await api.vfsListShares(sessionId);
        } catch (e) {
          err = e;
        }
        hud.openMenu(anchor, (close) => [
          h('div', { class: 'menu-label', i18n: 'session.hud.drives' }),
          ...(err
            ? [h('div', { class: 'hint', style: { padding: '8px 10px', maxWidth: '260px', whiteSpace: 'normal' }, text: describeError(err) })]
            : (shares || []).length
              ? shares.map((s) =>
                  h(
                    'button',
                    {
                      on: {
                        click: async () => {
                          close();
                          try {
                            const res = await api.vfsMount(sessionId, s.id);
                            toastSuccess('session.mounted', { path: res.mountPoint || '' });
                          } catch (e) {
                            toastError(e);
                          }
                        },
                      },
                    },
                    icon('drive', 15),
                    h('span', { class: 'spacer', text: s.name }),
                    s.readOnly && h('span', { class: 'chip', i18n: 'drives.readOnly' }),
                  ),
                )
              : [h('div', { class: 'hint', style: { padding: '8px 10px' }, i18n: 'session.noShares' })]),
        ]);
      },
      openPorts: () => ports && ports.toggle(),
      toggleTerminal: () => terminal && terminal.toggle(),
      openPalette: () => openPalette(),
      toggleDvr: () => dvr && dvr.toggle(),
      toggleGhost: () => setGhost(!ghost),
      togglePip: () => hud.setPressed('pip', pip.toggle()),
      disconnect: () => disconnect(),
    },
  });
  hud.setPressed('lossless', lossless);

  // ---------------------------------------------------------------- audio
  // Preferences persist per remote target (stream on/off, mute, volume, output device).
  const AUDIO_DEFAULTS = { enabled: false, muted: false, volume: 0.8, outputId: null };
  const audioKey = `aetherlink.audio.${pending.target}`;
  const audio = (() => {
    try {
      return { ...AUDIO_DEFAULTS, ...JSON.parse(localStorage.getItem(audioKey) || '{}') };
    } catch {
      return { ...AUDIO_DEFAULTS };
    }
  })();
  const saveAudio = () => {
    try {
      localStorage.setItem(audioKey, JSON.stringify(audio));
    } catch {
      /* storage unavailable: preferences stay in memory */
    }
  };
  const reflectAudio = () => hud.setPressed('audio', audio.enabled && !audio.muted);
  let audioStats = null;
  let audioRate = { bytes: 0, at: 0, kbps: 0 };
  let audioRestoredFor = null;

  async function setAudioEnabled(v) {
    await api.setAudio(sessionId, v);
    audio.enabled = v;
    saveAudio();
    reflectAudio();
  }

  async function restoreAudio() {
    if (!sessionId || audioRestoredFor === sessionId) return;
    audioRestoredFor = sessionId;
    if (audio.outputId) {
      try {
        await api.setAudioOutput(sessionId, audio.outputId);
      } catch {
        audio.outputId = null; // device gone: fall back to the system default
        saveAudio();
      }
    }
    await api.setAudioVolume(sessionId, audio.volume, audio.muted).catch(() => {});
    if (audio.enabled) {
      try {
        await api.setAudio(sessionId, true);
      } catch (e) {
        audio.enabled = false;
        toastError(e);
      }
    }
    reflectAudio();
  }

  let volumeQueued = false;
  const pushVolume = () => {
    if (volumeQueued || !sessionId) return;
    volumeQueued = true;
    requestAnimationFrame(() => {
      volumeQueued = false;
      api.setAudioVolume(sessionId, audio.volume, audio.muted).catch(() => {});
    });
  };

  async function openAudioMenu(anchor) {
    if (!sessionId) return;
    let outputs = [];
    let listError = null;
    try {
      outputs = (await api.listAudioOutputs()) || [];
    } catch (e) {
      listError = e;
    }
    hud.openMenu(anchor, (close) => {
      const percent = h('span', { class: 'audio-volume-value', i18n: 'session.audio.percent', i18nArgs: { n: Math.round(audio.volume * 100) } });
      const setPercent = () => {
        percent.dataset.i18nArgs = JSON.stringify({ n: Math.round(audio.volume * 100) });
        replace(percent, t('session.audio.percent', { n: Math.round(audio.volume * 100) }));
      };
      const setMuted = (m) => {
        audio.muted = m;
        muteBtn.setAttribute('aria-pressed', String(m));
        replace(muteBtn, icon(m ? 'speakerOff' : 'speaker', 15));
        saveAudio();
        reflectAudio();
        pushVolume();
      };
      const muteBtn = h(
        'button',
        {
          class: 'icon-only',
          type: 'button',
          attrs: { 'aria-pressed': String(audio.muted) },
          i18nAttr: { title: 'session.audio.mute', 'aria-label': 'session.audio.mute' },
          on: { click: () => setMuted(!audio.muted) },
        },
        icon(audio.muted ? 'speakerOff' : 'speaker', 15),
      );
      const slider = h('input', {
        attrs: { type: 'range', min: 0, max: 100, step: 1, value: Math.round(audio.volume * 100) },
        i18nAttr: { 'aria-label': 'session.audio.volume', title: 'session.audio.volume' },
        on: {
          input: (e) => {
            audio.volume = Math.max(0, Math.min(1, Number(e.target.value) / 100));
            setPercent();
            if (audio.muted) setMuted(false);
            else pushVolume();
          },
          change: () => saveAudio(),
        },
      });
      const streamSwitch = toggleSwitch({
        checked: audio.enabled,
        labelKey: 'session.audio.stream',
        onChange: async (v) => {
          try {
            await setAudioEnabled(v);
          } catch (e) {
            streamSwitch.setChecked(!v);
            toastError(e);
          }
        },
      });
      const device = (id, label) =>
        h(
          'button',
          {
            attrs: { role: 'menuitemradio', 'aria-checked': String((audio.outputId || null) === id) },
            on: {
              click: async () => {
                close();
                try {
                  await api.setAudioOutput(sessionId, id);
                  audio.outputId = id;
                  saveAudio();
                } catch (e) {
                  toastError(e);
                }
              },
            },
          },
          icon('speaker', 15),
          label,
        );
      const stats =
        audio.enabled && audioStats
          ? h('div', {
              class: 'audio-stats',
              i18n: 'session.audio.stats',
              i18nArgs: {
                kbps: Math.round(audioRate.kbps),
                delay: Math.round((audioStats.targetDelayUs || 0) / 1000),
                lost: (audioStats.fecFrames || 0) + (audioStats.plcFrames || 0),
              },
            })
          : null;
      return [
        h(
          'div',
          { class: 'audio-menu' },
          h('div', { class: 'menu-label', i18n: 'session.audio.title' }),
          h('div', { class: 'audio-row' }, h('span', { class: 'spacer', i18n: 'session.audio.stream' }), streamSwitch),
          h('div', { class: 'audio-row' }, muteBtn, slider, percent),
          stats,
          h('div', { class: 'menu-sep' }),
          h('div', { class: 'menu-label', i18n: 'session.audio.output' }),
          device(null, h('span', { class: 'spacer', i18n: 'session.audio.systemDefault' })),
          ...outputs.map((d) => device(d.id, h('span', { class: 'spacer', text: d.name }))),
          listError ? h('div', { class: 'hint', style: { padding: '8px 10px', whiteSpace: 'normal' }, text: describeError(listError) }) : null,
        ),
      ];
    });
  }

  async function setGhost(v) {
    if (!sessionId) return;
    try {
      await api.setGhostMode(sessionId, v);
      ghost = v;
      hud.setPressed('ghost', v);
      toast({ key: v ? 'session.ghostOn' : 'session.ghostOff' });
    } catch (e) {
      toastError(e);
    }
  }

  async function disconnect() {
    input.releaseAll();
    try {
      if (sessionId) await api.disconnect(sessionId);
    } catch {
      /* closing anyway */
    }
    cleanup();
    api.closeWindow();
  }

  function cleanup() {
    input.dispose();
    renderer.dispose();
    transfers && transfers.dispose();
    for (const off of offs) off();
  }

  function markConnected() {
    if (connected) return;
    connected = true;
    hud.setConnected(true);
    handshake.setStage('connected');
    handshake.complete();
    restoreAudio();
    // Briefly reveal the HUD so users discover it, then tuck it away.
    setTimeout(() => {
      hud.show(false);
      setTimeout(() => hud.hide(), 2400);
    }, 500);
  }

  // ---------------------------------------------------------------- events
  const offs = [];
  // Events can race the `connect` reply; hold them until the session id is known.
  const early = [];
  const mine = (p) => p && (p.sessionId === undefined || p.sessionId === null || String(p.sessionId) === String(sessionId));
  const scoped = (fn) => (p) => {
    if (sessionId === null) early.push(() => fn(p));
    else if (mine(p)) fn(p);
  };

  const onSessionState = scoped((p) => {
    const st = String(p.state || '').toLowerCase();
    if (st === 'connecting' || st === 'authenticating') handshake.setStage(st);
    else if (st === 'connected') {
      markConnected();
      // Non-fatal host-side problems (missing permissions…) arrive on a 'connected' state.
      if (p.errorCode) toast({ key: p.errorCode, kind: 'error', timeout: 9000 });
      // Ghost Mode refused or no curtain could be raised: the button must not claim protection.
      if (['errors.ghostUnavailable', 'errors.permission.ghostMode'].includes(p.errorCode) && ghost) {
        ghost = false;
        hud.setPressed('ghost', false);
      }
      // The host could not capture audio: stop local playback (the saved preference stays).
      if (['errors.audioUnavailable', 'errors.audioPermission', 'errors.permission.audio'].includes(p.errorCode) && audio.enabled) {
        api.setAudio(sessionId, false).catch(() => {});
        audio.enabled = false;
        reflectAudio();
      }
    }
    else if (st === 'reconnecting') {
      connected = false;
      hud.setConnected(false);
      handshake.reset();
      handshake.setStage('connecting', { titleKey: 'session.state.reconnecting' });
    } else if (st === 'closed' || st === 'failed') {
      connected = false;
      input.releaseAll();
      handshake.reset();
      const reason = p.errorCode ? t(p.errorCode) : p.reason;
      handshake.showError(reason ? t('session.closedReason', { reason }) : t('session.closed'), [
        h('button', { class: 'btn', on: { click: () => api.closeWindow() } }, h('span', { i18n: 'common.close' })),
        h('button', { class: 'btn primary', on: { click: () => start() } }, h('span', { i18n: 'common.retry' })),
      ]);
    }
  });

  offs.push(
    on('session-state', (p) => {
      // Handshake progress is purely visual: show it even before `connect` returns.
      const st0 = String((p && p.state) || '').toLowerCase();
      if (sessionId === null && (st0 === 'connecting' || st0 === 'authenticating')) handshake.setStage(st0);
      else onSessionState(p);
    }),
    on('session-telemetry', scoped((p) => {
      const tm = p.telemetry || p;
      hud.setTelemetry({ fps: tm.fps ?? renderer.fps, rttMs: tm.rttMs, bitrateKbps: tm.bitrateKbps });
      if (p.audio) {
        const now = performance.now();
        const bytes = p.audio.bytesReceived || 0;
        if (audioRate.at && bytes >= audioRate.bytes) audioRate.kbps = ((bytes - audioRate.bytes) * 8) / (now - audioRate.at);
        audioRate = { bytes, at: now, kbps: audioRate.kbps };
        audioStats = p.audio;
      }
    })),
    on('session-permissions', scoped((p) => {
      const perms = p.permissions || {};
      permissions = perms;
      hud.setAllowed({
        audio: perms.audio !== false,
        drives: perms.drives !== false,
        ports: perms.portForwarding === true,
        terminal: perms.terminal === true,
        search: perms.semanticUi !== false && perms.input !== false,
        ghost: perms.ghostMode === true,
      });
      if (perms.ghostMode !== true && ghost) {
        ghost = false;
        hud.setPressed('ghost', false);
      }
    })),
    on('session-displays', scoped((p) => {
      displays = Array.isArray(p.displays) ? p.displays : [];
      activeDisplay = p.activeDisplayId ?? p.active?.Display?.displayId ?? activeDisplay;
      pip.setDisplays(displays, activeDisplay);
    })),
    on('tray-ghost-mode', (p) => setGhost(!!p.enabled)),
    on('hud-toggle', () => hud.toggle()),
  );

  // local fps fallback when the engine sends no telemetry
  const fpsTimer = setInterval(() => connected && hud.setTelemetry({ fps: renderer.fps }), 1000);
  offs.push(() => clearInterval(fpsTimer));

  // ---------------------------------------------------------------- connect
  async function start(password = pending.password) {
    sessionId = null;
    connected = false;
    audioRestoredFor = null;
    early.length = 0;
    handshake.reset();
    handshake.setStage('connecting');
    try {
      const res = await api.connect(String(pending.target), password, (bytes) => renderer.handle(bytes));
      sessionId = res.sessionId;
      bindSession(sessionId);
      for (const replay of early.splice(0)) replay();
    } catch (e) {
      const pwInput = h('input', { class: 'input', attrs: { type: 'password', autocomplete: 'off' }, i18nAttr: { placeholder: 'dashboard.quickConnect.passwordPlaceholder', 'aria-label': 'dashboard.quickConnect.passwordLabel' } });
      const needsPassword = e && (e.code === 'unauthorized' || /password/i.test(e.detail || ''));
      const retry = () => start(needsPassword ? pwInput.value : password);
      pwInput.addEventListener('keydown', (ev) => ev.key === 'Enter' && retry());
      handshake.showError(describeError(e), [
        needsPassword ? pwInput : null,
        h('button', { class: 'btn', on: { click: () => api.closeWindow() } }, h('span', { i18n: 'common.close' })),
        h('button', { class: 'btn primary', on: { click: retry } }, icon('refresh', 14), h('span', { i18n: 'common.retry' })),
      ]);
    }
  }

  let boundTo = null;
  function bindSession(id) {
    if (boundTo === id) return;
    if (boundTo !== null) {
      for (const c of [ports, terminal, dvr]) c && c.el.remove();
      if (terminal) terminal.dispose();
      if (transfers) transfers.dispose();
    }
    boundTo = id;
    ports = createPortsDrawer({ root: shell, sessionId: id });
    terminal = createQuakeTerminal({ root: shell, sessionId: id, onVisibility: (v) => v && input.releaseAll() });
    dvr = createDvr({ root: shell, sessionId: id, onVisibility: (v) => hud.setPressed('dvr', v) });
    transfers = createTransfers({ root: shell, sessionId: id, peerName, isEnabled: () => connected });
  }

  addEventListener('beforeunload', () => {
    if (sessionId) api.disconnect(sessionId).catch(() => {});
  });

  start();
}
