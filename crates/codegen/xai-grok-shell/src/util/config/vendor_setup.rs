//! First-run vendor setup: explicit opt-in persistence for builtin vendors.
//!
//! The TUI setup wizard collects a vendor choice + credential, then persists
//! `[vendors.<id>] enabled = true` with an explicit `env_key` (or `api_key`)
//! to the trusted user `config.toml`. Only the trusted file can enable
//! vendors (`PATCH_STRIP_KEYS` strips them from `--config`/campaign patches),
//! so this module writes via the raw-table rewrite path: `save_config_locked`
//! does not merge `vendors` and must never drop it.
//!
//! Explicitness rules (see `vendors/mod.rs`):
//! * No builtin env default is ever read: detection only reports presence +
//!   length for the vendor's suggested key, after the user selected it.
//!   Values are never logged or echoed.
//! * Nothing is enabled until this write lands: disabled vendors map nothing
//!   and read nothing.

use anyhow::{Context, Result};
use std::path::PathBuf;

/// One builtin vendor the wizard can offer. Sourced from `VENDORS` so new
/// snapshots appear automatically; the suggested key is UI-only and is only
/// written after user confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinVendorOption {
    pub id: &'static str,
    pub display_name: &'static str,
    pub suggested_env_key: &'static str,
}

/// Builtin vendors in wizard order.
pub fn builtin_vendor_options() -> Vec<BuiltinVendorOption> {
    crate::agent::vendors::VENDORS
        .iter()
        .map(|v| BuiltinVendorOption {
            id: v.id,
            display_name: v.display_name,
            suggested_env_key: crate::agent::vendors::OPENCODE_ENV_KEY,
        })
        .collect()
}

/// Presence of a credential env var, without its value.
/// `len` is the trimmed value length, for `已检测到 (32 字符)` style UI.
/// Never carries the secret itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvPresence {
    pub present: bool,
    pub len: Option<usize>,
}

/// Testable presence check with injected getenv.
pub fn detect_env_presence_with(
    name: &str,
    mut getenv: impl FnMut(&str) -> Option<String>,
) -> EnvPresence {
    let trimmed = getenv(name).map(|v| v.trim().to_string());
    match trimmed {
        Some(v) if !v.is_empty() => EnvPresence {
            present: true,
            len: Some(v.len()),
        },
        _ => EnvPresence {
            present: false,
            len: None,
        },
    }
}

/// Live presence check: reads one explicitly named var, reports only
/// existence + length. Call only after the user selected the vendor.
pub fn detect_env_presence(name: &str) -> EnvPresence {
    detect_env_presence_with(name, |key| {
        std::env::var_os(key).and_then(|v| v.into_string().ok())
    })
}

/// Credential the wizard persists: exactly one of the two must be set.
/// `env_key` names the var (recommended, nothing secret on disk);
/// `api_key` stores the pasted secret directly (convenient, plaintext).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorSetupRequest {
    pub vendor_id: String,
    pub env_key: Option<String>,
    pub api_key: Option<String>,
}

impl VendorSetupRequest {
    fn normalized(&self) -> Result<(String, Option<String>, Option<String>)> {
        let vendor_id = self.vendor_id.trim().to_string();
        let is_builtin = crate::agent::vendors::VENDORS
            .iter()
            .any(|v| v.id == vendor_id.as_str());
        if !is_builtin {
            anyhow::bail!(
                "unknown vendor '{vendor_id}'; supported: {}",
                crate::agent::vendors::VENDORS
                    .iter()
                    .map(|v| v.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
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
        Ok((vendor_id, env_key, api_key))
    }
}

/// Pure table edit, unit-testable without touching disk.
/// Merges into an existing `[vendors.<id>]` table (keeps `base_url`,
/// `snapshot_file`, `session_header` overrides); sets `enabled = true` and
/// exactly one credential.
pub(crate) fn write_vendor_table(
    root: &mut toml::map::Map<String, toml::Value>,
    vendor_id: &str,
    env_key: Option<&str>,
    api_key: Option<&str>,
) -> Result<()> {
    let vendors = root
        .entry("vendors".to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let vendors_table = vendors
        .as_table_mut()
        .with_context(|| "vendors must be a table")?;
    let entry = vendors_table
        .entry(vendor_id.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let table = entry
        .as_table_mut()
        .with_context(|| format!("vendors.{vendor_id} must be a table"))?;
    table.insert("enabled".to_owned(), toml::Value::Boolean(true));
    match (env_key, api_key) {
        (Some(name), _) => {
            table.insert("env_key".to_owned(), toml::Value::String(name.to_string()));
            table.remove("api_key");
        }
        (None, Some(secret)) => {
            table.insert(
                "api_key".to_owned(),
                toml::Value::String(secret.to_string()),
            );
            table.remove("env_key");
        }
        (None, None) => anyhow::bail!("no credential"),
    }
    Ok(())
}

/// Persist a builtin vendor opt-in to the trusted user `config.toml`.
/// Returns the config path written. Values are never logged.
pub async fn enable_builtin_vendor(request: VendorSetupRequest) -> Result<PathBuf> {
    let (vendor_id, env_key, api_key) = request.normalized()?;
    let guard = crate::util::config::persist::lock_config_writes()
        .await
        .map_err(|e| anyhow::anyhow!("lock config.toml for vendor setup: {e}"))?;
    let path = crate::util::config::mcp::user_config_path();
    let result = guard
        .run_blocking(move || {
            let (dest, content) = crate::util::config::persist::read_follow_bound(&path)
                .map_err(|e| anyhow::anyhow!("read {} for vendor setup: {e}", path.display()))?;
            let mut root = crate::util::config::persist::parse_existing_config_toml(&content)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "parse {} for vendor setup: {}",
                        path.display(),
                        xai_grok_config::toml_error_detail(&content, &e)
                    )
                })?;
            let table = root
                .as_table_mut()
                .with_context(|| format!("{} must contain a TOML table", path.display()))?;
            write_vendor_table(table, &vendor_id, env_key.as_deref(), api_key.as_deref())?;
            let serialized = toml::to_string_pretty(&root).map_err(|e| {
                anyhow::anyhow!("serialize {} for vendor setup: {e}", path.display())
            })?;
            crate::util::config::persist::atomic_write_follow_bound(&path, &dest, &serialized)
                .map_err(|e| anyhow::anyhow!("write {} for vendor setup: {e}", path.display()))?;
            Ok::<_, anyhow::Error>(path)
        })
        .await
        .map_err(|e| anyhow::anyhow!("vendor setup task failed: {e}"))??;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_options_follow_vendors_table() {
        let options = builtin_vendor_options();
        assert_eq!(options.len(), crate::agent::vendors::VENDORS.len());
        for opt in &options {
            assert!(!opt.id.is_empty());
            assert!(!opt.display_name.is_empty());
            assert_eq!(
                opt.suggested_env_key,
                crate::agent::vendors::OPENCODE_ENV_KEY
            );
        }
    }

    #[test]
    fn detect_presence_reports_length_only() {
        let present = detect_env_presence_with("K", |_| Some("  sk-abc  ".to_string()));
        assert_eq!(
            present,
            EnvPresence {
                present: true,
                len: Some("sk-abc".len())
            }
        );
        assert!(!detect_env_presence_with("K", |_| None).present);
        assert!(!detect_env_presence_with("K", |_| Some("   ".to_string())).present);
    }

    #[test]
    fn write_merges_without_clobbering_overrides() {
        let mut root = toml::map::Map::new();
        let mut existing = toml::map::Map::new();
        existing.insert(
            "base_url".to_owned(),
            toml::Value::String("https://custom.example/v1".to_owned()),
        );
        existing.insert("api_key".to_owned(), toml::Value::String("old".to_owned()));
        let mut vendors = toml::map::Map::new();
        vendors.insert(
            crate::agent::vendors::VENDOR_OPENCODE.to_owned(),
            toml::Value::Table(existing),
        );
        root.insert("vendors".to_owned(), toml::Value::Table(vendors));
        write_vendor_table(
            &mut root,
            crate::agent::vendors::VENDOR_OPENCODE,
            Some("OPENCODE_API_KEY"),
            None,
        )
        .unwrap();
        let table = root["vendors"][crate::agent::vendors::VENDOR_OPENCODE]
            .as_table()
            .unwrap();
        assert_eq!(table["enabled"], toml::Value::Boolean(true));
        assert_eq!(
            table["env_key"],
            toml::Value::String("OPENCODE_API_KEY".to_owned())
        );
        assert!(
            !table.contains_key("api_key"),
            "credential switches cleanly"
        );
        assert_eq!(
            table["base_url"],
            toml::Value::String("https://custom.example/v1".to_owned()),
            "overrides survive"
        );
    }

    #[test]
    fn request_validation_rejects_unknown_and_empty() {
        let err = VendorSetupRequest {
            vendor_id: "bogus".to_string(),
            env_key: Some("K".to_string()),
            api_key: None,
        }
        .normalized()
        .unwrap_err();
        assert!(err.to_string().contains("unknown vendor"), "{err}");
        let err = VendorSetupRequest {
            vendor_id: crate::agent::vendors::VENDOR_OPENCODE.to_string(),
            env_key: None,
            api_key: None,
        }
        .normalized()
        .unwrap_err();
        assert!(err.to_string().contains("no credential"), "{err}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enable_writes_trusted_config_only() {
        let home = tempfile::tempdir().unwrap();
        let _env = xai_grok_test_support::EnvGuard::set("GROK_HOME", home.path());
        // Isolate ambient credential env so this test never borrows the dev machine key.
        let _key = xai_grok_test_support::EnvGuard::unset(crate::agent::vendors::OPENCODE_ENV_KEY);
        let path = enable_builtin_vendor(VendorSetupRequest {
            vendor_id: crate::agent::vendors::VENDOR_OPENCODE_GO.to_string(),
            env_key: Some(crate::agent::vendors::OPENCODE_ENV_KEY.to_string()),
            api_key: None,
        })
        .await
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("OPENCODE_API_KEY"),
            "credential name lands on disk, got:\n{content}"
        );
        let root: toml::Value = toml::from_str(&content).unwrap();
        let table = root["vendors"][crate::agent::vendors::VENDOR_OPENCODE_GO]
            .as_table()
            .unwrap();
        assert_eq!(table["enabled"], toml::Value::Boolean(true));
        // Second write with a pasted key switches credential shape cleanly.
        enable_builtin_vendor(VendorSetupRequest {
            vendor_id: crate::agent::vendors::VENDOR_OPENCODE_GO.to_string(),
            env_key: None,
            api_key: Some("sk-test".to_string()),
        })
        .await
        .unwrap();
        let root: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let table = root["vendors"][crate::agent::vendors::VENDOR_OPENCODE_GO]
            .as_table()
            .unwrap();
        assert_eq!(table["api_key"], toml::Value::String("sk-test".to_owned()));
        assert!(!table.contains_key("env_key"));
    }
}
