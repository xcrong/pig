#!/usr/bin/env node
// Postinstall: fetch the prebuilt pig binary matching this package version
// from the xcrong/pig GitHub Release and install it into the managed layout
// (`<pig-home>/bin/pig`), then pin `[cli].installer = "npm"` so `pig update`
// delegates future updates to the package manager.
//
// Only linux-x64 and macos-arm64 have release assets today; other platforms
// print a pointer and succeed (never fail the install).
// Requires `curl` + `tar` on PATH (present on macOS and virtually all Linux).
'use strict';

const path = require('path');
const fs = require('fs');
const os = require('os');
const { execSync, execFileSync } = require('child_process');

const REPO = 'xcrong/pig';
const ASSETS = {
  // node `${process.platform}-${process.arch}` -> release asset name
  'darwin-arm64': 'pig-macos-arm64.tar.gz',
  'linux-x64': 'pig-linux-x86_64.tar.gz',
};

let version;
try { version = require('../package.json').version; } catch { version = undefined; }

function pigHome() {
  for (const key of ['PIG_HOME', 'GROK_HOME']) {
    const v = process.env[key];
    if (v && v.trim()) return v;
  }
  return path.join(os.homedir(), '.config', 'pig');
}

function sh(cmd) {
  execSync(cmd, { stdio: ['ignore', 'pipe', 'pipe'] }).toString();
}

function main() {
  const key = `${process.platform}-${process.arch}`;
  const asset = ASSETS[key];
  if (!asset || !version) {
    console.error(`@xcrong/pig: no prebuilt binary for ${key}; see https://github.com/${REPO}/releases`);
    return;
  }
  const home = pigHome();
  const binDir = path.join(home, 'bin');
  const vendorDir = path.join(__dirname, '..', 'vendor');
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'pig-npm-'));
  try {
    const url = `https://github.com/${REPO}/releases/download/v${version}/${asset}`;
    console.error(`@xcrong/pig: downloading pig v${version} (${key})...`);
    sh(`curl -fsSL --retry 3 --max-time 600 -o ${JSON.stringify(path.join(tmp, asset))} ${JSON.stringify(url)}`);
    sh(`tar -xzf ${JSON.stringify(path.join(tmp, asset))} -C ${JSON.stringify(tmp)}`);
    const binName = process.platform === 'win32' ? 'pig.exe' : 'pig';
    const src = path.join(tmp, binName);
    if (!fs.existsSync(src)) throw new Error(`asset ${asset} did not contain ${binName}`);
    fs.mkdirSync(binDir, { recursive: true });
    fs.mkdirSync(vendorDir, { recursive: true });
    for (const dst of [path.join(binDir, binName), path.join(vendorDir, binName)]) {
      fs.copyFileSync(src, dst);
      if (process.platform !== 'win32') fs.chmodSync(dst, 0o755);
    }
    pinInstallerNpm(home);
    try {
      execFileSync(path.join(binDir, binName), ['--version'], { stdio: ['ignore', 'pipe', 'pipe'] });
    } catch {
      console.error('@xcrong/pig: warning: installed binary failed --version smoke check');
    }
    console.error(`@xcrong/pig: installed pig v${version} to ${binDir}`);
  } catch (e) {
    console.error(`@xcrong/pig: postinstall download failed (${e.message || e});`);
    console.error(`  install manually from https://github.com/${REPO}/releases and re-run, or set PIG_HOME.`);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// Minimal TOML append: ensure a `[cli]` table with `installer = "npm"` exists
// without disturbing anything else. Pure string surgery, no TOML dep.
function pinInstallerNpm(home) {
  const cfgPath = path.join(home, 'config.toml');
  let text = '';
  try { text = fs.readFileSync(cfgPath, 'utf8'); } catch { text = ''; }
  if (/^\s*installer\s*=/m.test(text.split(/^\[cli\]/m)[1]?.split(/^\[/m)[0] ?? '')) return;
  if (!/^\[cli\]/m.test(text)) {
    if (text && !text.endsWith('\n')) text += '\n';
    text += '\n[cli]\ninstaller = "npm"\n';
  } else {
    text = text.replace(/^\[cli\]/m, '[cli]\ninstaller = "npm"');
  }
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(cfgPath, text);
}

main();
