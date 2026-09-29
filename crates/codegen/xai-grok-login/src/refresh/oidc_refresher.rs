use std::sync::Arc;

use crate::error::RefreshTokenFailedReason;
use crate::manager::RefreshReason;
use crate::oidc::OidcRefreshResult;

use super::{AuthSnapshot, RefreshOutcome, TokenRefresher};

#[cfg(test)]
use crate::manager::AuthManager;

/// Escalate to `PermanentFailure` after this many consecutive transient failures (then `PERMANENT_FAILURE_TTL` allows recovery).
/// Same budget as `ExternalBinaryRefresher`, whose ladder is additionally time-based (see `run_cooldown`).
/// Kept above the number of attempts `try_recover_unauthorized` makes per recovery, so one 401 recovery alone cannot escalate.
const MAX_CONSECUTIVE_TRANSIENT_FAILURES: u32 = 5;

/// Counts consecutive transient failures, scoped to the credential they accrued against.
/// The whole struct sits under one lock so the credential check, reset, and increment are a single atomic step.
#[derive(Default)]
struct TransientBudget {
    /// Credential the count belongs to.
    /// A new credential, e.g. after re-login on this long-lived refresher, resets the count so a fresh token never inherits a dead one's escalation.
    key: Option<String>,
    count: u32,
}

pub struct OidcRefresher {
    auth: Arc<dyn AuthSnapshot>,
    transient_budget: parking_lot::Mutex<TransientBudget>,
}

impl OidcRefresher {
    pub fn new(auth: Arc<dyn AuthSnapshot>) -> Self {
        Self {
            auth,
            transient_budget: parking_lot::Mutex::new(TransientBudget::default()),
        }
    }

    /// Clear the transient-failure count on refresh progress (a fresh token or an adopted sibling token), so later failures start from a full budget.
    fn note_refresh_progress(&self) {
        *self.transient_budget.lock() = TransientBudget::default();
    }

    fn record_transient_failure(
        &self,
        message: String,
        tried_key: Option<String>,
        network_unreachable: bool,
    ) -> RefreshOutcome {
        // A request that never reached the IdP proves nothing about the credential, so it does not consume the escalation budget
        // It does not reset the budget either; only real refresh progress does. See `OidcRefreshResult::Failed`.
        if network_unreachable {
            tracing::debug!(%message, "auth: transient refresh failure (network unreachable), not counted toward escalation");
            return RefreshOutcome::transient(message);
        }
        let escalate = {
            let mut budget = self.transient_budget.lock();
            // Reset the count when the credential changes so a fresh token never inherits a prior credential's failures
            if budget.key != tried_key {
                budget.key = tried_key.clone();
                budget.count = 0;
            }
            budget.count += 1;
            let escalate = budget.count >= MAX_CONSECUTIVE_TRANSIENT_FAILURES;
            // On escalation reset the count so the next TTL window gets the full budget (the verdict gates refresh() meanwhile)
            // The key is left in place; a retry with the same key resumes from zero, and a new key resets the budget
            if escalate {
                budget.count = 0;
            }
            escalate
        };
        if escalate {
            tracing::warn!(%message, "auth: escalating consecutive transient failures to permanent");
            RefreshOutcome::permanent(RefreshTokenFailedReason::Other, tried_key)
        } else {
            RefreshOutcome::transient(message)
        }
    }

    /// One-shot retry with the refresh token on disk after `invalid_grant`.
    /// If disk already holds a valid (unexpired) access token with a different key, adopt it directly.
    /// That spends no refresh token on another IdP call and prevents cascading `invalid_grant` when a sibling already refreshed.
    async fn retry_with_fresh_disk_token(&self, tried: &crate::GrokAuth) -> Option<RefreshOutcome> {
        let disk_now = self.auth.read_disk_auth()?;

        // If disk has a valid access token that differs from what we tried, a sibling already refreshed
        // Adopt it directly, with no IdP call
        if !crate::is_expired(&disk_now) && disk_now.key != tried.key {
            self.note_refresh_progress();
            return Some(RefreshOutcome::success(disk_now));
        }

        if disk_now.refresh_token.is_none()
            || disk_now.refresh_token.as_deref() == tried.refresh_token.as_deref()
        {
            return None;
        }


        match crate::oidc::oidc_token_exchange(&disk_now).await {
            OidcRefreshResult::Success(new_auth) => {
                self.note_refresh_progress();
                Some(RefreshOutcome::Success(new_auth))
            }
            OidcRefreshResult::TerminalError { reason } => {
                Some(RefreshOutcome::permanent_for(reason, &disk_now))
            }
            OidcRefreshResult::Failed { .. } => {
                Some(RefreshOutcome::transient("OIDC disk-retry refresh failed"))
            }
        }
    }
}

#[async_trait::async_trait]
impl TokenRefresher for OidcRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {

        let disk_auth = self.auth.read_disk_auth();

        // If disk holds a valid unexpired access token that differs from the in-memory one, a sibling already refreshed
        // That happened between refresh_chain step 2 (the disk check under lock) and here
        // Adopt it directly; no IdP call is needed
        if let Some(ref d) = disk_auth
            && !crate::is_expired(d)
            && self.auth.current().map(|a| a.key).as_deref() != Some(&d.key)
        {
            self.note_refresh_progress();
            return RefreshOutcome::success(d.clone());
        }

        let auth = super::resolve_refresh_credential(self.auth.as_ref(), disk_auth, reason);

        let Some(auth) = auth else {
            return RefreshOutcome::transient("no token with refresh_token available");
        };


        match crate::oidc::oidc_token_exchange(&auth).await {
            OidcRefreshResult::Success(new_auth) => {
                self.note_refresh_progress();
                RefreshOutcome::Success(new_auth)
            }
            OidcRefreshResult::TerminalError { reason } => {
                // A sibling may have rotated the refresh token, so disk can hold a fresher one than we tried. Retry once with it.
                if reason == RefreshTokenFailedReason::RefreshTokenRejected
                    && let Some(retry_outcome) = self.retry_with_fresh_disk_token(&auth).await
                {
                    return retry_outcome;
                }

                RefreshOutcome::permanent_for(reason, &auth)
            }
            OidcRefreshResult::Failed {
                network_unreachable,
            } => {
                tracing::warn!(
                    refresh_reason = ?reason,
                    user_id = %auth.user_id,
                    has_refresh_token = auth.refresh_token.is_some(),
                    network_unreachable,
                    issuer = ?auth.oidc_issuer,
                    client_id = ?auth.oidc_client_id,
                    expires_at = ?auth.expires_at,
                    "auth: OIDC token refresh failed"
                );
                self.record_transient_failure(
                    "OIDC token refresh failed".into(),
                    Some(auth.key.clone()),
                    network_unreachable,
                )
            }
        }
    }
}

#[cfg(test)]
#[path = "oidc_refresher_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "auth_backend_contract_tests.rs"]
mod auth_backend_contract_tests;
