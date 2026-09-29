#!/usr/bin/env node
// Shim: exec the installed pig binary.
//
// Resolution order:
//   1. $PIG_HOME / $GROK_HOME (compat) / ~/.config/pig, `<home>/bin/pig`
//      (postinstall.js puts it there, so npm installs converge onto the same
//      managed layout a direct download uses; the updater then sees installer="npm"
//      from config and delegates future updates to npm).
//   2. Package-local `vendor/pig` fallback (kept for offline/air-gapped use).
//   3. Otherwise print where to get it and exit non-zero.
'use strict';

const path = require('path');
const fs = require('fs');
const os = require('os');
const { spawnSync } = require('child_process');

function pigHome() {
  for (const key of ['PIG_HOME', 'GROK_HOME']) {
    const v = process.env[key];
    if (v && v.trim()) return v;
  }
  return path.join(os.homedir(), '.config', 'pig');
}

const candidates = [
  path.join(pigHome(), 'bin', process.platform === 'win32' ? 'pig.exe' : 'pig'),
  path.join(__dirname, '..', 'vendor', process.platform === 'win32' ? 'pig.exe' : 'pig'),
];

for (const bin of candidates) {
  try {
    if (fs.statSync(bin).isFile()) {
      const r = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
      process.exit(r.status == null ? 1 : r.status);
    }
  } catch { /* try next */ }
}

console.error('@xcrong/pig: pig binary not found.');
console.error('  Reinstall to re-run postinstall, or fetch it manually:');
console.error('  https://github.com/xcrong/pig/releases');
process.exit(1);
