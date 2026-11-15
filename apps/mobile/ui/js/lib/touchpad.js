// Touch gesture recognizer shared by the session viewport and the Bluetooth
// controller trackpad. Emits high-level intents; the caller maps them to
// InputEvents (session) or HID reports (controller).
//
// Trackpad mode            Direct-touch mode
//  1 finger drag → move     1 finger → absolute down / move / up
//  tap → left click         long-press → right click
//  2-finger tap → right     2 fingers → scroll, or pinch (zoom + pan)
//  long-press + drag → drag with the button held
//  2 fingers → scroll, or pinch (zoom + pan)

const TAP_MS = 240;
const TAP_SLOP = 10;
const LONG_MS = 430;
const PINCH_RATIO = 0.06; // relative finger-distance change that turns a 2-finger gesture into a pinch

export class Touchpad {
  /**
   * @param {HTMLElement} surface
   * @param {{
   *   mode?: 'trackpad'|'direct',
   *   onMove?: (dx:number, dy:number) => void,                 // trackpad relative motion (CSS px)
   *   onButton?: (button:'Left'|'Right', pressed:boolean) => void,
   *   onClick?: (button:'Left'|'Right') => void,
   *   onAbsolute?: (phase:'down'|'move'|'up', clientX:number, clientY:number) => void,
   *   onScroll?: (dx:number, dy:number, phase:'Begin'|'Change'|'End') => void, // content-direction pixels
   *   onPinch?: (phase:'begin'|'change'|'end', scale:number, cx:number, cy:number, panDx:number, panDy:number) => void,
   *   onLongPress?: (clientX:number, clientY:number) => void,
   *   isEnabled?: () => boolean,
   * }} handlers
   */
  constructor(surface, handlers) {
    this.s = surface;
    this.h = handlers;
    this.mode = handlers.mode || 'trackpad';
    this.pts = new Map();
    this.reset();
    const on = (ev, fn) => {
      surface.addEventListener(ev, fn, { passive: false });
      (this._off ||= []).push(() => surface.removeEventListener(ev, fn));
    };
    on('pointerdown', (e) => this._down(e));
    on('pointermove', (e) => this._move(e));
    on('pointerup', (e) => this._up(e));
    on('pointercancel', (e) => this._up(e, true));
    on('contextmenu', (e) => e.preventDefault());
  }

  dispose() {
    clearTimeout(this.longTimer);
    for (const off of this._off || []) off();
  }

  setMode(mode) {
    this.mode = mode;
    this.reset();
  }

  reset() {
    clearTimeout(this.longTimer);
    this.pts.clear();
    this.g = null; // current gesture
  }

  _enabled() {
    return !this.h.isEnabled || this.h.isEnabled();
  }

  _down(e) {
    if (!this._enabled()) return;
    e.preventDefault();
    try {
      this.s.setPointerCapture(e.pointerId);
    } catch {
      /* synthetic pointer */
    }
    this.pts.set(e.pointerId, { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY });
    const n = this.pts.size;
    const now = performance.now();
    if (n === 1) {
      this.g = { kind: 'one', t0: now, moved: false, dragging: false, absDown: false, lx: e.clientX, ly: e.clientY };
      clearTimeout(this.longTimer);
      this.longTimer = setTimeout(() => this._long(), LONG_MS);
      if (this.mode === 'direct') {
        this.g.absDown = true;
        this.h.onAbsolute && this.h.onAbsolute('down', e.clientX, e.clientY);
      }
    } else if (n === 2) {
      clearTimeout(this.longTimer);
      // A second finger cancels a direct-touch press / trackpad drag.
      if (this.g && this.g.absDown) this.h.onAbsolute && this.h.onAbsolute('up', this.g.lx, this.g.ly);
      if (this.g && this.g.dragging) this.h.onButton && this.h.onButton('Left', false);
      const [a, b] = [...this.pts.values()];
      this.g = { kind: 'two', t0: now, moved: false, d0: dist(a, b), c0: mid(a, b), lastC: mid(a, b), pinch: false, scrolling: false };
    } else if (this.g) {
      this.g.kind = 'many';
    }
  }

  _long() {
    const g = this.g;
    if (!g || g.kind !== 'one' || g.moved) return;
    g.long = true;
    if (this.mode === 'trackpad') {
      g.dragging = true;
      this.h.onButton && this.h.onButton('Left', true);
    } else {
      // direct: long-press = right click at the finger
      if (g.absDown) this.h.onAbsolute && this.h.onAbsolute('up', g.lx, g.ly);
      g.absDown = false;
      this.h.onLongPress && this.h.onLongPress(g.lx, g.ly);
    }
  }

  _move(e) {
    const p = this.pts.get(e.pointerId);
    if (!p || !this.g) return;
    e.preventDefault();
    const px = p.x;
    const py = p.y;
    p.x = e.clientX;
    p.y = e.clientY;
    const g = this.g;
    if (g.kind === 'one') {
      if (!g.moved && Math.hypot(p.x - p.x0, p.y - p.y0) > TAP_SLOP) {
        g.moved = true;
        if (!g.long) clearTimeout(this.longTimer);
      }
      g.lx = p.x;
      g.ly = p.y;
      if (this.mode === 'trackpad') {
        if (g.moved || g.dragging) this.h.onMove && this.h.onMove(p.x - px, p.y - py);
      } else if (g.absDown && g.moved) {
        this.h.onAbsolute && this.h.onAbsolute('move', p.x, p.y);
      }
    } else if (g.kind === 'two') {
      const [a, b] = [...this.pts.values()];
      const c = mid(a, b);
      const d = dist(a, b);
      if (!g.moved && (Math.abs(d - g.d0) > TAP_SLOP || Math.hypot(c.x - g.c0.x, c.y - g.c0.y) > TAP_SLOP)) g.moved = true;
      if (!g.pinch && !g.scrolling && g.moved) {
        if (Math.abs(d / g.d0 - 1) > PINCH_RATIO) {
          g.pinch = true;
          this.h.onPinch && this.h.onPinch('begin', 1, c.x, c.y, 0, 0);
        } else {
          g.scrolling = true;
          this.h.onScroll && this.h.onScroll(0, 0, 'Begin');
        }
      }
      if (g.pinch) {
        this.h.onPinch && this.h.onPinch('change', d / g.d0, c.x, c.y, c.x - g.lastC.x, c.y - g.lastC.y);
      } else if (g.scrolling) {
        // Natural scrolling: content follows the fingers.
        this.h.onScroll && this.h.onScroll(-(c.x - g.lastC.x), -(c.y - g.lastC.y), 'Change');
      }
      g.lastC = c;
    }
  }

  _up(e, cancelled = false) {
    const p = this.pts.get(e.pointerId);
    if (!p) return;
    this.pts.delete(e.pointerId);
    const g = this.g;
    if (!g) return;
    const quick = performance.now() - g.t0 < TAP_MS;
    if (g.kind === 'one') {
      clearTimeout(this.longTimer);
      if (this.mode === 'trackpad') {
        if (g.dragging) this.h.onButton && this.h.onButton('Left', false);
        else if (!g.moved && quick && !cancelled) this.h.onClick && this.h.onClick('Left');
      } else if (g.absDown) {
        this.h.onAbsolute && this.h.onAbsolute('up', e.clientX, e.clientY);
      }
      this.g = null;
    } else if (g.kind === 'two') {
      if (this.pts.size === 1) {
        // first finger of two lifted: finish the gesture now, ignore the remaining finger
        if (g.pinch) this.h.onPinch && this.h.onPinch('end', 1, 0, 0, 0, 0);
        else if (g.scrolling) this.h.onScroll && this.h.onScroll(0, 0, 'End');
        else if (!g.moved && quick && !cancelled) this.h.onClick && this.h.onClick('Right');
        g.kind = 'done';
      }
    }
    if (this.pts.size === 0) this.g = null;
  }
}

function dist(a, b) {
  return Math.max(1, Math.hypot(a.x - b.x, a.y - b.y));
}

function mid(a, b) {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

/** Pointer acceleration for relative motion (gentle curve, like a laptop trackpad). */
export function accelerate(dx, dy, base = 1.4) {
  const v = Math.hypot(dx, dy);
  const k = base * (1 + Math.min(2.2, v / 18));
  return [dx * k, dy * k];
}
