//! Local compaction trigger classification for logs and control flow.

/// What triggered a compaction pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTrigger {
    Manual,
    Auto,
}

impl CompactionTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}
