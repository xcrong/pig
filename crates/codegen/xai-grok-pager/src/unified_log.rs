//! Local log sink for pager UI events.
//!
//! These entries were previously forwarded to the shell for off-machine
//! telemetry. They now go straight to local `tracing` under the `pager_ui`
//! target, which the TUI log pane already captures.

/// Log an info entry locally.
pub fn info(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    tracing::info!(target: "pager_ui", sid, ctx = ?ctx, "{msg}");
}

/// Log a warning locally.
pub fn warn(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    tracing::warn!(target: "pager_ui", sid, ctx = ?ctx, "{msg}");
}

/// Log an error locally.
pub fn error(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    tracing::error!(target: "pager_ui", sid, ctx = ?ctx, "{msg}");
}

/// Log a debug entry locally.
pub fn debug(msg: &str, sid: Option<&str>, ctx: Option<serde_json::Value>) {
    tracing::debug!(target: "pager_ui", sid, ctx = ?ctx, "{msg}");
}
