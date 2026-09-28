#!/usr/bin/env bash
# Refresh pig's third-party vendor snapshots from pi's runtime model catalog.
#
#   pig/scripts/sync-pi-vendors.sh [--check]
#
# pi generates its catalog at build time (models.dev, OpenRouter, NVIDIA NIM,
# Vercel AI Gateway, Radius) and serves the merged result at
# https://pi.dev/api/models/providers/<id>. pig reuses the *served* catalog so
# this sync needs only curl + python3 -- no Node, no pi checkout. Run it
# manually or from CI (weekly cron), then review the diff to
# crates/codegen/xai-grok-shell/src/agent/vendors/{data,manifest.json}.
#
# --check: verify the checked-in snapshots match the manifest hashes without
# downloading (for CI drift guards).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VENDOR_DIR="$REPO_ROOT/crates/codegen/xai-grok-shell/src/agent/vendors"
DATA_DIR="$VENDOR_DIR/data"
MANIFEST="$VENDOR_DIR/manifest.json"
PROVIDERS="opencode opencode-go"
UA="pig-vendor-sync/1.0"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

if [[ "${1:-}" == "--check" ]]; then
  python3 - "$MANIFEST" "$DATA_DIR" <<'EOF'
import hashlib, json, sys
manifest_path, data_dir = sys.argv[1], sys.argv[2]
manifest = json.load(open(manifest_path))
ok = True
for pid, info in manifest["providers"].items():
    raw = open(f"{data_dir}/{pid}.json", "rb").read()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != info["sha256"]:
        print(f"drift: {pid}.json sha256 {digest} != manifest {info['sha256']}", file=sys.stderr)
        ok = False
    items = json.loads(raw)
    items = items if isinstance(items, list) else items.get("models", [])
    if len(items) != info["totalModels"]:
        print(f"drift: {pid} has {len(items)} models, manifest says {info['totalModels']}", file=sys.stderr)
        ok = False
sys.exit(0 if ok else 1)
EOF
  echo "vendor snapshots match manifest"
  exit 0
fi

mkdir -p "$DATA_DIR"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

for pid in $PROVIDERS; do
  url="https://pi.dev/api/models/providers/${pid}?types=chat"
  echo "fetching $pid from $url"
  curl -fsSL -m 60 -A "$UA" --retry 3 "$url" -o "$TMP/${pid}.json"
  python3 - "$TMP/${pid}.json" "$pid" <<'EOF'
import json, sys
path, pid = sys.argv[1], sys.argv[2]
items = json.load(open(path))
items = items if isinstance(items, list) else items.get("models", [])
assert items, f"{pid}: empty catalog"
for m in items:
    for field in ("id", "api", "baseUrl", "contextWindow"):
        assert field in m, f"{pid}/{m.get('id')}: missing {field}"
    assert m.get("type", "chat") == "chat", f"{pid}/{m.get('id')}: non-chat entry"
print(f"{pid}: {len(items)} models validated")
EOF
  cp "$TMP/${pid}.json" "$DATA_DIR/${pid}.json"
done

python3 - "$MANIFEST" "$DATA_DIR" <<'EOF'
import datetime, hashlib, json, sys
from collections import Counter
manifest_path, data_dir = sys.argv[1], sys.argv[2]
manifest = json.load(open(manifest_path))
supported = {"openai-completions", "openai-responses", "anthropic-messages"}
for pid, info in manifest["providers"].items():
    raw = open(f"{data_dir}/{pid}.json", "rb").read()
    items = json.loads(raw)
    items = items if isinstance(items, list) else items.get("models", [])
    chat = [m for m in items if m.get("type", "chat") == "chat"]
    skipped = Counter(m.get("api") for m in chat if m.get("api") not in supported)
    info["sha256"] = hashlib.sha256(raw).hexdigest()
    info["totalModels"] = len(items)
    info["mappedModels"] = sum(1 for m in chat if m.get("api") in supported)
    info["skippedApis"] = dict(sorted(skipped.items()))
manifest["fetchedAt"] = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
json.dump(manifest, open(manifest_path, "w"), indent=2)
open(manifest_path, "a").write("\n")
print(f"manifest updated: {manifest_path}")
EOF

echo "done. review the diff, then run: cargo test -p xai-grok-shell vendors::"
