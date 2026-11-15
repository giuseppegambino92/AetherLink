// Tiny element builder. User-visible copy is passed as i18n keys
// (`i18n`, `i18nAttr`) so `i18n.apply()` can re-translate live; `text` is
// reserved for runtime data (names, IDs, numbers) coming from the engine.
import { t } from '../i18n.js';

/**
 * @param {string} tag
 * @param {object} [props]
 *   class, text, i18n, i18nArgs, i18nAttr {attr: key}, attrs {}, style {}, dataset {}, on {event: fn}, html (trusted SVG only)
 * @param  {...any} children Node | string(data) | null | arrays
 */
export function h(tag, props = {}, ...children) {
  const el = tag.startsWith('svg:')
    ? document.createElementNS('http://www.w3.org/2000/svg', tag.slice(4))
    : document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v === undefined || v === null || v === false) continue;
    switch (k) {
      case 'class':
        el.setAttribute('class', Array.isArray(v) ? v.filter(Boolean).join(' ') : v);
        break;
      case 'text':
        el.textContent = String(v);
        break;
      case 'i18n':
        el.dataset.i18n = v;
        if (props.i18nArgs) el.dataset.i18nArgs = JSON.stringify(props.i18nArgs);
        el.textContent = t(v, props.i18nArgs);
        break;
      case 'i18nArgs':
        break;
      case 'i18nAttr': {
        const pairs = Object.entries(v);
        el.dataset.i18nAttr = pairs.map(([a, key]) => `${a}:${key}`).join(';');
        for (const [a, key] of pairs) el.setAttribute(a, t(key, props.i18nArgs));
        break;
      }
      case 'attrs':
        for (const [a, av] of Object.entries(v)) {
          if (av === undefined || av === null || av === false) continue;
          el.setAttribute(a, av === true ? '' : String(av));
        }
        break;
      case 'style':
        for (const [sk, sv] of Object.entries(v)) {
          if (sk.startsWith('--')) el.style.setProperty(sk, sv);
          else el.style[sk] = sv;
        }
        break;
      case 'dataset':
        Object.assign(el.dataset, v);
        break;
      case 'on':
        for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
        break;
      case 'html':
        el.innerHTML = v; // only used for static, trusted SVG markup
        break;
      default:
        if (k in el && typeof v !== 'string') el[k] = v;
        else el.setAttribute(k, v === true ? '' : v);
    }
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    if (Array.isArray(c)) append(el, c);
    else if (c instanceof Node) el.appendChild(c);
    else el.appendChild(document.createTextNode(String(c)));
  }
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

export function replace(el, ...children) {
  clear(el);
  append(el, children);
  return el;
}

export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

/** Debounce helper. */
export function debounce(fn, ms) {
  let id = 0;
  const d = (...args) => {
    clearTimeout(id);
    id = setTimeout(() => fn(...args), ms);
  };
  d.cancel = () => clearTimeout(id);
  return d;
}

/** Throttle to at most once per `ms`, always delivering the latest call. */
export function throttle(fn, ms) {
  let last = 0;
  let timer = 0;
  let pending = null;
  return (...args) => {
    pending = args;
    const now = performance.now();
    const wait = ms - (now - last);
    if (wait <= 0) {
      last = now;
      fn(...pending);
      pending = null;
    } else if (!timer) {
      timer = setTimeout(() => {
        timer = 0;
        last = performance.now();
        if (pending) fn(...pending);
        pending = null;
      }, wait);
    }
  };
}
