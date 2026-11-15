#!/usr/bin/env node
// i18n gate for the AetherLink UIs. Fails (exit 1) when, in a UI tree:
//  1. en.json and it.json differ in keys, value types or {placeholders};
//  2. code references an i18n key (literal or template) that does not exist;
//  3. a .js/.html file contains a hard-coded user-facing string
//     (text nodes / title / aria-label / placeholder / alt / textContent …).
//
// Trees: `desktop` (apps/desktop/ui, the default) and `mobile` (apps/mobile/ui).
//   node scripts/check-i18n.mjs            desktop only (unchanged behaviour)
//   node scripts/check-i18n.mjs --mobile   mobile only
//   node scripts/check-i18n.mjs --all      desktop + mobile: the one command for both apps
//
// Sample data in ui/js/dev-preview.js is exempt from rule 3 (it is the
// browser-only design-review adapter); vendored libraries and the mobile app's
// generated ui/shared/ copies (their sources are checked in the desktop tree)
// are not scanned.
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const errors = [];
const summaries = [];

// ------------------------------------------------------------------ helpers
function flatten(obj, prefix = '', out = new Map()) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === 'object' && !Array.isArray(v)) flatten(v, key, out);
    else out.set(key, v);
  }
  return out;
}
const placeholders = (s) => new Set([...String(s).matchAll(/\{(\w+)\}/g)].map((m) => m[1]));

const lineOf = (src, idx) => src.slice(0, idx).split('\n').length;

// Minimal JS lexer: yields string/template literals with their offsets,
// skipping comments and (heuristically) regex literals.
function* literals(src) {
  let i = 0;
  let lastSignificant = '';
  while (i < src.length) {
    const c = src[i];
    const n = src[i + 1];
    if (c === '/' && n === '/') {
      i = src.indexOf('\n', i);
      if (i < 0) return;
      continue;
    }
    if (c === '/' && n === '*') {
      i = src.indexOf('*/', i + 2) + 2;
      if (i < 2) return;
      continue;
    }
    if (c === '/' && /[(,=:[!&|?{};+\-*%<>~^]|^$|return$/.test(lastSignificant)) {
      // regex literal
      let j = i + 1;
      let inClass = false;
      while (j < src.length) {
        if (src[j] === '\\') j += 2;
        else if (src[j] === '[') (inClass = true), j++;
        else if (src[j] === ']') (inClass = false), j++;
        else if (src[j] === '/' && !inClass) break;
        else if (src[j] === '\n') break;
        else j++;
      }
      i = j + 1;
      lastSignificant = '/';
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      const start = i;
      let j = i + 1;
      let depth = 0;
      while (j < src.length) {
        if (src[j] === '\\') {
          j += 2;
          continue;
        }
        if (c === '`' && src[j] === '$' && src[j + 1] === '{') {
          depth++;
          j += 2;
          continue;
        }
        if (c === '`' && depth > 0 && src[j] === '}') {
          depth--;
          j++;
          continue;
        }
        if (src[j] === c && depth === 0) break;
        j++;
      }
      yield { quote: c, start, end: j + 1, body: src.slice(start + 1, j) };
      i = j + 1;
      lastSignificant = 'x';
      continue;
    }
    if (!/\s/.test(c)) {
      const word = /[A-Za-z_$]/.test(c) ? src.slice(i).match(/^[A-Za-z_$][\w$]*/)[0] : c;
      lastSignificant = word;
      i += word.length;
      continue;
    }
    i++;
  }
}

const stripExpr = (s) => s.replace(/\$\{[^}]*\}/g, ' ');
const hasWords = (s) => /[A-Za-zÀ-ÿ]{2,}/.test(stripExpr(s).replace(/&[a-z]+;/g, ''));
const KEY_RE = /^[a-z][A-Za-z0-9]*(\.[A-Za-z0-9]+)+$/;
const SINK_RE = [
  /\b(text|title|placeholder|alt|label|message|tooltip)\s*:\s*$/,
  /['"](aria-label|aria-description|title|placeholder|alt)['"]\s*:\s*$/,
  /\.(textContent|innerText|title|placeholder|ariaLabel)\s*=\s*$/,
  /setAttribute\(\s*['"](title|aria-label|placeholder|alt)['"]\s*,\s*$/,
  /\b(alert|confirm|prompt|createTextNode)\(\s*$/,
];
const SENTENCE_RE = /^[A-Z][a-z]+(?: [A-Za-z]+){2,}[.!?…]?$/;
const DEV_CONTEXT_RE = /(console\.\w+|new Error|throw|import|from)\s*\(?\s*$/;

// ------------------------------------------------------------------ one UI tree
function checkTree(name, cfg) {
  const ui = cfg.ui;
  const fail = (file, line, msg) => errors.push(`[${name}] ${file}${line ? `:${line}` : ''}  ${msg}`);
  const en = flatten(JSON.parse(readFileSync(join(ui, 'locales/en.json'), 'utf8')));
  const it = flatten(JSON.parse(readFileSync(join(ui, 'locales/it.json'), 'utf8')));
  for (const [k, v] of en) {
    if (!it.has(k)) fail('locales/it.json', 0, `missing key "${k}"`);
    else {
      const w = it.get(k);
      if (typeof v !== 'string' || typeof w !== 'string') fail('locales', 0, `"${k}" must be a string in both locales`);
      else {
        const a = [...placeholders(v)].sort().join(',');
        const b = [...placeholders(w)].sort().join(',');
        if (a !== b) fail('locales', 0, `"${k}" placeholders differ: en {${a}} vs it {${b}}`);
        if (!w.trim()) fail('locales/it.json', 0, `"${k}" is empty`);
      }
    }
  }
  for (const k of it.keys()) if (!en.has(k)) fail('locales/en.json', 0, `missing key "${k}" (present in it.json)`);

  const namespaces = new Set([...en.keys()].map((k) => k.split('.')[0]));
  const keyExists = (k) => en.has(k) || en.has(`${k}.other`);

  function walk(dir, out = []) {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      if (cfg.skip.includes(name)) continue;
      if (statSync(p).isDirectory()) walk(p, out);
      else if (/\.(m?js|html)$/.test(name)) out.push(p);
    }
    return out;
  }
  const files = walk(ui);

  for (const file of files) {
    const rel = relative(ui, file);
    const src = readFileSync(file, 'utf8');

    if (file.endsWith('.html')) {
      const clean = src
        .replace(/<!--[\s\S]*?-->/g, '')
        .replace(/<script[\s\S]*?<\/script>/gi, '')
        .replace(/<style[\s\S]*?<\/style>/gi, '')
        .replace(/<svg[\s\S]*?<\/svg>/gi, '');
      for (const m of clean.matchAll(/>([^<]+)</g)) {
        if (hasWords(m[1])) fail(rel, lineOf(src, src.indexOf(m[1])), `hard-coded text "${m[1].trim()}" (use data-i18n)`);
      }
      for (const m of clean.matchAll(/\s(title|aria-label|placeholder|alt)="([^"]*)"/g)) {
        if (hasWords(m[2])) fail(rel, lineOf(src, src.indexOf(m[0])), `hard-coded ${m[1]}="${m[2]}" (use data-i18n-attr)`);
      }
      for (const m of clean.matchAll(/data-i18n="([^"]+)"/g)) if (!keyExists(m[1])) fail(rel, lineOf(src, src.indexOf(m[0])), `unknown i18n key "${m[1]}"`);
      continue;
    }

    const exemptCopy = cfg.exempt.includes(rel.split('\\').join('/'));
    for (const lit of literals(src)) {
      const line = lineOf(src, lit.start);
      const ctx = src.slice(Math.max(0, lit.start - 80), lit.start);
      const body = lit.body;

      // (2) key references
      const keyContext = /\b(t|tp)\(\s*$/.test(ctx) || /\b(i18n|\w+Key)\s*:\s*$/.test(ctx);
      if (lit.quote !== '`' && keyContext && !keyExists(body)) {
        fail(rel, line, `unknown i18n key "${body}"`);
        continue;
      }
      if (lit.quote !== '`' && KEY_RE.test(body) && namespaces.has(body.split('.')[0])) {
        if (!keyExists(body)) fail(rel, line, `unknown i18n key "${body}"`);
        continue;
      }
      if (lit.quote === '`' && /\$\{/.test(body) && /^[a-z][A-Za-z0-9]*\./.test(body) && namespaces.has(body.split('.')[0]) && !/\s/.test(body)) {
        const re = new RegExp(`^${body.replace(/\$\{[^}]*\}/g, '\u0000').replace(/[.*+?^()|[\]\\]/g, '\\$&').replace(/\u0000/g, '[A-Za-z0-9_-]+')}$`);
        const matches = [...en.keys()].filter((k) => re.test(k) || re.test(k.replace(/\.(zero|one|two|few|many|other)$/, '')));
        if (!matches.length) fail(rel, line, `template i18n key \`${body}\` matches no locale key`);
        continue;
      }

      // (3) hard-coded user-facing copy
      if (exemptCopy || !hasWords(body) || DEV_CONTEXT_RE.test(ctx)) continue;
      if (/(^|[^\w])\/\/\s*i18n-ignore/.test(src.slice(lit.end, src.indexOf('\n', lit.end)))) continue;
      if (SINK_RE.some((re) => re.test(ctx))) {
        fail(rel, line, `hard-coded user-facing string ${lit.quote}${body.slice(0, 60)}${lit.quote} (use t()/i18n keys)`);
        continue;
      }
      if (/\.innerHTML\s*=\s*$/.test(ctx) || /\bhtml\s*:\s*$/.test(ctx)) {
        if (/>[^<${]*[A-Za-z]{2,}[^<]*</.test(stripExpr(body))) fail(rel, line, 'hard-coded text inside innerHTML');
        continue;
      }
      if (SENTENCE_RE.test(stripExpr(body).trim())) fail(rel, line, `looks like hard-coded UI copy: "${body.slice(0, 60)}"`);
    }
  }

    for (const [prefix, names] of Object.entries(cfg.families)) for (const n of names) if (!keyExists(prefix + n)) fail('locales/en.json', 0, `missing dynamic key "${prefix}${n}"`);
    if (cfg.desktopPermissions) for (const kind of ['screenRecording', 'accessibility', 'inputMonitoring']) for (const leaf of ['title', 'body']) if (!keyExists(`settings.permissions.${kind}.${leaf}`)) fail('locales/en.json', 0, `missing "settings.permissions.${kind}.${leaf}"`);
    summaries.push(`${name}: ${en.size} keys in en/it, ${files.length} files scanned`);
}

// ------------------------------------------------------------------ trees
const COMMON_FAMILIES = {
  'os.': ['macos', 'windows', 'linux', 'android', 'ios', 'unknown'],
  'roles.': ['Window', 'Button', 'Link', 'TextField', 'TextArea', 'SearchField', 'CheckBox', 'RadioButton', 'Switch', 'Slider', 'ComboBox', 'PopUpButton', 'Menu', 'MenuItem', 'MenuBar', 'Tab', 'TabGroup', 'List', 'ListItem', 'Table', 'Row', 'Cell', 'Tree', 'TreeItem', 'Toolbar', 'Image', 'StaticText', 'Heading', 'Group', 'ScrollArea', 'WebArea', 'Dialog', 'Other'],
  'dvr.kinds.': ['Keyframe', 'Clipboard', 'Terminal', 'FileTransfer', 'ResourceSpike', 'Alert', 'Input'],
  'session.state.': ['connecting', 'authenticating', 'connected', 'reconnecting', 'failed'],
  'session.decoder.': ['webcodecs-unavailable', 'codec-unsupported', 'decode-failed'],
};
const TREES = {
  desktop: {
    ui: join(root, 'ui'),
    skip: ['vendor', 'node_modules'],
    exempt: ['js/dev-preview.js'],
    families: { ...COMMON_FAMILIES, 'settings.permissions.status.': ['granted', 'denied', 'notApplicable'] },
    desktopPermissions: true,
  },
  mobile: {
    ui: join(root, '..', 'mobile', 'ui'),
    skip: ['vendor', 'node_modules', 'shared'],
    exempt: ['js/dev-preview.js'],
    families: {
      ...COMMON_FAMILIES,
      'host.backend.': ['shizuku', 'accessibility', 'none'],
      'host.shizukuState.': ['ready', 'noPermission', 'unavailable'],
      // error keys produced by apps/mobile/src-tauri (mobile.rs / android.rs) and the Kotlin plugin
      'errors.': ['engineNotWired', 'notWired', 'unsupported', 'native', 'hidNotConnected', 'projectionDenied', 'cameraDenied', 'bluetoothUnavailable', 'bluetoothPermission', 'accessibilityDisabled', 'shizukuUnavailable', 'invalid', 'wrongPassword', 'peerNotFound', 'captureUnavailable', 'inputPermission'],
    },
    desktopPermissions: false,
  },
};

const args = process.argv.slice(2);
const selected = args.includes('--all') ? ['desktop', 'mobile'] : args.includes('--mobile') ? ['mobile'] : ['desktop'];
for (const name of selected) {
  if (!existsSync(join(TREES[name].ui, 'locales', 'en.json'))) {
    errors.push(`[${name}] ${TREES[name].ui}: no locales/en.json`);
    continue;
  }
  checkTree(name, TREES[name]);
}

// ------------------------------------------------------------------ report
if (errors.length) {
  console.error(`check-i18n: ${errors.length} problem(s)\n`);
  for (const e of errors) console.error(`  ✗ ${e}`);
  process.exit(1);
}
console.log(`check-i18n: OK — ${summaries.join('; ')}; no hard-coded UI strings.`);
