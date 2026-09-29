# @xcrong/pig

Pig Agent: a general-purpose agent harness (terminal UI), installed via npm.

```sh
npm i -g @xcrong/pig
pig --help
```

## How it works

Postinstall downloads the prebuilt `pig` binary matching this package version
from the [xcrong/pig GitHub Release](https://github.com/xcrong/pig/releases)
(currently `linux-x64` and `macos-arm64`) into the managed layout
(`~/.config/pig/bin/pig`, or `$PIG_HOME` when set), pins
`[cli] installer = "npm"` in `config.toml`, and the `pig` shim on PATH
execs it. Future updates go through the package manager:
`pig update` delegates to `npm i -g @xcrong/pig`.

## Versioning

`package.json` version tracks the pig release tag (`v<version>`).
Only publish a version whose GitHub Release (with both tarballs) exists.
