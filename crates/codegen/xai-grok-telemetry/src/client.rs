//! Legacy internal telemetry client (no first-party sinks).
//!
//! Pig Agent ships no data to non-user endpoints. [`track`] is a no-op kept
//! for call-site compatibility; real emission fans out to the user-owned
//! external OTEL stream via [`crate::external::emit`] before this layer.
use crate::config::{TelemetryConfig, TelemetryMode};
use crate::http::OriginClientInfo;
use crate::session_ctx::EmitterOrigin;
use chrono::{Local, SecondsFormat};
use std::sync::{Mutex, OnceLock};
/// Event property map shared by all telemetry modules.
pub type Metadata = serde_json::Map<String, serde_json::Value>;
/// Strips the [`EmitterOrigin`] prefix so shell events keep their historical `event_value` and workspace events collapse to the same bare suffix.
/// Retained for the emitter-prefix invariant tests; production emission no longer uses it.
#[allow(dead_code)]
fn event_value(event_name: &str) -> &str {
    for origin in EmitterOrigin::ALL {
        if let Some(suffix) = event_name.strip_prefix(origin.event_prefix()) {
            return suffix;
        }
    }
    event_name
}
#[derive(Clone)]
pub struct TelemetryClient {
    mode: TelemetryMode,
}
impl std::fmt::Debug for TelemetryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryClient")
            .field("mode", &self.mode)
            .finish()
    }
}
impl TelemetryClient {
    pub fn from_config(
        _config: TelemetryConfig,
        mode: TelemetryMode,
        _user_id: Option<String>,
        _team_id: Option<String>,
        _deployment_key: Option<String>,
        _origin_client: Option<OriginClientInfo>,
        _shell_version: String,
        _subscription_tier: Option<String>,
        _http_client: reqwest::Client,
    ) -> Self {
        Self { mode }
    }
}
static TELEMETRY_CLIENT: OnceLock<Mutex<Option<TelemetryClient>>> = OnceLock::new();
/// Returns `true` when telemetry mode is `Enabled`.
/// Used by `log_event`; product analytics events only fire in `Enabled` mode.
pub fn is_enabled() -> bool {
    TELEMETRY_CLIENT
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|g| g.as_ref().is_some_and(|c| c.mode.is_enabled()))
}
/// Returns `true` when telemetry mode is `Enabled` or `SessionMetrics`.
/// Used by `session_metrics`; lifecycle events fire in both modes.
pub fn is_session_metrics_enabled() -> bool {
    TELEMETRY_CLIENT
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|g| g.as_ref().is_some_and(|c| c.mode.session_metrics_enabled()))
}
pub struct UserContext {
    pub country: String,
    pub language: String,
    pub timestamp: String,
}
impl UserContext {
    pub fn collect() -> Self {
        let default_language = whoami::Language::En(whoami::Country::Any);
        let lang = whoami::langs()
            .ok()
            .and_then(|mut langs| langs.next())
            .unwrap_or(default_language);
        Self {
            country: lang.country().to_string(),
            language: lang.to_string(),
            timestamp: Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        }
    }
}
#[allow(dead_code)]
static IS_CI: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
#[allow(dead_code)]
fn is_ci_env() -> bool {
    std::env::var("CI").is_ok_and(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
}
/// Per-event enrichment; the serde field names are the wire keys.
/// Retained for the reserved-keys schema test; production emission no longer attaches it.
#[allow(dead_code)]
#[derive(serde::Serialize)]
struct EventEnrichment {
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_leader_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_interactive: Option<bool>,
    is_ci: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_channel: Option<&'static str>,
    dev_build: bool,
    os: &'static str,
    arch: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_cores: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_share_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_window_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_cpu_share_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_cpu_time_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_user_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_system_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    footprint_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_limit_bytes: Option<u64>,
    uptime_secs: u64,
}
impl EventEnrichment {
    #[allow(dead_code)]
    fn capture() -> Self {
        use crate::process_info::{Interactivity, LeaderMode};
        let identity = crate::process_info::identity();
        let process = crate::process_metrics::snapshot();
        Self {
            entrypoint: identity.map(|i| i.entrypoint.into()),
            is_leader_mode: identity.map(|i| i.leader == LeaderMode::Attached),
            is_interactive: identity.map(|i| i.interactivity == Interactivity::Interactive),
            is_ci: *IS_CI.get_or_init(is_ci_env),
            release_channel: crate::process_info::release_channel().map(|c| c.into()),
            dev_build: xai_grok_version::IS_DEV_BUILD,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpu_cores: process.cpu_cores,
            cpu_share_percent: process.cpu.map(|w| w.share_percent),
            cpu_window_ms: process.cpu.map(|w| w.window_ms),
            child_cpu_share_percent: process.cpu.and_then(|w| w.child_share_percent),
            cpu_time_ms: process.cpu_time_ms,
            child_cpu_time_ms: process.child_cpu_time_ms,
            cpu_user_ms: process.cpu_user_ms,
            cpu_system_ms: process.cpu_system_ms,
            rss_bytes: process.rss_bytes,
            footprint_bytes: process.footprint_bytes,
            memory_limit_bytes: process.memory_limit_bytes,
            uptime_secs: process.uptime_secs,
        }
    }
}
#[doc(hidden)]
pub const RESERVED_EVENT_KEYS: &[&str] = &[
    "entrypoint",
    "is_leader_mode",
    "is_interactive",
    "is_ci",
    "release_channel",
    "dev_build",
    "os",
    "arch",
    "cpu_cores",
    "cpu_share_percent",
    "cpu_window_ms",
    "child_cpu_share_percent",
    "cpu_time_ms",
    "child_cpu_time_ms",
    "cpu_user_ms",
    "cpu_system_ms",
    "rss_bytes",
    "footprint_bytes",
    "memory_limit_bytes",
    "uptime_secs",
    crate::activity::SESSIONS_ACTIVE_KEY,
    crate::activity::SUBAGENTS_ACTIVE_KEY,
    crate::activity::COMPACTIONS_ACTIVE_KEY,
    crate::activity::MCP_SERVERS_CONNECTED_KEY,
    crate::activity::TURNS_ACTIVE_KEY,
    crate::activity::WORKFLOW_RUNS_ACTIVE_KEY,
    "session_id",
    "turn_number",
];
/// Legacy internal emitter. No first-party sinks remain, so this is a no-op.
/// External (user-owned OTEL) fan-out happens in [`crate::session_ctx`] before this layer.
pub async fn track(_event_name: &str, _request_id: &str, _ctx: &UserContext, _metadata: Metadata) {}
/// Resolved mode of the initialized client, `None` when off.
/// Lets a parent pass its mode to a spawned child that cannot re-resolve remote settings.
pub fn current_mode() -> Option<TelemetryMode> {
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    guard.as_ref().map(|c| c.mode)
}
/// Legacy profile sync. No first-party sinks remain, so this is a no-op.
pub fn sync_profile() {}
/// Safe to call multiple times. `Disabled`: no client; `SessionMetrics`: client active (only `session_metrics::*` events
/// fire); `Enabled`: client active (all events fire). `shell_version` is stamped into every event payload (legacy field
/// name kept for analytics continuity); shell passes its `CARGO_PKG_VERSION`.
pub fn init(
    config: TelemetryConfig,
    mode: TelemetryMode,
    user_id: Option<String>,
    team_id: Option<String>,
    deployment_key: Option<String>,
    origin_client: Option<OriginClientInfo>,
    shell_version: String,
    subscription_tier: Option<String>,
    http_client: reqwest::Client,
) {
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    *guard = if mode.is_disabled() {
        None
    } else {
        Some(TelemetryClient::from_config(
            config,
            mode,
            user_id,
            team_id,
            deployment_key,
            origin_client,
            shell_version,
            subscription_tier,
            http_client,
        ))
    };
    drop(guard);
    sync_profile();
}
/// Re-initialize the telemetry client if it was not created at startup (e.g. because auth was not yet available).
/// No-op when the client is already set, so safe to call unconditionally after auth succeeds.
pub fn init_if_needed(
    config: TelemetryConfig,
    mode: TelemetryMode,
    user_id: Option<String>,
    team_id: Option<String>,
    deployment_key: Option<String>,
    origin_client: Option<OriginClientInfo>,
    shell_version: String,
    subscription_tier: Option<String>,
    http_client: reqwest::Client,
) {
    if mode.is_disabled() {
        return;
    }
    let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    if guard.is_none() {
        *guard = Some(TelemetryClient::from_config(
            config,
            mode,
            user_id,
            team_id,
            deployment_key,
            origin_client,
            shell_version,
            subscription_tier,
            http_client,
        ));
        drop(guard);
        sync_profile();
    }
}
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    /// Shell events must still strip to their bare suffix, byte-for-byte identical to the previous `strip_prefix("grok-shell-")` behavior.
    #[test]
    fn event_value_strips_shell_prefix() {
        assert_eq!(event_value("grok-shell-turn"), "turn");
        assert_eq!(event_value("grok-shell-session_started"), "session_started");
    }
    /// Workspace events strip their own prefix to the same bare suffix.
    #[test]
    fn event_value_strips_workspace_prefix() {
        assert_eq!(event_value("grok-workspace-turn"), "turn");
    }
    /// No first-party sinks remain; sync_profile is always a no-op.
    #[test]
    fn sync_profile_is_noop_in_session_metrics_mode() {
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "this test must run without a tokio runtime"
        );
        struct ClearClient;
        impl Drop for ClearClient {
            fn drop(&mut self) {
                let lock = TELEMETRY_CLIENT.get_or_init(|| Mutex::new(None));
                *lock.lock().unwrap_or_else(|err| err.into_inner()) = None;
            }
        }
        let _clear = ClearClient;
        let cfg = TelemetryConfig::default();
        init(
            cfg,
            TelemetryMode::SessionMetrics,
            Some("user-1".into()),
            None,
            None,
            None,
            "0.0.0-test".into(),
            None,
            reqwest::Client::new(),
        );
        sync_profile();
        assert!(
            is_session_metrics_enabled(),
            "client must be live for session metrics"
        );
        assert!(!is_enabled(), "product analytics must stay off");
    }
    /// Names without a known emitter prefix pass through unchanged.
    #[test]
    fn event_value_passes_through_unprefixed() {
        assert_eq!(event_value("turn"), "turn");
        assert_eq!(event_value(""), "");
    }
    /// Only the leading emitter prefix is stripped; a suffix that itself looks like another prefix is left intact.
    #[test]
    fn event_value_strips_only_leading_prefix() {
        assert_eq!(event_value("grok-shell-workspace-x"), "workspace-x");
    }
    /// The stripper recovers the bare suffix for every origin the emitter can produce, tying `event_value` to `EmitterOrigin::event_prefix`.
    #[test]
    fn event_value_round_trips_every_emitter_prefix() {
        for origin in EmitterOrigin::ALL {
            let name = format!("{}my_event", origin.event_prefix());
            assert_eq!(event_value(&name), "my_event");
        }
    }
    /// Enrichment and session keys derive from the struct that owns them;
    /// activity-gauge keys are defined in their domain crates and enumerated at
    /// runtime, so they are reserved here explicitly. The const must cover all.
    #[test]
    fn reserved_event_keys_derive_from_the_serialized_schema() {
        let enrichment = EventEnrichment {
            entrypoint: Some("cli"),
            is_leader_mode: Some(false),
            is_interactive: Some(false),
            is_ci: false,
            release_channel: Some("stable"),
            dev_build: false,
            os: "linux",
            arch: "x86_64",
            cpu_cores: Some(1),
            cpu_share_percent: Some(0.0),
            cpu_window_ms: Some(1),
            child_cpu_share_percent: Some(0.0),
            cpu_time_ms: Some(0),
            child_cpu_time_ms: Some(0),
            cpu_user_ms: Some(0),
            cpu_system_ms: Some(0),
            rss_bytes: Some(1),
            footprint_bytes: Some(1),
            memory_limit_bytes: Some(1),
            uptime_secs: 0,
        };
        let mut expected: std::collections::BTreeSet<String> = serde_json::to_value(&enrichment)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        expected.extend(
            [
                crate::activity::SESSIONS_ACTIVE_KEY,
                crate::activity::SUBAGENTS_ACTIVE_KEY,
                crate::activity::COMPACTIONS_ACTIVE_KEY,
                crate::activity::MCP_SERVERS_CONNECTED_KEY,
                crate::activity::TURNS_ACTIVE_KEY,
                crate::activity::WORKFLOW_RUNS_ACTIVE_KEY,
            ]
            .map(String::from),
        );
        expected.extend(["session_id".to_string(), "turn_number".to_string()]);
        let reserved: std::collections::BTreeSet<String> =
            RESERVED_EVENT_KEYS.iter().map(|k| k.to_string()).collect();
        assert_eq!(
            RESERVED_EVENT_KEYS.len(),
            reserved.len(),
            "RESERVED_EVENT_KEYS must not repeat a key"
        );
        assert_eq!(reserved, expected);
    }
    /// `event_value`'s first-match-wins over `EmitterOrigin::ALL` is only correct because no origin's `event_prefix()` is a prefix of another's.
    /// A future origin like `"grok-shell-ext-"` would let an earlier `ALL` entry strip the shorter prefix first and yield the wrong `event_value`.
    /// Pin the invariant so adding such a variant fails the suite rather than silently corrupting analytics.
    #[test]
    fn emitter_prefixes_are_mutually_exclusive() {
        for a in EmitterOrigin::ALL {
            for b in EmitterOrigin::ALL {
                if a != b {
                    assert!(
                        !a.event_prefix().starts_with(b.event_prefix()),
                        "{a:?} prefix {:?} must not start with {b:?} prefix {:?}",
                        a.event_prefix(),
                        b.event_prefix(),
                    );
                }
            }
        }
    }
}
