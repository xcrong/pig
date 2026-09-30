//! Custom provider setup: explicit opt-in persistence.
//!
//! The setup wizard collects a provider id, base URL, model key, wire id,
//! display name, backend, optional messages-direct auth (Anthropic `x-api-key`
//! + `anthropic-version`), and credential, then persists a shared
//! `[model_providers.<id>]` block plus a `[model.<key>]` entry (`model` +
//! `model_provider`, plus optional `name` / `auth_scheme` / `extra_headers`)
//! to the trusted user `config.toml`. Like vendors, these tables are not
//! merged by `save_config_locked`, so this module writes via the raw-table
//! rewrite path under the config write guard: only the trusted file can
//! create them.
//!
//! Explicitness rules:
//! * Exactly one credential (`env_key` name or pasted `api_key`) is required;
//!   values are never logged or echoed.
//! * Existing keys on either table (e.g. hand-added `extra_headers`) are
//!   preserved; only the wizard-owned keys are upserted.

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::agent::vendors::normalize_base_url;
use crate::sampling::ApiBackend;

/// Supported API backends, serialized snake_case like the sampler.
pub const CUSTOM_BACKENDS: &[(&str, &str)] = &[
    (
        "chat_completions",
        "OpenAI Chat Completions (/v1/chat/completions)",
    ),
    ("responses", "OpenAI Responses (/v1/responses)"),
    ("messages", "Anthropic Messages (/v1/messages)"),
];

/// Default `anthropic-version` header for the messages-direct branch.
/// Matches the user guide's Anthropic example.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Wizard-collected custom provider. `wire_model` empty means the wire id
/// equals the catalog key. `display_name` empty means no `name` is written.
/// `auth_scheme` is `Some("x_api_key")` only for the messages-direct branch
/// (Anthropic直连); `None` keeps the default Bearer scheme (gateways).
/// `anthropic_version` empty means no `anthropic-version` header is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomProviderRequest {
    pub provider_id: String,
    pub base_url: String,
    pub model_key: String,
    pub wire_model: String,
    pub display_name: String,
    pub api_backend: String,
    pub auth_scheme: Option<String>,
    pub anthropic_version: String,
    pub env_key: Option<String>,
    pub api_key: Option<String>,
}

fn valid_table_key(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl CustomProviderRequest {
    pub(crate) fn normalized(&self) -> Result<NormalizedCustomProvider> {
        let provider_id = self.provider_id.trim().to_string();
        let model_key = self.model_key.trim().to_string();
        let wire_model = self.wire_model.trim().to_string();
        let display_name = self.display_name.trim().to_string();
        let base_url = self.base_url.trim().to_string();
        let api_backend = self.api_backend.trim().to_string();
        let auth_scheme = self
            .auth_scheme
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let anthropic_version = self.anthropic_version.trim().to_string();
        if !valid_table_key(&provider_id) {
            anyhow::bail!(
                "invalid provider id '{provider_id}': use [A-Za-z0-9_-], max 64 （仅限字母数字、-、_，最长 64）"
            );
        }
        if !valid_table_key(&model_key) {
            anyhow::bail!(
                "invalid model key '{model_key}': use [A-Za-z0-9_-], max 64 （仅限字母数字、-、_，最长 64）"
            );
        }
        if !wire_model.is_empty()
            && (wire_model.contains(char::is_whitespace) || wire_model.len() > 128)
        {
            anyhow::bail!(
                "invalid wire_model '{wire_model}': must not contain whitespace, max 128 （不能含空白，最长 128）"
            );
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            anyhow::bail!(
                "invalid base_url '{base_url}': must start with http(s):// （须以 http(s):// 开头）"
            );
        }
        if !CUSTOM_BACKENDS.iter().any(|(id, _)| *id == api_backend) {
            anyhow::bail!("invalid api_backend '{api_backend}'");
        }
        if let Some(scheme) = auth_scheme.as_deref() {
            if scheme != "x_api_key" {
                anyhow::bail!(
                    "invalid auth_scheme '{scheme}': only \"x_api_key\" is supported （仅支持 x_api_key）"
                );
            }
            if api_backend != "messages" {
                anyhow::bail!(
                    "auth_scheme 'x_api_key' requires api_backend 'messages' （x_api_key 仅用于 messages）"
                );
            }
        }
        if !anthropic_version.is_empty() {
            if auth_scheme.as_deref() != Some("x_api_key") {
                anyhow::bail!(
                    "anthropic_version requires messages-direct (auth_scheme x_api_key) （版本号仅用于 Anthropic 直连）"
                );
            }
            if anthropic_version.contains(char::is_whitespace) || anthropic_version.len() > 64 {
                anyhow::bail!(
                    "invalid anthropic_version '{anthropic_version}': must not contain whitespace, max 64"
                );
            }
        }
        // Normalize like vendor snapshots so `https://x` and `https://x/v1`
        // behave identically under `messages` (see `normalize_base_url`).
        let backend_enum = match api_backend.as_str() {
            "chat_completions" => ApiBackend::ChatCompletions,
            "responses" => ApiBackend::Responses,
            _ => ApiBackend::Messages,
        };
        let base_url = normalize_base_url(&base_url, &backend_enum);
        let anthropic_version =
            if auth_scheme.as_deref() == Some("x_api_key") && anthropic_version.is_empty() {
                DEFAULT_ANTHROPIC_VERSION.to_string()
            } else {
                anthropic_version
            };
        let wire_model = if wire_model.is_empty() {
            model_key.clone()
        } else {
            wire_model
        };
        let env_key = self
            .env_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let api_key = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if env_key.is_none() && api_key.is_none() {
            anyhow::bail!("no credential: set env_key or paste an api_key");
        }
        if let Some(name) = env_key.as_deref()
            && (name.contains(char::is_whitespace) || name.contains('=') || name.contains('"'))
        {
            anyhow::bail!("invalid env_key '{name}' （变量名不能含空白、= 或 \"）");
        }
        Ok(NormalizedCustomProvider {
            provider_id,
            base_url,
            model_key,
            wire_model,
            display_name,
            api_backend,
            auth_scheme,
            anthropic_version,
            env_key,
            api_key,
        })
    }
}

pub(crate) struct NormalizedCustomProvider {
    provider_id: String,
    base_url: String,
    model_key: String,
    wire_model: String,
    display_name: String,
    api_backend: String,
    auth_scheme: Option<String>,
    anthropic_version: String,
    env_key: Option<String>,
    api_key: Option<String>,
}

/// Pure table edit, unit-testable without touching disk.
/// Merges into existing tables, preserving hand-added keys.
pub(crate) fn write_custom_provider_table(
    root: &mut toml::map::Map<String, toml::Value>,
    req: &NormalizedCustomProvider,
) -> Result<()> {
    let providers = root
        .entry("model_providers".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let providers_table = providers
        .as_table_mut()
        .with_context(|| "model_providers must be a table")?;
    let provider_entry = providers_table
        .entry(req.provider_id.clone())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let provider_table = provider_entry
        .as_table_mut()
        .with_context(|| format!("model_providers.{} must be a table", req.provider_id))?;
    provider_table.insert(
        "base_url".to_owned(),
        toml::Value::String(req.base_url.clone()),
    );
    provider_table.insert(
        "api_backend".to_owned(),
        toml::Value::String(req.api_backend.clone()),
    );
    match (&req.env_key, &req.api_key) {
        (Some(name), _) => {
            provider_table.insert("env_key".to_owned(), toml::Value::String(name.clone()));
            provider_table.remove("api_key");
        }
        (None, Some(secret)) => {
            provider_table.insert("api_key".to_owned(), toml::Value::String(secret.clone()));
            provider_table.remove("env_key");
        }
        (None, None) => anyhow::bail!("no credential"),
    }

    let models = root
        .entry("model".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let models_table = models
        .as_table_mut()
        .with_context(|| "model must be a table")?;
    let model_entry = models_table
        .entry(req.model_key.clone())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let model_table = model_entry
        .as_table_mut()
        .with_context(|| format!("model.{} must be a table", req.model_key))?;
    model_table.insert(
        "model".to_owned(),
        toml::Value::String(req.wire_model.clone()),
    );
    model_table.insert(
        "model_provider".to_owned(),
        toml::Value::String(req.provider_id.clone()),
    );
    // Wizard-owned model-layer keys are declarative: a re-run with an empty
    // value clears them instead of leaving stale state behind. Hand-added
    // keys under `extra_headers` other than `anthropic-version` are preserved.
    if req.display_name.is_empty() {
        model_table.remove("name");
    } else {
        model_table.insert(
            "name".to_owned(),
            toml::Value::String(req.display_name.clone()),
        );
    }
    if let Some(scheme) = req.auth_scheme.as_deref() {
        model_table.insert(
            "auth_scheme".to_owned(),
            toml::Value::String(scheme.to_owned()),
        );
    } else {
        model_table.remove("auth_scheme");
    }
    if req.anthropic_version.is_empty() {
        if let Some(toml::Value::Table(extra)) = model_table.get_mut("extra_headers") {
            extra.remove("anthropic-version");
        }
    } else {
        let extra = model_table
            .entry("extra_headers".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let extra_table = extra
            .as_table_mut()
            .with_context(|| format!("model.{} extra_headers must be a table", req.model_key))?;
        extra_table.insert(
            "anthropic-version".to_owned(),
            toml::Value::String(req.anthropic_version.clone()),
        );
    }
    Ok(())
}

/// Persist a custom provider + model to the trusted user `config.toml`.
/// Returns the config path written. Values are never logged.
pub async fn enable_custom_provider(request: CustomProviderRequest) -> Result<PathBuf> {
    let req = request.normalized()?;
    let guard = crate::util::config::persist::lock_config_writes()
        .await
        .map_err(|e| anyhow::anyhow!("lock config.toml for provider setup: {e}"))?;
    let path = crate::util::config::mcp::user_config_path();
    let result = guard
        .run_blocking(move || {
            let (dest, content) = crate::util::config::persist::read_follow_bound(&path)
                .map_err(|e| anyhow::anyhow!("read {} for provider setup: {e}", path.display()))?;
            let mut root = crate::util::config::persist::parse_existing_config_toml(&content)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "parse {} for provider setup: {}",
                        path.display(),
                        xai_grok_config::toml_error_detail(&content, &e)
                    )
                })?;
            let table = root
                .as_table_mut()
                .with_context(|| format!("{} must contain a TOML table", path.display()))?;
            write_custom_provider_table(table, &req)?;
            let serialized = toml::to_string_pretty(&root).map_err(|e| {
                anyhow::anyhow!("serialize {} for provider setup: {e}", path.display())
            })?;
            crate::util::config::persist::atomic_write_follow_bound(&path, &dest, &serialized)
                .map_err(|e| anyhow::anyhow!("write {} for provider setup: {e}", path.display()))?;
            Ok::<_, anyhow::Error>(path)
        })
        .await
        .map_err(|e| anyhow::anyhow!("provider setup task failed: {e}"))??;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CustomProviderRequest {
        CustomProviderRequest {
            provider_id: "acme".to_string(),
            base_url: "https://api.acme.example/v1".to_string(),
            model_key: "acme-model".to_string(),
            wire_model: String::new(),
            display_name: String::new(),
            api_backend: "chat_completions".to_string(),
            auth_scheme: None,
            anthropic_version: String::new(),
            env_key: Some("ACME_API_KEY".to_string()),
            api_key: None,
        }
    }

    #[test]
    fn write_creates_provider_and_model() {
        let req = request().normalized().unwrap();
        assert_eq!(req.wire_model, "acme-model", "empty wire defaults to key");
        let mut root = toml::map::Map::new();
        write_custom_provider_table(&mut root, &req).unwrap();
        let provider = root["model_providers"]["acme"].as_table().unwrap();
        assert_eq!(
            provider["base_url"],
            toml::Value::String("https://api.acme.example/v1".to_owned())
        );
        assert_eq!(
            provider["api_backend"],
            toml::Value::String("chat_completions".to_owned())
        );
        assert_eq!(
            provider["env_key"],
            toml::Value::String("ACME_API_KEY".to_owned())
        );
        let model = root["model"]["acme-model"].as_table().unwrap();
        assert_eq!(model["model"], toml::Value::String("acme-model".to_owned()));
        assert_eq!(
            model["model_provider"],
            toml::Value::String("acme".to_owned())
        );
    }

    #[test]
    fn write_preserves_hand_added_keys() {
        let mut root = toml::map::Map::new();
        let mut provider = toml::map::Map::new();
        provider.insert(
            "extra_headers".to_owned(),
            toml::Value::Table(toml::map::Map::new()),
        );
        provider.insert("api_key".to_owned(), toml::Value::String("old".to_owned()));
        let mut providers = toml::map::Map::new();
        providers.insert("acme".to_owned(), toml::Value::Table(provider));
        root.insert("model_providers".to_owned(), toml::Value::Table(providers));
        let req = request().normalized().unwrap();
        write_custom_provider_table(&mut root, &req).unwrap();
        let provider = root["model_providers"]["acme"].as_table().unwrap();
        assert!(
            provider.contains_key("extra_headers"),
            "hand-added keys survive"
        );
        assert!(
            !provider.contains_key("api_key"),
            "credential switches cleanly"
        );
    }

    #[test]
    fn messages_base_url_gains_v1_suffix() {
        let mut direct = request();
        direct.base_url = "https://api.anthropic.com".to_string();
        direct.api_backend = "messages".to_string();
        let req = direct.normalized().unwrap();
        assert_eq!(req.base_url, "https://api.anthropic.com/v1");
        // Trailing slashes collapse; an existing suffix is kept as-is.
        let mut slashed = request();
        slashed.base_url = "https://api.anthropic.com/".to_string();
        slashed.api_backend = "messages".to_string();
        assert_eq!(
            slashed.normalized().unwrap().base_url,
            "https://api.anthropic.com/v1"
        );
        let mut suffixed = request();
        suffixed.base_url = "https://api.anthropic.com/v1".to_string();
        suffixed.api_backend = "messages".to_string();
        assert_eq!(
            suffixed.normalized().unwrap().base_url,
            "https://api.anthropic.com/v1"
        );
        // Non-messages backends only lose trailing slashes (shared rule).
        let mut chat = request();
        chat.base_url = "https://api.acme.example/v1/".to_string();
        assert_eq!(
            chat.normalized().unwrap().base_url,
            "https://api.acme.example/v1",
            "chat backends keep the URL verbatim apart from slash trim"
        );
    }

    #[test]
    fn messages_direct_writes_model_layer_auth_and_version() {
        let mut direct = request();
        direct.provider_id = "anthropic".to_string();
        direct.base_url = "https://api.anthropic.com".to_string();
        direct.model_key = "claude".to_string();
        direct.api_backend = "messages".to_string();
        direct.auth_scheme = Some("x_api_key".to_string());
        direct.anthropic_version = "2023-06-01".to_string();
        direct.display_name = "Claude".to_string();
        let req = direct.normalized().unwrap();
        assert_eq!(req.base_url, "https://api.anthropic.com/v1");
        let mut root = toml::map::Map::new();
        write_custom_provider_table(&mut root, &req).unwrap();
        // Provider layer stays Bearer-oriented: no auth_scheme there.
        let provider = root["model_providers"]["anthropic"].as_table().unwrap();
        assert!(!provider.contains_key("auth_scheme"));
        assert_eq!(
            provider["env_key"],
            toml::Value::String("ACME_API_KEY".to_owned())
        );
        // Model layer carries the direct-auth contract.
        let model = root["model"]["claude"].as_table().unwrap();
        assert_eq!(
            model["auth_scheme"],
            toml::Value::String("x_api_key".to_owned())
        );
        assert_eq!(model["name"], toml::Value::String("Claude".to_owned()));
        assert_eq!(
            model["extra_headers"]["anthropic-version"],
            toml::Value::String("2023-06-01".to_owned())
        );
    }

    #[test]
    fn messages_direct_defaults_version_and_rerun_clears() {
        let mut direct = request();
        direct.api_backend = "messages".to_string();
        direct.auth_scheme = Some("x_api_key".to_string());
        let req = direct.normalized().unwrap();
        assert_eq!(req.anthropic_version, DEFAULT_ANTHROPIC_VERSION);
        let mut root = toml::map::Map::new();
        write_custom_provider_table(&mut root, &req).unwrap();
        assert!(
            root["model"]["acme-model"]["extra_headers"]
                .as_table()
                .is_some()
        );
        // A later gateway re-run clears the direct-only keys but keeps
        // hand-added sibling headers.
        let model_table = root["model"]["acme-model"].as_table_mut().unwrap();
        let extra = model_table
            .entry("extra_headers".to_owned())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        extra.as_table_mut().unwrap().insert(
            "x-team".to_owned(),
            toml::Value::String("codegen".to_owned()),
        );
        let gateway = request().normalized().unwrap();
        write_custom_provider_table(&mut root, &gateway).unwrap();
        let model = root["model"]["acme-model"].as_table().unwrap();
        assert!(!model.contains_key("auth_scheme"));
        let extra = model["extra_headers"].as_table().unwrap();
        assert!(!extra.contains_key("anthropic-version"));
        assert_eq!(extra["x-team"], toml::Value::String("codegen".to_owned()));
    }

    #[test]
    fn validation_rejects_wire_and_direct_misuse() {
        let mut bad = request();
        bad.wire_model = "has space".to_string();
        assert!(bad.normalized().is_err());
        let mut bad = request();
        bad.wire_model = "x".repeat(129);
        assert!(bad.normalized().is_err());
        let mut bad = request();
        bad.auth_scheme = Some("x_api_key".to_string());
        assert!(
            bad.normalized().is_err(),
            "x_api_key requires the messages backend"
        );
        let mut bad = request();
        bad.api_backend = "messages".to_string();
        bad.anthropic_version = "2023-06-01".to_string();
        assert!(
            bad.normalized().is_err(),
            "version without direct auth is rejected"
        );
        let mut bad = request();
        bad.auth_scheme = Some("bearer".to_string());
        assert!(bad.normalized().is_err());
    }

    #[test]
    fn validation_rejects_bad_input() {
        let mut bad = request();
        bad.provider_id = "has space".to_string();
        assert!(bad.normalized().is_err());
        let mut bad = request();
        bad.base_url = "ftp://example.com".to_string();
        assert!(bad.normalized().is_err());
        let mut bad = request();
        bad.api_backend = "carrier-pigeon".to_string();
        assert!(bad.normalized().is_err());
        let mut bad = request();
        bad.env_key = None;
        bad.api_key = None;
        assert!(bad.normalized().is_err());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enable_writes_trusted_config_only() {
        let home = tempfile::tempdir().unwrap();
        let _env = xai_grok_test_support::EnvGuard::set("GROK_HOME", home.path());
        let _key = xai_grok_test_support::EnvGuard::unset("ACME_API_KEY");
        let path = enable_custom_provider(request()).await.unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("ACME_API_KEY"), "got:\n{content}");
        let root: toml::Value = toml::from_str(&content).unwrap();
        assert_eq!(
            root["model"]["acme-model"]["model_provider"],
            toml::Value::String("acme".to_owned())
        );
    }
}
