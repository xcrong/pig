//! Custom OpenAI-compatible provider setup: explicit opt-in persistence.
//!
//! The setup wizard collects a provider id, base URL, model key, backend,
//! and credential, then persists a shared `[model_providers.<id>]` block plus
//! a minimal `[model.<key>]` entry (`model` + `model_provider`) to the
//! trusted user `config.toml`. Like vendors, these tables are not merged by
//! `save_config_locked`, so this module writes via the raw-table rewrite path
//! under the config write guard: only the trusted file can create them.
//!
//! Explicitness rules:
//! * Exactly one credential (`env_key` name or pasted `api_key`) is required;
//!   values are never logged or echoed.
//! * Existing keys on either table (e.g. hand-added `extra_headers`) are
//!   preserved; only the wizard-owned keys are upserted.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Supported API backends, serialized snake_case like the sampler.
pub const CUSTOM_BACKENDS: &[(&str, &str)] = &[
    ("chat_completions", "OpenAI Chat Completions (/v1/chat/completions)"),
    ("responses", "OpenAI Responses (/v1/responses)"),
    ("messages", "Anthropic Messages (/v1/messages)"),
];

/// Wizard-collected custom provider. `wire_model` empty means the wire id
/// equals the catalog key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomProviderRequest {
    pub provider_id: String,
    pub base_url: String,
    pub model_key: String,
    pub wire_model: String,
    pub api_backend: String,
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
        let base_url = self.base_url.trim().to_string();
        let api_backend = self.api_backend.trim().to_string();
        if !valid_table_key(&provider_id) {
            anyhow::bail!("invalid provider id '{provider_id}': use [A-Za-z0-9_-], max 64");
        }
        if !valid_table_key(&model_key) {
            anyhow::bail!("invalid model key '{model_key}': use [A-Za-z0-9_-], max 64");
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            anyhow::bail!("invalid base_url '{base_url}': must start with http(s)://");
        }
        if !CUSTOM_BACKENDS.iter().any(|(id, _)| *id == api_backend) {
            anyhow::bail!("invalid api_backend '{api_backend}'");
        }
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
            anyhow::bail!("invalid env_key '{name}'");
        }
        Ok(NormalizedCustomProvider {
            provider_id,
            base_url,
            model_key,
            wire_model,
            api_backend,
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
    api_backend: String,
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
            let (dest, content) =
                crate::util::config::persist::read_follow_bound(&path).map_err(|e| {
                    anyhow::anyhow!("read {} for provider setup: {e}", path.display())
                })?;
            let mut root =
                crate::util::config::persist::parse_existing_config_toml(&content).map_err(
                    |e| {
                        anyhow::anyhow!(
                            "parse {} for provider setup: {}",
                            path.display(),
                            xai_grok_config::toml_error_detail(&content, &e)
                        )
                    },
                )?;
            let table = root.as_table_mut().with_context(|| {
                format!("{} must contain a TOML table", path.display())
            })?;
            write_custom_provider_table(table, &req)?;
            let serialized = toml::to_string_pretty(&root).map_err(|e| {
                anyhow::anyhow!("serialize {} for provider setup: {e}", path.display())
            })?;
            crate::util::config::persist::atomic_write_follow_bound(&path, &dest, &serialized)
                .map_err(|e| {
                    anyhow::anyhow!("write {} for provider setup: {e}", path.display())
                })?;
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
            api_backend: "chat_completions".to_string(),
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
        assert_eq!(
            model["model"],
            toml::Value::String("acme-model".to_owned())
        );
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
        provider.insert(
            "api_key".to_owned(),
            toml::Value::String("old".to_owned()),
        );
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
