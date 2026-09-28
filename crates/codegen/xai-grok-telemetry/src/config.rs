//! These types live here so the data-collector engine can construct a [`TelemetryClient`](crate::client::TelemetryClient) without depending on shell.
//!
//! Shell still re-exports these types from their original paths so existing call sites (and `Config` derive impls) compile unchanged.
use serde::{Deserialize, Serialize};
/// Telemetry mode: `true`/`false` (legacy bool) or `"session_metrics"` (string). `Disabled`: nothing sent;
///
/// Pig Agent ships no first-party sinks: the mode only gates legacy internal
/// emission paths (now no-ops). User-owned external OTEL (`resolve_external_otel_config`)
/// is independent of this mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryMode {
    #[default]
    Disabled,
    SessionMetrics,
    Enabled,
}
impl TelemetryMode {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
    /// True for both `SessionMetrics` and `Enabled`.
    pub fn session_metrics_enabled(&self) -> bool {
        matches!(self, Self::SessionMetrics | Self::Enabled)
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" | "full" => Some(Self::Enabled),
            "0" | "false" | "no" | "off" | "disabled" => Some(Self::Disabled),
            "session-metrics" | "session_metrics" => Some(Self::SessionMetrics),
            _ => None,
        }
    }
}
#[cfg(test)]
mod telemetry_mode_tests {
    use super::TelemetryMode;
    /// A parent process hands its resolved mode to spawned children via `GROK_TELEMETRY_ENABLED={mode}` (Display).
    /// Every Display output must parse back to the same mode.
    #[test]
    fn display_round_trips_through_parse() {
        for mode in [
            TelemetryMode::Enabled,
            TelemetryMode::Disabled,
            TelemetryMode::SessionMetrics,
        ] {
            assert_eq!(
                TelemetryMode::parse(&mode.to_string()),
                Some(mode),
                "Display value for {mode:?} must parse back to itself"
            );
        }
    }
}
impl std::fmt::Display for TelemetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "false"),
            Self::SessionMetrics => write!(f, "session_metrics"),
            Self::Enabled => write!(f, "true"),
        }
    }
}
impl From<bool> for TelemetryMode {
    fn from(b: bool) -> Self {
        if b { Self::Enabled } else { Self::Disabled }
    }
}
impl serde::Serialize for TelemetryMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Enabled => serializer.serialize_bool(true),
            Self::SessionMetrics => serializer.serialize_str("session_metrics"),
        }
    }
}
/// Wire format for `[features] telemetry`: accepts `true`, `false`, or `"session_metrics"`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum TelemetryModeValue {
    Bool(bool),
    Str(String),
}
impl<'de> serde::Deserialize<'de> for TelemetryMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match TelemetryModeValue::deserialize(deserializer)? {
            TelemetryModeValue::Bool(b) => Ok(Self::from(b)),
            TelemetryModeValue::Str(s) => Ok(Self::parse(&s).unwrap_or_else(|| {
                tracing::warn!(
                    value = %s,
                    "TELEMETRY_MODE_UNKNOWN: unrecognized telemetry mode; treating as disabled",
                );
                Self::Disabled
            })),
        }
    }
}
/// Parse an env var as a `TelemetryMode`. Returns `None` if unset or empty.
pub fn env_telemetry_mode(name: &str) -> Option<TelemetryMode> {
    let value = std::env::var(name).ok()?;
    TelemetryMode::parse(&value)
}
/// Parse `[telemetry] otel_timeout` / `otel_metric_export_interval`: docs say
/// `number`, so TOML integers must not fail-close config load. Strings still
/// work (`"10000"`). Stored as decimal strings to match the env-var overlay.
fn deserialize_opt_ms_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IntOrString {
        Int(i64),
        Str(String),
    }
    Ok(match Option::<IntOrString>::deserialize(deserializer)? {
        None => None,
        Some(IntOrString::Int(i)) => Some(i.to_string()),
        Some(IntOrString::Str(s)) => Some(s),
    })
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Declared for `serde_ignored`. Actual toggle is `[features] telemetry`.
    #[serde(default)]
    pub enabled: Option<bool>,
    pub otel_enabled: Option<bool>,
    /// External OTEL metrics exporter: `otlp` | `console` | `none`.
    pub otel_metrics_exporter: Option<String>,
    /// External OTEL logs/events exporter: `otlp` | `console` | `none`.
    pub otel_logs_exporter: Option<String>,
    /// External OTLP base endpoint (`/v1/logs`, `/v1/metrics` appended for HTTP).
    pub otel_endpoint: Option<String>,
    /// External OTLP transport: `http/protobuf` | `grpc`.
    #[serde(alias = "otel_transport")]
    pub otel_protocol: Option<String>,
    pub otel_certificate: Option<String>,
    pub otel_client_certificate: Option<String>,
    pub otel_client_key: Option<String>,
    /// External OTEL content gate (admins can pin to `false` via requirements).
    pub otel_log_user_prompts: Option<bool>,
    /// External OTEL content gate (admins can pin to `false` via requirements).
    pub otel_log_tool_details: Option<bool>,
    /// External OTEL content gate. Unset follows `otel_log_user_prompts`.
    pub otel_log_assistant_responses: Option<bool>,
    /// External OTEL content gate for full tool/MCP bodies (default off).
    pub otel_log_tool_content: Option<bool>,
    /// Milliseconds as a decimal string. TOML accepts integer or string.
    #[serde(default, deserialize_with = "deserialize_opt_ms_string")]
    pub otel_timeout: Option<String>,
    /// Milliseconds as a decimal string. TOML accepts integer or string.
    #[serde(default, deserialize_with = "deserialize_opt_ms_string")]
    pub otel_metric_export_interval: Option<String>,
    pub otel_logs_endpoint: Option<String>,
    pub otel_metrics_endpoint: Option<String>,
    pub otel_logs_protocol: Option<String>,
    pub otel_metrics_protocol: Option<String>,
    pub otel_logs_certificate: Option<String>,
    pub otel_metrics_certificate: Option<String>,
    pub otel_logs_client_certificate: Option<String>,
    pub otel_logs_client_key: Option<String>,
    pub otel_metrics_client_certificate: Option<String>,
    pub otel_metrics_client_key: Option<String>,
    pub otel_metrics_include_session_id: Option<bool>,
}
impl TelemetryConfig {
    pub fn apply_env_overrides(&mut self) {}
}
/// Derive a stable deployment ID (UUIDv5) from the deployment key.
pub fn deployment_id_from_key(key: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes()).to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_carries_no_first_party_sink() {
        let cfg = TelemetryConfig::default();
        let json = serde_json::to_value(&cfg).unwrap();
        for key in [
            "events_url",
            "events_api_key",
            "mixpanel_token",
            "mixpanel_enabled",
            "trace_upload",
        ] {
            assert!(
                json.get(key).is_none(),
                "first-party sink key {key} must not exist"
            );
        }
    }
    #[test]
    fn otel_timeout_fields_accept_int_or_string() {
        let from_int: TelemetryConfig =
            serde_json::from_str(r#"{"otel_timeout":10000,"otel_metric_export_interval":60000}"#)
                .unwrap();
        assert_eq!(from_int.otel_timeout.as_deref(), Some("10000"));
        assert_eq!(
            from_int.otel_metric_export_interval.as_deref(),
            Some("60000")
        );
        let from_str: TelemetryConfig = serde_json::from_str(
            r#"{"otel_timeout":"10000","otel_metric_export_interval":"60000"}"#,
        )
        .unwrap();
        assert_eq!(from_str.otel_timeout.as_deref(), Some("10000"));
        assert_eq!(
            from_str.otel_metric_export_interval.as_deref(),
            Some("60000")
        );
    }
}
