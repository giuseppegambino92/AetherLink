// Pointer / keyboard / wheel capture → aether_protocol::InputEvent JSON.
// Mouse moves and wheel deltas are coalesced to one message per animation
// frame; coordinates are normalized to the active remote display.
import { domCodeToHid, modifiersOf, mouseButtonName, InputEvents } from './hid.js';

export class InputCapture {
  /**
   * @param {{ surface: HTMLElement, renderer: import('./renderer.js').FrameRenderer,
   *           send: (event: any) => void, isEnabled: () => boolean,
   *           intercept?: (e: KeyboardEvent) => boolean, onPointer?: (n: {x:number,y:number}) => void }} opts
   */
  constructor(opts) {
    this.o = opts;
    this.pendingMove = null;
    this.wheel = null;
    this.raf = 0;
    this.buttons = new Set();
    this.keys = new Set();
    this.composing = false;
    this._bind();
  }

  _schedule() {
    if (!this.raf) this.raf = requestAnimationFrame(() => this.flush());
  }

  flush() {
    this.raf = 0;
    if (this.pendingMove) {
      const { x, y } = this.pendingMove;
      this.pendingMove = null;
      const d = this.o.renderer.displayId;
      const l = this.lastMove;
      if (!l || l.x !== x || l.y !== y || l.d !== d) {
        this.lastMove = { x, y, d };
        this.o.send(InputEvents.mouseMove(d, x, y));
      }
    }
    if (this.wheel) {
      const { dx, dy, unit } = this.wheel;
      this.wheel = null;
      if (dx || dy) this.o.send(InputEvents.scroll(dx, dy, unit, null));
    }
  }

  releaseAll() {
    this.pendingMove = null;
    this.wheel = null;
    if (this.buttons.size || this.keys.size) {
      this.buttons.clear();
      this.keys.clear();
    }
    this.o.send(InputEvents.releaseAll());
  }

  _bind() {
    const s = this.o.surface;
    const on = (t, ev, fn, opt) => {
      t.addEventListener(ev, fn, opt);
      (this._off ||= []).push(() => t.removeEventListener(ev, fn, opt));
    };

    on(s, 'pointermove', (e) => {
      if (!this.o.isEnabled()) return;
      const events = e.getCoalescedEvents ? e.getCoalescedEvents() : [e];
      const last = events[events.length - 1] || e;
      const n = this.o.renderer.clientToNormalized(last.clientX, last.clientY);
      if (!n) return;
      this.pendingMove = n;
      this.o.onPointer && this.o.onPointer(n);
      this._schedule();
    });

    on(s, 'pointerdown', (e) => {
      if (!this.o.isEnabled()) return;
      const n = this.o.renderer.clientToNormalized(e.clientX, e.clientY);
      if (!n) return;
      e.preventDefault();
      s.setPointerCapture(e.pointerId);
      this.pendingMove = n;
      this.flush();
      const b = mouseButtonName(e.button);
      if (!b) return;
      this.buttons.add(b);
      this.o.send(InputEvents.mouseButton(b, true));
    });

    on(s, 'pointerup', (e) => {
      const b = mouseButtonName(e.button);
      if (!b || !this.buttons.has(b)) return;
      const n = this.o.renderer.clientToNormalized(e.clientX, e.clientY);
      if (n) this.pendingMove = n;
      this.flush();
      this.buttons.delete(b);
      this.o.send(InputEvents.mouseButton(b, false));
    });

    on(s, 'contextmenu', (e) => e.preventDefault());
    on(s, 'auxclick', (e) => e.preventDefault());

    on(
      s,
      'wheel',
      (e) => {
        if (!this.o.isEnabled()) return;
        e.preventDefault();
        // Trackpad pinch arrives as ctrl+wheel: zoom the local view instead.
        if (e.ctrlKey && e.deltaMode === 0) {
          this.o.renderer.zoomBy(Math.exp(-e.deltaY * 0.01), e.clientX, e.clientY);
          return;
        }
        const unit = e.deltaMode === 0 ? 'Pixel' : 'Line';
        const scale = e.deltaMode === 2 ? 20 : 1; // page → ~lines
        if (!this.wheel || this.wheel.unit !== unit) {
          this.flush();
          this.wheel = { dx: 0, dy: 0, unit };
        }
        this.wheel.dx += e.deltaX * scale;
        this.wheel.dy += e.deltaY * scale;
        this._schedule();
      },
      { passive: false },
    );

    on(window, 'keydown', (e) => {
      if (this.o.intercept && this.o.intercept(e)) return;
      if (!this.o.isEnabled() || this.composing || e.isComposing) return;
      const code = domCodeToHid(e.code);
      e.preventDefault();
      if (code === null) {
        if (e.key && e.key.length === 1 && !e.metaKey && !e.ctrlKey) this.o.send(InputEvents.text(e.key));
        return;
      }
      this.keys.add(code);
      this.o.send(InputEvents.key(code, true, modifiersOf(e)));
    });

    on(window, 'keyup', (e) => {
      const code = domCodeToHid(e.code);
      if (code === null || !this.keys.has(code)) return;
      e.preventDefault();
      this.keys.delete(code);
      this.o.send(InputEvents.key(code, false, modifiersOf(e)));
    });

    // IME composition (CJK, dead keys) → committed text.
    on(window, 'compositionstart', () => (this.composing = true));
    on(window, 'compositionend', (e) => {
      this.composing = false;
      if (this.o.isEnabled() && e.data) this.o.send(InputEvents.text(e.data));
    });

    // Never leave keys/buttons stuck on the remote.
    on(window, 'blur', () => this.releaseAll());
    on(document, 'visibilitychange', () => document.hidden && this.releaseAll());
  }

  dispose() {
    if (this.raf) cancelAnimationFrame(this.raf);
    for (const f of this._off || []) f();
  }
}
