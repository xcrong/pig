//! Unauthorized (401) recovery state machine.
//!
//! When the server rejects a token, `UnauthorizedRecovery` walks through a sequence of recovery steps before giving up:
//!
//! 1. **ReloadFromDisk**: re-read `auth.json` under a file lock.
//!    If the on-disk token differs from the rejected one, accept it (another process may have refreshed).
//! 2. **RefreshFromAuthority**: run the appropriate refresh chain (OIDC token refresh, external binary, etc.) based on `TokenType`.
//!    Skipped when the live token was minted moments ago (fresh-mint guard).
//! 3. **Done**: all recovery strategies exhausted.
use crate::error::{AuthError, RefreshTokenError, RefreshTokenFailedReason};
use crate::manager::AuthManager;
use crate::model::GrokAuth;
use crate::token_type::TokenType;
use std::sync::Arc;
/// Whether the relay should stop reconnecting on this recovery error.
/// Exhaustive and independent of the login-banner decision below: an error reclassification must not change how long a relay lives. The two differ in both directions: `ApiKeyAuthDisabled` cancels but is outside the KPI, and a non-sticky permanent verdict (`ProviderInteractiveRequired`) counts toward the KPI but does not cancel. Non-sticky verdicts age out via `PERMANENT_FAILURE_TTL`, and the relay runs on a child cancellation token, so cancelling on one would leave a headless leader alive but unreachable for its whole lifetime.
pub fn relay_should_cancel(err: &AuthError) -> bool {
    match err {
        AuthError::NotLoggedIn | AuthError::Refresh(RefreshTokenError::Transient(_)) => false,
        AuthError::Refresh(RefreshTokenError::Permanent(e)) => e.reason.is_sticky(),
        AuthError::TokenExpiredNoRefresh
        | AuthError::ServerRejectedNoRecovery
        | AuthError::RecoveryExhausted
        | AuthError::PinnedTeamMismatch { .. }
        | AuthError::ApiKeyAuthDisabled => true,
    }
}
/// Fresh-mint guard window (±) for `ServerRejected` refreshes ([`UnauthorizedRecovery::fresh_mint_guard`]).
/// 120s outlasts in-flight requests sent with a previous key plus validation lag (observed stale 401s land ~20s after mint). `current()`'s 300s early-invalidation buffer keeps any guard-returned token wire-valid.
/// A genuinely-dead fresh token waits at most this long to re-mint; the symmetric bound caps that delay when the clock stepped back.
const FRESH_MINT_GUARD_SECS: i64 = 120;
/// Which recovery step to attempt next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryStep {
    /// Re-read auth.json from disk (file-locked).
    ReloadFromDisk,
    /// Refresh via the authority (OIDC, external binary, etc.).
    RefreshFromAuthority,
    /// All strategies exhausted.
    Done,
}
/// State machine that walks through recovery strategies after a 401.
pub struct UnauthorizedRecovery {    auth_manager: Arc<AuthManager>,
    /// The token that was rejected by the server.
    rejected_token: String,
    /// Current step in the recovery sequence.
    step: RecoveryStep,
    /// Error from `RefreshFromAuthority`, propagated as fallback when devbox recovery doesn't apply.
    authority_error: Option<AuthError>,
    /// Whether the last authority failure was transient.
    /// Kept after `authority_error` is taken so exhaustion preserves the transient/permanent axis (see the `Done` arm).
    authority_was_transient: bool,
}
impl UnauthorizedRecovery {
    /// `rejected` is the credential the server rejected: its key drives recovery.
    pub fn new(auth_manager: Arc<AuthManager>, rejected: Option<GrokAuth>) -> Self {
        let rejected_token = rejected.as_ref().map(|a| a.key.clone()).unwrap_or_default();
        Self {
            auth_manager,
            rejected_token,
            step: RecoveryStep::ReloadFromDisk,
            authority_error: None,
            authority_was_transient: false,
        }
    }
    /// Attempt the next recovery step. Walks disk reload, then the token authority, then any last-resort host recovery.
    /// The `token_type` span field is recorded lazily via `Span::is_disabled()` to avoid the lock when tracing is off.
    #[tracing::instrument(
        skip(self),
        fields(step = ?self.step, token_type = tracing::field::Empty),
    )]
    pub async fn next(&mut self) -> Result<GrokAuth, AuthError> {
        let span = tracing::Span::current();
        if !span.is_disabled() {
            span.record(
                "token_type",
                tracing::field::debug(self.auth_manager.token_type()),
            );
        }
        let result = self.resolve_next().await;
        result
    }
    /// Walk the recovery steps and apply the team-pin policy gate.
    async fn resolve_next(&mut self) -> Result<GrokAuth, AuthError> {
        let auth = self.next_step_loop().await?;
        if let Some(e) = self.auth_manager.cached_token_policy_error(&auth) {
            self.auth_manager.reject_and_clear(&e);
            return Err(e);
        }
        Ok(auth)
    }
    async fn next_step_loop(&mut self) -> Result<GrokAuth, AuthError> {
        loop {
            match self.step {
                RecoveryStep::ReloadFromDisk => {
                    self.step = RecoveryStep::RefreshFromAuthority;
                    if let Some(auth) = self.try_reload_from_disk().await {
                        return Ok(auth);
                    }
                }
                RecoveryStep::RefreshFromAuthority => {
                    {
                        self.step = RecoveryStep::Done;
                    }
                    match self.try_refresh_from_authority().await {
                        Ok(auth) => return Ok(auth),
                        Err(e) => {
                            self.authority_was_transient =
                                matches!(e, AuthError::Refresh(RefreshTokenError::Transient(_)));
                            self.authority_error = Some(e);
                            {
                                return Err(self
                                    .authority_error
                                    .take()
                                    .unwrap_or(AuthError::RecoveryExhausted));
                            }
                        }
                    }
                }
                RecoveryStep::Done => {
                    return Err(if self.authority_was_transient {
                        AuthError::transient("recovery exhausted after transient refresh failure")
                    } else {
                        AuthError::RecoveryExhausted
                    });
                }
            }
        }
    }
    /// Re-read `auth.json` from disk. Accept the token only if it differs from the one that was rejected.
    async fn try_reload_from_disk(&self) -> Option<GrokAuth> {
        let _lock = self
            .auth_manager
            .try_lock_auth_file_async(
                crate::manager::AUTH_LOCK_TIMEOUT,
                crate::manager::lock::Heartbeat::Skip,
            )
            .await
            .into_guard();
        if _lock.is_none() {
            tracing::warn!("auth recovery: proceeding without file lock");
        }
        let Some(disk_auth) = self.auth_manager.read_disk_auth() else {
            return None;
        };
        if crate::is_expired(&disk_auth) {
            tracing::debug!("auth recovery: disk token is expired, skipping");
            return None;
        }
        if self.is_different_token(&disk_auth) {
            tracing::info!("auth recovery: disk has a different token, accepting");
            self.auth_manager.hot_swap(disk_auth.clone());
            Some(disk_auth)
        } else {
            tracing::debug!("auth recovery: disk token is same as rejected, skipping");
            None
        }
    }
    /// Return the live token instead of refreshing when its mint age is within ±[`FRESH_MINT_GUARD_SECS`]. Anything outside (including a clock that stepped far back) falls through to a normal refresh.
    /// A 401 moments after a successful mint is a stale rejection or validation lag on the new key. A stale rejection was sent with the previous key and mis-attributed; see `is_stale_snapshot`.
    /// Re-minting fixes neither, and a crash between the IdP grant and persisting the response orphans the replacement RT (forced re-login). Lives here, not in `refresh_chain`, so paywall claims re-mints that call `refresh_chain(ServerRejected)` directly are unaffected.
    fn fresh_mint_guard(&self) -> Option<GrokAuth> {
        let auth = self.auth_manager.current()?;
        let mint_age_seconds = auth.mint_age_seconds();
        if !(-FRESH_MINT_GUARD_SECS..FRESH_MINT_GUARD_SECS).contains(&mint_age_seconds) {
            return None;
        }
        tracing::info!(
            mint_age_seconds,
            "auth recovery: current token freshly minted, skipping refresh"
        );
        Some(auth)
    }
    /// Dispatch to the correct refresh chain based on the current `TokenType`. Per-variant outcome: **OidcSession / ExternalBinary**: full refresh chain via the authority.
    /// Skipped when the live token is inside the fresh-mint guard window ([`Self::fresh_mint_guard`]). **LegacySession / ApiKey**: no refresh authority for these types.
    /// We've already tried `ReloadFromDisk` (the previous recovery step), so the server's 401 stands. Surface [`AuthError::ServerRejectedNoRecovery`], *not* `TokenExpiredNoRefresh`. The trigger here is the server rejecting the token; it may not have aged past any local TTL (ApiKey in particular has no expiry).
    async fn try_refresh_from_authority(&self) -> Result<GrokAuth, AuthError> {
        let tt = self.auth_manager.token_type();
        match tt {
            TokenType::OidcSession | TokenType::ExternalBinary => {
                if let Some(auth) = self.fresh_mint_guard() {
                    return Ok(auth);
                }
                let result = self
                    .auth_manager
                    .refresh_chain(tt, crate::manager::RefreshReason::ServerRejected)
                    .await;
                match &result {
                    Ok(auth) => {
                    }
                    Err(e) => {
                    }
                }
                result
            }
            TokenType::LegacySession | TokenType::ApiKey => {
                Err(AuthError::ServerRejectedNoRecovery)
            }
            TokenType::None => Err(AuthError::NotLoggedIn),
        }
    }
    /// Check if a candidate token is different from the rejected one.
    fn is_different_token(&self, candidate: &GrokAuth) -> bool {
        candidate.key != self.rejected_token
    }
}
#[cfg(test)]
mod tests {
    //! State-machine matrix tests for `UnauthorizedRecovery`. Coverage targets: All 5 `TokenType` variants crossed with dispatch in `try_refresh_from_authority`. `try_reload_from_disk`: same/different/no token on disk.
    //! `next()` exhaustion (`Done` surfaces `RecoveryExhausted`). Fresh-mint guard: ±window bounds, ExternalBinary, verdict grace, policy-hidden fall-through (fail closed).
    //! These tests use the same in-process `AuthManager` that production does. They inject a counting refresher so we can observe whether the authority was consulted.
    use super::*;
    use crate::config::GrokComConfig;
    use crate::error::{RefreshTokenError, RefreshTokenFailedReason};
    use crate::model::{AuthMode, GrokAuth};
    use crate::refresh::{RefreshOutcome, TokenRefresher};
    use crate::storage::{read_auth_json, write_auth_json};
    use chrono::{Duration, Utc};
    use std::sync::atomic::{AtomicU32, Ordering};
    /// The rejected wire bearer these tests seed into the manager.
    fn rejected_cred() -> Option<GrokAuth> {
        Some(GrokAuth {
            key: "rejected-tok".into(),
            ..GrokAuth::test_default()
        })
    }
    /// Refresher fake: returns Success with a fresh token on every call.
    struct OkRefresher {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl TokenRefresher for OkRefresher {
        async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::Success(Box::new(GrokAuth {
                key: "fresh-from-authority".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt-new".into()),
                expires_at: Some(Utc::now() + Duration::hours(1)),
                ..GrokAuth::test_default()
            }))
        }
    }
    /// Refresher fake: returns PermanentFailure (invalid_grant).
    struct FailRefresher {
        calls: Arc<AtomicU32>,
    }
    #[async_trait::async_trait]
    impl TokenRefresher for FailRefresher {
        async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            RefreshOutcome::permanent(RefreshTokenFailedReason::RefreshTokenRejected, None)
        }
    }
    fn mgr() -> (tempfile::TempDir, Arc<AuthManager>) {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        (dir, m)
    }
    fn seed(mgr: &AuthManager, mode: AuthMode, refresh_token: Option<&str>) {
        let auth = GrokAuth {
            key: "rejected-tok".into(),
            auth_mode: mode,
            refresh_token: refresh_token.map(str::to_string),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(auth);
    }
    #[tokio::test]
    async fn dispatch_oidc_session_uses_refresh_chain() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("recovery should succeed");
        assert_eq!(auth.key, "fresh-from-authority");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn dispatch_external_binary_uses_refresh_chain() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::External, None);
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("external-binary recovery succeeds");
        assert_eq!(auth.key, "fresh-from-authority");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    /// Seed a *valid* (unexpired) in-memory token whose `create_time` lies `mint_age` in the past (negative means the clock stepped back since mint).
    fn seed_valid(mgr: &AuthManager, mode: AuthMode, mint_age: Duration) {
        mgr.hot_swap(GrokAuth {
            key: "rejected-tok".into(),
            auth_mode: mode,
            refresh_token: Some("rt".into()),
            create_time: Utc::now() - mint_age,
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::test_default()
        });
    }
    /// Run one recovery against a counting refresher; return the outcome and how many times the authority was consulted.
    async fn recover_with_ok_refresher(m: &Arc<AuthManager>) -> (Result<GrokAuth, AuthError>, u32) {
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let result = rec.next().await;
        (result, calls.load(Ordering::SeqCst))
    }
    #[tokio::test]
    async fn fresh_mint_guard_skips_idp_for_freshly_minted_token() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::Oidc, Duration::seconds(10));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("guard returns the live token").key,
            "rejected-tok"
        );
        assert_eq!(calls, 0, "a 10s-old token must not be re-minted");
    }
    #[tokio::test]
    async fn fresh_mint_guard_applies_to_external_binary_tokens() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::External, Duration::seconds(10));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("guard returns the live token").key,
            "rejected-tok"
        );
        assert_eq!(calls, 0);
    }
    #[tokio::test]
    async fn fresh_mint_guard_treats_small_negative_age_as_fresh() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::Oidc, Duration::seconds(-60));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("guard returns the live token").key,
            "rejected-tok"
        );
        assert_eq!(calls, 0);
    }
    #[tokio::test]
    async fn fresh_mint_guard_refreshes_when_clock_stepped_far_back() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::Oidc, Duration::hours(-1));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("recovery should succeed").key,
            "fresh-from-authority"
        );
        assert_eq!(calls, 1, "far-negative mint age must reach the IdP");
    }
    #[tokio::test]
    async fn fresh_mint_guard_lets_old_token_refresh() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::Oidc, Duration::minutes(10));
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result.expect("recovery should succeed").key,
            "fresh-from-authority"
        );
        assert_eq!(
            calls, 1,
            "outside the guard window ServerRejected must reach the IdP"
        );
    }
    #[tokio::test]
    async fn fresh_mint_guard_wins_over_cached_permanent_failure() {
        let (_d, m) = mgr();
        seed_valid(&m, AuthMode::Oidc, Duration::seconds(10));
        m.record_permanent_failure(
            "rejected-tok".into(),
            RefreshTokenFailedReason::RefreshTokenRejected.into(),
        );
        let (result, calls) = recover_with_ok_refresher(&m).await;
        assert_eq!(
            result
                .expect("guard precedes the verdict short-circuit")
                .key,
            "rejected-tok"
        );
        assert_eq!(calls, 0);
    }
    #[tokio::test]
    async fn fresh_mint_guard_never_returns_policy_hidden_token() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::Single("team-good".into())),
            ..GrokComConfig::default()
        };
        let m = Arc::new(AuthManager::new(dir.path(), cfg));
        m.hot_swap(GrokAuth {
            key: team_jwt("team-wrong"),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt".into()),
            create_time: Utc::now(),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let result = rec.next().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "hidden token must not satisfy the guard"
        );
        if let Ok(auth) = result {
            assert_ne!(
                auth.key,
                team_jwt("team-wrong"),
                "wrong-team token must never be returned"
            );
        }
    }
    #[tokio::test]
    async fn dispatch_legacy_session_returns_server_rejected_no_recovery() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::WebLogin, None);
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::ServerRejectedNoRecovery),
            "LegacySession recovery should surface ServerRejectedNoRecovery, got {err:?}",
        );
    }
    #[tokio::test]
    async fn dispatch_oidc_without_refresh_token_returns_server_rejected_no_recovery() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, None);
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(matches!(err, AuthError::ServerRejectedNoRecovery));
    }
    #[tokio::test]
    async fn dispatch_api_key_returns_server_rejected_no_recovery() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::ApiKey, None);
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::ServerRejectedNoRecovery),
            "ApiKey recovery should surface ServerRejectedNoRecovery (not \
             TokenExpiredNoRefresh), got {err:?}",
        );
    }
    #[tokio::test]
    async fn dispatch_none_returns_not_logged_in() {
        let (_d, m) = mgr();
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::NotLoggedIn),
            "None token type should surface NotLoggedIn, got {err:?}",
        );
    }
    #[tokio::test]
    async fn reload_from_disk_picks_up_different_token() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        let scope = m.grok_com_config().auth_scope();
        let fresh = GrokAuth {
            key: "fresh-from-disk".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, fresh);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec
            .next()
            .await
            .expect("recovery should pick up the disk token");
        assert_eq!(auth.key, "fresh-from-disk");
    }
    #[tokio::test]
    async fn reload_from_disk_skips_same_token_then_proceeds_to_authority() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        let scope = m.grok_com_config().auth_scope();
        let same = GrokAuth {
            key: "rejected-tok".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt".into()),
            expires_at: Some(Utc::now() + Duration::hours(1)),
            ..GrokAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, same);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("authority refresh succeeds");
        assert_eq!(auth.key, "fresh-from-authority");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fall-through to authority must invoke the refresher exactly once",
        );
    }
    /// With no stored authority error (the first `next()` succeeded), driving past `Done` surfaces `RecoveryExhausted`.
    /// The transient-failure case is pinned by `exhaustion_after_transient_failure_stays_transient`.
    #[tokio::test]
    async fn next_after_done_returns_recovery_exhausted() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        m.set_refresher(Arc::new(OkRefresher {
            calls: Arc::new(AtomicU32::new(0)),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let _ = rec.next().await.unwrap();
        let err = loop {
            if let Err(e) = rec.next().await {
                break e;
            }
        };
        assert!(
            matches!(err, AuthError::RecoveryExhausted),
            "Done state must surface RecoveryExhausted, got {err:?}",
        );
    }
    /// Exhaustion after a *transient* authority failure preserves the transient axis.
    /// Surfacing `RecoveryExhausted` would count a network blip as a forced re-login and make the relay cancel instead of reconnect.
    #[tokio::test]
    async fn exhaustion_after_transient_failure_stays_transient() {
        /// Refresher fake: transient failure on every call.
        struct TransientFailRefresher;
        #[async_trait::async_trait]
        impl TokenRefresher for TransientFailRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                RefreshOutcome::transient("network blip")
            }
        }
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        m.set_refresher(Arc::new(TransientFailRefresher));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let first = rec.next().await.unwrap_err();
        assert!(
            matches!(first, AuthError::Refresh(RefreshTokenError::Transient(_))),
            "authority transient must propagate, got {first:?}",
        );
        let err = loop {
            if let Err(e) = rec.next().await {
                break e;
            }
        };
        assert!(
            matches!(err, AuthError::Refresh(RefreshTokenError::Transient(_))),
            "exhaustion after a transient failure must stay transient, got {err:?}",
        );
        assert!(
            !relay_should_cancel(&err),
            "the relay must reconnect (not cancel) on a transient exhaustion",
        );
    }
    /// A failed authority refresh must surface that error, not `RecoveryExhausted`.
    /// A later `next()` must not retry the authority.
    #[tokio::test]
    async fn failed_authority_refresh_is_not_recovery_exhausted() {
        struct InteractiveRequiredRefresher {
            calls: Arc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl TokenRefresher for InteractiveRequiredRefresher {
            async fn refresh(&self, _reason: crate::manager::RefreshReason) -> RefreshOutcome {
                self.calls.fetch_add(1, Ordering::SeqCst);
                RefreshOutcome::permanent(
                    RefreshTokenFailedReason::ProviderInteractiveRequired,
                    None,
                )
            }
        }
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(InteractiveRequiredRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(
                err,
                AuthError::Refresh(RefreshTokenError::Permanent(ref e))
                    if e.reason == RefreshTokenFailedReason::ProviderInteractiveRequired
            ),
            "authority error must propagate, got {err:?}",
        );
        assert!(
            !matches!(err, AuthError::RecoveryExhausted),
            "must not collapse a non-sticky permanent into RecoveryExhausted"
        );
        assert!(
            !relay_should_cancel(&err),
            "ProviderInteractiveRequired must not cancel the relay, got {err:?}",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first next() must invoke the refresher once",
        );
        let again = rec.next().await.unwrap_err();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a later next() must not retry the authority, got {again:?}",
        );
    }
    #[tokio::test]
    async fn refresh_authority_short_circuits_on_cached_permanent_failure() {
        let (_d, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        m.record_permanent_failure(
            "rejected-tok".into(),
            RefreshTokenFailedReason::RefreshTokenRejected.into(),
        );
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(FailRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(matches!(
            err,
            AuthError::Refresh(RefreshTokenError::Permanent(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "refresher must not be invoked when permanent_failure is cached",
        );
    }
    /// Regression: disk holds a different but expired token.
    /// Recovery must skip it and fall through to RefreshFromAuthority, not return it for the caller to send on the wire (instant 401).
    #[tokio::test]
    async fn reload_from_disk_rejects_expired_different_token() {
        let (dir, m) = mgr();
        seed(&m, AuthMode::Oidc, Some("rt"));
        let scope = m.grok_com_config().auth_scope();
        let expired_different = GrokAuth {
            key: "different-but-expired".into(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(Utc::now() - Duration::hours(1)),
            ..GrokAuth::test_default()
        };
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(scope, expired_different);
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        m.set_refresher(Arc::new(OkRefresher {
            calls: calls.clone(),
        }));
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let auth = rec.next().await.expect("should fall through to authority");
        assert_eq!(
            auth.key, "fresh-from-authority",
            "must skip the expired disk token and use the refresher"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    fn ensure_crypto_provider() {
        let _ = jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.install_default();
    }
    fn team_jwt(principal_id: &str) -> String {
        ensure_crypto_provider();
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &serde_json::json!({
                "sub": "user-1",
                "principal_type": "Team",
                "principal_id": principal_id,
                "exp": 9999999999u64,
            }),
            &jsonwebtoken::EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap()
    }
    /// A sibling writes a wrong-team token to disk; 401 recovery (relay path) must reject and clear it at `next()`, not hand it back as a bearer.
    #[tokio::test]
    async fn recovery_rejects_wrong_team_adopted_disk_token() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::Single("team-good".into())),
            ..GrokComConfig::default()
        };
        let scope = cfg.auth_scope();
        let m = Arc::new(AuthManager::new(dir.path(), cfg));
        seed(&m, AuthMode::Oidc, Some("rt"));
        let mut store = read_auth_json(&dir.path().join("auth.json")).unwrap_or_default();
        store.insert(
            scope,
            GrokAuth {
                key: team_jwt("team-wrong"),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt-sibling".into()),
                expires_at: Some(Utc::now() + Duration::hours(1)),
                ..GrokAuth::test_default()
            },
        );
        write_auth_json(&dir.path().join("auth.json"), &store).unwrap();
        let mut rec = m.unauthorized_recovery(rejected_cred());
        let err = rec.next().await.unwrap_err();
        assert!(
            matches!(err, AuthError::PinnedTeamMismatch { .. }),
            "recovery must reject a wrong-team disk token, got {err:?}"
        );
    }
}
