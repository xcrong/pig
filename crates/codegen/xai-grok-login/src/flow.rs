use crate::backend::{ActiveAuthBackend, AuthBackend, LoginRequest};
use crate::config::LEGACY_AUTH_SCOPE;
use crate::{AuthManager, GrokAuth, GrokComConfig, parse_output};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::{mpsc, oneshot};
use xai_grok_shell_base::util::grok_home;
pub type StderrCallback = Box<dyn Fn(&str)>;
/// Guidance shown when no non-interactive credential is available.
/// The generic harness never opens a browser; users configure a vendor or provider instead.
pub const VENDOR_AUTH_GUIDANCE: &str = "Configure a model provider: set `[vendors.<id>] enabled = true` with its `env_key`, or add `[model_providers.*]` (see docs/user-guide/11-custom-models.md).";
/// Reject a cached credential that lacks `oidc_issuer`, has a mismatched issuer, or whose team principal violates the `force_login_team_uuid` pin.
fn is_cached_credential_compatible(auth: &GrokAuth, grok_com_config: &GrokComConfig) -> bool {
    let expected_issuer = grok_com_config
        .oidc
        .as_ref()
        .map(|c| c.issuer.as_str())
        .or_else(|| grok_com_config.oauth2.as_ref().map(|c| c.issuer.as_str()));
    let issuer_compatible = match (auth.oidc_issuer.as_deref(), expected_issuer) {
        (Some(actual), Some(expected)) => actual == expected,
        (None, Some(_)) => false,
        _ => true,
    };
    if !issuer_compatible {
        return false;
    }
    if let Some(policy) = crate::oidc::login_principal_policy(grok_com_config) {
        let actual = crate::oidc::peek_access_token_principal_id(&auth.key);
        if crate::oidc::enforce_login_principal(Some(&policy), actual.as_deref()).is_err() {
            return false;
        }
    }
    true
}
/// How login presents itself; sent to the TUI via `x.ai/auth/get_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthUrlMode {
    /// Loopback-callback flow: the TUI shows a copyable URL and a paste box.
    Loopback,
    /// External auth provider opened its own browser: the TUI shows a waiting status.
    Command,
    /// RFC 8628 device flow: the TUI shows the device code and a copyable URL, no paste box.
    Device,
}
impl AuthUrlMode {
    /// Wire string for the `x.ai/auth/get_url` ACP response.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Command => "command",
            Self::Device => "device",
        }
    }
    /// Back-compat flag for older clients that only read `external_provider`.
    pub fn is_external_provider(self) -> bool {
        matches!(self, Self::Command)
    }
}
/// Auth URL pushed from the auth flow to the TUI.
pub struct AuthUrlInfo {
    pub url: String,
    pub mode: AuthUrlMode,
}
/// Channels for interactive login between the auth flow and the TUI/extension.
pub struct AuthChannels {
    pub url_tx: Option<oneshot::Sender<AuthUrlInfo>>,
    pub code_rx: mpsc::Receiver<String>,
}
/// Sets no `GROK_AUTH_EXPIRED`: operator binaries, which live outside this repo, read that variable as "headless, don't prompt" and decline the run.
pub async fn run_external_auth_provider(
    command: &str,
    auth_manager: &Arc<AuthManager>,
    over_stale_credential: bool,
    on_stderr: Option<StderrCallback>,
) -> anyhow::Result<(GrokAuth, bool)> {
    let inherit_stderr = on_stderr.is_none();
    tracing::info!(
        cmd = %command,
        over_stale_credential,
        inherit_stderr,
        "auth: running external auth provider (interactive login)"
    );
    let mut cmd = xai_grok_shell_base::util::subprocess::shell_c(command);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    if inherit_stderr {
        cmd.stderr(std::process::Stdio::inherit());
    } else {
        cmd.stderr(std::process::Stdio::piped());
    }
    xai_grok_tools::util::detach_command(&mut cmd);
    cmd.envs(xai_grok_tools::util::pager_env());
    #[allow(clippy::disallowed_methods)]
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to start auth provider `{command}`: {e}"))?;
    let stderr_task = if let Some(cb) = on_stderr {
        let stderr = child.stderr.take().expect("stderr was set to piped");
        Some(tokio::task::spawn_local(async move {
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        tracing::debug!(line = trimmed, "auth: provider stderr");
                        cb(trimmed);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "auth: error reading provider stderr");
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("external auth provider `{command}` timed out after 300s"))?
    .map_err(|e| anyhow::anyhow!("external auth provider `{command}` IO error: {e}"))?;
    if let Some(task) = stderr_task {
        let _ = task.await;
    }
    let mut auth = parse_output(&output)
        .map_err(|e| anyhow::anyhow!("external auth provider `{command}`: {e}"))?;
    let principal_policy = crate::oidc::login_principal_policy(auth_manager.grok_com_config());
    crate::oidc::enforce_login_principal(
        principal_policy.as_ref(),
        crate::oidc::peek_access_token_principal_id(&auth.key).as_deref(),
    )?;
    match (over_stale_credential, auth_manager.current_or_expired()) {
        (true, Some(prev)) => auth.carry_user_profile_from(&prev),
        _ => auth_manager.enrich_auth_inline(&mut auth).await,
    }
    let auth = auth_manager
        .update(auth)
        .await
        .map_err(|e| anyhow::anyhow!("failed to save external auth credentials: {e}"))?;
    tracing::info!(
        user_id = %auth.user_id,
        email = ?auth.email,
        "auth: external provider login complete"
    );
    Ok((auth, true))
}
/// GUI auth: bridges external provider stderr to `url_tx`, pipes code submission via `code_rx`.
pub async fn run_auth_flow_with_stderr_bridge(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    channels: AuthChannels,
    reauth: bool,
    force_interactive: bool,
) -> anyhow::Result<(GrokAuth, bool)> {
    let url_tx = Rc::new(RefCell::new(channels.url_tx));
    let stderr_lines: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let writer = stderr_lines.clone();
    let on_stderr: StderrCallback = Box::new(move |line: &str| {
        writer.borrow_mut().push(line.to_owned());
    });
    let reader = stderr_lines.clone();
    let url_tx_bridge = url_tx.clone();
    let bridge = async move {
        loop {
            tokio::task::yield_now().await;
            let content = {
                let lines = reader.borrow();
                if lines.is_empty() {
                    None
                } else {
                    Some(lines.join("\n"))
                }
            };
            if let Some(joined) = content
                && let Some(tx) = url_tx_bridge.borrow_mut().take()
            {
                let url = joined
                    .split_whitespace()
                    .find(|w| w.starts_with("https://"))
                    .map(|u| u.to_owned())
                    .unwrap_or(joined);
                let _ = tx.send(AuthUrlInfo {
                    url,
                    mode: AuthUrlMode::Command,
                });
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    };
    if force_interactive {
        let auth = run_auth_flow_interactive(
            auth_manager,
            grok_com_config,
            Some(on_stderr),
            Some(url_tx),
            Some(channels.code_rx),
        );
        tokio::select! {
            r = auth => r,
            _ = bridge => {
                tracing::error!("auth stderr bridge exited unexpectedly during interactive login");
                Err(anyhow::anyhow!("Login failed. Please try again."))
            },
        }
    } else {
        let auth = run_auth_flow(
            auth_manager,
            grok_com_config,
            reauth,
            Some(on_stderr),
            Some(url_tx),
            Some(channels.code_rx),
        );
        tokio::select! {
            r = auth => r,
            _ = bridge => {
                tracing::error!("auth stderr bridge exited unexpectedly during login");
                Err(anyhow::anyhow!("Login failed. Please try again."))
            },
        }
    }
}
/// Full auth chain: cache, then refresh, then external provider.
/// Browser-based OAuth was removed; without a provider the caller gets vendor guidance.
pub async fn run_auth_flow(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    reauth: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
) -> anyhow::Result<(GrokAuth, bool)> {
    run_auth_flow_inner(
        auth_manager,
        grok_com_config,
        reauth,
        false,
        on_stderr,
        url_tx,
        code_rx,
    )
    .await
}
/// Like [`run_auth_flow`] but with `force_interactive`: skip cached credentials without clearing them.
pub async fn run_auth_flow_interactive(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
) -> anyhow::Result<(GrokAuth, bool)> {
    run_auth_flow_inner(
        auth_manager,
        grok_com_config,
        false,
        true,
        on_stderr,
        url_tx,
        code_rx,
    )
    .await
}
/// Every interactive login returns through here, so reporting the failure here costs one event per attempt.
/// A retried request, or the discovery cache that the background token refresh shares, can't inflate the count.
/// The reporting never changes the result.
async fn run_auth_flow_inner(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    reauth: bool,
    force_interactive: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
) -> anyhow::Result<(GrokAuth, bool)> {
    let result = ActiveAuthBackend::default()
        .login(LoginRequest {
            auth_manager,
            grok_com_config,
            reauth,
            force_interactive,
            on_stderr,
            url_tx,
            code_rx,
        })
        .await;
    result
}
pub(super) async fn run_auth_flow_steps(
    auth_manager: &Arc<AuthManager>,
    grok_com_config: &GrokComConfig,
    reauth: bool,
    force_interactive: bool,
    on_stderr: Option<StderrCallback>,
    url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    code_rx: Option<mpsc::Receiver<String>>,
) -> anyhow::Result<(GrokAuth, bool)> {
    tracing::info!(
        has_oidc = grok_com_config.oidc.is_some(),
        has_oauth2 = grok_com_config.oauth2.is_some(),
        has_external_auth = grok_com_config.auth_provider_command.is_some(),
        reauth,
        "auth: starting auth flow"
    );
    if reauth {
        auth_manager.clear()?;
        let _ = auth_manager.remove_scope(LEGACY_AUTH_SCOPE);
    }
    if !force_interactive && let Some(auth) = auth_manager.current() {
        if is_cached_credential_compatible(&auth, grok_com_config) {
            tracing::info!(auth_mode = ?auth.auth_mode, "auth: using cached credentials");
            return Ok((auth, false));
        }
        tracing::info!(
            auth_mode = ?auth.auth_mode,
            "auth: cached credential incompatible with requested flow, proceeding to interactive login"
        );
        if auth.auth_mode == super::AuthMode::WebLogin
            && let Err(e) = auth_manager.remove_scope(LEGACY_AUTH_SCOPE)
        {
            tracing::warn!(error = ?e, "auth: failed to remove legacy scope entry (non-fatal)");
        }
    }
    if !force_interactive && !reauth && auth_manager.is_expired() {
        let file_lock = auth_manager
            .try_lock_auth_file_async(
                crate::manager::AUTH_LOCK_TIMEOUT,
                crate::manager::lock::Heartbeat::Skip,
            )
            .await
            .into_guard();
        let disk_auth = auth_manager.read_disk_auth();
        let disk_expired = disk_auth.as_ref().is_some_and(crate::is_expired);
        if disk_auth.as_ref().is_some_and(|d| {
            !crate::is_expired(d) && is_cached_credential_compatible(d, grok_com_config)
        }) {
            let d = disk_auth.unwrap();
            let ret = d.clone();
            auth_manager.hot_swap(d);
            return Ok((ret, false));
        }
        drop(file_lock);
        match auth_manager.auth().await {
            Ok(fresh) => return Ok((fresh, false)),
            Err(e) => {
                if let Some(d) = disk_auth.filter(|d| {
                    matches!(
                        &e,
                        crate::error::AuthError::Refresh(
                            crate::error::RefreshTokenError::Transient(_)
                        )
                    ) && d.refresh_token.is_some()
                }) {
                    let ret = d.clone();
                    auth_manager.hot_swap(d);
                    return Ok((ret, false));
                }
            }
        }
    }
    if let Some(ref cmd) = grok_com_config.auth_provider_command {
        let over_stale_credential = reauth || auth_manager.is_expired();
        match run_external_auth_provider(cmd, auth_manager, over_stale_credential, on_stderr).await
        {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "auth: external auth provider failed"
                );
                anyhow::bail!("{e:#}. {VENDOR_AUTH_GUIDANCE}");
            }
        }
    }
    let _ = (url_tx, code_rx);
    anyhow::bail!(VENDOR_AUTH_GUIDANCE)
}
/// Non-interactive auth refresh: returns valid credentials if available without ever triggering interactive login (browser, device code, etc.).
/// Tries cached non-expired credentials, then OIDC silent refresh (needs a refresh_token), then the external auth provider command (if configured).
/// Returns `None` when no valid credentials can be obtained non-interactively.
pub async fn try_ensure_fresh_auth(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Option<GrokAuth> {
    try_ensure_fresh_auth_with(&build_startup_auth_manager(grok_com_config, proxy_base_url)).await
}
/// Builds and configures the startup `AuthManager`; the policy helpers below take it injected so tests can substitute their own.
fn build_startup_auth_manager(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Arc<AuthManager> {
    let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
        &grok_home::grok_home(),
        grok_com_config.clone(),
        proxy_base_url,
    ));
    auth_manager.configure_refresher(grok_com_config.auth_provider_command.clone());
    auth_manager
}
/// Uses cached valid credentials, else a silent refresh; never interactive login.
async fn try_ensure_fresh_auth_with(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    match auth_manager.auth().await {
        Ok(auth) => Some(auth),
        Err(e) => {
            tracing::debug!(error = %e, "try_ensure_fresh_auth: no valid credentials available");
            None
        }
    }
}
/// Readiness-path auth: a bounded refresh plus the expired-but-refreshable cached session, but no cold mint (which can run a provider command up to `STARTUP_AUTH_TIMEOUT`). Minting is deferred to the post-readiness background task, so readiness waits at most `STARTUP_AUTH_REFRESH_TIMEOUT`.
pub async fn try_noninteractive_auth_no_mint(
    grok_com_config: &GrokComConfig,
    proxy_base_url: String,
) -> Option<GrokAuth> {
    try_noninteractive_auth_no_mint_with(&build_startup_auth_manager(
        grok_com_config,
        proxy_base_url,
    ))
    .await
}
/// Policy behind [`try_noninteractive_auth_no_mint`], with the `AuthManager`
/// injected for tests.
async fn try_noninteractive_auth_no_mint_with(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    match tokio::time::timeout(
        xai_grok_http::STARTUP_AUTH_REFRESH_TIMEOUT,
        try_ensure_fresh_auth_with(auth_manager),
    )
    .await
    {
        Ok(Some(auth)) => return Some(auth),
        Ok(None) => {}
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = xai_grok_http::STARTUP_AUTH_REFRESH_TIMEOUT.as_secs(),
                "boot auth refresh timed out; using cached/expired session (mint deferred to background)"
            );
        }
    }
    expired_refreshable_session(auth_manager)
}
/// A cached, refreshable session (not BYOK/ApiKey).
/// Reached only after fresh auth failed, so in practice the token is expired but recoverable on 401.
fn expired_refreshable_session(auth_manager: &AuthManager) -> Option<GrokAuth> {
    auth_manager
        .current_or_expired()
        .filter(|a| a.is_xai_auth() && a.refresh_token.is_some())
}
/// Cold-start mint via non-interactive providers (external command, devbox); `None` when none is available. Persists the result into `auth_manager` (disk and in-memory) so per-request `auth()` self-heals.
/// Carries no timeout of its own: the readiness-path caller imposes `STARTUP_AUTH_TIMEOUT`. The leader's background re-mint runs uncapped (only the provider's ~300s ceiling).
pub async fn mint_session_noninteractive(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    let grok_com_config = auth_manager.grok_com_config();
    if !ActiveAuthBackend::default().is_xai_authority() {
        return None;
    }
    if grok_com_config.blocks_automatic_oidc() {
        tracing::debug!(
            "mint_session_noninteractive: skipped (preferred_method=api_key blocks automatic OIDC)"
        );
        return None;
    }
    if let Some(cmd) = grok_com_config.auth_provider_command.as_deref() {
        match run_external_auth_provider(cmd, auth_manager, false, None).await {
            Ok((auth, _)) => return Some(auth),
            Err(e) => {
                tracing::debug!(error = %e, "mint_session_noninteractive: external provider failed");
            }
        }
    }
    None
}
/// Persist a minted token; on persist failure, return it unpersisted rather than dropping a valid credential.
#[cfg(test)]
async fn persist_or_use_minted(auth_manager: &AuthManager, new_auth: GrokAuth) -> GrokAuth {
    match auth_manager.save_without_enrichment(new_auth.clone()).await {
        Ok(auth) => {
            let _ = auth_manager.remove_scope(LEGACY_AUTH_SCOPE);
            auth
        }
        Err(e) => {
            tracing::warn!(error = %e, "mint persist failed; using unpersisted token");
            new_auth
        }
    }
}
/// Print the CLI "signed in" confirmation, clearing the spinner line first.
pub fn report_signed_in(auth: &GrokAuth) {
    eprint!("\r\x1b[K");
    match auth.email {
        Some(ref email) => eprintln!("✓ Signed in as {email}"),
        None => eprintln!("✓ Signed in"),
    }
}
/// Result of a logout operation.
/// Both the CLI subcommand and the ACP `/logout` slash command use it, so the presentation layer formats the outcome without duplicating auth logic.
pub struct LogoutResult {
    /// `true` if a cached OAuth session was found and cleared.
    pub was_logged_in: bool,
    /// Email of the session that was cleared (if available).
    pub email: Option<String>,
    /// `true` if `XAI_API_KEY` / `GROK_CODE_XAI_API_KEY` env var is set.
    pub api_key_still_set: bool,
}
/// Core logout logic shared by the CLI subcommand and the ACP handler.
/// `None` scope clears the default (same as `/logout`); `Some` removes only that scope entry.
/// `clear_orphan_managed_config` is injected so this crate stays off the shell's managed config.
pub fn perform_logout(
    auth_manager: &AuthManager,
    scope: Option<&str>,
    clear_orphan_managed_config: impl FnOnce(),
) -> std::io::Result<LogoutResult> {
    let auth = auth_manager.current_or_expired();
    let email = auth.as_ref().and_then(|a| a.email.clone());
    let was_logged_in = auth.is_some();
    if was_logged_in {
        if let Some(scope) = scope {
            auth_manager.remove_scope(scope)?;
        } else {
            auth_manager.clear()?;
        }
        clear_orphan_managed_config();
    }
    Ok(LogoutResult {
        was_logged_in,
        email,
        api_key_still_set: crate::auth_method::has_xai_api_key_env(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthMode;
    use crate::config::XAI_OAUTH2_ISSUER;
    use chrono::Utc;
    use std::path::Path;
    fn oidc_session(key: &str, refresh: Option<&str>) -> GrokAuth {
        GrokAuth {
            key: key.into(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.to_string()),
            refresh_token: refresh.map(str::to_string),
            ..GrokAuth::test_default()
        }
    }
    #[test]
    fn expired_refreshable_session_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = AuthManager::new(dir.path(), GrokComConfig::default());
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-but-refreshable", Some("rt"))
        });
        assert!(
            mgr.current().is_none(),
            "precondition: token must be expired"
        );
        assert_eq!(
            expired_refreshable_session(&mgr).map(|a| a.key),
            Some("expired-but-refreshable".to_string())
        );
        mgr.hot_swap(oidc_session("no-rt", None));
        assert!(expired_refreshable_session(&mgr).is_none());
        mgr.hot_swap(GrokAuth {
            auth_mode: AuthMode::External,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-external", Some("rt"))
        });
        assert_eq!(
            expired_refreshable_session(&mgr).map(|a| a.key),
            Some("expired-external".to_string())
        );
        mgr.hot_swap(GrokAuth {
            oidc_issuer: None,
            auth_mode: AuthMode::External,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("expired-external-3p", Some("rt"))
        });
        assert!(expired_refreshable_session(&mgr).is_none());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn persist_or_use_minted_returns_token_when_save_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        let minted = oidc_session("minted-token", Some("rt"));
        let save = mgr.save_without_enrichment(minted.clone()).await;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert!(
            save.is_err(),
            "non-root: save into a read-only dir must fail"
        );
        let out = persist_or_use_minted(&mgr, minted).await;
        assert_eq!(
            out.key, "minted-token",
            "must return the unpersisted minted token"
        );
    }
    /// Proxy URL on a closed port: inline enrichment fails fast instead of reaching outside the test.
    fn dead_proxy_url() -> String {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        format!("http://127.0.0.1:{port}")
    }
    #[tokio::test]
    async fn mint_session_noninteractive_uses_external_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_provider_command: Some("printf '%s' xai-ext-token".to_string()),
            ..GrokComConfig::default()
        };
        let mgr = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url(&dead_proxy_url()),
        );
        let auth = mint_session_noninteractive(&mgr).await;
        assert_eq!(auth.map(|a| a.key), Some("xai-ext-token".to_string()));
    }
    #[tokio::test]
    async fn interactive_login_carries_no_expired_flag_even_over_a_stale_credential() {
        let echo_env = "printf '%s' \"e=${GROK_AUTH_EXPIRED:-unset}\"";
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("stale-token", None)
        });
        let (auth, _) = run_external_auth_provider(echo_env, &mgr, true, None)
            .await
            .expect("provider output must parse");
        assert_eq!(
            auth.key, "e=unset",
            "a login over an expired credential must not tell the binary to take its silent path"
        );
    }
    /// The script is the one published in `README.md`, which operators copy.
    #[tokio::test]
    async fn a_provider_written_to_the_published_contract_can_sign_in_after_an_expiry() {
        let conforming =
            r#"if [ "$GROK_AUTH_EXPIRED" = "1" ]; then exit 1; else printf '%s' sso-token; fi"#;
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..oidc_session("stale-token", None)
        });
        let (auth, _) = run_external_auth_provider(conforming, &mgr, true, None)
            .await
            .expect("the sign-in run must reach the binary's interactive branch");
        assert_eq!(auth.key, "sso-token");
    }
    /// External-provider output is team-pinned before persist (parity with OIDC / device-code).
    /// A wrong-team token is rejected and nothing is written.
    #[tokio::test]
    async fn external_provider_rejects_wrong_team_and_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), pinned_cfg("team-good"))
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = format!("printf '%s' {}", team_jwt("team-wrong"));
        assert!(
            run_external_auth_provider(&cmd, &mgr, false, None)
                .await
                .is_err(),
            "wrong-team external token must be rejected"
        );
        assert!(
            mgr.current_or_expired().is_none(),
            "rejected external login must persist nothing"
        );
        assert!(
            !dir.path().join("auth.json").exists(),
            "rejected external login must not write auth.json"
        );
    }
    /// A matching-team external token is accepted and persisted.
    #[tokio::test]
    async fn external_provider_accepts_matching_team() {
        let dir = tempfile::tempdir().unwrap();
        let jwt = team_jwt("team-good");
        let mgr = Arc::new(
            AuthManager::new(dir.path(), pinned_cfg("team-good"))
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = format!("printf '%s' {jwt}");
        let (auth, _) = run_external_auth_provider(&cmd, &mgr, false, None)
            .await
            .expect("matching-team external token must be accepted");
        assert_eq!(auth.key, jwt);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn external_reauth_without_prev_auth_enriches_inline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().route(
            "/user",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "userId": "u-1",
                    "teamBlockedReasons": ["BLOCKED_REASON_NO_LOGS"],
                }))
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&format!("http://127.0.0.1:{port}")),
        );
        assert!(mgr.current_or_expired().is_none(), "precondition: no auth");
        let (auth, _) = run_external_auth_provider("printf '%s' fresh-token", &mgr, true, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "flags must come from /user fetch");
        assert_eq!(auth.user_id, "u-1");
    }
    #[tokio::test]
    async fn external_refresh_carries_profile_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        mgr.hot_swap(GrokAuth {
            team_blocked_reasons: vec!["BLOCKED_REASON_NO_LOGS".into()],
            organization_id: Some("org-1".into()),
            ..oidc_session("old-token", None)
        });
        let (auth, _) = run_external_auth_provider("printf '%s' fresh-token", &mgr, true, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token");
        assert!(auth.is_zdr_team(), "flags must carry from previous auth");
        assert_eq!(auth.user_id, "test-user");
        assert_eq!(auth.organization_id.as_deref(), Some("org-1"));
    }
    #[tokio::test]
    async fn external_provider_still_runs_without_browser() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            auth_provider_command: Some("printf '%s' xai-ext-token".to_string()),
            ..GrokComConfig::default()
        };
        let mgr = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url(&dead_proxy_url()),
        );
        let (auth, did_auth) = run_auth_flow_interactive(&mgr, &cfg, None, None, None)
            .await
            .expect("external provider should satisfy login without browser");
        assert_eq!(
            auth.key, "xai-ext-token",
            "external provider token must win"
        );
        assert!(did_auth);
    }
    fn legacy_auth() -> GrokAuth {
        GrokAuth {
            key: "k".into(),
            auth_mode: AuthMode::WebLogin,
            user_id: "u".into(),
            coding_data_retention_opt_out: false,
            ..GrokAuth::default()
        }
    }
    fn oidc_auth(issuer: &str) -> GrokAuth {
        GrokAuth {
            oidc_issuer: Some(issuer.into()),
            auth_mode: AuthMode::Oidc,
            ..legacy_auth()
        }
    }
    #[test]
    fn weblogin_cred_is_never_compatible() {
        let cfg = GrokComConfig::default();
        assert!(!is_cached_credential_compatible(&legacy_auth(), &cfg));
    }
    #[test]
    fn oidc_cred_with_matching_issuer_is_compatible() {
        let cfg = GrokComConfig::default();
        assert!(is_cached_credential_compatible(
            &oidc_auth(XAI_OAUTH2_ISSUER),
            &cfg,
        ));
    }
    #[test]
    fn external_cred_compatibility_follows_issuer() {
        let cfg = GrokComConfig::default();
        assert!(is_cached_credential_compatible(
            &GrokAuth {
                auth_mode: AuthMode::External,
                ..oidc_auth(XAI_OAUTH2_ISSUER)
            },
            &cfg,
        ));
        assert!(!is_cached_credential_compatible(
            &GrokAuth {
                auth_mode: AuthMode::External,
                oidc_issuer: None,
                ..legacy_auth()
            },
            &cfg,
        ));
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
    fn pinned_cfg(team: &str) -> GrokComConfig {
        GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::Single(team.into())),
            ..GrokComConfig::default()
        }
    }
    /// Under a team pin, a cached session for a different team is not reused by interactive login; it falls through to a fresh, compliant login.
    #[test]
    fn cached_cred_with_wrong_team_is_incompatible() {
        let auth = GrokAuth {
            key: team_jwt("team-wrong"),
            ..oidc_auth(XAI_OAUTH2_ISSUER)
        };
        assert!(!is_cached_credential_compatible(
            &auth,
            &pinned_cfg("team-good")
        ));
    }
    /// A cached session for the pinned team is reused normally.
    #[test]
    fn cached_cred_with_matching_team_is_compatible() {
        let auth = GrokAuth {
            key: team_jwt("team-good"),
            ..oidc_auth(XAI_OAUTH2_ISSUER)
        };
        assert!(is_cached_credential_compatible(
            &auth,
            &pinned_cfg("team-good")
        ));
    }
    /// When the in-memory token is expired but disk has a valid token, run_auth_flow should return the disk token without interactive login.
    #[tokio::test]
    async fn run_auth_flow_uses_valid_disk_token_when_expired() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let valid_disk = GrokAuth {
            key: "fresh-token-from-disk".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            refresh_token: Some("new-rt".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        writer.update(valid_disk).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let expired = GrokAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("old-rt".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(expired);
        assert!(mgr.is_expired());
        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None, None, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "fresh-token-from-disk");
        assert!(!is_new_login, "should not be a new login");
        assert_eq!(mgr.current().unwrap().key, "fresh-token-from-disk");
    }
    /// When the in-memory token is valid (not expired), run_auth_flow should return it directly without checking disk.
    #[tokio::test]
    async fn run_auth_flow_returns_cached_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        let valid = GrokAuth {
            key: "still-valid".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        mgr.hot_swap(valid);
        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None, None, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "still-valid");
        assert!(!is_new_login);
    }
    #[tokio::test]
    async fn run_auth_flow_defers_to_consumer_refresh_on_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig::default();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let expired_with_rt = GrokAuth {
            key: "expired-access-token".into(),
            auth_mode: AuthMode::Oidc,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: Some("valid-refresh-token".into()),
            oidc_issuer: Some(XAI_OAUTH2_ISSUER.into()),
            oidc_client_id: Some("client-1".into()),
            ..GrokAuth::test_default()
        };
        writer.update(expired_with_rt.clone()).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        mgr.hot_swap(expired_with_rt);
        assert!(mgr.is_expired());
        mgr.set_refresher(std::sync::Arc::new(AlwaysTransientRefresher));
        let (auth, is_new_login) = run_auth_flow(&mgr, &cfg, false, None, None, None)
            .await
            .unwrap();
        assert_eq!(auth.key, "expired-access-token");
        assert!(auth.refresh_token.is_some());
        assert!(!is_new_login);
    }
    #[tokio::test]
    async fn run_auth_flow_falls_through_when_no_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = GrokComConfig::default();
        cfg.oauth2.as_mut().unwrap().issuer = "http://127.0.0.1:1".into();
        let writer = Arc::new(
            AuthManager::new(dir.path(), cfg.clone()).with_proxy_base_url("http://127.0.0.1:1"),
        );
        let expired_no_rt = GrokAuth {
            key: "expired-legacy".into(),
            auth_mode: AuthMode::WebLogin,
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            refresh_token: None,
            ..GrokAuth::test_default()
        };
        writer.update(expired_no_rt.clone()).await.unwrap();
        let mgr = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        mgr.hot_swap(expired_no_rt);
        assert!(mgr.is_expired());
        mgr.set_refresher(std::sync::Arc::new(AlwaysTransientRefresher));
        let result = run_auth_flow(&mgr, &cfg, false, None, None, None).await;
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Configure a model provider"),
            "expected vendor guidance (browser login removed), got: {err}"
        );
    }
    #[test]
    fn extract_url_from_external_provider_stderr() {
        let extract = |input: &str| -> String {
            input
                .split_whitespace()
                .find(|w| w.starts_with("https://"))
                .map(|u| u.to_owned())
                .unwrap_or_else(|| input.to_owned())
        };
        assert_eq!(
            extract(
                "Visit the following link to sign into Grok: https://auth.example.com/login?code=abc"
            ),
            "https://auth.example.com/login?code=abc"
        );
        assert_eq!(
            extract("Please sign in below\nhttps://auth.example.com/sso"),
            "https://auth.example.com/sso"
        );
        assert_eq!(
            extract("https://auth.example.com/login"),
            "https://auth.example.com/login"
        );
        assert_eq!(extract("some opaque output"), "some opaque output");
    }
    /// CLI `grok login` passes `on_stderr=None`; stderr must be inherited so sign-in URLs appear in real time.
    /// Piped stderr with no reader deadlocks once the child writes past the pipe buffer (~64 KiB).
    #[tokio::test]
    async fn external_provider_cli_path_does_not_deadlock_on_large_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(
            AuthManager::new(dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy_url()),
        );
        let cmd = r#"sh -c 'i=0; while [ $i -lt 2000 ]; do printf "%s" "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" >&2; i=$((i+1)); done; printf token'"#;
        let (auth, _) = run_external_auth_provider(cmd, &mgr, false, None)
            .await
            .expect("CLI path must inherit stderr so large stderr does not deadlock");
        assert_eq!(auth.key, "token");
    }
    struct AlwaysTransientRefresher;
    #[async_trait::async_trait]
    impl crate::refresh::TokenRefresher for AlwaysTransientRefresher {
        async fn refresh(
            &self,
            _reason: crate::manager::RefreshReason,
        ) -> crate::refresh::RefreshOutcome {
            crate::refresh::RefreshOutcome::TransientFailure {
                message: "simulated network failure".into(),
            }
        }
    }
    /// Faithful reproduction of the cached-token bypass: the exact repro JWT (wrong team) cached in `auth.json` under a pin.
    /// The token runs through the same `AuthManager::new` and `auth()` engine that `try_ensure_fresh_auth` uses; it must be rejected and cleared.
    #[tokio::test]
    async fn noninteractive_auth_rejects_wrong_team_cached_token() {
        const REPRO_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJwcmluY2lwYWxfaWQiOiJ0ZWFtLXdyb25nIiwic3ViIjoidXNlci0xIn0.Signature";
        let dir = tempfile::tempdir().unwrap();
        let cfg = GrokComConfig {
            force_login_team_uuid: Some(crate::config::ForceLoginTeam::AnyOf(vec![
                "team-good".into(),
            ])),
            ..GrokComConfig::default()
        };
        let mut store = crate::model::AuthStore::new();
        store.insert(
            cfg.auth_scope(),
            GrokAuth {
                key: REPRO_JWT.into(),
                auth_mode: AuthMode::Oidc,
                team_id: Some("team-wrong".into()),
                expires_at: chrono::DateTime::from_timestamp(9_999_999_999, 0),
                ..GrokAuth::test_default()
            },
        );
        let auth_path = dir.path().join("auth.json");
        crate::storage::write_auth_json(&auth_path, &store).unwrap();
        let auth_manager = Arc::new(AuthManager::new(dir.path(), cfg.clone()));
        auth_manager.configure_refresher(cfg.auth_provider_command.clone());
        assert!(
            auth_manager.auth().await.is_err(),
            "non-interactive auth must reject the wrong-team cached token"
        );
        assert!(
            auth_manager.current().is_none(),
            "wrong-team token must not be usable via current()"
        );
        assert!(
            !auth_path.exists(),
            "wrong-team auth.json must be cleared, forcing a compliant re-login"
        );
    }
    /// Mock OIDC IdP whose `/token` endpoint never responds, so a refresh attempt hangs until the caller bounds it.
    async fn start_hanging_oidc_idp() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let b = base.clone();
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(move || {
                    let b = b.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "authorization_endpoint": format!("{b}/authorize"),
                            "token_endpoint": format!("{b}/token"),
                        }))
                    }
                }),
            )
            .route(
                "/token",
                axum::routing::post(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    axum::Json(serde_json::json!({}))
                }),
            );
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, handle)
    }
    fn expired_oidc_manager(dir: &Path, issuer: &str) -> Arc<AuthManager> {
        let cfg = GrokComConfig::default();
        let am = Arc::new(AuthManager::new(dir, cfg.clone()));
        am.configure_refresher(cfg.auth_provider_command.clone());
        am.hot_swap(GrokAuth {
            key: "expired".into(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(issuer.into()),
            oidc_client_id: Some("test-client".into()),
            refresh_token: Some("rt".into()),
            expires_at: Some(Utc::now() - chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        });
        am
    }
    /// The readiness-path `_no_mint` variant bounds the refresh (~5s) and never engages the cold-mint fallback.
    /// Leader readiness therefore can't block on a provider command up to the 60s `STARTUP_AUTH_TIMEOUT` cap.
    #[tokio::test]
    async fn no_mint_readiness_auth_is_bounded() {
        let (idp_base, server) = start_hanging_oidc_idp().await;
        let dir = tempfile::tempdir().unwrap();
        let am = expired_oidc_manager(dir.path(), &idp_base);
        let started = std::time::Instant::now();
        let result = try_noninteractive_auth_no_mint_with(&am).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= xai_grok_http::STARTUP_AUTH_REFRESH_TIMEOUT,
            "expected a bounded refresh attempt (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < xai_grok_http::STARTUP_AUTH_TIMEOUT,
            "no-mint readiness auth must not engage the 60s cold-mint cap (elapsed {elapsed:?}); readiness would block on a provider command"
        );
        assert!(
            result.is_none(),
            "a non-xAI expired session is no first-party fallback and no mint runs on this path, so no auth is produced"
        );
        server.abort();
    }
    const _: () = assert!(
        xai_grok_http::STARTUP_AUTH_REFRESH_TIMEOUT.as_millis()
            < crate::manager::REFRESH_LOCK_TIMEOUT.as_millis(),
        "the startup refresh bound must fire before the lock convoy budget"
    );
    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_auth_stays_bounded_when_auth_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let am = expired_oidc_manager(dir.path(), "http://127.0.0.1:1/");
        let auth_path = dir.path().join("auth.json");
        let lock_path = auth_path.with_file_name(crate::manager::lock::LOCK_FILE_NAME);
        let _held_lock = crate::manager::lock::test_support::hold_backdated_stale_lock(&lock_path);
        let holder_info = std::fs::read_to_string(&lock_path).unwrap();
        let started = std::time::Instant::now();
        let _ = try_noninteractive_auth_no_mint_with(&am).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= xai_grok_http::STARTUP_AUTH_REFRESH_TIMEOUT,
            "refresh must block on the held lock, not fast-return (elapsed {elapsed:?})"
        );
        assert!(
            elapsed < crate::manager::REFRESH_LOCK_TIMEOUT,
            "refresh must not fall through to the lock convoy (elapsed {elapsed:?})"
        );
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            holder_info,
            "the live stale lock must be left untouched, never broken"
        );
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .open(&lock_path)
            .unwrap();
        assert!(
            fs2::FileExt::try_lock_exclusive(&probe).is_err(),
            "the flock must still be held exclusively after the bounded refresh"
        );
    }
}
