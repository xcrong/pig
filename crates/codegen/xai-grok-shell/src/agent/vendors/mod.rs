//! Third-party vendor model catalog, mirroring pi's provider directory.
//!
//! pi generates its catalog at build time (`pi/packages/ai/scripts/generate-models.ts`
//! from `models.dev`, OpenRouter, NVIDIA NIM, Vercel AI Gateway, Radius) and serves
//! runtime overlays from `https://pi.dev/api/models/providers/<id>`. Running pi's
//! Node generator inside `cargo build` would break offline builds, so pig reuses
//! the *served* catalog instead: `data/*.json` are verbatim snapshots of pi's
//! runtime API, refreshed by `scripts/sync-pi-vendors.sh` (see `manifest.json`).
//!
//! Mapping notes (pi model -> [`ModelEntry`]):
//! * Only `type: "chat"` entries are mapped; image/classifier entries are ignored.
//! * `api` maps to [`ApiBackend`]: `openai-completions` -> `ChatCompletions`,
//!   `openai-responses` -> `Responses`, `anthropic-messages` -> `Messages`.
//!   `google-generative-ai` is explicitly unsupported (see
//!   [`UNSUPPORTED_VENDOR_APIS`]) and filtered out; any other unknown shape is
//!   likewise skipped but logs a warning so it cannot disappear silently.
//! * pi's Anthropic base URLs omit `/v1` (their SDK appends `/v1/messages`); pig
//!   appends only `messages`, so `/v1` is added when missing.
//! * Every mapped model gets `env_key = OPENCODE_API_KEY` and
//!   `session_header = x-opencode-session`. The per-turn value is the session id,
//!   injected by `xai_grok_sampler::SamplingClient` (see `apply_session_header`).
//! * Catalog keys are namespaced as `<vendor>/<model-id>`; bare model ids still
//!   resolve via [`find_model_by_id`](super::config::find_model_by_id), which also
//!   matches the wire slug. User `[model.*]` entries always win over vendor keys.

use std::num::NonZeroU64;

use indexmap::IndexMap;

use super::config::{EnvKeys, ModelEntry, ModelInfo};
use super::model_providers::ModelProviderConfig;
use crate::sampling::ApiBackend;

/// Per-conversation routing header required by OpenCode Zen / OpenCode Go.
/// Mirrors pi's `withOpenCodeSessionHeader`
/// (`pi/packages/ai/src/providers/opencode-headers.ts`).
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";
/// Environment variable holding the OpenCode API key (both vendors).
pub const OPENCODE_ENV_KEY: &str = "OPENCODE_API_KEY";

/// Built-in vendor ids, matching pi's provider ids.
pub const VENDOR_OPENCODE: &str = "opencode";
pub const VENDOR_OPENCODE_GO: &str = "opencode-go";

/// API shapes pig's sampler cannot speak. Models on these are filtered out of
/// vendor snapshots **by design** (`google-generative-ai` is deliberately
/// unsupported -- its wire protocol is too exotic to map onto pig's three
/// backends). If pi ever serves a shape outside both the supported mapping and
/// this list, `map_vendor_snapshot` logs a warning and the
/// `unsupported_apis_are_explicit` test fails, forcing a conscious decision
/// instead of a silent drop.
pub const UNSUPPORTED_VENDOR_APIS: &[&str] = &["google-generative-ai"];

/// A third-party vendor pig ships a catalog snapshot for.
pub struct Vendor {
    /// Matches pi's provider id (`opencode`, `opencode-go`).
    pub id: &'static str,
    /// Human-readable label for logs/docs.
    pub display_name: &'static str,
    /// Snapshot of pi's runtime catalog (`GET /api/models/providers/<id>`).
    pub snapshot_json: &'static str,
    /// Default inference base URL for models that omit one.
    pub default_base_url: &'static str,
}

const OPENCODE_SNAPSHOT: &str = include_str!("data/opencode.json");
const OPENCODE_GO_SNAPSHOT: &str = include_str!("data/opencode-go.json");

/// Vendors in load order. Keys are namespaced per vendor, so order only matters
/// for logs.
pub const VENDORS: &[Vendor] = &[
    Vendor {
        id: VENDOR_OPENCODE,
        display_name: "OpenCode Zen",
        snapshot_json: OPENCODE_SNAPSHOT,
        default_base_url: "https://opencode.ai/zen/v1",
    },
    Vendor {
        id: VENDOR_OPENCODE_GO,
        display_name: "OpenCode Go",
        snapshot_json: OPENCODE_GO_SNAPSHOT,
        default_base_url: "https://opencode.ai/zen/go/v1",
    },
];

/// Whether `id` names a builtin vendor preset (`opencode`, `opencode-go`).
/// Used by config validation so `model_provider = "opencode"` does not warn as
/// undefined when the user relies on the preset instead of a hand-written block.
pub fn is_builtin_vendor(id: &str) -> bool {
    VENDORS.iter().any(|v| v.id == id)
}

/// Provider presets so `[model.x] model_provider = "opencode"` works without the
/// user hand-writing `base_url`/`env_key`/`session_header`.
pub fn builtin_vendor_providers() -> IndexMap<String, ModelProviderConfig> {
    let mut providers = IndexMap::new();
    for vendor in VENDORS {
        providers.insert(
            vendor.id.to_string(),
            ModelProviderConfig {
                base_url: Some(vendor.default_base_url.to_string()),
                env_key: Some(EnvKeys::single(OPENCODE_ENV_KEY)),
                session_header: Some(OPENCODE_SESSION_HEADER.to_string()),
                ..Default::default()
            },
        );
    }
    providers
}

/// Effective `[model_providers]` table: builtin vendor presets fill gaps, explicit
/// user entries always win.
pub fn effective_model_providers(
    configured: &IndexMap<String, ModelProviderConfig>,
) -> IndexMap<String, ModelProviderConfig> {
    let mut effective = builtin_vendor_providers();
    for (id, provider) in configured {
        effective.insert(id.clone(), provider.clone());
    }
    effective
}

fn api_backend_for(pi_api: &str) -> Option<ApiBackend> {
    match pi_api {
        "openai-completions" => Some(ApiBackend::ChatCompletions),
        "openai-responses" => Some(ApiBackend::Responses),
        "anthropic-messages" => Some(ApiBackend::Messages),
        _ => None,
    }
}

/// pig appends only `messages` for the Messages backend, while pi's Anthropic
/// base URLs omit `/v1` (their SDK appends `/v1/messages`). Normalize so vendor
/// snapshots route correctly.
fn normalize_base_url(base_url: &str, api_backend: &ApiBackend) -> String {
    let base = base_url.trim_end_matches('/').to_string();
    if *api_backend == ApiBackend::Messages && !base.ends_with("/v1") {
        format!("{base}/v1")
    } else {
        base
    }
}

fn is_chat_model(value: &serde_json::Value) -> bool {
    value
        .get("type")
        .and_then(|t| t.as_str())
        .is_none_or(|t| t == "chat")
}

fn context_window_of(value: &serde_json::Value) -> NonZeroU64 {
    value
        .get("contextWindow")
        .and_then(|v| v.as_u64())
        .and_then(NonZeroU64::new)
        .unwrap_or_else(|| {
            tracing::warn!(
                model = value.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                "vendor model missing contextWindow; defaulting to 200000"
            );
            NonZeroU64::new(200_000).expect("200000 is non-zero")
        })
}

/// Parse one vendor snapshot into catalog entries keyed `<vendor>/<model-id>`.
/// Returns `(mapped, skipped_by_api)` so callers can log coverage.
fn map_vendor_snapshot(vendor: &Vendor) -> (IndexMap<String, ModelEntry>, Vec<String>) {
    let mut mapped = IndexMap::new();
    let mut skipped = Vec::new();
    let parsed: serde_json::Value = match serde_json::from_str(vendor.snapshot_json) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(vendor = vendor.id, %error, "vendor snapshot is not valid JSON; skipping");
            return (mapped, skipped);
        }
    };
    let models = parsed
        .as_array()
        .map(|arr| arr.iter().collect::<Vec<_>>())
        .or_else(|| {
            parsed
                .get("models")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    for model in models {
        if !is_chat_model(model) {
            continue;
        }
        let Some(id) = model.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let api = model
            .get("api")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let Some(api_backend) = api_backend_for(api) else {
            if UNSUPPORTED_VENDOR_APIS.contains(&api) {
                skipped.push(format!("{id} ({api})"));
            } else {
                // A shape pi started serving that pig knows nothing about.
                // Skip it like the rest, but loudly: extending support (or the
                // explicit unsupported list) is a conscious decision.
                tracing::warn!(
                    vendor = vendor.id,
                    model = id,
                    api,
                    "vendor model skipped: unknown api backend"
                );
                skipped.push(format!("{id} ({api})"));
            }
            continue;
        };
        let base_url = model
            .get("baseUrl")
            .and_then(|v| v.as_str())
            .unwrap_or(vendor.default_base_url);
        let mut info = ModelInfo::fallback(id);
        info.base_url = normalize_base_url(base_url, &api_backend);
        info.name = model
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        info.api_backend = api_backend;
        info.context_window = context_window_of(model);
        info.max_completion_tokens = model
            .get("maxTokens")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        info.session_header = Some(OPENCODE_SESSION_HEADER.to_string());
        let entry = ModelEntry {
            info,
            mtls_cert_dir: None,
            api_key: None,
            env_key: Some(EnvKeys::single(OPENCODE_ENV_KEY)),
            auth_provider: None,
            api_base_url: None,
        };
        mapped.insert(format!("{}/{id}", vendor.id), entry);
    }
    (mapped, skipped)
}

/// All builtin vendor models. Keys are namespaced; user `[model.*]` entries take
/// precedence (see `resolve_model_list`).
pub fn builtin_vendor_models() -> IndexMap<String, ModelEntry> {
    let mut all = IndexMap::new();
    for vendor in VENDORS {
        let (models, skipped) = map_vendor_snapshot(vendor);
        tracing::debug!(
            vendor = vendor.id,
            mapped = models.len(),
            skipped = skipped.len(),
            "loaded vendor catalog snapshot"
        );
        if !skipped.is_empty() {
            tracing::debug!(
                vendor = vendor.id,
                skipped = ?skipped,
                "vendor models skipped: unsupported api backend"
            );
        }
        all.extend(models);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_snapshots_map_to_namespaced_models() {
        let models = builtin_vendor_models();
        assert!(!models.is_empty(), "expected mapped vendor models");
        for (key, entry) in &models {
            let (vendor, id) = key.split_once('/').expect("namespaced key");
            assert!(
                vendor == VENDOR_OPENCODE || vendor == VENDOR_OPENCODE_GO,
                "unexpected vendor in {key}"
            );
            assert_eq!(entry.info.model, id);
            assert_eq!(
                entry.info.session_header.as_deref(),
                Some(OPENCODE_SESSION_HEADER)
            );
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary).as_deref(),
                Some(OPENCODE_ENV_KEY)
            );
            assert!(
                entry.info.base_url.starts_with("https://opencode.ai/"),
                "unexpected base_url for {key}: {}",
                entry.info.base_url
            );
            if entry.info.api_backend == ApiBackend::Messages {
                assert!(
                    entry.info.base_url.ends_with("/v1"),
                    "messages backend must carry /v1 for {key}"
                );
            }
        }
    }

    #[test]
    fn unsupported_apis_are_skipped_not_fatal() {
        for vendor in VENDORS {
            let (mapped, skipped) = map_vendor_snapshot(vendor);
            assert!(!mapped.is_empty(), "{} mapped nothing", vendor.id);
            for entry in &skipped {
                let api = entry
                    .rsplit('(')
                    .next()
                    .and_then(|s| s.strip_suffix(')'))
                    .unwrap_or("?");
                assert!(
                    UNSUPPORTED_VENDOR_APIS.contains(&api),
                    "{} skipped {entry}: unknown api shapes must be added to UNSUPPORTED_VENDOR_APIS or mapped, never silently dropped",
                    vendor.id
                );
            }
        }
        // opencode serves Gemini via google-generative-ai, which pig explicitly
        // does not support.
        let (_, skipped) = map_vendor_snapshot(&VENDORS[0]);
        assert!(
            skipped.iter().any(|s| s.contains("google-generative-ai")),
            "expected google-generative-ai skips, got {skipped:?}"
        );
    }

    #[test]
    fn user_providers_win_over_vendor_presets() {
        let mut configured = IndexMap::new();
        configured.insert(
            VENDOR_OPENCODE.to_string(),
            ModelProviderConfig {
                base_url: Some("https://custom.example/v1".to_string()),
                ..Default::default()
            },
        );
        let effective = effective_model_providers(&configured);
        assert_eq!(
            effective
                .get(VENDOR_OPENCODE)
                .and_then(|p| p.base_url.as_deref()),
            Some("https://custom.example/v1")
        );
        // The untouched vendor still gets its preset, including the session header.
        assert_eq!(
            effective
                .get(VENDOR_OPENCODE_GO)
                .and_then(|p| p.session_header.as_deref()),
            Some(OPENCODE_SESSION_HEADER)
        );
    }

    #[test]
    fn provider_preset_carries_session_header() {
        let providers = builtin_vendor_providers();
        for id in [VENDOR_OPENCODE, VENDOR_OPENCODE_GO] {
            let preset = providers.get(id).expect("preset exists");
            assert_eq!(
                preset.session_header.as_deref(),
                Some(OPENCODE_SESSION_HEADER),
                "{id} preset must opt into the session header"
            );
        }
    }
}
