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
//! * Every mapped model of an enabled vendor gets the credential from its explicit
//!   `[vendors.<id>]` config (`env_key` / `api_key`); enabled-without-credential
//!   loads credential-less (BYOK). Disabled vendors map nothing and read nothing.
//!   `session_header = x-opencode-session` is always set: the per-turn value is the
//!   session id, injected by `xai_grok_sampler::SamplingClient` (see `apply_session_header`).
//! * Catalog keys are namespaced as `<vendor>/<model-id>`; bare model ids still
//!   resolve via [`find_model_by_id`](super::config::find_model_by_id), which also
//!   matches the wire slug. User `[model.*]` entries always win over vendor keys.

use std::num::NonZeroU64;

use indexmap::IndexMap;

use super::config::{EnvKeys, ModelEntry, ModelInfo};
use super::config_model_override_parse::{ConfigWarning, ConfigWarningKind};
use super::model_providers::ModelProviderConfig;
use crate::sampling::ApiBackend;
use crate::sampling::types::{ReasoningEffort, ReasoningEffortOption, effort_label};

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

/// Whether `id` names an explicitly enabled builtin vendor preset.
/// Vendors are opt-in: only `[vendors.<id>] enabled = true` loads the snapshot
/// catalog and registers the preset. Used by config validation so
/// `model_provider = "opencode"` warns as undefined unless the vendor is enabled
/// (or the user hand-wrote `[model_providers.<id>]`).
pub fn is_enabled_vendor(id: &str, vendors: &IndexMap<String, VendorConfig>) -> bool {
    vendors.get(id).is_some_and(|v| v.enabled)
        && (VENDORS.iter().any(|v| v.id == id) || is_custom_vendor_complete(id, vendors))
}

/// Whether `id` is a custom (non-builtin) vendor with a complete snapshot
/// source: both `base_url` and `snapshot_file` set. Completeness is about
/// declared fields, not file existence; an unreadable snapshot warns and maps
/// nothing at load time.
fn is_custom_vendor_complete(id: &str, vendors: &IndexMap<String, VendorConfig>) -> bool {
    vendors.get(id).is_some_and(custom_source_complete)
}

/// Field-level half of [`is_custom_vendor_complete`], for entries not yet stored.
fn custom_source_complete(cfg: &VendorConfig) -> bool {
    cfg.base_url
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
        && cfg
            .snapshot_file
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
}

/// User-declared `[vendors.<id>]` entry: explicit opt-in for a builtin vendor catalog.
///
/// Disabled by default: the snapshot stays out of the model catalog, no preset is
/// registered, and no environment variable is read for the vendor. Enabling with
/// neither `env_key` nor `api_key` loads the models credential-less (BYOK); a
/// warning is emitted so the missing credential is a conscious state.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct VendorConfig {
    pub enabled: bool,
    /// Explicit credential source for this vendor's models, read name-driven when
    /// a vendor model resolves credentials. Never a builtin default: without this
    /// (or `api_key`), the vendor's env var is never touched.
    pub env_key: Option<EnvKeys>,
    /// Static alternative to `env_key`; wins when both are set.
    pub api_key: Option<String>,
    /// Custom vendors only: default inference base URL for snapshot models that
    /// omit one. Also overrides the builtin default when set on a builtin id.
    pub base_url: Option<String>,
    /// Custom vendors only: pi-shaped catalog snapshot to map, resolved against
    /// the pig home when relative. Also overrides the builtin snapshot when set
    /// on a builtin id. Save any pi.dev `/api/models/providers/<id>?types=chat`
    /// response there to mirror a provider pig does not ship.
    pub snapshot_file: Option<String>,
    /// Custom vendors only: per-conversation routing header inherited by mapped
    /// models (e.g. a session header like the builtin `x-opencode-session`).
    /// Builtins always use [`OPENCODE_SESSION_HEADER`].
    pub session_header: Option<String>,
}

/// A `[vendors.<id>]` entry carries custom-vendor fields when the id is not a
/// builtin. Their presence declares intent, so a typo'd builtin id without them
/// still warns as unknown instead of silently becoming a custom vendor.
fn has_custom_fields(value: &toml::Value) -> bool {
    value.as_table().is_some_and(|t| {
        t.contains_key("base_url")
            || t.contains_key("snapshot_file")
            || t.contains_key("session_header")
    })
}

/// Parse `[vendors.<id>]` tables leniently: unknown vendor ids and malformed
/// entries warn and are skipped instead of failing the whole config.
pub(crate) fn parse_vendor_configs(
    raw_config: &toml::Value,
) -> (IndexMap<String, VendorConfig>, Vec<ConfigWarning>) {
    let mut vendors = IndexMap::new();
    let mut warnings = Vec::new();
    let Some(section) = raw_config.get("vendors") else {
        return (vendors, warnings);
    };
    let Some(table) = section.as_table() else {
        warnings.push(ConfigWarning::config_key(
            "vendors".to_owned(),
            ConfigWarningKind::NotATable,
            format!(
                "`vendors` must be a table of [vendors.<id>] entries, got {}; \
                 all vendors stay disabled",
                section.type_str()
            ),
        ));
        return (vendors, warnings);
    };
    for (id, value) in table {
        let is_builtin = VENDORS.iter().any(|v| v.id == id.as_str());
        if !is_builtin && !has_custom_fields(value) {
            warnings.push(ConfigWarning::config_key(
                format!("vendors.{id}"),
                ConfigWarningKind::UnknownField,
                format!(
                    "unknown vendor '{id}'; supported vendors are {}. \
                     To mirror another pi provider, add base_url + snapshot_file. \
                     Entry ignored.",
                    VENDORS.iter().map(|v| v.id).collect::<Vec<_>>().join(", ")
                ),
            ));
            continue;
        }
        let mut unknown = Vec::new();
        match serde_ignored::deserialize::<_, _, VendorConfig>(value.clone(), |path| {
            unknown.push(path.to_string());
        }) {
            Ok(entry) => {
                for key in unknown {
                    warnings.push(ConfigWarning::config_key(
                        format!("vendors.{id}.{key}"),
                        ConfigWarningKind::UnknownField,
                        "unrecognized key; field ignored".to_owned(),
                    ));
                }
                check_credential_warnings(id, &entry, &mut warnings);
                if !is_builtin && entry.enabled && !custom_source_complete(&entry) {
                    warnings.push(ConfigWarning::config_key(
                        format!("vendors.{id}"),
                        ConfigWarningKind::InvalidValue,
                        "custom vendor needs both base_url and snapshot_file; vendor stays disabled"
                            .to_owned(),
                    ));
                    continue;
                }
                vendors.insert(id.clone(), entry);
            }
            Err(error) => {
                warnings.push(ConfigWarning::config_key(
                    format!("vendors.{id}"),
                    ConfigWarningKind::InvalidValue,
                    format!("failed to parse ({error}); vendor stays disabled"),
                ));
            }
        }
    }
    (vendors, warnings)
}

/// Shared `env_key` / `api_key` warnings for builtin and custom vendors.
fn check_credential_warnings(id: &str, entry: &VendorConfig, warnings: &mut Vec<ConfigWarning>) {
    let has_static_key = entry
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|k| !k.is_empty());
    if has_static_key && entry.env_key.is_some() {
        warnings.push(ConfigWarning::config_key(
            format!("vendors.{id}"),
            ConfigWarningKind::ConflictingFields,
            "api_key shadows env_key; the static key always takes precedence".to_owned(),
        ));
    } else if entry.enabled
        && !has_static_key
        && entry.env_key.as_ref().and_then(EnvKeys::primary).is_none()
    {
        warnings.push(ConfigWarning::config_key(
            format!("vendors.{id}"),
            ConfigWarningKind::InvalidValue,
            "enabled with no env_key/api_key; vendor models resolve with no \
             credential (BYOK)"
                .to_owned(),
        ));
    }
}

/// Provider presets for explicitly enabled vendors, so `[model.x]
/// model_provider = "opencode-go"` works with only a `[vendors.opencode-go]`
/// block. Credentials come solely from the vendor config: no implicit env key.
pub fn vendor_providers(
    vendors: &IndexMap<String, VendorConfig>,
) -> IndexMap<String, ModelProviderConfig> {
    let mut providers = IndexMap::new();
    for vendor in VENDORS {
        let Some(cfg) = vendors.get(vendor.id).filter(|v| v.enabled) else {
            continue;
        };
        providers.insert(
            vendor.id.to_string(),
            ModelProviderConfig {
                base_url: Some(
                    cfg.base_url
                        .clone()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| vendor.default_base_url.to_string()),
                ),
                env_key: cfg.env_key.clone(),
                api_key: cfg.api_key.clone(),
                session_header: Some(OPENCODE_SESSION_HEADER.to_string()),
                ..Default::default()
            },
        );
    }
    for (id, cfg) in vendors {
        if VENDORS.iter().any(|v| v.id == id.as_str()) || !cfg.enabled {
            continue;
        }
        if !custom_source_complete(cfg) {
            tracing::debug!(vendor = id.as_str(), "custom vendor incomplete; no preset");
            continue;
        }
        providers.insert(
            id.clone(),
            ModelProviderConfig {
                base_url: cfg.base_url.clone(),
                env_key: cfg.env_key.clone(),
                api_key: cfg.api_key.clone(),
                session_header: cfg.session_header.clone().filter(|s| !s.trim().is_empty()),
                ..Default::default()
            },
        );
    }
    providers
}

/// Effective `[model_providers]` table: presets of enabled vendors fill gaps,
/// explicit user entries always win.
pub fn effective_model_providers(
    configured: &IndexMap<String, ModelProviderConfig>,
    vendors: &IndexMap<String, VendorConfig>,
) -> IndexMap<String, ModelProviderConfig> {
    let mut effective = vendor_providers(vendors);
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

/// pi thinking levels, ascending, each paired with pig's canonical effort.
/// pi `off` is pig `ReasoningEffort::None`; the other six spell identically.
const PI_THINKING_LEVELS: [(&str, ReasoningEffort); 7] = [
    ("off", ReasoningEffort::None),
    ("minimal", ReasoningEffort::Minimal),
    ("low", ReasoningEffort::Low),
    ("medium", ReasoningEffort::Medium),
    ("high", ReasoningEffort::High),
    ("xhigh", ReasoningEffort::Xhigh),
    ("max", ReasoningEffort::Max),
];

/// Derive the effort menu for one pi snapshot entry, purely data-driven.
/// Mirrors pi's `getSupportedThinkingLevels`
/// (`pi/packages/ai/src/models.ts`):
/// * `reasoning: false` (or absent) offers no level.
/// * `compat.supportsReasoningEffort: false` is an explicit opt-out.
/// * `thinkingLevelMap[level] === null` marks that level unsupported.
/// * `xhigh`/`max` need an explicit non-null entry; the other five default to
///   offered when the map says nothing about them.
/// Returns `None` when the model offers no level at all, so `/effort` keeps
/// refusing it exactly like a non-reasoning model.
fn vendor_effort_menu(model: &serde_json::Value) -> Option<Vec<ReasoningEffortOption>> {
    if !model
        .get("reasoning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return None;
    }
    if model
        .get("compat")
        .and_then(|c| c.get("supportsReasoningEffort"))
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        return None;
    }
    let map = model.get("thinkingLevelMap").and_then(|v| v.as_object());
    let mut menu = Vec::new();
    for (level, effort) in PI_THINKING_LEVELS {
        match map.and_then(|m| m.get(level)) {
            Some(serde_json::Value::Null) => continue,
            None if matches!(effort, ReasoningEffort::Xhigh | ReasoningEffort::Max) => {
                continue;
            }
            _ => menu.push(ReasoningEffortOption {
                id: effort.as_ref().to_string(),
                value: effort,
                label: effort_label(effort),
                description: None,
                default: false,
            }),
        }
    }
    if menu.is_empty() { None } else { Some(menu) }
}

/// Parse one vendor snapshot into catalog entries keyed `<vendor>/<model-id>`.
/// Credentials come from the explicit `[vendors.<id>]` config: `None`/`None`
/// loads the models credential-less (BYOK). Returns `(mapped, skipped_by_api)`
/// so callers can log coverage.
fn map_vendor_snapshot(
    vendor_id: &str,
    snapshot_json: &str,
    default_base_url: &str,
    session_header: Option<&str>,
    api_key: Option<String>,
    env_key: Option<EnvKeys>,
) -> (IndexMap<String, ModelEntry>, Vec<String>) {
    let mut mapped = IndexMap::new();
    let mut skipped = Vec::new();
    let parsed: serde_json::Value = match serde_json::from_str(snapshot_json) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(vendor = vendor_id, %error, "vendor snapshot is not valid JSON; skipping");
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
                    vendor = vendor_id,
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
            .unwrap_or(default_base_url);
        let mut info = ModelInfo::fallback(id);
        info.base_url = normalize_base_url(base_url, &api_backend);
        info.name = model
            .get("name")
            .and_then(|v| v.as_str())
            .map(|name| format!("{name} ({vendor_id}/{id})"));
        info.api_backend = api_backend;
        info.context_window = context_window_of(model);
        info.max_completion_tokens = model
            .get("maxTokens")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok());
        info.session_header = session_header.map(str::to_string);
        if let Some(menu) = vendor_effort_menu(model) {
            info.supports_reasoning_effort = true;
            info.reasoning_efforts = menu;
        }
        let entry = ModelEntry {
            info,
            mtls_cert_dir: None,
            api_key: api_key.clone(),
            env_key: env_key.clone(),
            auth_provider: None,
            api_base_url: None,
        };
        mapped.insert(format!("{vendor_id}/{id}"), entry);
    }
    (mapped, skipped)
}

/// Resolved snapshot source for one enabled vendor: catalog JSON, fallback
/// base URL, and optional per-conversation routing header.
struct ResolvedVendor {
    id: String,
    snapshot_json: String,
    default_base_url: String,
    session_header: Option<String>,
}

/// Resolve the snapshot source for an enabled vendor: builtin `include_str!`
/// snapshot by default, a runtime `snapshot_file` when set (relative paths
/// resolve against the pig home). `base_url` / `session_header` overrides win
/// over builtin defaults; custom vendors fall back to no routing header.
/// `None` means skip with a warning (fail closed, like a malformed snapshot).
fn resolve_vendor_source(
    id: &str,
    cfg: &VendorConfig,
    builtin: Option<&Vendor>,
) -> Option<ResolvedVendor> {
    let default_base_url = match cfg.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(url) => url.to_string(),
        None => builtin.map(|b| b.default_base_url.to_string())?,
    };
    let snapshot_json = match cfg
        .snapshot_file
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        Some(path) => match read_snapshot_file(path) {
            Some(json) => json,
            None => {
                tracing::warn!(
                    vendor = id,
                    "vendor snapshot_file unreadable; skipping vendor"
                );
                return None;
            }
        },
        None => builtin.map(|b| b.snapshot_json.to_string())?,
    };
    let session_header = match cfg
        .session_header
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        Some(header) => Some(header.to_string()),
        None => builtin.map(|_| OPENCODE_SESSION_HEADER.to_string()),
    };
    Some(ResolvedVendor {
        id: id.to_string(),
        snapshot_json,
        default_base_url,
        session_header,
    })
}

fn read_snapshot_file(path: &str) -> Option<String> {
    let raw = std::path::Path::new(path);
    let full = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        crate::util::grok_home::grok_home().join(raw)
    };
    std::fs::read_to_string(&full).ok()
}

fn load_vendor_into(
    all: &mut IndexMap<String, ModelEntry>,
    id: &str,
    cfg: &VendorConfig,
    builtin: Option<&Vendor>,
) {
    let Some(src) = resolve_vendor_source(id, cfg, builtin) else {
        return;
    };
    let (models, skipped) = map_vendor_snapshot(
        &src.id,
        &src.snapshot_json,
        &src.default_base_url,
        src.session_header.as_deref(),
        cfg.api_key.clone(),
        cfg.env_key.clone(),
    );
    tracing::debug!(
        vendor = id,
        mapped = models.len(),
        skipped = skipped.len(),
        "loaded vendor catalog snapshot"
    );
    if !skipped.is_empty() {
        tracing::debug!(
            vendor = id,
            skipped = ?skipped,
            "vendor models skipped: unsupported api backend"
        );
    }
    all.extend(models);
}

/// Enabled vendor models, builtin and custom. Keys are namespaced; user
/// `[model.*]` entries take precedence (see `resolve_model_list`). Disabled
/// vendors contribute nothing, so their env keys are never read.
///
/// Display names are derived, not verbatim: the suffix ` (<vendor>/<model-id>)` is
/// always appended, so every surface (`/model` picker, settings panel, status bar)
/// shows where a vendor model comes from and rows stay distinguishable even when pi
/// reuses names across vendors (`opencode/kimi-k2.6` vs `opencode-go/kimi-k2.6`).
/// The suffix is the catalog key users already type in config and on the command
/// line, so what you see is what you can submit. Nameless entries keep `None` and
/// fall back to their (already unique) id downstream.
pub fn vendor_models(vendors: &IndexMap<String, VendorConfig>) -> IndexMap<String, ModelEntry> {
    let mut all = IndexMap::new();
    for vendor in VENDORS {
        let Some(cfg) = vendors.get(vendor.id).filter(|v| v.enabled) else {
            tracing::debug!(vendor = vendor.id, "vendor not enabled; skipping snapshot");
            continue;
        };
        load_vendor_into(&mut all, vendor.id, cfg, Some(vendor));
    }
    for (id, cfg) in vendors {
        if VENDORS.iter().any(|v| v.id == id.as_str()) || !cfg.enabled {
            continue;
        }
        if !custom_source_complete(cfg) {
            tracing::debug!(
                vendor = id.as_str(),
                "custom vendor incomplete; skipping snapshot"
            );
            continue;
        }
        load_vendor_into(&mut all, id, cfg, None);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_vendors() -> IndexMap<String, VendorConfig> {
        VENDORS
            .iter()
            .map(|v| {
                (
                    v.id.to_string(),
                    VendorConfig {
                        enabled: true,
                        env_key: Some(EnvKeys::single(OPENCODE_ENV_KEY)),
                        api_key: None,
                        ..Default::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn vendor_snapshots_map_to_namespaced_models() {
        let models = vendor_models(&enabled_vendors());
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
    fn vendor_display_names_always_carry_catalog_key_suffix() {
        let models = vendor_models(&enabled_vendors());
        assert!(!models.is_empty());
        let mut named = 0;
        for (key, entry) in &models {
            let Some(name) = entry.info.name.as_deref() else {
                continue;
            };
            named += 1;
            assert!(
                name.ends_with(&format!(" ({key})")),
                "{key} must render as Name ({key}), got {name}"
            );
        }
        assert!(named > 0, "expected named vendor entries");
    }

    #[test]
    fn disabled_vendors_map_nothing() {
        assert!(vendor_models(&IndexMap::new()).is_empty());
        let mut vendors = enabled_vendors();
        vendors.get_mut(VENDOR_OPENCODE).expect("entry").enabled = false;
        let models = vendor_models(&vendors);
        assert!(!models.is_empty(), "enabled vendor still maps");
        assert!(
            models.keys().all(|k| k.starts_with("opencode-go/")),
            "disabled vendor must contribute no keys"
        );
        assert!(vendor_providers(&IndexMap::new()).is_empty());
        assert!(vendor_providers(&vendors).get(VENDOR_OPENCODE).is_none());
    }

    #[test]
    fn vendor_credential_comes_from_config_only() {
        let mut vendors = IndexMap::new();
        vendors.insert(
            VENDOR_OPENCODE_GO.to_string(),
            VendorConfig {
                enabled: true,
                env_key: Some(EnvKeys::single("MY_VENDOR_KEY")),
                api_key: None,
                ..Default::default()
            },
        );
        let models = vendor_models(&vendors);
        assert!(!models.is_empty());
        for entry in models.values() {
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary).as_deref(),
                Some("MY_VENDOR_KEY")
            );
            assert!(entry.api_key.is_none());
        }
        let providers = vendor_providers(&vendors);
        let preset = providers.get(VENDOR_OPENCODE_GO).expect("preset exists");
        assert_eq!(
            preset
                .env_key
                .as_ref()
                .and_then(EnvKeys::primary)
                .as_deref(),
            Some("MY_VENDOR_KEY")
        );
    }

    #[test]
    fn parse_vendor_configs_validates_entries() {
        let raw: toml::Value = toml::from_str(
            r#"
            [vendors.opencode-go]
            enabled = true
            env_key = "MY_KEY"

            [vendors.bogus]
            enabled = true

            [vendors.opencode]
            enabled = true
            "#,
        )
        .unwrap();
        let (vendors, warnings) = parse_vendor_configs(&raw);
        assert!(vendors.get(VENDOR_OPENCODE_GO).expect("entry").enabled);
        assert!(!vendors.contains_key("bogus"), "unknown vendor is skipped");
        assert!(
            warnings.iter().any(|w| w.reason.contains("unknown vendor")),
            "unknown vendor warns, got {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.reason.contains("no env_key/api_key")),
            "enabled-without-credential warns, got {warnings:?}"
        );
    }

    #[test]
    fn unsupported_apis_are_skipped_not_fatal() {
        for vendor in VENDORS {
            let (mapped, skipped) = map_vendor_snapshot(
                vendor.id,
                vendor.snapshot_json,
                vendor.default_base_url,
                Some(OPENCODE_SESSION_HEADER),
                None,
                None,
            );
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
        let (_, skipped) = map_vendor_snapshot(
            VENDORS[0].id,
            VENDORS[0].snapshot_json,
            VENDORS[0].default_base_url,
            Some(OPENCODE_SESSION_HEADER),
            None,
            None,
        );
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
        let effective = effective_model_providers(&configured, &enabled_vendors());
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
    fn disabled_vendor_registers_no_preset() {
        let effective = effective_model_providers(&IndexMap::new(), &IndexMap::new());
        assert!(effective.is_empty());
        // An explicit hand-written block is still honored: only the implicit
        // preset is gated, never user config.
        let mut configured = IndexMap::new();
        configured.insert(
            VENDOR_OPENCODE.to_string(),
            ModelProviderConfig {
                base_url: Some("https://custom.example/v1".to_string()),
                ..Default::default()
            },
        );
        let effective = effective_model_providers(&configured, &IndexMap::new());
        assert_eq!(
            effective
                .get(VENDOR_OPENCODE)
                .and_then(|p| p.base_url.as_deref()),
            Some("https://custom.example/v1")
        );
    }

    #[test]
    fn provider_preset_carries_session_header() {
        let providers = vendor_providers(&enabled_vendors());
        for id in [VENDOR_OPENCODE, VENDOR_OPENCODE_GO] {
            let preset = providers.get(id).expect("preset exists");
            assert_eq!(
                preset.session_header.as_deref(),
                Some(OPENCODE_SESSION_HEADER),
                "{id} preset must opt into the session header"
            );
        }
    }

    fn effort_ids(menu: &[ReasoningEffortOption]) -> Vec<&str> {
        menu.iter().map(|opt| opt.id.as_str()).collect()
    }

    #[test]
    fn effort_menu_mirrors_pi_thinking_levels() {
        // deepseek-v4-flash shape: explicit nulls drop minimal/medium, xhigh
        // needs an entry so it stays out, max is mapped in.
        let model = serde_json::json!({
            "reasoning": true,
            "thinkingLevelMap": {
                "minimal": null, "low": "low", "medium": null, "high": "high", "max": "max"
            },
        });
        let menu = vendor_effort_menu(&model).expect("menu");
        assert_eq!(effort_ids(&menu), ["none", "low", "high", "max"]);
    }

    #[test]
    fn effort_menu_defaults_without_thinking_level_map() {
        // No thinkingLevelMap: pi offers the base five, xhigh/max stay out.
        let model = serde_json::json!({"reasoning": true});
        let menu = vendor_effort_menu(&model).expect("menu");
        assert_eq!(
            effort_ids(&menu),
            ["none", "minimal", "low", "medium", "high"]
        );
    }

    #[test]
    fn effort_menu_respects_explicit_opt_outs() {
        // compat.supportsReasoningEffort: false (kimi-k2.6 shape).
        let model = serde_json::json!({
            "reasoning": true,
            "compat": {"supportsReasoningEffort": false},
        });
        assert!(vendor_effort_menu(&model).is_none());
        // reasoning: false offers nothing.
        let model = serde_json::json!({"reasoning": false});
        assert!(vendor_effort_menu(&model).is_none());
        // An absent reasoning flag means unknown, not supported.
        assert!(vendor_effort_menu(&serde_json::json!({})).is_none());
        // Every level nulled out leaves no menu.
        let model = serde_json::json!({
            "reasoning": true,
            "thinkingLevelMap": {
                "off": null, "minimal": null, "low": null, "medium": null,
                "high": null, "xhigh": null, "max": null
            },
        });
        assert!(vendor_effort_menu(&model).is_none());
    }

    #[test]
    fn vendor_snapshots_carry_effort_support() {
        let models = vendor_models(&enabled_vendors());
        assert!(!models.is_empty(), "expected mapped vendor models");
        // Raw snapshot entries keyed `<vendor>/<model-id>`. pi retires model
        // ids regularly (e.g. opencode-go dropped kimi-k2.6/glm-5.1 in
        // 2026-09), so this test follows the catalog instead of pinning ids.
        // The exact menu rules stay pinned in the synthetic
        // `effort_menu_*` tests; here we prove the wiring for whatever pi
        // serves today.
        let mut raw_by_key = std::collections::HashMap::new();
        for vendor in VENDORS {
            let parsed: serde_json::Value =
                serde_json::from_str(vendor.snapshot_json).expect("snapshot is JSON");
            let items = parsed
                .as_array()
                .map(|arr| arr.iter().collect::<Vec<_>>())
                .or_else(|| {
                    parsed
                        .get("models")
                        .and_then(|v| v.as_array())
                        .map(|arr| arr.iter().collect::<Vec<_>>())
                })
                .unwrap_or_default();
            for model in items {
                if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                    raw_by_key.insert(format!("{}/{}", vendor.id, id), model.clone());
                }
            }
        }
        let mut with_menu = 0;
        for (key, entry) in &models {
            let raw = raw_by_key
                .get(key)
                .unwrap_or_else(|| panic!("snapshot entry missing for {key}"));
            match vendor_effort_menu(raw) {
                Some(expected) => {
                    assert!(
                        entry.info.supports_reasoning_effort,
                        "{key} should support reasoning effort"
                    );
                    assert_eq!(
                        effort_ids(&entry.info.reasoning_efforts),
                        effort_ids(&expected),
                        "effort menu drift for {key}"
                    );
                    with_menu += 1;
                }
                None => {
                    assert!(
                        !entry.info.supports_reasoning_effort,
                        "{key} should not support reasoning effort"
                    );
                    assert!(
                        entry.info.reasoning_efforts.is_empty(),
                        "{key} menu should be empty"
                    );
                }
            }
        }
        assert!(with_menu > 0, "expected at least one reasoning model");
        // The ACP projection carries the flag + menu, which is what /effort
        // and /model <model> <level> actually read. Prefer the long-lived
        // deepseek-v4-flash example when pi still serves it, otherwise use
        // the first model with a menu.
        let probe_key = models
            .keys()
            .find(|k| k.as_str() == "opencode-go/deepseek-v4-flash")
            .or_else(|| {
                models
                    .iter()
                    .find_map(|(k, v)| v.info.supports_reasoning_effort.then_some(k))
            })
            .expect("reasoning model")
            .clone();
        let probe = models.get(&probe_key).expect("mapped");
        let expected_ids = effort_ids(&probe.info.reasoning_efforts);
        assert!(!expected_ids.is_empty());
        let acp_models = super::super::config::to_acp_model_info(&models);
        let probe_acp = acp_models
            .iter()
            .find(|(id, _)| id.0.as_ref() == probe_key)
            .map(|(_, info)| info)
            .expect("projected");
        let meta = probe_acp.meta.as_ref().expect("meta");
        assert_eq!(
            meta.get("supportsReasoningEffort"),
            Some(&serde_json::Value::Bool(true))
        );
        let values: Vec<&str> = meta
            .get("reasoningEfforts")
            .and_then(|v| v.as_array())
            .expect("menu")
            .iter()
            .filter_map(|o| o.get("value").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(values, expected_ids);
    }

    const CUSTOM_SNAPSHOT_FIXTURE: &str = r#"[
        {"id": "custom-chat", "name": "Custom Chat", "api": "openai-completions",
         "baseUrl": "https://custom.example/v1", "contextWindow": 64000, "maxTokens": 8000},
        {"id": "fallback-chat", "name": "Fallback Chat", "api": "anthropic-messages",
         "contextWindow": 32000},
        {"id": "custom-image", "name": "Custom Image", "api": "openai-completions",
         "type": "image", "contextWindow": 32000}
    ]"#;

    fn write_temp_snapshot(name: &str, contents: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "pig-vendor-test-{}-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            name
        ));
        std::fs::write(&path, contents).expect("write temp snapshot");
        path
    }

    fn custom_vendor_config(snapshot: &std::path::Path) -> VendorConfig {
        VendorConfig {
            enabled: true,
            env_key: Some(EnvKeys::single("MYCORP_API_KEY")),
            api_key: None,
            base_url: Some("https://models.mycorp.example/v1".to_string()),
            snapshot_file: Some(snapshot.to_string_lossy().into_owned()),
            session_header: Some("x-mycorp-session".to_string()),
        }
    }

    #[test]
    fn custom_vendor_maps_snapshot_file() {
        let snapshot = write_temp_snapshot("basic", CUSTOM_SNAPSHOT_FIXTURE);
        let mut vendors = IndexMap::new();
        vendors.insert("mycorp".to_string(), custom_vendor_config(&snapshot));
        let models = vendor_models(&vendors);
        // Image entries never map; chat entries namespace under the custom id.
        assert_eq!(models.len(), 2, "unexpected keys: {:?}", models.keys());
        let chat = models.get("mycorp/custom-chat").expect("mapped");
        assert_eq!(chat.info.base_url, "https://custom.example/v1");
        assert_eq!(
            chat.info.name.as_deref(),
            Some("Custom Chat (mycorp/custom-chat)")
        );
        assert_eq!(
            chat.env_key.as_ref().and_then(EnvKeys::primary).as_deref(),
            Some("MYCORP_API_KEY")
        );
        assert_eq!(
            chat.info.session_header.as_deref(),
            Some("x-mycorp-session")
        );
        // Entries without baseUrl fall back to the vendor base_url, with the
        // same /v1 normalization builtins get.
        let fallback = models.get("mycorp/fallback-chat").expect("mapped");
        assert_eq!(fallback.info.base_url, "https://models.mycorp.example/v1");
        assert_eq!(fallback.info.api_backend, ApiBackend::Messages);
        // The preset carries the same declared wiring for model_provider use.
        let providers = vendor_providers(&vendors);
        let preset = providers.get("mycorp").expect("preset exists");
        assert_eq!(
            preset.base_url.as_deref(),
            Some("https://models.mycorp.example/v1")
        );
        assert_eq!(preset.session_header.as_deref(), Some("x-mycorp-session"));
        assert!(is_enabled_vendor("mycorp", &vendors));
        std::fs::remove_file(&snapshot).ok();
    }

    #[test]
    fn custom_vendor_needs_base_url_and_snapshot_file() {
        let raw: toml::Value = toml::from_str(
            r#"
            [vendors.mycorp]
            enabled = true
            base_url = "https://models.mycorp.example/v1"
            snapshot_file = "/tmp/mycorp.json"
            env_key = "MYCORP_API_KEY"

            [vendors.half]
            enabled = true
            base_url = "https://half.example/v1"

            [vendors.typo]
            enabled = true
            "#,
        )
        .unwrap();
        let (vendors, warnings) = parse_vendor_configs(&raw);
        assert!(vendors.get("mycorp").expect("entry").enabled);
        assert!(
            !vendors.contains_key("half"),
            "enabled custom vendor without snapshot_file is skipped"
        );
        assert!(
            !vendors.contains_key("typo"),
            "unknown id without custom fields still warns as unknown"
        );
        assert!(
            warnings.iter().any(|w| w.reason.contains("needs both")),
            "incomplete custom vendor warns, got {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.reason.contains("unknown vendor")),
            "typo guard intact, got {warnings:?}"
        );
        assert!(is_enabled_vendor("mycorp", &vendors));
        assert!(!is_enabled_vendor("half", &vendors));
    }

    #[test]
    fn custom_vendor_with_unreadable_snapshot_maps_nothing() {
        let mut vendors = IndexMap::new();
        let mut cfg = custom_vendor_config(std::path::Path::new("unused"));
        cfg.snapshot_file = Some("/nonexistent/pig-vendor-test-missing.json".to_string());
        vendors.insert("mycorp".to_string(), cfg);
        // Fail closed at load time: no models, no panic. The declared preset
        // still registers so model_provider wiring stays predictable.
        assert!(vendor_models(&vendors).is_empty());
        assert!(vendor_providers(&vendors).contains_key("mycorp"));
    }

    #[test]
    fn builtin_snapshot_file_override_replaces_snapshot() {
        let snapshot = write_temp_snapshot("override", CUSTOM_SNAPSHOT_FIXTURE);
        let mut vendors = IndexMap::new();
        vendors.insert(
            VENDOR_OPENCODE.to_string(),
            VendorConfig {
                enabled: true,
                env_key: Some(EnvKeys::single(OPENCODE_ENV_KEY)),
                api_key: None,
                base_url: None,
                snapshot_file: Some(snapshot.to_string_lossy().into_owned()),
                session_header: None,
            },
        );
        let models = vendor_models(&vendors);
        // The override replaces the builtin snapshot; nothing merges.
        assert_eq!(models.len(), 2, "unexpected keys: {:?}", models.keys());
        assert!(models.contains_key("opencode/custom-chat"));
        // Builtin defaults still apply where the override says nothing.
        let chat = models.get("opencode/custom-chat").expect("mapped");
        assert_eq!(
            chat.info.session_header.as_deref(),
            Some(OPENCODE_SESSION_HEADER)
        );
        std::fs::remove_file(&snapshot).ok();
    }
}
