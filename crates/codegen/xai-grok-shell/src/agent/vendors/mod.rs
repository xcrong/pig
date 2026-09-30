//! Third-party vendor model catalog, mirroring pi's provider directory.
//!
//! pi generates its catalog at build time (`pi/packages/ai/scripts/generate-models.ts`
//! from `models.dev`, OpenRouter, NVIDIA NIM, Vercel AI Gateway, Radius) and serves
//! runtime overlays from `https://pi.dev/api/models/providers/<id>`. pig reuses
//! the *served* catalog (no Node, no pi checkout) through three layers:
//!
//! 1. Build-time refresh: `build.rs` pulls the latest pi.dev slices into
//!    `OUT_DIR` (same URLs and validation rules as
//!    `scripts/sync-pi-vendors.sh`) and the binary embeds them. Offline or
//!    sandboxed builds fall back to the checked-in `data/*.json`, so a missing
//!    network never fails the build. The pulled version is logged via
//!    `cargo:warning=`; `manifest.json` stays the manual-sync channel.
//! 2. Runtime auto-cache: enabled builtin vendors refresh
//!    `<grok_home>/vendors/<id>.json` in the background (see
//!    [`spawn_vendor_snapshot_refresh`]). Load priority is explicit
//!    `snapshot_file` > auto-cache file > embedded snapshot. The cache only
//!    ever applies to builtin ids and only when the vendor is explicitly
//!    enabled; it never enables anything and never touches `config.toml`.
//! 3. Manual sync: `scripts/sync-pi-vendors.sh` refreshes `data/*.json` +
//!    `manifest.json` for review, commit, and release.
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
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// pi.dev runtime catalog this crate mirrors. Shared by build-time refresh
/// (`build.rs`), the manual sync script, and the runtime auto-cache so all
/// three pull the same slices.
pub const VENDOR_CATALOG_URL_BASE: &str = "https://pi.dev/api/models/providers";
/// UA for all three pull channels, so pi.dev sees one client shape.
pub const VENDOR_SYNC_USER_AGENT: &str = "pig-vendor-sync/1.0";
/// Subdirectory of `grok_home()` holding auto-refreshed snapshots
/// (`<grok_home>/vendors/<id>.json`), mirroring how hand-written
/// `snapshot_file` relative paths resolve.
pub const VENDOR_CACHE_DIR_NAME: &str = "vendors";
/// How long an auto-cached snapshot stays fresh before the background task
/// pulls again. Missing cache always pulls.
pub const VENDOR_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Delay before the first background check so startup / first paint wins the
/// network. The check itself never blocks startup: it runs on a spawned task.
const VENDOR_REFRESH_INITIAL_DELAY: Duration = Duration::from_secs(30);
/// Interval between background checks after the first one.
const VENDOR_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Upper bound of the per-loop schedule jitter.
const VENDOR_REFRESH_JITTER: Duration = Duration::from_secs(30 * 60);
/// Network timeout for one runtime snapshot fetch.
const VENDOR_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Catalog URL for one vendor id (`?types=chat`, like the sync script).
pub fn vendor_catalog_url(id: &str) -> String {
    format!("{VENDOR_CATALOG_URL_BASE}/{id}?types=chat")
}

/// Directory holding auto-refreshed snapshots.
pub fn vendor_cache_dir() -> PathBuf {
    vendor_cache_dir_in(&crate::util::grok_home::grok_home())
}

fn vendor_cache_dir_in(home: &Path) -> PathBuf {
    home.join(VENDOR_CACHE_DIR_NAME)
}

/// Auto-cache file for one builtin vendor id.
pub fn vendor_cache_path(id: &str) -> PathBuf {
    vendor_cache_path_in(&crate::util::grok_home::grok_home(), id)
}

fn vendor_cache_path_in(home: &Path, id: &str) -> PathBuf {
    vendor_cache_dir_in(home).join(format!("{id}.json"))
}

/// API shapes pig's sampler speaks. Mirrors the match arms of
/// [`api_backend_for`]; `build.rs` duplicates the list (standalone crate).
const SUPPORTED_VENDOR_APIS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
];
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

/// Embedded snapshots, refreshed from pi.dev at build time by `build.rs`
/// (offline builds fall back to the checked-in `data/*.json`; see the build
/// script). `data/` stays the reviewable ground truth for manual sync.
const OPENCODE_SNAPSHOT: &str = include_str!(concat!(
    env!("OUT_DIR"),
    "/pig-vendor-snapshots/opencode.json"
));
const OPENCODE_GO_SNAPSHOT: &str = include_str!(concat!(
    env!("OUT_DIR"),
    "/pig-vendor-snapshots/opencode-go.json"
));

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
/// snapshots route correctly. Also reused by the custom-provider wizard so
/// `https://x` and `https://x/v1` behave identically under `messages`.
pub(crate) fn normalize_base_url(base_url: &str, api_backend: &ApiBackend) -> String {
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

/// Coverage of one validated pi.dev snapshot: total entries, chat entries on
/// backends pig speaks, per-api skip counts for chat entries (mirrors the
/// sync script's `mappedModels` / `skippedApis`), and per-type skip counts
/// for side types pi serves under `?types=chat` (e.g. `classifier`).
pub(crate) struct VendorSnapshotStats {
    pub total: usize,
    pub mapped: usize,
    pub skipped: Vec<(String, usize)>,
    pub skipped_types: Vec<(String, usize)>,
}

/// Strict validation shared by build-time refresh, the runtime auto-cache,
/// and the sync script's rules: the payload must parse and be a non-empty
/// catalog; every `chat` entry must carry `id`/`api`/`baseUrl`/`contextWindow`;
/// at least one entry must sit on a backend pig speaks. Side types pi serves
/// under `?types=chat` (e.g. `classifier`) are skipped and counted -- the
/// same leniency the mapping layer's `is_chat_model` has always applied.
/// Anything else is rejected so a corrupt or shape-shifted payload can never
/// poison the catalog.
pub(crate) fn validate_vendor_snapshot(
    snapshot_json: &str,
    vendor_id: &str,
) -> Result<VendorSnapshotStats, String> {
    let parsed: serde_json::Value = serde_json::from_str(snapshot_json)
        .map_err(|error| format!("{vendor_id}: snapshot is not valid JSON ({error})"))?;
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
        return Err(format!("{vendor_id}: empty catalog"));
    }
    // Side types are skipped and counted; only chat entries are validated.
    let mut chat_count = 0;
    let mut skipped_type_counts = std::collections::BTreeMap::new();
    for model in &items {
        let typ = model.get("type").and_then(|v| v.as_str()).unwrap_or("chat");
        if typ != "chat" {
            *skipped_type_counts.entry(typ.to_string()).or_insert(0) += 1;
            continue;
        }
        chat_count += 1;
        for field in ["id", "api", "baseUrl", "contextWindow"] {
            if model.get(field).is_none() {
                let mid = model.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                return Err(format!("{vendor_id}/{mid}: missing {field}"));
            }
        }
    }
    if chat_count == 0 {
        return Err(format!("{vendor_id}: no chat entries"));
    }
    let is_chat = |m: &&serde_json::Value| is_chat_model(m);
    let mapped = items
        .iter()
        .filter(|m| {
            is_chat(m)
                && m.get("api")
                    .and_then(|v| v.as_str())
                    .is_some_and(|api| SUPPORTED_VENDOR_APIS.contains(&api))
        })
        .count();
    if mapped == 0 {
        return Err(format!("{vendor_id}: no mappable entries"));
    }
    let mut skipped_counts = std::collections::BTreeMap::new();
    for model in items.iter().filter(|m| is_chat(m)) {
        let api = model.get("api").and_then(|v| v.as_str()).unwrap_or("?");
        if !SUPPORTED_VENDOR_APIS.contains(&api) {
            *skipped_counts.entry(api.to_string()).or_insert(0) += 1;
        }
    }
    Ok(VendorSnapshotStats {
        total: items.len(),
        mapped,
        skipped: skipped_counts.into_iter().collect(),
        skipped_types: skipped_type_counts.into_iter().collect(),
    })
}

/// Whether one vendor entry is eligible for the runtime auto-cache: a builtin
/// id, explicitly enabled, without a hand-written `snapshot_file` (explicit
/// files always win and opt out of the cache). Custom ids never qualify.
/// Disabled vendors are never pulled, stored, or read -- the opt-in default
/// stays closed.
pub fn vendor_auto_refresh_eligible(id: &str, cfg: &VendorConfig) -> bool {
    cfg.enabled
        && VENDORS.iter().any(|v| v.id == id)
        && cfg
            .snapshot_file
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
}

/// Builtin vendor ids due for a background pull, in config order.
pub(crate) fn refresh_eligible_vendor_ids(vendors: &IndexMap<String, VendorConfig>) -> Vec<String> {
    vendors
        .iter()
        .filter(|(id, cfg)| vendor_auto_refresh_eligible(id, cfg))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether the auto-cache at `path` needs a pull: missing/unreadable metadata
/// always pulls; otherwise only when older than [`VENDOR_CACHE_TTL`]. A
/// future mtime counts as fresh (clock skew must not spin the fetcher).
pub(crate) fn vendor_cache_needs_refresh(path: &Path) -> bool {
    let mtime = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok();
    match mtime {
        None => true,
        Some(stamp) => SystemTime::now()
            .duration_since(stamp)
            .is_ok_and(|age| age >= VENDOR_CACHE_TTL),
    }
}

/// Read one builtin vendor's auto-cache. `None` means "use the embedded
/// snapshot": the file is missing (first run, nothing pulled yet) or it
/// fails strict validation, in which case a warning fires and the embedded
/// snapshot wins -- a corrupt cache never empties the catalog.
fn read_auto_cache(home: &Path, id: &str) -> Option<String> {
    let path = vendor_cache_path_in(home, id);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(vendor = id, "no vendor auto-cache; using embedded snapshot");
            return None;
        }
        Err(error) => {
            tracing::debug!(
                vendor = id,
                %error,
                "vendor auto-cache unreadable; using embedded snapshot"
            );
            return None;
        }
    };
    match validate_vendor_snapshot(&text, id) {
        Ok(_) => Some(text),
        Err(reason) => {
            tracing::warn!(
                vendor = id,
                path = %path.display(),
                reason = reason.as_str(),
                "vendor auto-cache invalid; using embedded snapshot"
            );
            None
        }
    }
}

/// Atomically replace the auto-cache file (write tmp + rename) with the
/// verbatim pi.dev payload. Only data lands here: never `config.toml`, never
/// the `enabled` flag.
fn write_vendor_cache_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

async fn fetch_vendor_snapshot(id: &str) -> Result<String, String> {
    let client = xai_grok_extra_ca::build_reqwest_client(|builder| {
        builder
            .timeout(VENDOR_FETCH_TIMEOUT)
            .user_agent(VENDOR_SYNC_USER_AGENT)
    })
    .map_err(|error| format!("{id}: client build failed ({error})"))?;
    let url = vendor_catalog_url(id);
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("{id}: fetch failed ({error})"))?;
    if !response.status().is_success() {
        return Err(format!("{id}: HTTP {}", response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("{id}: read body failed ({error})"))
}

/// Pull every eligible vendor whose auto-cache is missing or expired.
/// Fail-closed throughout: offline / 404 / validation failures only warn and
/// keep the previous cache (or the embedded snapshot). Returns the ids whose
/// cache files were rewritten.
pub async fn refresh_vendor_snapshots(vendors: &IndexMap<String, VendorConfig>) -> Vec<String> {
    refresh_vendor_snapshots_in(vendors, &crate::util::grok_home::grok_home()).await
}

pub(crate) async fn refresh_vendor_snapshots_in(
    vendors: &IndexMap<String, VendorConfig>,
    home: &Path,
) -> Vec<String> {
    let mut refreshed = Vec::new();
    for id in refresh_eligible_vendor_ids(vendors) {
        let path = vendor_cache_path_in(home, &id);
        if !vendor_cache_needs_refresh(&path) {
            tracing::debug!(
                vendor = id.as_str(),
                "vendor auto-cache fresh; skipping pull"
            );
            continue;
        }
        let text = match fetch_vendor_snapshot(&id).await {
            Ok(text) => text,
            Err(reason) => {
                tracing::warn!(
                    vendor = id.as_str(),
                    reason = reason.as_str(),
                    "vendor snapshot pull failed (offline?); keeping previous cache or embedded snapshot"
                );
                continue;
            }
        };
        let stats = match validate_vendor_snapshot(&text, &id) {
            Ok(stats) => stats,
            Err(reason) => {
                tracing::warn!(
                    vendor = id.as_str(),
                    reason = reason.as_str(),
                    "pulled vendor snapshot failed validation; keeping previous cache or embedded snapshot"
                );
                continue;
            }
        };
        match write_vendor_cache_atomic(&path, &text) {
            Ok(()) => {
                tracing::info!(
                    vendor = id.as_str(),
                    total = stats.total,
                    mapped = stats.mapped,
                    skipped = ?stats.skipped,
                    skipped_types = ?stats.skipped_types,
                    path = %path.display(),
                    "vendor auto-cache refreshed from pi.dev"
                );
                refreshed.push(id);
            }
            Err(error) => {
                tracing::warn!(
                    vendor = id.as_str(),
                    %error,
                    "vendor auto-cache write failed; keeping previous cache or embedded snapshot"
                );
            }
        }
    }
    refreshed
}

/// Kick off the background vendor snapshot refresh: one check shortly after
/// startup, then every [`VENDOR_REFRESH_INTERVAL`] (plus jitter). The task
/// never blocks startup and never throws: without a tokio runtime (or under
/// `cfg(test)`) it logs and returns, and pulls only run for explicitly
/// enabled builtin vendors without a hand-written `snapshot_file`. Call once
/// per process (e.g. from agent bootstrap); extra calls just spawn extra
/// loops that converge on the same cache files.
pub fn spawn_vendor_snapshot_refresh(vendors: &IndexMap<String, VendorConfig>) {
    let watched: IndexMap<String, VendorConfig> = vendors
        .iter()
        .filter(|(id, cfg)| vendor_auto_refresh_eligible(id, cfg))
        .map(|(id, cfg)| (id.clone(), cfg.clone()))
        .collect();
    if watched.is_empty() {
        tracing::debug!("no enabled builtin vendors; vendor auto-refresh idle");
        return;
    }
    if cfg!(test) {
        tracing::debug!("cfg(test): skipping vendor auto-refresh background task");
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no tokio runtime; skipping vendor auto-refresh background task");
        return;
    };
    let home = crate::util::grok_home::grok_home();
    // Schedule jitter spreads process starts so a fleet release does not
    // thundering-herd pi.dev; derived from the pid so no extra dep is needed.
    let jitter = Duration::from_secs(
        u64::from(std::process::id() % 1000) * VENDOR_REFRESH_JITTER.as_secs() / 1000,
    );
    handle.spawn(async move {
        tokio::time::sleep(VENDOR_REFRESH_INITIAL_DELAY).await;
        loop {
            refresh_vendor_snapshots_in(&watched, &home).await;
            tokio::time::sleep(VENDOR_REFRESH_INTERVAL + jitter).await;
        }
    });
}

/// Resolve the snapshot source for an enabled vendor: a hand-written runtime
/// `snapshot_file` when set (relative paths resolve against the pig home),
/// else the background auto-cache file (`<grok_home>/vendors/<id>.json`) for
/// builtin ids, else the embedded snapshot. `base_url` / `session_header`
/// overrides win over builtin defaults; custom vendors fall back to no
/// routing header. `None` means skip with a warning (fail closed, like a
/// malformed snapshot).
///
/// `home` is the pig home injected by the caller (production: `grok_home()`)
/// so tests can point the auto-cache at a temp dir without touching the
/// process-global home.
fn resolve_vendor_source(
    id: &str,
    cfg: &VendorConfig,
    builtin: Option<&Vendor>,
    home: &Path,
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
        None => match builtin {
            // Explicit files opt out of the auto-cache; only builtin ids read
            // it, and only as a fallback below the hand-written file.
            Some(vendor) => {
                read_auto_cache(home, id).unwrap_or_else(|| vendor.snapshot_json.to_string())
            }
            // Custom vendors without a readable explicit file stay skipped.
            None => return None,
        },
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
    home: &Path,
) {
    let Some(src) = resolve_vendor_source(id, cfg, builtin, home) else {
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
    vendor_models_in(vendors, &crate::util::grok_home::grok_home())
}

/// Same as [`vendor_models`], with the pig home injected for the auto-cache
/// (see [`resolve_vendor_source`]).
pub(crate) fn vendor_models_in(
    vendors: &IndexMap<String, VendorConfig>,
    home: &Path,
) -> IndexMap<String, ModelEntry> {
    let mut all = IndexMap::new();
    for vendor in VENDORS {
        let Some(cfg) = vendors.get(vendor.id).filter(|v| v.enabled) else {
            tracing::debug!(vendor = vendor.id, "vendor not enabled; skipping snapshot");
            continue;
        };
        load_vendor_into(&mut all, vendor.id, cfg, Some(vendor), home);
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
        load_vendor_into(&mut all, id, cfg, None, home);
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

    /// Distinctive one-model snapshot used to prove the auto-cache wins over
    /// the embedded catalog (and vice versa for explicit files).
    const AUTO_CACHE_FIXTURE: &str = r#"[
        {"id": "cached-only-model", "name": "Cached Only", "api": "openai-completions",
         "baseUrl": "https://cache.example/v1", "contextWindow": 128000}
    ]"#;

    /// Strict-validation success case: chat-only catalog with one unsupported
    /// api (counts as skipped, not as failure).
    const VALIDATION_FIXTURE: &str = r#"[
        {"id": "chat-a", "api": "openai-completions",
         "baseUrl": "https://a.example/v1", "contextWindow": 1000},
        {"id": "gemini-x", "api": "google-generative-ai",
         "baseUrl": "https://b.example/v1", "contextWindow": 2000}
    ]"#;

    fn temp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pig-vendor-home-{}-{}-{}.d",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            name
        ));
        std::fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    fn write_auto_cache(home: &std::path::Path, id: &str, contents: &str) {
        let dir = home.join(VENDOR_CACHE_DIR_NAME);
        std::fs::create_dir_all(&dir).expect("create cache dir");
        std::fs::write(dir.join(format!("{id}.json")), contents).expect("write cache");
    }

    fn enabled_builtin(id: &str) -> IndexMap<String, VendorConfig> {
        let mut vendors = IndexMap::new();
        vendors.insert(
            id.to_string(),
            VendorConfig {
                enabled: true,
                env_key: Some(EnvKeys::single(OPENCODE_ENV_KEY)),
                ..Default::default()
            },
        );
        vendors
    }

    #[test]
    fn auto_cache_beats_embedded_snapshot() {
        let home = temp_home("priority");
        write_auto_cache(&home, VENDOR_OPENCODE, AUTO_CACHE_FIXTURE);
        let models = vendor_models_in(&enabled_builtin(VENDOR_OPENCODE), &home);
        // The cache replaces the embedded snapshot; nothing merges.
        assert_eq!(models.len(), 1, "unexpected keys: {:?}", models.keys());
        let cached = models
            .get("opencode/cached-only-model")
            .expect("cached model");
        assert_eq!(cached.info.base_url, "https://cache.example/v1");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn illegal_auto_cache_falls_back_to_embedded() {
        for (name, contents) in [
            ("garbage", "{not json"),
            // Valid JSON but failing strict validation: empty catalog...
            ("empty", "[]"),
            // ...or missing required fields.
            ("missing-fields", r#"[{"id": "x"}]"#),
        ] {
            let home = temp_home(name);
            write_auto_cache(&home, VENDOR_OPENCODE, contents);
            let models = vendor_models_in(&enabled_builtin(VENDOR_OPENCODE), &home);
            assert!(!models.is_empty(), "{name}: must fall back to embedded");
            assert!(
                models.keys().all(|k| k.starts_with("opencode/")),
                "{name}: unexpected keys: {:?}",
                models.keys()
            );
            assert!(
                !models.contains_key("opencode/cached-only-model"),
                "{name}: corrupt cache must not leak entries"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    #[test]
    fn explicit_snapshot_file_beats_auto_cache() {
        let home = temp_home("explicit-wins");
        write_auto_cache(&home, VENDOR_OPENCODE, AUTO_CACHE_FIXTURE);
        let snapshot = write_temp_snapshot("explicit", CUSTOM_SNAPSHOT_FIXTURE);
        let mut vendors = enabled_builtin(VENDOR_OPENCODE);
        vendors
            .get_mut(VENDOR_OPENCODE)
            .expect("entry")
            .snapshot_file = Some(snapshot.to_string_lossy().into_owned());
        let models = vendor_models_in(&vendors, &home);
        assert_eq!(models.len(), 2, "unexpected keys: {:?}", models.keys());
        assert!(models.contains_key("opencode/custom-chat"));
        assert!(!models.contains_key("opencode/cached-only-model"));
        std::fs::remove_file(&snapshot).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn disabled_vendors_are_never_refresh_eligible() {
        // Nothing configured: nothing to pull.
        assert!(refresh_eligible_vendor_ids(&IndexMap::new()).is_empty());
        // Disabled builtin: never pulled, stored, or read -- even with a
        // cache file sitting on disk.
        let home = temp_home("disabled");
        write_auto_cache(&home, VENDOR_OPENCODE, AUTO_CACHE_FIXTURE);
        let mut vendors = IndexMap::new();
        vendors.insert(
            VENDOR_OPENCODE.to_string(),
            VendorConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(refresh_eligible_vendor_ids(&vendors).is_empty());
        assert!(vendor_models_in(&vendors, &home).is_empty());
        // Enabled but hand-overridden: the explicit file opts out of the cache.
        let cfg = vendors.get_mut(VENDOR_OPENCODE).expect("entry");
        cfg.enabled = true;
        cfg.snapshot_file = Some("/tmp/pig-vendor-test-explicit.json".to_string());
        assert!(refresh_eligible_vendor_ids(&vendors).is_empty());
        // Enabled builtin without override: the only pullable shape.
        vendors
            .get_mut(VENDOR_OPENCODE)
            .expect("entry")
            .snapshot_file = None;
        assert_eq!(
            refresh_eligible_vendor_ids(&vendors),
            vec![VENDOR_OPENCODE.to_string()]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn vendor_cache_staleness_gate() {
        let home = temp_home("staleness");
        let path = home.join(VENDOR_CACHE_DIR_NAME).join("opencode.json");
        // Missing cache always pulls.
        assert!(vendor_cache_needs_refresh(&path));
        write_auto_cache(&home, VENDOR_OPENCODE, AUTO_CACHE_FIXTURE);
        // Just-written cache is fresh.
        assert!(!vendor_cache_needs_refresh(&path));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn snapshot_validation_mirrors_sync_script() {
        // Chat-only catalog: unsupported apis pass validation (they only
        // affect the mapped/skip counts, like the sync script).
        let stats = validate_vendor_snapshot(VALIDATION_FIXTURE, "test").expect("valid");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.mapped, 1);
        assert_eq!(stats.skipped, vec![("google-generative-ai".to_string(), 1)]);
        assert!(stats.skipped_types.is_empty());
        // Non-chat entries are skipped and counted, not fatal -- pi serves
        // side types (e.g. `classifier`) under `?types=chat`.
        let stats = validate_vendor_snapshot(
            r#"[{"id": "c", "api": "openai-completions", "baseUrl": "https://c.example/v1", "contextWindow": 5},
                {"id": "cl", "type": "classifier", "api": "typesafe-system-one", "baseUrl": "https://c.example/v1", "contextWindow": 5}]"#,
            "test",
        )
        .expect("valid");
        assert_eq!(stats.total, 2);
        assert_eq!(stats.mapped, 1);
        assert_eq!(stats.skipped_types, vec![("classifier".to_string(), 1)]);
        for invalid in [
            "{not json",
            "[]",
            // Chat entry missing required fields (`fallback-chat` has no
            // baseUrl: explicit snapshot files still map it leniently via
            // the vendor default, but the auto-cache requires full fields).
            r#"[{"id": "x"}]"#,
            CUSTOM_SNAPSHOT_FIXTURE,
            // No chat entries at all.
            r#"[{"id": "x", "api": "openai-completions", "baseUrl": "https://x/v1", "contextWindow": 1, "type": "image"}]"#,
            // Chat entries, but nothing on a backend pig speaks.
            r#"[{"id": "x", "api": "google-generative-ai", "baseUrl": "https://x/v1", "contextWindow": 1}]"#,
        ] {
            assert!(
                validate_vendor_snapshot(invalid, "test").is_err(),
                "must reject {invalid}"
            );
        }
    }
}
