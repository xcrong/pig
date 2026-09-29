use xai_grok_memory::{
    MemoryObservationSink, MemorySearchObservation, MemoryWatcherSyncObservation,
};

/// Local outcome of a memory injection attempt, for local memory logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryInjectionOutcome {
    Results,
    Empty,
    Error,
    Skipped,
}

/// Local failure class for memory-v2 model/maintenance errors, for local memory logs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum V2FailureClass {
    #[default]
    Disabled,
    Storage,
    Lease,
    Model,
    MalformedOutput,
    EmptyOutput,
    Timeout,
    Convergence,
    AccessPolicy,
}

/// Local per-call model usage for memory-v2 dream/capture accounting.
#[derive(Debug, Default, Clone)]
pub(crate) struct V2ModelUsage {
    pub model_id: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub reasoning_tokens: Option<u32>,
    pub cached_prompt_tokens: Option<u32>,
    pub cache_creation_tokens: Option<u32>,
    /// USD ticks (1e10 ticks = $1); `None` when unpriced.
    pub cost_usd_ticks: Option<i64>,
}

pub(crate) struct LoggingMemoryObservationSink {
    pub(crate) session_id: String,
}

#[derive(Default)]
pub(crate) struct MemoryInjectionMetrics {
    pub(crate) is_greeting_fallback: bool,
    pub(crate) result_count: usize,
    pub(crate) total_snippet_chars: usize,
    pub(crate) top_score: f64,
    pub(crate) configured_min_score: f64,
    pub(crate) duration_ms: u64,
    pub(crate) injected_bytes: u64,
    pub(crate) estimated_tokens: u64,
    pub(crate) global_entry_count: usize,
    pub(crate) workspace_entry_count: usize,
    pub(crate) was_reused: bool,
}

pub(crate) fn log_memory_injection(
    session_id: String,
    outcome: MemoryInjectionOutcome,
    metrics: MemoryInjectionMetrics,
) {
    tracing::debug!(
        target: crate::session::memory::MEMORY_LOG_TARGET,
        session_id = %session_id,
        outcome = ?outcome,
        result_count = metrics.result_count,
        total_snippet_chars = metrics.total_snippet_chars,
        top_score = metrics.top_score,
        configured_min_score = metrics.configured_min_score,
        injection_duration_ms = metrics.duration_ms,
        injected_bytes = metrics.injected_bytes,
        estimated_tokens = metrics.estimated_tokens,
        global_entry_count = metrics.global_entry_count,
        workspace_entry_count = metrics.workspace_entry_count,
        was_reused = metrics.was_reused,
        is_greeting_fallback = metrics.is_greeting_fallback,
        "MEMORY_INJECT: injection outcome",
    );
}

pub(crate) fn memory_v2_model_usage(
    model: &str,
    response: &xai_grok_sampling_types::ConversationResponse,
) -> V2ModelUsage {
    V2ModelUsage {
        model_id: Some(model.to_owned()),
        prompt_tokens: response.usage.as_ref().map(|usage| usage.prompt_tokens),
        completion_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.completion_tokens),
        reasoning_tokens: response.usage.as_ref().map(|usage| usage.reasoning_tokens),
        cached_prompt_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cached_prompt_tokens),
        cache_creation_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cache_creation_prompt_tokens),
        cost_usd_ticks: response.cost_usd_ticks,
    }
}

impl MemoryObservationSink for LoggingMemoryObservationSink {
    fn observe_search(&self, observation: MemorySearchObservation) {
        tracing::debug!(
            target: crate::session::memory::MEMORY_LOG_TARGET,
            session_id = %self.session_id,
            source = ?observation.source,
            mode = ?observation.mode,
            outcome = ?observation.outcome,
            query_length = observation.query_length,
            keyword_count = observation.keyword_count,
            result_count = observation.result_count,
            top_score = observation.top_score,
            min_score_threshold = observation.min_score_threshold,
            duration_ms = observation.duration_ms,
            vec_available = observation.is_vector_available,
            error_class = ?observation.error_class,
            "MEMORY_SEARCH: search observation",
        );
    }

    fn observe_watcher_sync(&self, observation: MemoryWatcherSyncObservation) {
        tracing::debug!(
            target: crate::session::memory::MEMORY_LOG_TARGET,
            session_id = %self.session_id,
            dirty_file_count = observation.dirty_file_count,
            claimed = observation.is_claimed,
            reindexed_count = observation.reindexed_count,
            embedded_count = observation.embedded_count,
            duration_ms = observation.duration_ms,
            "MEMORY_WATCHER: sync observation",
        );
    }
}
