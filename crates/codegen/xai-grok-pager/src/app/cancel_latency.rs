//! User-cancel latency measurement.
//!
//! The small data types plus the settle rule behind user-cancel latency tracking.
//! All consumers live in `agent_view` (anchor/settle) and `dispatch` (the cancel call sites); nothing in `agent.rs` uses them.
use std::time::Instant;
/// Which unit of work a user cancel targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CancellationScope {
    Turn,
    Compaction,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CancelOrigin {
    UserGesture,
    #[allow(dead_code)]
    Programmatic,
}
/// How a turn ended, which decides whether a pending user-cancel anchor is measured.
/// `Completed` means the turn reached its own terminal outcome (finished, or an honored cancel settled); the anchor is measured and emitted.
/// `Aborted` means the view was force-idled by reload/fork/session-failure; the anchor is discarded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TurnEnd {
    Completed,
    Aborted,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct CancelLatency {
    pub(crate) requested_at: Instant,
    pub(crate) scope: CancellationScope,
}
/// Settled user-cancel latency: how long the cancel took, and what it targeted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CancellationCompleted {
    pub(crate) latency_ms: u64,
    pub(crate) scope: CancellationScope,
}
impl CancelLatency {
    pub(crate) fn new(requested_at: Instant, scope: CancellationScope) -> Self {
        Self {
            requested_at,
            scope,
        }
    }
}
