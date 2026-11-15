#!/usr/bin/env node
// Copies the few third-party browser libraries the UI uses into ui/vendor.
// Run after `npm install` (wired as `postinstall`). No bundler involved.
import { copyFileSync, mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const nm = join(root, 'node_modules');
const out = join(root, 'ui', 'vendor');

const files = [
  ['@xterm/xterm/lib/xterm.mjs', 'xterm/xterm.mjs', true],
  ['@xterm/xterm/css/xterm.css', 'xterm/xterm.css', false],
  ['@xterm/xterm/LICENSE', 'xterm/LICENSE', false],
  ['@xterm/addon-fit/lib/addon-fit.mjs', 'xterm/addon-fit.mjs', true],
  ['qrcode-generator/dist/qrcode.mjs', 'qrcode/qrcode.mjs', true],
];

for (const [from, to, stripMap] of files) {
  const src = join(nm, from);
  if (!existsSync(src)) {
    console.error(`vendor: missing ${from} — run npm install first`);
    process.exit(1);
  }
  const dest = join(out, to);
  mkdirSync(dirname(dest), { recursive: true });
  if (stripMap) {
    const code = readFileSync(src, 'utf8').replace(/\n?\/\/# sourceMappingURL=.*$/m, '\n');
    writeFileSync(dest, code);
  } else {
    copyFileSync(src, dest);
  }
  console.log(`vendor: ${from} -> ui/vendor/${to}`);
}

// qrcode-generator ships its MIT notice in the README header; keep a copy next to it.
const qrPkg = JSON.parse(readFileSync(join(nm, 'qrcode-generator/package.json'), 'utf8'));
writeFileSync(
  join(out, 'qrcode', 'LICENSE'),
  `qrcode-generator ${qrPkg.version} — Copyright (c) 2009 Kazuhiko Arase — MIT License\nhttps://github.com/kazuhikoarase/qrcode-generator\n`,
);
