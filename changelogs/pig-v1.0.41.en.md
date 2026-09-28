# pig v1.0.41 (2026-09-28)

The first pig release: based on upstream 1.0.41, it completes the Pig Agent rebrand, renames the config directory and binary, and adds out-of-the-box third-party vendor models.

## New features

- Third-party vendor model catalog (pi-compatible): ships built-in opencode / opencode-go model snapshots under the `<vendor>/<id>` namespace, both pre-registered and ready to use; includes `scripts/sync-pi-vendors.sh` for snapshot refresh (with drift check).
- Session routing header `session_header`: sends the session id as the header value on every turn (e.g. `x-opencode-session`); subagents and auxiliary models inherit the parent session.

## Branding & UI

- Welcome screen rebranded to Pig Agent with a new braille pig logo (two sizes, selected automatically by terminal height).
- Binary renamed to `pig`; exit resume hints and relaunch failure messages follow the actual binary name.
- Config directory moved to `~/.config/pig` (resolution order: `$PIG_HOME` → `$GROK_HOME` for existing users → default); the legacy `~/.grok` is migrated once on first run (copy only, never delete; falls back to an empty directory on failure).

## Build & release

- Automatic builds on `pig-v*` tags across five targets: Linux x86_64 / ARM64, macOS ARM64 / Intel, Windows x86_64, with binaries attached to an auto-created GitHub Release.
- Bilingual (Chinese/English) release notes shipped per version under `changelogs/`.

## Other

- Upstream baseline: monorepo `036a5d8` (version 1.0.41).
- `google-generative-ai` is explicitly unsupported and filtered out; enterprise locked mode (custom endpoint) skips vendor snapshots.
