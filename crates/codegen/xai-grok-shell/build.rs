//! Build script for the grok-shell crate.
//!
//! 1. Vendor snapshots: pull pi's runtime model catalog
//!    (`https://pi.dev/api/models/providers/<id>?types=chat`, same source and
//!    validation rules as `scripts/sync-pi-vendors.sh`) into `OUT_DIR`, where
//!    the crate embeds them via `include_str!(concat!(env!("OUT_DIR"), ...))`.
//!    Offline / sandboxed builds fall back to the checked-in
//!    `src/agent/vendors/data/*.json`, so a missing network never fails the
//!    build. The pulled version is reported via `cargo:warning=`; nothing is
//!    written back to git (`manifest.json` stays the manual-sync channel).
//! 2. ripgrep bundling (release builds only, see below):
//!    - If `GROK_SHELL_BUNDLE_RG_PATH` is set, always bundle it
//!    - Otherwise, only bundle in release builds
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RG_VER: &str = "15.0.0";

/// Builtin vendor ids, mirroring `VENDORS` in `src/agent/vendors/mod.rs` and
/// `PROVIDERS` in `scripts/sync-pi-vendors.sh`. Deliberately duplicated:
/// build scripts are standalone crates and cannot share the library's items.
const VENDOR_PROVIDERS: &[&str] = &["opencode", "opencode-go"];
const VENDOR_API_BASE: &str = "https://pi.dev/api/models/providers";
/// Same UA as the manual sync script so pi.dev sees one client shape.
const VENDOR_SYNC_UA: &str = "pig-vendor-sync/1.0";
const VENDOR_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Backends pig's sampler speaks; mirrors `api_backend_for` in
/// `src/agent/vendors/mod.rs` minus the wire mapping (build.rs only counts).
const VENDOR_SUPPORTED_APIS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only bundle in release builds to avoid slowing down cargo check.
    println!("cargo:rerun-if-env-changed=GROK_SHELL_BUNDLE_RG_PATH");
    println!("cargo:rerun-if-env-changed=GROK_SHELL_RG_DOWNLOAD_BASE");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    println!("cargo:rerun-if-env-changed=PIG_VENDOR_FETCH");
    // Declare our custom cfg to the compiler so cfg(bundle_rg) is recognized by lints
    println!("cargo:rustc-check-cfg=cfg(bundle_rg)");

    // Refresh vendor snapshots on every build-script run (any package file
    // change reruns this script; no `rerun-if-changed` is emitted on purpose
    // so online builds embed the build-time catalog, not a stale copy).
    // Always succeeds: offline failures fall back to the checked-in data.
    refresh_vendor_snapshots()?;

    // Bundle when a path override is set or this is a release build
    // Bail before touching the filesystem so debug `cargo check` needs no environment
    let path_override = env::var("GROK_SHELL_BUNDLE_RG_PATH").ok();
    let is_release = env::var("PROFILE").as_deref() == Ok("release");
    if path_override.is_none() && !is_release {
        return Ok(());
    }

    // In Bazel builds, write into OUT_DIR; XAI_ROOT/target/tmp is read-only inside the sandbox
    // Outside Bazel, prefer XAI_ROOT's shared cache dir and fall back to OUT_DIR for standalone checkouts where XAI_ROOT is unset
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let in_bazel = is_bazel_build(&manifest_dir);
    let gen_dir = if in_bazel {
        // OUT_DIR is always set by Cargo/Bazel for build scripts.
        PathBuf::from(env::var("OUT_DIR")?)
    } else if let Ok(xai_root) = env::var("XAI_ROOT") {
        PathBuf::from(xai_root).join("target/tmp/grok-shell-bundle-rg")
    } else {
        PathBuf::from(env::var("OUT_DIR")?)
    };
    fs::create_dir_all(&gen_dir)?;

    // Skip auto-bundling on Windows: ripgrep ships .zip archives there and this script only extracts .tar.gz
    // Returning before `cargo:rustc-cfg=bundle_rg` keeps the include_bytes! macros compiled out The runtime then falls back to `rg` on PATH (see src/util/ripgrep.rs::rg_path)
    // Users install via `winget install BurntSushi.ripgrep.MSVC` or `scoop install ripgrep` An explicit GROK_SHELL_BUNDLE_RG_PATH still bundles on Windows; the override branch below copies any binary regardless of target
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" && path_override.is_none() {
        return Ok(());
    }

    // Expose cfg so the crate can include the bundled bytes.
    println!("cargo:rustc-cfg=bundle_rg");
    println!("cargo:rustc-env=GROK_SHELL_RG_VER={}", RG_VER);
    println!(
        "cargo:rustc-env=GROK_SHELL_RG_GEN_DIR={}",
        gen_dir.display()
    );

    // If a local rg binary is provided, copy it directly and skip the target check
    if let Some(path) = path_override {
        let dest = gen_dir.join(format!("rg-{}-override.bin", RG_VER));
        println!("cargo:rustc-env=GROK_SHELL_RG_TARGET=override");
        let _ = fs::remove_file(&dest);
        fs::copy(PathBuf::from(path.clone()), &dest).map_err(|e| {
            format!(
                "Failed copying GROK_SHELL_BUNDLE_RG_PATH: {e} from path {path} to dest {}",
                dest.display()
            )
        })?;
        return Ok(());
    }

    // Determine supported ripgrep asset triple for auto-download.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let asset_triple = match (target_os.as_str(), target_arch.as_str()) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => {
            return Err(format!(
                "Unsupported target for ripgrep bundling: {os}-{arch}. Set GROK_SHELL_BUNDLE_RG_PATH to a local rg binary for offline or unsupported builds.",
                os = target_os,
                arch = target_arch
            ).into());
        }
    };

    println!("cargo:rustc-env=GROK_SHELL_RG_TARGET={}", asset_triple);
    let dest = gen_dir.join(format!("rg-{}-{}.bin", RG_VER, asset_triple));
    let _ = fs::remove_file(&dest);

    // The download base is overridable so sandboxed or offline CI can point at an internal mirror; it defaults to the public GitHub releases URL
    // Example: GROK_SHELL_RG_DOWNLOAD_BASE=http://<mirror>/github/BurntSushi/ripgrep/releases/download
    let download_base = env::var("GROK_SHELL_RG_DOWNLOAD_BASE")
        .unwrap_or_else(|_| "https://github.com/BurntSushi/ripgrep/releases/download".to_string());
    let url = format!(
        "{base}/{v}/ripgrep-{v}-{t}.tar.gz",
        base = download_base.trim_end_matches('/'),
        v = RG_VER,
        t = asset_triple
    );

    let bytes: Vec<u8> = {
        let resp = reqwest::blocking::get(&url).map_err(|e| {
            format!(
                "Failed to download ripgrep: {}\nSet GROK_SHELL_BUNDLE_RG_PATH to a local rg for offline builds.",
                e
            )
        })?;
        if !resp.status().is_success() {
            return Err(format!(
                "HTTP {} downloading ripgrep. Set GROK_SHELL_BUNDLE_RG_PATH for offline builds.",
                resp.status()
            )
            .into());
        }
        resp.bytes()?.to_vec()
    };

    let gz = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut ar = tar::Archive::new(gz);
    let mut found = false;
    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?;
        if p.file_name().is_some_and(|n| n == "rg") {
            let data: Vec<u8> = {
                let mut v = Vec::new();
                io::copy(&mut e, &mut v)?;
                v
            };
            fs::write(&dest, &data)?;
            found = true;
            break;
        }
    }

    if !found {
        return Err(format!(
            "Could not find 'rg' in ripgrep archive {}. Set GROK_SHELL_BUNDLE_RG_PATH for offline builds.",
            url
        )
        .into());
    }

    Ok(())
}

/// Pull pi's runtime catalog for every builtin vendor into
/// `OUT_DIR/pig-vendor-snapshots/<id>.json` (embedded by the crate via
/// `include_str!`). Fetch failures (offline, sandbox without network,
/// validation mismatch) fall back to the checked-in
/// `src/agent/vendors/data/<id>.json` with a `cargo:warning=` note, so
/// offline builds keep working. Only a missing fallback file is fatal --
/// exactly like the old direct `include_str!("data/...")`.
fn refresh_vendor_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    // OUT_DIR is always set by Cargo for build scripts (Bazel included).
    let dest_dir = PathBuf::from(env::var("OUT_DIR")?).join("pig-vendor-snapshots");
    fs::create_dir_all(&dest_dir)?;
    for id in VENDOR_PROVIDERS {
        let dest = dest_dir.join(format!("{id}.json"));
        let fallback = manifest_dir
            .join("src/agent/vendors/data")
            .join(format!("{id}.json"));
        match fetch_validated_vendor_snapshot(id) {
            Ok((text, stats)) => {
                fs::write(&dest, &text)?;
                let mut notes = Vec::new();
                if !stats.skipped.is_empty() {
                    notes.push(format!("skipped {:?}", stats.skipped));
                }
                if !stats.skipped_types.is_empty() {
                    notes.push(format!("skipped types {:?}", stats.skipped_types));
                }
                let notes = if notes.is_empty() {
                    String::new()
                } else {
                    format!(", {}", notes.join(", "))
                };
                println!(
                    "cargo:warning=pig vendor snapshot {id}: embedded pi.dev build-time refresh ({total} models, {mapped} mapped{notes})",
                    total = stats.total,
                    mapped = stats.mapped,
                );
            }
            Err(reason) => {
                fs::copy(&fallback, &dest).map_err(|e| {
                    format!(
                        "vendor snapshot {id}: build-time refresh failed ({reason}) and checked-in fallback unreadable: {e}"
                    )
                })?;
                println!(
                    "cargo:warning=pig vendor snapshot {id}: using checked-in fallback ({reason})"
                );
            }
        }
    }
    Ok(())
}

struct VendorStats {
    total: usize,
    mapped: usize,
    skipped: Vec<String>,
    skipped_types: Vec<String>,
}

/// Explicit opt-out for hermetic builds (`PIG_VENDOR_FETCH=0`), plus cargo's
/// own offline flag. Anything else attempts the fetch; failures still fall
/// back instead of failing the build.
fn vendor_fetch_allowed() -> bool {
    if env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true") {
        return false;
    }
    match env::var("PIG_VENDOR_FETCH") {
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "off" | "no" | "false" | "never"
        ),
        Err(_) => true,
    }
}

fn fetch_validated_vendor_snapshot(id: &str) -> Result<(String, VendorStats), String> {
    if !vendor_fetch_allowed() {
        return Err("fetch disabled (offline env)".to_string());
    }
    let url = format!("{VENDOR_API_BASE}/{id}?types=chat");
    let client = xai_grok_extra_ca::build_blocking_reqwest_client(|builder| {
        builder
            .timeout(VENDOR_FETCH_TIMEOUT)
            .user_agent(VENDOR_SYNC_UA)
    })
    .map_err(|e| format!("client build failed: {e}"))?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("fetch failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let text = resp.text().map_err(|e| format!("read body failed: {e}"))?;
    let stats = validate_vendor_snapshot(id, &text)?;
    Ok((text, stats))
}

/// Same validation rules as `scripts/sync-pi-vendors.sh`: a non-empty
/// catalog whose `chat` entries (pi serves side types such as `classifier`
/// under `?types=chat`; those are skipped and counted, exactly like the
/// mapping layer's `is_chat_model`) each carry
/// `id`/`api`/`baseUrl`/`contextWindow`, with at least one entry on a backend
/// pig speaks. Anything else is rejected so a corrupt or shape-shifted
/// payload can never poison the catalog.
fn validate_vendor_snapshot(_id: &str, text: &str) -> Result<VendorStats, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let items: Vec<&serde_json::Value> = parsed
        .as_array()
        .map(|arr| arr.iter().collect())
        .or_else(|| {
            parsed
                .get("models")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().collect())
        })
        .unwrap_or_default();
    if items.is_empty() {
        return Err("empty catalog".to_string());
    }
    // pi serves side types (e.g. `classifier`) under `?types=chat`; skip and
    // count them like the mapping layer does, and only validate chat entries.
    let mut chat_count = 0;
    let mut skipped_type_counts = std::collections::BTreeMap::new();
    for m in &items {
        let typ = m.get("type").and_then(|v| v.as_str()).unwrap_or("chat");
        if typ != "chat" {
            *skipped_type_counts.entry(typ.to_string()).or_insert(0) += 1;
            continue;
        }
        chat_count += 1;
        for field in ["id", "api", "baseUrl", "contextWindow"] {
            if m.get(field).is_none() {
                let mid = m.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                return Err(format!("{mid}: missing {field}"));
            }
        }
    }
    if chat_count == 0 {
        return Err("no chat entries".to_string());
    }
    // Coverage counts only consider chat entries; side types are reported
    // separately as skipped types.
    let is_chat =
        |m: &&serde_json::Value| m.get("type").and_then(|v| v.as_str()).unwrap_or("chat") == "chat";
    let mapped = items
        .iter()
        .filter(|m| {
            is_chat(m)
                && m.get("api")
                    .and_then(|v| v.as_str())
                    .is_some_and(|api| VENDOR_SUPPORTED_APIS.contains(&api))
        })
        .count();
    if mapped == 0 {
        return Err("no mappable entries".to_string());
    }
    let mut skipped_counts = std::collections::BTreeMap::new();
    for m in items.iter().filter(|m| is_chat(m)) {
        let api = m.get("api").and_then(|v| v.as_str()).unwrap_or("?");
        if !VENDOR_SUPPORTED_APIS.contains(&api) {
            *skipped_counts.entry(api.to_string()).or_insert(0) += 1;
        }
    }
    let skipped = skipped_counts
        .into_iter()
        .map(|(api, n)| format!("{api} x{n}"))
        .collect();
    let skipped_types = skipped_type_counts
        .into_iter()
        .map(|(typ, n)| format!("{typ} x{n}"))
        .collect();
    Ok(VendorStats {
        total: items.len(),
        mapped,
        skipped,
        skipped_types,
    })
}

fn is_bazel_build(manifest_dir: &Path) -> bool {
    let manifest_dir_str = manifest_dir.to_string_lossy();
    env::var_os("BAZEL_WORKSPACE").is_some()
        || env::var_os("BUILD_WORKSPACE_DIRECTORY").is_some()
        || env::var_os("BAZEL_EXECUTION_ROOT").is_some()
        || env::var_os("BAZEL_OUTPUT_BASE").is_some()
        || manifest_dir_str.contains("/execroot/")
        || manifest_dir_str.contains("/bazel-out/")
}
