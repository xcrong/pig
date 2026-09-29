//! This manager coordinates:
//! - Signal tracking via SessionSignalsHandle
//! - Heuristics evaluation to determine when to request feedback
//! - Background loading of feedback configuration from the backend
//! - Creating feedback request records when triggered
//! - Sending feedback request notifications to clients

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::agent::feedback_client::{FeedbackApiError, FeedbackClient};
use crate::session::feedback::{
    FeedbackHeuristics, FeedbackRequest, FeedbackTier, TriggerCondition,
};
use crate::session::signals::{SessionSignalsActor, SessionSignalsHandle};

use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, ContextType, CreateFeedbackRequestInput, FeedbackContent, FeedbackMode,
    FeedbackSubmission, FeedbackToolOutcome,
};

use crate::session::persistence::{LocalFeedbackEntry, PersistenceMsg, UserFeedbackEntry};
use xai_grok_feedback::parse_structured_feedback;

pub(crate) enum SubmitOutcome {
    Submitted,
    /// No server configured for this session.
    LocalOnly,
    /// Server request failed.
    Failed(anyhow::Error),
}

pub(crate) fn new_submission(
    session_id: String,
    client_type: ClientType,
    content: FeedbackContent,
) -> FeedbackSubmission {
    let mut s = FeedbackSubmission::with_content(session_id, client_type, content);
    s.shell_version = Some(xai_grok_version::VERSION.to_string());
    s
}

#[derive(Debug)]
pub(crate) struct SubmitFeedbackOptions {
    pub solicited: bool,
    pub author_identity: Option<crate::util::user_identity::ResolvedUserIdentity>,
}

pub(crate) async fn submit_feedback_workflow(
    submission: &mut FeedbackSubmission,
    feedback_client: Option<&FeedbackClient>,
    persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
    opts: SubmitFeedbackOptions,
) -> SubmitOutcome {
    let SubmitFeedbackOptions {
        solicited,
        author_identity,
    } = opts;

    if let Some(mut user_meta) = crate::util::parse_json_object_env("GROK_USER_METADATA") {
        // `structured_feedback` is reserved for the client's typed envelope. The shallow merge
        // is later-wins, so an env copy would silently replace the client's enums (or invent
        // the key on reports that carry none); every other env key keeps later-wins.
        if let Some(user_meta) = user_meta.as_object_mut() {
            user_meta.remove("structured_feedback");
        }
        if user_meta.as_object().is_some_and(|meta| !meta.is_empty()) {
            submission.merge_metadata(user_meta);
        }
    }
    // Exhaustive destructure (no `..`) so a new field must be handled, not dropped.
    if let Some(crate::util::user_identity::ResolvedUserIdentity { name, email }) = author_identity
    {
        if let Some(name) = name {
            submission.author_name = Some(name);
        }
        if let Some(email) = email {
            submission.author_email = Some(email);
        }
    }

    if let Some(tx) = persistence_tx {
        // Persist the image inventory, never the payloads feedback.jsonl rides in trace archives with a hard per-file size cap (one screenshot's base64 would sink the whole record).
        // The bytes are also cleartext terminal captures.
        // Take/restore around the clone so the megabytes are never copied either.
        let images = std::mem::take(&mut submission.images);
        let mut persisted = submission.clone();
        persisted.images = images
            .iter()
            .map(
                |i| prod_mc_cli_chat_proxy_types::feedback_types::FeedbackImage {
                    data: format!("<{} base64 bytes stripped>", i.data.len()),
                    mime_type: i.mime_type.clone(),
                    file_name: i.file_name.clone(),
                },
            )
            .collect();
        submission.images = images;
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: submission.session_id.clone(),
            turn_number: submission.turn_number,
            solicited,
            request_id: submission.request_id.clone(),
            dismissed: false,
            submission: Some(persisted),
        });
        if tx.send(PersistenceMsg::Feedback(entry)).is_err() {
            tracing::warn!(
                session_id = %submission.session_id,
                "feedback persistence channel closed; entry dropped",
            );
        }
    }

    let request_id = submission.request_id.clone();
    let appearance_id = request_id.clone();

    // Send the full submission: the feedback backend shows these triage fields, so session context and metadata are intentionally not stripped here

    let outcome = if let Some(client) = feedback_client {
        let result = if let Some(req_id) = request_id {
            with_one_shot_auth_retry(client, || async {
                client
                    .complete_request(&req_id, submission)
                    .await
                    .map(|_| ())
            })
            .await
        } else {
            with_one_shot_auth_retry(client, || async {
                client.submit_feedback(submission).await.map(|_| ())
            })
            .await
        };
        match result {
            Ok(()) => SubmitOutcome::Submitted,
            Err(error) => {
                tracing::warn!(%error, "feedback submission failed");
                SubmitOutcome::Failed(error)
            }
        }
    } else {
        SubmitOutcome::LocalOnly
    };

    {
        let feedback_span = tracing::info_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "responded",
            appearance_id = %appearance_id.as_deref().unwrap_or(""),
            has_feedback_text = submission
                .feedback_text
                .as_ref()
                .is_some_and(|text| !text.is_empty()),
            rating = tracing::field::Empty,
            is_solicited = solicited,
        );
        // Record `rating` only for star ratings; text-only feedback has no rating and must not log a fake 0
        if let Some(rating) = submission.rating_value {
            feedback_span.record("rating", rating);
        }
        feedback_span.in_scope(|| {});
    }

    outcome
}

/// Chat-state fields the session actor passes to [`FeedbackManager::submit_text_feedback`].
pub(crate) struct SessionFeedbackData {
    pub model_id: Option<String>,
    pub resolved_model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub client_version: Option<String>,
    pub session_cwd: String,
}

/// Feedback feature flags threaded through session spawn.
#[derive(Debug, Clone, Default)]
pub(crate) struct FeedbackFlags {
    pub enabled: bool,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

#[derive(Debug, Clone)]
pub struct FeedbackManagerConfig {
    /// Interval for the signals actor snapshot cadence (default: 30s)
    pub sync_interval: Duration,
    /// Whether user-facing feedback features are enabled (popups, `/feedback`, ratings).
    /// Gated by `GROK_FEEDBACK_ENABLED`.
    pub feedback_enabled: bool,
    pub client_type: ClientType,
    /// Whether LOC attribution tracking is enabled for this session.
    /// Propagated into every `SessionTurnDelta`.
    /// The server can then distinguish "tracking off" (zeros are noise) from "tracking on, no code changed" (zeros are real data).
    pub loc_tracking_enabled: bool,
    /// Preferred timeout for draining the upload queue on shutdown (default: 30s).
    /// Process exit clamps this under [`SHUTDOWN_DRAIN_CAP`] (or `GROK_SESSION_EXIT_DRAIN_SECS`) so a hung upload cannot exceed the agent join grace.
    /// Abandoned durable pairs are recovered on next-session startup.
    pub drain_timeout: Duration,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

impl Default for FeedbackManagerConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(60),
            feedback_enabled: false,
                        client_type: ClientType::Agent,
            loc_tracking_enabled: false,
            drain_timeout: Duration::from_secs(30),
            user: None,
        }
    }
}

/// Manages feedback collection for a single session.
pub struct FeedbackManager {
    session_id: String,
    /// Handle for sending signals (cheap to clone)
    signals_handle: SessionSignalsHandle,
    heuristics: Arc<RwLock<FeedbackHeuristics>>,
    /// REST client for the feedback/analytics backend
    feedback_client: Option<FeedbackClient>,
    config: FeedbackManagerConfig,
    config_loaded: Arc<AtomicBool>,
    /// GCS upload queue stats for periodic snapshots into signals.
    /// Set once after the first upload queue is created via `set_upload_queue_stats()`.
    /// `OnceLock` because `FeedbackManager` is behind `Arc` and this is set after construction.
    upload_queue_stats: std::sync::OnceLock<Arc<xai_file_utils::queue::UploadQueueStats>>,
}

impl FeedbackManager {
    /// If `feedback_client` is None, signal syncing is disabled but local tracking and heuristics evaluation still work.
    pub fn new(
        session_id: impl Into<String>,
        feedback_client: Option<FeedbackClient>,
        config: FeedbackManagerConfig,
    ) -> Self {
        let (signals_handle, actor) = SessionSignalsActor::with_sync_interval(config.sync_interval);

        tokio::spawn(actor.run());

        let session_id = session_id.into();
        let feedback_client = feedback_client.map(|c| c.with_session_id(session_id.clone()));
        tracing::info!(
            session_id = %session_id,
            feedback_enabled = config.feedback_enabled,
            has_client = feedback_client.is_some(),
            "FeedbackManager initialized"
        );

        Self {
            session_id,
            signals_handle,
            heuristics: Arc::new(RwLock::new(FeedbackHeuristics::new())),
            feedback_client,
            config,
            config_loaded: Arc::new(AtomicBool::new(false)),
            upload_queue_stats: std::sync::OnceLock::new(),
        }
    }

    pub fn local_only(session_id: impl Into<String>) -> Self {
        Self::new(session_id, None, FeedbackManagerConfig::default())
    }

    pub(crate) fn set_upload_queue_stats(
        &self,
        stats: Arc<xai_file_utils::queue::UploadQueueStats>,
    ) {
        let _ = self.upload_queue_stats.set(stats);
    }

    pub fn signals_handle(&self) -> SessionSignalsHandle {
        self.signals_handle.clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_enabled(&self) -> bool {
        self.config.feedback_enabled
    }

    pub fn feedback_client(&self) -> Option<&FeedbackClient> {
        self.feedback_client.as_ref()
    }

    pub fn client_type(&self) -> prod_mc_cli_chat_proxy_types::feedback_types::ClientType {
        self.config.client_type
    }

    /// Build and submit text feedback from the `/feedback` slash command.
    pub(crate) async fn submit_text_feedback(
        &self,
        text: String,
        session_data: SessionFeedbackData,
        persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
            ) -> SubmitOutcome {
        let sh = self.signals_handle();
        let (signals, tool_outcomes) = tokio::join!(sh.snapshot(), sh.last_turn_tool_outcomes());
        let signals = signals.unwrap_or_default();
        let turn_number = signals.turn_count.saturating_sub(1) as i64;
        let tool_outcomes: Vec<FeedbackToolOutcome> = tool_outcomes
            .into_iter()
            .map(|o| FeedbackToolOutcome {
                tool_name: o.tool_name,
                calls: o.successes + o.failures,
                failures: o.failures,
            })
            .collect();

        let mut submission = new_submission(
            self.session_id.clone(),
            self.config.client_type,
            FeedbackContent::Text(text),
        );
        submission.turn_number = Some(turn_number);
        submission.model_id = session_data.model_id;
        submission.resolved_model_id = session_data.resolved_model_id;
        submission.reasoning_effort = session_data
            .reasoning_effort
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        submission.last_user_message = None;
        submission.last_assistant_message = None;
        submission.tool_outcomes = tool_outcomes;
        submission.session_cwd = Some(session_data.session_cwd);
        submission.compaction_count = Some(signals.compaction_count as i64);
        submission.context_window_usage = Some(signals.context_window_usage);
        submission.context_tokens_used = Some(signals.context_tokens_used);
        submission.context_window_tokens = Some(signals.context_window_tokens);
        submission.client_version = session_data.client_version;

        let author_identity =
            crate::util::user_identity::cached_identity(self.config.user.as_ref()).await;

        submit_feedback_workflow(
            &mut submission,
            self.feedback_client.as_ref(),
            persistence_tx,
            SubmitFeedbackOptions {
                solicited: false,
                                author_identity,
            },
        )
        .await
    }

    /// Load feedback heuristics config from the backend. Can be called manually.
    /// Does not block: errors are logged and defaults are used.
    #[tracing::instrument(name = "feedback.load_config", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn load_config(&self) {
        let Some(client) = &self.feedback_client else {
            return; // No client, use defaults
        };

        if self.config.feedback_enabled {
            match client.get_feedback_config().await {
                Ok(config) => {
                    let mut heuristics = self.heuristics.write().await;
                    heuristics.update_config(&config);
                    self.config_loaded.store(true, Ordering::Relaxed);
                    tracing::info!(
                        session_id = %self.session_id,
                        config_id = %config.config_id,
                        config_version = config.config_version,
                        enabled = config.enabled,
                        "Loaded feedback heuristics config from server"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to load feedback heuristics config, using defaults"
                    );
                }
            }
        }
    }

    /// Evaluate heuristics and return a FeedbackRequest if one should be sent.
    /// Call this after each turn to check if feedback should be requested.
    /// The tier was already triggered this session.
    #[tracing::instrument(name = "feedback.maybe_request_feedback", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn maybe_request_feedback(
        &self,
        prompt_id: Option<String>,
    ) -> Option<FeedbackRequest> {
        if !self.config.feedback_enabled {
            return None;
        }

        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;

        // Check if heuristics are globally enabled (from server config)
        if !heuristics.is_enabled() {
            return None;
        }

        let eval = heuristics.evaluate(&signals);

        if let (true, Some(trigger_condition)) =
            (eval.should_request, eval.trigger_condition.as_ref())
        {
            let tier = trigger_condition.tier;
            let feedback_mode = heuristics.feedback_mode(tier);
            let dismissible = heuristics.dismissible(tier);
            let prompt = heuristics.prompt(tier);
            let request = FeedbackRequest::with_mode(
                self.session_id.clone(),
                trigger_condition.clone(),
                feedback_mode,
                dismissible,
                Some(prompt),
            );
            tracing::info!(
                session_id = %self.session_id,
                tier = ?request.tier,
                trigger_type = %request.trigger_type,
                feedback_mode = ?request.feedback_mode,
                "Feedback request triggered"
            );

            self.record_feedback_request(&request, trigger_condition, feedback_mode, prompt_id)
                .await;

            return Some(request);
        }

        None
    }

    /// Engineers developing clients can call this via the `x.ai/debug/trigger_feedback` ACP extension method.
    /// When a `feedback_client` is configured, the request is also recorded via the feedback API, exactly like a real trigger.
    /// The subsequent `complete_request` / `dismiss_request` round-trip from the client then works end-to-end.
    #[tracing::instrument(name = "feedback.force_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub(crate) async fn force_feedback_request(
        &self,
        tier: FeedbackTier,
        mode: FeedbackMode,
    ) -> FeedbackRequest {
        use crate::session::feedback::TriggerSignalSnapshot;

        let condition = TriggerCondition {
            tier,
            condition: "debug/trigger_feedback (manual test trigger)".to_string(),
            signal_snapshot: TriggerSignalSnapshot {
                turn_count: 0,
                tool_calls_count: 0,
                compactions_count: 0,
                errors_count: 0,
                cancellations_count: 0,
                has_reverted: false,
            },
        };

        // Manual/debug triggers are always dismissible regardless of tier config
        let request = FeedbackRequest::with_mode(
            self.session_id.clone(),
            condition.clone(),
            mode,
            true,
            None,
        );

        self.record_feedback_request(&request, &condition, mode, None)
            .await;

        request
    }

    /// This is a best-effort operation: errors are logged but do not prevent the request from being sent to the client.
    #[tracing::instrument(name = "feedback.record_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    async fn record_feedback_request(
        &self,
        request: &FeedbackRequest,
        trigger_condition: &TriggerCondition,
        feedback_mode: FeedbackMode,
        prompt_id: Option<String>,
    ) {
        let Some(client) = &self.feedback_client else {
            return;
        };

        let input = CreateFeedbackRequestInput {
            request_id: request.request_id.clone(),
            session_id: self.session_id.clone(),
            client_type: self.config.client_type,
            feedback_mode,
            feedback_prompt: Some(request.prompt.clone()),
            priority: tier_to_priority(trigger_condition.tier),
            trigger_type: request.trigger_type.clone(),
            trigger_reason: Some(trigger_condition.trigger_reason()),
            context_type: Some(ContextType::Session),
            context_message_ids: vec![],
            expires_at: None,
            experiment_id: None,
            trigger_condition: serde_json::to_value(trigger_condition).ok(),
            prompt_id,
        };

        match with_one_shot_auth_retry(client, || client.create_feedback_request(&input)).await {
            Ok(response) => {
                tracing::debug!(
                    request_id = %response.request_id,
                    "Feedback request recorded with feedback API"
                );
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    error = %e,
                    "Failed to record feedback request (continuing anyway)"
                );
            }
        }
    }

    /// Loads feedback heuristics config on startup.
    /// This should be spawned as a background task.
    #[tracing::instrument(skip_all, parent = None, fields(session_id = %self.session_id))]
    /// Shutdown: optional upload drain, then signals actor stop.
    /// Non-empty drains use `min(config.drain_timeout, cap)` (default cap 5s, `GROK_SESSION_EXIT_DRAIN_SECS` up to hard max 7s).
    pub async fn shutdown(&self, queue: Option<&xai_file_utils::queue::UploadQueue>) {
        let pending = queue
            .map(|q| q.stats().pending.load(Ordering::Relaxed))
            .unwrap_or(0);

        // Drain only when needed: an empty session with zero pending is skipped
        if let Some(queue) = queue {
            if pending == 0 {
                tracing::debug!(
                    session_id = %self.session_id,
                    "Skipping upload drain on empty session with nothing pending"
                );
            } else {
                let budget = if pending == 0 {
                    SHUTDOWN_EMPTY_DRAIN_TIMEOUT
                } else {
                    nonempty_drain_budget(self.config.drain_timeout)
                };
                let remaining = queue.drain(budget).await;
                if remaining > 0 {
                    let pending_bytes = queue.stats().pending_bytes.load(Ordering::Relaxed);
                    tracing::warn!(
                        session_id = %self.session_id,
                        remaining,
                        pending_bytes,
                        budget_ms = budget.as_millis() as u64,
                        "Upload queue drain incomplete, {} items deferred to next-session recovery ({} bytes pending)",
                        remaining,
                        pending_bytes
                    );
                } else {
                    tracing::debug!(
                        session_id = %self.session_id,
                        "Upload queue drained successfully"
                    );
                }
            }
        }

        self.signals_handle.shutdown();
    }
}

pub(crate) const SHUTDOWN_SIGNAL_SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// Default ceiling on non-empty upload-queue drain at process exit.
/// Keeps sync and drain under [`crate::agent::activity::SESSION_FLUSH_GRACE`] with residual time for hooks/memory.
/// Override with `GROK_SESSION_EXIT_DRAIN_SECS` (still hard-capped by [`SHUTDOWN_DRAIN_HARD_MAX`]).
const SHUTDOWN_DRAIN_CAP: Duration = Duration::from_secs(5);

/// Absolute max non-empty drain at process exit.
/// The 10s flush grace, less the 2s signal-sync budget and the turn-end queue's two 250ms waits, leaves 7.5s; held at 7s for residual.
pub(crate) const SHUTDOWN_DRAIN_HARD_MAX: Duration = Duration::from_secs(7);

/// When nothing is pending, only wait this long for the upload worker to exit after the shutdown signal; in practice this takes milliseconds.
const SHUTDOWN_EMPTY_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Non-empty drain wait: honor config (tests / shorter defaults) but never exceed the process-exit cap.
/// Fleets on slow networks may raise it with `GROK_SESSION_EXIT_DRAIN_SECS`, up to [`SHUTDOWN_DRAIN_HARD_MAX`].
/// Raising it does not slow the empty-session fast path.
fn nonempty_drain_budget(config_timeout: Duration) -> Duration {
    let cap = std::env::var("GROK_SESSION_EXIT_DRAIN_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(SHUTDOWN_DRAIN_CAP)
        .min(SHUTDOWN_DRAIN_HARD_MAX)
        // A zero or garbage env value must not skip the drain entirely
        .max(Duration::from_secs(1));
    config_timeout.min(cap)
}

/// Check if an error is an HTTP 401 Unauthorized response.
/// Uses typed downcast on [`FeedbackApiError`] instead of string matching, so it stays correct even if error messages change.
fn is_auth_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_unauthorized())
}

/// Check if an error is an HTTP 403 Forbidden response.
/// 403 from the signals endpoint means the session does not belong to the current user, a permanent condition that will never self-resolve.
fn is_forbidden_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_forbidden())
}

/// Run `op` once; on 401, wait for an in-flight refresh to land, then retry once.
/// Prefers waiting for the proactive-refresh task or main-request-path recovery over driving a `ServerRejected` refresh itself.
/// That avoids amplifying 401 bursts during token-expiry windows.
async fn with_one_shot_auth_retry<T, F, Fut>(
    client: &FeedbackClient,
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(e) if is_auth_error(&e) => {
            // 1. Wait briefly for the proactive refresh or main-path recovery to land a fresh token.
            let refreshed = client.wait_for_token_refresh(Duration::from_secs(3)).await;
            // 2. If nobody refreshed, drive our own recovery as fallback.
            if refreshed || client.try_refresh_credentials().await {
                op().await
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Convert a FeedbackTier to a priority value (1-10, higher is more important).
fn tier_to_priority(tier: crate::session::feedback::FeedbackTier) -> i32 {
    use crate::session::feedback::FeedbackTier;
    match tier {
        FeedbackTier::Tier1 => 5, // Standard engagement
        FeedbackTier::Tier2 => 6, // Complex session with recovery
        FeedbackTier::Tier3 => 7, // Recovery from friction
    }
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_feedback_manager_local_only() {
        let manager = FeedbackManager::local_only("test-session-123");

        // Track some events
        let signals = manager.signals_handle();
        for _ in 0..10 {
            signals.increment_turn();
        }
        for _ in 0..5 {
            signals.record_tool_call("read_file");
        }
        for _ in 0..2 {
            signals.record_compaction(10_000);
        }

        // Give time for actor to process
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Check signals were tracked
        let snapshot = signals.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_count, 10);
        assert_eq!(snapshot.tool_call_count, 5);
        assert_eq!(snapshot.compaction_count, 2);

        let eval = manager.heuristics.write().await.evaluate(&snapshot);
        assert!(eval.trigger_condition.is_some());
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            crate::session::feedback::FeedbackTier::Tier1
        );

        manager.shutdown(None).await;
    }

    #[test]
    fn test_is_auth_error_detects_401() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "Invalid or expired credentials".to_string(),
        }
        .into();
        assert!(is_auth_error(&err));
    }

    #[test]
    fn test_is_auth_error_ignores_other_statuses() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err_500: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            context: "Signals update",
            body: "oops".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_500));

        let err_403: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            context: "Signals update",
            body: "ZDR team".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_403));
    }

    #[test]
    fn test_is_auth_error_ignores_non_api_errors() {
        assert!(!is_auth_error(&anyhow::anyhow!("network timeout")));
        assert!(!is_auth_error(&anyhow::anyhow!("connection refused")));
    }


    #[test]
    fn test_is_auth_error_works_through_anyhow_conversion() {
        use crate::agent::feedback_client::FeedbackApiError;
        // Verify the FeedbackApiError survives anyhow::Error round-trip (this is the actual path: send_json returns FeedbackApiError.into())
        let api_err = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "token expired".to_string(),
        };
        let anyhow_err: anyhow::Error = api_err.into();
        assert!(is_auth_error(&anyhow_err));
    }

    #[tokio::test]
    async fn test_feedback_manager_disabled() {
        let config = FeedbackManagerConfig {
            feedback_enabled: false,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session", None, config);

        // Even with signals, disabled manager should not request feedback
        let signals = manager.signals_handle();
        for _ in 0..20 {
            signals.increment_turn();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let request = manager.maybe_request_feedback(None).await;
        assert!(request.is_none());

        manager.shutdown(None).await;
    }

    #[tokio::test]
    async fn test_shutdown_without_upload_queue_completes() {
        // This tests the None path in the drain logic.
        let manager = FeedbackManager::local_only("test-session-no-queue");

        manager.shutdown(None).await;

        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    #[tokio::test]
    async fn test_shutdown_with_upload_queue_drains() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use std::sync::Arc;
        use xai_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};

        struct MockResolver;
        impl TraceExportSource for MockResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Direct {
                        service_account_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(
            temp.path(),
            Arc::new(MockResolver),
            UploadRetryPolicy::default(),
        );

        let manager = FeedbackManager::local_only("test-session-with-queue");

        manager.shutdown(Some(&queue)).await;

        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    /// Exit budgets must stay under the agent join grace so hung telemetry cannot push past `SESSION_FLUSH_GRACE` again.
    #[test]
    #[serial_test::serial]
    fn test_shutdown_budgets_fit_under_session_flush_grace() {
        use crate::agent::activity::SESSION_FLUSH_GRACE;
        // `nonempty_drain_budget` reads env; pin default regardless of CI presets.
        let _unset = xai_grok_test_support::env::EnvGuard::unset("GROK_SESSION_EXIT_DRAIN_SECS");
        assert!(
            SHUTDOWN_SIGNAL_SYNC_TIMEOUT + SHUTDOWN_DRAIN_HARD_MAX <= SESSION_FLUSH_GRACE,
            "sync + hard-max drain must fit under flush grace"
        );
        assert!(
            SHUTDOWN_SIGNAL_SYNC_TIMEOUT + SHUTDOWN_DRAIN_CAP < SESSION_FLUSH_GRACE,
            "sync + default drain cap must leave residual grace for hooks/memory"
        );
        assert!(
            SHUTDOWN_EMPTY_DRAIN_TIMEOUT < SHUTDOWN_DRAIN_CAP,
            "empty drain must be strictly shorter than the non-empty cap"
        );
        assert_eq!(
            nonempty_drain_budget(Duration::from_secs(30)),
            SHUTDOWN_DRAIN_CAP,
            "default config 30s must clamp to the process-exit cap"
        );
        assert_eq!(
            nonempty_drain_budget(Duration::from_secs(2)),
            Duration::from_secs(2),
            "config shorter than the cap must be honored"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_nonempty_drain_budget_env_raises_cap() {
        {
            let _guard =
                xai_grok_test_support::env::EnvGuard::set("GROK_SESSION_EXIT_DRAIN_SECS", "7");
            assert_eq!(
                nonempty_drain_budget(Duration::from_secs(30)),
                Duration::from_secs(7),
                "env may raise the cap for slow networks"
            );
        }
        {
            let _guard =
                xai_grok_test_support::env::EnvGuard::set("GROK_SESSION_EXIT_DRAIN_SECS", "99");
            assert_eq!(
                nonempty_drain_budget(Duration::from_secs(30)),
                SHUTDOWN_DRAIN_HARD_MAX,
                "env must not exceed the hard max under flush grace"
            );
        }
    }


    /// Empty upload queue uses the short worker-exit budget.
    #[tokio::test]
    async fn test_shutdown_empty_queue_uses_short_drain_budget() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use std::sync::Arc;
        use std::time::Instant;
        use xai_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};

        struct MockResolver;
        impl TraceExportSource for MockResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Direct {
                        service_account_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(
            temp.path(),
            Arc::new(MockResolver),
            UploadRetryPolicy::default(),
        );
        let manager = FeedbackManager::local_only("test-session-empty-drain");

        let started = Instant::now();
        manager.shutdown(Some(&queue)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "empty-queue shutdown must finish well under the non-empty budget, got {elapsed:?}"
        );
    }

    /// Non-empty queue drain is clamped to the process-exit cap (default 5s), not left open-ended at the 30s config default.
    /// Abandoned durable-pair items stay on disk for next-session orphan recovery.
    #[tokio::test]
    async fn test_shutdown_nonempty_queue_clamps_drain_and_leaves_durable_pair() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        use std::sync::Arc;
        use std::time::Instant;
        use xai_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};

        async fn slow_handler(_body: Body) -> impl IntoResponse {
            tokio::time::sleep(Duration::from_secs(60)).await;
            (StatusCode::OK, "ok")
        }

        let app = Router::new().route("/v1/storage", post(slow_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        struct SlowProxyResolver {
            base: String,
        }
        impl TraceExportSource for SlowProxyResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Proxy {
                        proxy_base_url: self.base.clone(),
                        user_token: "test-token".to_string(),
                        deployment_key: None,
                        alpha_test_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let policy = UploadRetryPolicy {
            max_attempts: 1,
            ..Default::default()
        };
        let queue = UploadQueue::spawn_with_concurrency(
            temp.path(),
            Arc::new(SlowProxyResolver {
                base: format!("http://{addr}/v1"),
            }),
            policy,
            1,
        );
        queue
            .enqueue(
                b"payload",
                "session/turn_0/slow.json",
                "application/json",
                "slow",
                "sess-shutdown-clamp",
                0,
            )
            .await
            .expect("enqueue");
        // Let the worker pick up the item so drain waits on a stuck upload.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            queue.stats().pending.load(Ordering::Relaxed) > 0,
            "item must still be pending against the slow handler"
        );

        let manager = FeedbackManager::local_only("test-session-nonempty-drain");
        let started = Instant::now();
        manager.shutdown(Some(&queue)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(8),
            "non-empty drain must clamp near 5s (+slack), got {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(4),
            "expected to wait most of the non-empty budget, got {elapsed:?}"
        );

        let queue_dir = temp.path().join("upload_queue");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir)
            .expect("queue dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !leftover.is_empty(),
            "abandoned durable-pair artifacts must remain for startup recovery"
        );
    }


    #[tokio::test]
    async fn test_is_auth_permanently_failed_reads_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::sync::Arc;
        use xai_grok_login::error::RefreshTokenFailedReason;
        use xai_grok_login::{AuthManager, GrokAuth, GrokComConfig};

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        let client = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());

        assert!(!client.is_auth_permanently_failed());

        // The verdict is scoped to the live credential's key.
        am.hot_swap(GrokAuth {
            key: "tok".into(),
            ..GrokAuth::test_default()
        });
        // Use a non-sticky reason: only recoverable verdicts age out (a sticky `RefreshTokenRejected` never expires), which exercises the TTL path
        am.record_permanent_failure("tok".to_string(), RefreshTokenFailedReason::Other.into());
        assert!(client.is_auth_permanently_failed());

        am.force_permanent_failure_aged_out();
        assert!(!client.is_auth_permanently_failed());
    }

    #[test]
    fn test_is_auth_permanently_failed_without_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        let client = FeedbackClient::new("http://example/v1", None);
        assert!(!client.is_auth_permanently_failed());
    }

    /// `has_token_refresher` requires BOTH an `AuthManager` AND a refresher wired in.
    /// Otherwise a static-deployment-key session would be mis-classified as recoverable.
    #[tokio::test]
    async fn test_has_token_refresher_requires_refresher_attached() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::sync::Arc;
        use xai_grok_login::{AuthManager, GrokComConfig};

        struct NoOpRefresher;
        #[async_trait::async_trait]
        impl xai_grok_login::refresh::TokenRefresher for NoOpRefresher {
            async fn refresh(
                &self,
                _reason: xai_grok_login::refresh::RefreshReason,
            ) -> xai_grok_login::refresh::RefreshOutcome {
                xai_grok_login::refresh::RefreshOutcome::TransientFailure {
                    message: "noop".into(),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));

        let bare = FeedbackClient::new("http://example/v1", None);
        assert!(!bare.has_token_refresher());

        let with_am = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());
        assert!(
            !with_am.has_token_refresher(),
            "AuthManager without a refresher must NOT be reported as recoverable"
        );

        am.set_refresher(std::sync::Arc::new(NoOpRefresher));
        assert!(with_am.has_token_refresher());
    }

}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
mod author_identity_tests {
    use super::*;
    use crate::util::user_identity::ResolvedUserIdentity;
    use axum::{Router, routing::post};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// Mock feedback backend: capture the POST /v1/feedback JSON body.
    async fn start_capture_server() -> (
        SocketAddr,
        Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
    ) {
        let captured = Arc::new(parking_lot::Mutex::new(None::<serde_json::Value>));
        let captured_for_handler = captured.clone();
        let router = Router::new().route(
            "/v1/feedback",
            post(move |body: axum::Json<serde_json::Value>| {
                let captured = captured_for_handler.clone();
                async move {
                    *captured.lock() = Some(body.0);
                    axum::Json(serde_json::json!({
                        "feedbackId": "fb-1",
                        "createdAt": chrono::Utc::now(),
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, captured)
    }

    fn text_submission() -> FeedbackSubmission {
        let mut s = new_submission(
            "sess-1".to_string(),
            ClientType::Tui,
            FeedbackContent::Text("great session".to_string()),
        );
        s.model_id = Some("grok-4".to_string());
        s
    }

    /// End-to-end: an env var (as a device-management launcher would inject) referenced by `[feedback.user]` with `$VAR` is expanded at config load.
    /// The identity is then resolved, carried on the feedback POST alongside the rest of the submission, and retained on the local entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn env_var_identity_reaches_the_wire_end_to_end() {
        let _email =
            xai_grok_test_support::env::EnvGuard::set("GROK_TEST_WORK_EMAIL", "ada@corp.example");
        let _name =
            xai_grok_test_support::env::EnvGuard::set("GROK_TEST_WORK_NAME", "Ada Lovelace");

        // The loader expands `$VAR` at load, exactly as a trusted config tier ships it.
        let mut value = toml::from_str::<toml::Value>(
            r#"
[feedback.user]
name = ["$GROK_TEST_WORK_NAME"]
email = ["$GROK_TEST_WORK_EMAIL"]
"#,
        )
        .unwrap();
        crate::config::expand_env_vars_in_toml(&mut value);
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&value).unwrap();
        let user = cfg.feedback.user.expect("[feedback.user] present");

        // Resolve through the real production entry point.
        let identity = crate::util::user_identity::cached_identity(Some(&user))
            .await
            .expect("identity resolved");
        assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(identity.email.as_deref(), Some("ada@corp.example"));

        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                                author_identity: Some(identity),
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        // Author identity rides on the same submission as the rest of the feedback; nothing is stripped here
        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("authorName"),
            Some(&serde_json::json!("Ada Lovelace"))
        );
        assert_eq!(
            body.get("authorEmail"),
            Some(&serde_json::json!("ada@corp.example"))
        );
        assert_eq!(body.get("modelId"), Some(&serde_json::json!("grok-4")));
        assert_eq!(
            body.get("feedbackText"),
            Some(&serde_json::json!("great session"))
        );

        // The local entry keeps the author fields and the full context.
        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(persisted.author_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(persisted.author_email.as_deref(), Some("ada@corp.example"));
        assert_eq!(persisted.model_id.as_deref(), Some("grok-4"));
    }

    /// `GROK_USER_METADATA` is merged into the submission and travels with it: onto the wire body for triage and onto the local feedback.jsonl entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_merges_user_metadata_into_submission() {
        let _guard = xai_grok_test_support::env::EnvGuard::set(
            "GROK_USER_METADATA",
            r#"{"team": "platform-tools"}"#,
        );
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );

        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(
            persisted.metadata.as_ref().and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );
    }

    /// `metadata.structured_feedback` is the client's typed envelope: the later-wins env merge
    /// must neither replace it nor invent it, while other env keys keep merging.
    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_env_metadata_cannot_touch_structured_feedback() {
        let _guard = xai_grok_test_support::env::EnvGuard::set(
            "GROK_USER_METADATA",
            r#"{"team": "platform-tools", "structured_feedback": {"type": "forged"}}"#,
        );
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let envelope = serde_json::json!({
            "schema_version": 1,
            "source": "write",
            "type": "bug",
        });
        let mut submission = text_submission();
        submission.metadata = Some(serde_json::json!({ "structured_feedback": envelope.clone() }));

        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            None,
            SubmitFeedbackOptions {
                solicited: false,
                                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("metadata")
                .and_then(|m| m.get("structured_feedback")),
            Some(&envelope)
        );
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );

        // A report without the envelope must not grow one from the environment either.
        *captured.lock() = None;
        let mut submission = text_submission();
        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            None,
            SubmitFeedbackOptions {
                solicited: false,
                                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));
        let body = captured.lock().clone().expect("server saw the POST");
        assert!(
            body.get("metadata")
                .and_then(|m| m.get("structured_feedback"))
                .is_none()
        );
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_without_identity_omits_author_fields() {
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );

        // Both no opt-in and an unresolved opt-in must leave the author keys out of the body and the local entry
        for (case, author_identity) in [
            ("no opt-in", None),
            ("unresolved", Some(ResolvedUserIdentity::default())),
        ] {
            // Reset so this case can't pass on the previous case's body.
            *captured.lock() = None;
            let mut submission = text_submission();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let outcome = submit_feedback_workflow(
                &mut submission,
                Some(&client),
                Some(&tx),
                SubmitFeedbackOptions {
                    solicited: false,
                                        author_identity,
                },
            )
            .await;
            assert!(matches!(outcome, SubmitOutcome::Submitted), "{case}");

            let body = captured.lock().clone().expect("server saw the POST");
            assert!(body.get("authorName").is_none(), "{case}: {body}");
            assert!(body.get("authorEmail").is_none(), "{case}: {body}");

            let msg = rx.try_recv().expect("persistence entry was sent");
            let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
                panic!("expected a feedback persistence entry");
            };
            let persisted = entry.submission.expect("submission persisted");
            assert_eq!(persisted.author_name, None, "{case}");
            assert_eq!(persisted.author_email, None, "{case}");
        }
    }
}
