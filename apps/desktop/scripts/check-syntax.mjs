#!/usr/bin/env node
// `node --check` every UI module (vendored libraries excluded) and verify
// that every relative ES import resolves to an existing file.
//   node scripts/check-syntax.mjs                     this app (apps/desktop)
//   node scripts/check-syntax.mjs --root ../mobile    another app tree (apps/mobile)
import { readdirSync, statSync, readFileSync, existsSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const rootArg = process.argv.indexOf('--root');
const root = rootArg > 0 ? resolve(process.argv[rootArg + 1]) : join(dirname(fileURLToPath(import.meta.url)), '..');
const ui = join(root, 'ui');
const files = [];
(function walk(d) {
  for (const n of readdirSync(d)) {
    const p = join(d, n);
    if (n === 'vendor') continue;
    if (statSync(p).isDirectory()) walk(p);
    else if (n.endsWith('.js') || n.endsWith('.mjs')) files.push(p);
  }
})(ui);
const scripts = join(root, 'scripts');
if (existsSync(scripts)) files.push(...readdirSync(scripts).filter((f) => f.endsWith('.mjs')).sort().map((f) => join(scripts, f)));

let bad = 0;
for (const f of files) {
  const r = spawnSync(process.execPath, ['--check', f], { encoding: 'utf8' });
  if (r.status !== 0) {
    bad++;
    console.error(`✗ ${relative(root, f)}\n${r.stderr}`);
  }
  const src = readFileSync(f, 'utf8');
  for (const m of src.matchAll(/(?:import|export)\s[^'"]*?from\s*['"](\.{1,2}\/[^'"]+)['"]|import\(\s*['"](\.{1,2}\/[^'"]+)['"]\s*\)/g)) {
    const spec = m[1] || m[2];
    if (!existsSync(resolve(dirname(f), spec))) {
      bad++;
      console.error(`✗ ${relative(root, f)}: unresolved import ${spec}`);
    }
  }
}
if (bad) process.exit(1);
console.log(`check-syntax: OK — ${relative(join(root, '..'), root) || root}: ${files.length} modules parsed, imports resolve.`);
