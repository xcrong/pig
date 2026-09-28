//! Session lifecycle event structs.
//!
//! Fires in both `Enabled` and `SessionMetrics` telemetry modes via `log_session_event`.

use serde::Serialize;

/// The ACP method the client called.
/// It stays separate from the warm/cold mechanism (`SessionStarted::restored_from_disk`) so intent and mechanism can be queried independently.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartKind {
    New,
    Load,
    Resume,
}

#[derive(Serialize)]
pub struct SessionStarted {
    pub session_id: String,
    pub kind: SessionStartKind,
    pub setup_duration_ms: u64,
    /// Whether setup rebuilt the session from disk (cold) rather than reconnecting to a resident actor (warm). Mirrors `SessionLoad`.
    pub restored_from_disk: bool,
}

impl SessionStarted {
    pub fn new(
        session_id: String,
        kind: SessionStartKind,
        setup_duration: std::time::Duration,
        restored_from_disk: bool,
    ) -> Self {
        Self {
            session_id,
            kind,
            setup_duration_ms: setup_duration.as_millis() as u64,
            restored_from_disk,
        }
    }
}

/// Itemized context occupancy once session setup (including MCP init) has finished.
/// Category token fields are counted with the model's tokenizer via `POST /v1/tokenize-text`.
/// `used_tokens` / `message_tokens` stay the chat-state occupancy already shown in `/context`.
#[derive(Serialize)]
pub struct SessionContextSnapshot {
    pub session_id: String,
    pub model_id: String,
    pub context_window: u64,
    pub used_tokens: u64,
    pub usage_pct: u8,
    pub free_tokens: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_tokens: u64,
    pub tool_definitions_count: u64,
    pub message_tokens: u64,
    pub skills_tokens: u64,
    pub skills_count: u64,
    pub mcp_tokens: u64,
    pub mcp_server_count: u64,
    pub agents_md_tokens: u64,
    pub agents_md_file_count: u64,
    pub workflows_tokens: u64,
    pub workflows_count: u64,
}

#[derive(Serialize)]
pub struct Turn {
    pub session_id: String,
    pub turn_number: u64,
}

#[derive(Serialize)]
pub struct TurnCompletedLifecycle {
    pub session_id: String,
    pub turn_number: u64,
}

/// Server-side doom-loop detection observed this turn. Aggregated detector metadata only, never generation content or token IDs.
#[derive(Serialize)]
pub struct DoomLoopDetected {
    pub session_id: String,
    pub turn_number: u64,
    pub trigger_count: u32,
    pub detector_kinds: Vec<String>,
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tightest_tail_threshold: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exact_sequence_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_exact_repeat_count: Option<u32>,
    pub recovery_attempts: u32,
    pub model: String,
}

/// Doom-loop recovery acted this turn.
/// Poisoned attempts were resampled and/or a response was accepted with confident signals after the budget was spent.
/// Trigger labels only, never generation content.
#[derive(Serialize)]
pub struct DoomLoopRecovery {
    pub session_id: String,
    pub turn_number: u64,
    /// Resamples this turn (doomed attempts discarded).
    pub attempts: u32,
    /// Whether the final response kept confident signals (budget spent).
    pub accepted_after_budget: bool,
    /// Tightest raw trigger label observed this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_trigger: Option<String>,
    /// Model that produced the doomed attempts.
    pub model: String,
}

/// Emitted whether or not the reminder is enabled so cohorts compare on the same properties. Counts come from `response.usage`, never content.
#[derive(Serialize)]
pub struct LongReasoningReminderTurn {
    pub session_id: String,
    pub turn_number: u64,
    pub enabled: bool,
    /// Reasoning tokens in one call that count as long.
    pub threshold_tokens: u32,
    pub delay: u32,
    pub model_calls: u32,
    /// Sum of hidden reasoning tokens across the turn's model calls.
    pub reasoning_tokens: u64,
    /// Sum of output (completion) tokens across the turn's model calls.
    pub completion_tokens: u64,
    pub max_call_reasoning_tokens: u32,
    /// Model calls whose reasoning exceeded `threshold_tokens`.
    pub long_calls: u32,
    pub reminders_fired: u32,
    pub model: String,
}

#[cfg(test)]
mod tests {

    #[test]
    fn doom_loop_detected_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(super::DoomLoopDetected::NAME, "doom_loop_detected");
        let event = serde_json::to_value(super::DoomLoopDetected {
            session_id: "s1".to_string(),
            turn_number: 7,
            trigger_count: 3,
            detector_kinds: vec![
                "tail_repetition".to_string(),
                "exact_repetition".to_string(),
            ],
            channels: vec!["thinking".to_string(), "response".to_string()],
            tightest_tail_threshold: Some(32),
            max_exact_sequence_tokens: Some(42),
            max_exact_repeat_count: Some(3),
            recovery_attempts: 1,
            model: "grok-4.6".to_string(),
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 7,
                "trigger_count": 3,
                "detector_kinds": ["tail_repetition", "exact_repetition"],
                "channels": ["thinking", "response"],
                "tightest_tail_threshold": 32,
                "max_exact_sequence_tokens": 42,
                "max_exact_repeat_count": 3,
                "recovery_attempts": 1,
                "model": "grok-4.6",
            })
        );
    }

    /// `session_context_snapshot` property keys are an external-stream contract; pin the shape so a rename cannot silently break queries.
    #[test]
    fn session_context_snapshot_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(
            super::SessionContextSnapshot::NAME,
            "session_context_snapshot"
        );
        let value = serde_json::to_value(super::SessionContextSnapshot {
            session_id: "s1".to_string(),
            model_id: "grok-4".to_string(),
            context_window: 1_000_000,
            used_tokens: 40_000,
            usage_pct: 4,
            free_tokens: 960_000,
            system_prompt_tokens: 8_000,
            tool_definitions_tokens: 5_000,
            tool_definitions_count: 12,
            message_tokens: 2_000,
            skills_tokens: 27_000,
            skills_count: 282,
            mcp_tokens: 1_200,
            mcp_server_count: 4,
            agents_md_tokens: 3_400,
            agents_md_file_count: 2,
            workflows_tokens: 800,
            workflows_count: 3,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "session_id": "s1",
                "model_id": "grok-4",
                "context_window": 1_000_000,
                "used_tokens": 40_000,
                "usage_pct": 4,
                "free_tokens": 960_000,
                "system_prompt_tokens": 8_000,
                "tool_definitions_tokens": 5_000,
                "tool_definitions_count": 12,
                "message_tokens": 2_000,
                "skills_tokens": 27_000,
                "skills_count": 282,
                "mcp_tokens": 1_200,
                "mcp_server_count": 4,
                "agents_md_tokens": 3_400,
                "agents_md_file_count": 2,
                "workflows_tokens": 800,
                "workflows_count": 3,
            })
        );
    }

    /// The `grok-shell-doom_loop_recovery` event's name and property keys are downstream contracts; pin them.
    #[test]
    fn doom_loop_recovery_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(super::DoomLoopRecovery::NAME, "doom_loop_recovery");
        let with_trigger = serde_json::to_value(super::DoomLoopRecovery {
            session_id: "s1".to_string(),
            turn_number: 7,
            attempts: 2,
            accepted_after_budget: true,
            top_trigger: Some("tail_repetition:4@thinking".to_string()),
            model: "grok-4.5".to_string(),
        })
        .unwrap();
        assert_eq!(
            with_trigger,
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 7,
                "attempts": 2,
                "accepted_after_budget": true,
                "top_trigger": "tail_repetition:4@thinking",
                "model": "grok-4.5",
            })
        );
        let no_trigger = serde_json::to_value(super::DoomLoopRecovery {
            session_id: "s1".to_string(),
            turn_number: 7,
            attempts: 1,
            accepted_after_budget: false,
            top_trigger: None,
            model: "grok-4.5".to_string(),
        })
        .unwrap();
        assert!(no_trigger.get("top_trigger").is_none(), "None is omitted");
    }

    /// The `grok-shell-long_reasoning_reminder` event's name and property keys are downstream contracts; pin them.
    #[test]
    fn long_reasoning_reminder_event_shape_is_stable() {
        use crate::events::TelemetryEvent;
        assert_eq!(
            super::LongReasoningReminderTurn::NAME,
            "long_reasoning_reminder"
        );
        let event = serde_json::to_value(super::LongReasoningReminderTurn {
            session_id: "s1".to_string(),
            turn_number: 7,
            enabled: true,
            threshold_tokens: 1000,
            delay: 1,
            model_calls: 3,
            reasoning_tokens: 6500,
            completion_tokens: 900,
            max_call_reasoning_tokens: 5000,
            long_calls: 1,
            reminders_fired: 1,
            model: "grok-4.7-build".to_string(),
        })
        .unwrap();
        assert_eq!(
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 7,
                "enabled": true,
                "threshold_tokens": 1000,
                "delay": 1,
                "model_calls": 3,
                "reasoning_tokens": 6500,
                "completion_tokens": 900,
                "max_call_reasoning_tokens": 5000,
                "long_calls": 1,
                "reminders_fired": 1,
                "model": "grok-4.7-build",
            }),
            event
        );
    }
}
