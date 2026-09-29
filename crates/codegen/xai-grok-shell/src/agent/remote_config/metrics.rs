//! Degraded-startup cause metrics.

use crate::managed_config::LaunchProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
pub(crate) enum DegradedStartCause {
    #[strum(serialize = "settings fetch failed")]
    FetchFailed,
    #[strum(serialize = "deadline missed")]
    DeadlineMissed,
}

pub(crate) fn record_degraded_start(
    cause: DegradedStartCause,
    profile: LaunchProfile,
    deadline: std::time::Duration,
    wait: std::time::Duration,
) {
    if matches!(profile, LaunchProfile::Managed) {
        tracing::warn!(
            cause = cause.as_ref(),
            deadline_ms = deadline.as_millis() as u64,
            wait_ms = wait.as_millis() as u64,
            outcome = "settings unavailable at gate time",
            "startup proceeding without remote settings",
        );
    } else {
        tracing::debug!(
            cause = cause.as_ref(),
            deadline_ms = deadline.as_millis() as u64,
            wait_ms = wait.as_millis() as u64,
            outcome = "settings unavailable at gate time",
            "startup proceeding without remote settings",
        );
    }
}

/// Local log level for degraded-startup records (managed boots warn, personal boots debug).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DegradedLogLevel {
    Warn,
    Debug,
}

pub(crate) fn degraded_log_level(profile: LaunchProfile) -> DegradedLogLevel {
    match profile {
        LaunchProfile::Managed => DegradedLogLevel::Warn,
        LaunchProfile::Personal => DegradedLogLevel::Debug,
    }
}
