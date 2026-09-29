//! Background `/user` enrichment spawned by `AuthManager::update()`.
use super::AuthManager;
use super::lock::{Heartbeat, try_lock_auth_file_async};
use crate::manager::AUTH_LOCK_TIMEOUT;
use crate::model::{GrokAuth, UserInfo, lookup_auth};
use crate::storage::{read_auth_json, write_auth_json};
use std::sync::Arc;
use std::time::Duration as StdDuration;
/// Timeout for the `/user` fetch, shared by the inline (login) and background paths.
const USER_FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(10);
/// Logs `auth update enrichment dropped` if the task is cancelled before it finishes.
/// Normal completion calls `disarm` first, which suppresses the log.
pub(super) struct EnrichmentExitGuard {
    pub(super) started: std::time::Instant,
    pub(super) armed: bool,
}
impl EnrichmentExitGuard {
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for EnrichmentExitGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::warn!(
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "auth update enrichment dropped"
        );
    }
}
pub(super) fn spawn(manager: Arc<AuthManager>, auth: GrokAuth) {
    tokio::spawn(async move {
        let mut exit_guard = EnrichmentExitGuard {
            started: std::time::Instant::now(),
            armed: true,
        };
        run_user_info_enrichment(&manager, auth).await;
        exit_guard.disarm();
    });
}
async fn fetch_user_info(manager: &AuthManager, key: &str, log_label: &str) -> Option<UserInfo> {
    let user_url = format!("{}/user", manager.proxy_base_url);
    let token_header = &manager.grok_com_config.token_header;
    let started = std::time::Instant::now();
    let http_client = xai_grok_http::shared_client();
    let response = http_client
        .get(&user_url)
        .timeout(USER_FETCH_TIMEOUT)
        .header("Authorization", format!("Bearer {}", key))
        .header("X-XAI-Token-Auth", token_header.as_str())
        .header("x-grok-client-version", xai_grok_version::VERSION)
        .header(
            xai_grok_http::CLIENT_MODE_HEADER,
            xai_grok_http::process_client_mode(),
        )
        .send()
        .await;
    match response {
        Ok(resp) if resp.status().is_success() => match resp.json::<UserInfo>().await {
            Ok(ui) if !ui.user_id.is_empty() => Some(ui),
            Ok(_) => {
                tracing::warn!(enrichment_ctx = %serde_json::json!({
                        "reason": "empty_user_id",
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                    }), "{log_label} skipped");
                None
            }
            Err(e) => {
                tracing::warn!(enrichment_ctx = %serde_json::json!({
                        "reason": "parse",
                        "error": e.to_string(),
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                    }), "{log_label} failed");
                None
            }
        },
        Ok(resp) => {
            tracing::warn!(enrichment_ctx = %serde_json::json!({
                    "reason": "http_status",
                    "http_status": resp.status().as_u16(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }), "{log_label} failed");
            None
        }
        Err(e) => {
            tracing::warn!(enrichment_ctx = %serde_json::json!({
                    "reason": if e.is_timeout() { "timeout" } else { "transport" },
                    "error": e.to_string(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }), "{log_label} failed");
            None
        }
    }
}
/// Blocking enrichment at login: merges `/user` fields into `auth` before the first save.
pub(super) async fn enrich_inline(manager: &AuthManager, auth: &mut GrokAuth) {
    let Some(ui) = fetch_user_info(manager, &auth.key, "auth login enrichment").await else {
        return;
    };
    apply_user_info_enrichment(auth, ui);
}
async fn run_user_info_enrichment(manager: &AuthManager, auth: GrokAuth) {
    let started = std::time::Instant::now();
    let Some(user_info) = fetch_user_info(manager, &auth.key, "auth update enrichment").await
    else {
        return;
    };
    let user_elapsed_ms = started.elapsed().as_millis() as u64;
    let lock_started = std::time::Instant::now();
    let lock_guard = try_lock_auth_file_async(&manager.path, AUTH_LOCK_TIMEOUT, Heartbeat::Skip)
        .await
        .into_guard();
    let lock_wait_ms = lock_started.elapsed().as_millis() as u64;
    let Some(_lock_guard) = lock_guard else {
        return;
    };
    let Ok(mut map) = read_auth_json(&manager.path) else {
        return;
    };
    let Some(mut disk) = lookup_auth(&map, &manager.scope) else {
        return;
    };
    if disk.key != auth.key || disk.refresh_token != auth.refresh_token {
        return;
    }
    apply_user_info_enrichment(&mut disk, user_info);
    map.insert(manager.scope.clone(), disk.clone());
    let write_started = std::time::Instant::now();
    if let Err(e) = write_auth_json(&manager.path, &map) {
        return;
    }
    manager.with_inner_write(|inner| *inner = Some(disk));
}
/// Keyed on account and principal rather than on `auth.key`, since a token refresh mid-GET rotates the bearer; merged only while still unresolved, since that refresh's own enrichment is newer. A same-identity caller hears the live value; another identity hears `None`.
/// Not written to disk and not via [`apply_user_info_enrichment`]: either would let a response delayed past a privacy write (which takes no file lock) put the older `coding_data_retention_opt_out` back.
pub(super) async fn hydrate_can_administer_team(
    manager: &AuthManager,
    auth: &GrokAuth,
) -> Option<bool> {
    let user_info = fetch_user_info(manager, &auth.key, "auth capability hydration").await?;
    let value = user_info.can_administer_team;
    let same_account = |live: &GrokAuth| {
        auth.email.is_some()
            && live.email == auth.email
            && live.team_id == auth.team_id
            && live.is_team_principal() == auth.is_team_principal()
    };
    let merged = manager.with_inner_write(|inner| match inner.as_mut() {
        Some(live) if same_account(live) => {
            if live.can_administer_team.is_none() {
                live.can_administer_team = value;
            }
            live.can_administer_team
        }
        _ => None,
    });
    tracing::info!(enrichment_ctx = %serde_json::json!({ "can_administer_team": value, "merged": merged }), "auth capability hydration done");
    manager.with_inner_read(|inner| match inner {
        Some(live) if same_account(live) => live.can_administer_team,
        _ => None,
    })
}
/// Merge enrichment fields into disk auth. Does not touch token fields.
pub(super) fn apply_user_info_enrichment(disk: &mut GrokAuth, user_info: UserInfo) {
    disk.user_id = user_info.user_id;
    disk.first_name = user_info.first_name.or(disk.first_name.take());
    disk.last_name = user_info.last_name.or(disk.last_name.take());
    disk.profile_image_asset_id = user_info
        .profile_image_asset_id
        .or(disk.profile_image_asset_id.take());
    disk.principal_type = user_info.principal_type.or(disk.principal_type.take());
    disk.principal_id = user_info.principal_id.or(disk.principal_id.take());
    disk.team_id = user_info.team_id.or(disk.team_id.take());
    disk.team_name = user_info.team_name.or(disk.team_name.take());
    disk.team_role = user_info.team_role.or(disk.team_role.take());
    disk.organization_id = user_info.organization_id.or(disk.organization_id.take());
    disk.organization_name = user_info
        .organization_name
        .or(disk.organization_name.take());
    disk.organization_role = user_info
        .organization_role
        .or(disk.organization_role.take());
    disk.user_blocked_reason = user_info
        .user_blocked_reason
        .or(disk.user_blocked_reason.take());
    if let Some(reasons) = user_info.team_blocked_reasons {
        disk.team_blocked_reasons = reasons;
    }
    if let Some(opt_out) = user_info.coding_data_retention_opt_out {
        disk.coding_data_retention_opt_out = opt_out;
    }
    disk.can_administer_team = user_info.can_administer_team;
    if let Some(ref email) = user_info.email
        && !email.is_empty()
    {
        disk.email = user_info.email;
    }
}
#[cfg(test)]
#[path = "enrichment_tests.rs"]
mod tests;
