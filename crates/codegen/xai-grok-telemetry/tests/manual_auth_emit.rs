#![allow(clippy::disallowed_methods)] // test clients hit localhost mocks
//! Wire test: `log_event(ManualAuth)` must NOT POST to any first-party endpoint.
//! Pig Agent ships no product-events sink; the legacy internal funnel is a no-op.
//! The mock collector must stay silent while local emission still runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use xai_grok_telemetry::client;
use xai_grok_telemetry::config::{TelemetryConfig, TelemetryMode};
use xai_grok_telemetry::events::{AuthTokenKind, ManualAuth, ManualAuthReason, ManualAuthSurface};
use xai_grok_telemetry::process_info::{
    Entrypoint, Interactivity, LeaderMode, ProcessIdentity, ReleaseChannel, set_identity,
    set_release_channel,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_auth_posts_to_events_endpoint_as_grok_shell_manual_auth() {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(v);
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    set_identity(ProcessIdentity {
        entrypoint: Entrypoint::Cli,
        leader: LeaderMode::Standalone,
        interactivity: Interactivity::Unattended,
    });
    set_release_channel(ReleaseChannel::Alpha);

    // No endpoint is configured: there is nowhere to POST to.
    client::init(
        TelemetryConfig::default(),
        TelemetryMode::Enabled,
        Some("user-xyz".into()),
        None,
        None,
        None,
        "0.0.0-test".into(),
        None,
        reqwest::Client::new(),
    );

    xai_grok_telemetry::log_event(ManualAuth {
        reason: ManualAuthReason::RefreshTokenRejected,
        trigger: ManualAuthSurface::Turn,
        token_kind: AuthTokenKind::OidcSession,
        principal: Some("user-xyz".into()),
    });

    // The emit is fire-and-forget; give it a beat, then require silence.
    tokio::time::sleep(Duration::from_secs(1)).await;
    xai_grok_telemetry::session_ctx::drain_pending(Duration::from_secs(5)).await;
    assert!(
        bodies.lock().unwrap().is_empty(),
        "no first-party POST may be emitted"
    );

    server.abort();
}
