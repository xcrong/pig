//! One snapshot feeds the local `tool.execution` span.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Host invocation id. Only a UUID is representable, so a provider call id or a path cannot be written here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationId(String);

impl InvocationId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }

    pub fn from_host(id: &str) -> Option<Self> {
        uuid::Uuid::parse_str(id).ok().map(|_| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Qualified registry id, or the one opaque class for unknown, custom, and dynamic names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalToolId(String);

impl CanonicalToolId {
    pub const OPAQUE: &'static str = "opaque";

    pub fn opaque() -> Self {
        Self(Self::OPAQUE.to_owned())
    }

    /// `Namespace:id` from a finalized registration. Rejects aliases, MCP names, and paths.
    pub fn from_qualified(id: &str) -> Option<Self> {
        let (namespace, tool) = id.split_once(':')?;
        if namespace.is_empty()
            || tool.is_empty()
            || namespace == "MCP"
            || tool.contains(':')
            || id.contains("__")
            || id.contains('/')
            || id.contains('\\')
            || id.contains(' ')
            || !namespace.chars().all(|c| c.is_ascii_alphanumeric())
            || !tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        Some(Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Managed behavior version. Anything else is omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolContractVersion(&'static str);

impl ToolContractVersion {
    pub const ALLOWED: &'static [&'static str] = &["current", "legacy-0.4.10"];

    pub fn from_registered(version: &str) -> Option<Self> {
        Self::ALLOWED
            .iter()
            .find(|known| **known == version)
            .map(|known| Self(known))
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// Requested model that already reads as a grok model id. Custom names stay absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductModelId(String);

impl ProductModelId {
    pub fn from_requested(model: &str) -> Option<Self> {
        if !is_approved_model_id(model) {
            return None;
        }
        Some(Self(model.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_approved_model_id(model: &str) -> bool {
    if model == "grok" {
        return true;
    }
    let Some(rest) = model.strip_prefix("grok-") else {
        return false;
    };
    !rest.is_empty()
        && rest.len() <= 64
        && !rest.starts_with('-')
        && !rest.ends_with('-')
        && !rest.contains("--")
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ToolSourceStatus {
    Unknown,
    Succeeded,
    Empty,
    Failed,
    Partial,
}

impl ToolSourceStatus {
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
pub enum ToolSourceReason {
    #[strum(serialize = "not_instrumented")]
    NotInstrumented,
    #[strum(serialize = "search.unclassified_exit")]
    SearchUnclassifiedExit,
    #[strum(serialize = "read.not_found")]
    ReadNotFound,
    #[strum(serialize = "read.directory")]
    ReadDirectory,
    #[strum(serialize = "read.denied")]
    ReadDenied,
    #[strum(serialize = "read.ignored")]
    ReadIgnored,
    #[strum(serialize = "read.binary")]
    ReadBinary,
    #[strum(serialize = "read.token_limit")]
    ReadTokenLimit,
    #[strum(serialize = "read.io")]
    ReadIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum InvocationSource {
    Model,
    UserDirect,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ToolOutputLimit {
    Unobserved,
    NotLimited,
    Limited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReadFileRole {
    SkillEntry,
    SkillSupport,
    Instruction,
    Memory,
    Ordinary,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReadSkillMatch {
    Registered,
    Unregistered,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReadSkillSource {
    Local,
    Repo,
    User,
    Server,
    Bundled,
    Plugin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReadSelection {
    Full,
    ModelWindow,
    DefaultWindow,
    SkillFullRead,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ReadLimitKind {
    None,
    Lines,
    Bytes,
    Tokens,
    Multiple,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CapApplicability {
    Applies,
    NotApplicable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum CapDisposition {
    Unobserved,
    WithinLimit,
    Truncated,
    Rejected,
    Exempt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadProfile {
    pub read_file_role: ReadFileRole,
    pub read_skill_match: ReadSkillMatch,
    pub read_skill_source: Option<ReadSkillSource>,
    pub read_selection: ReadSelection,
    pub read_source_bytes: Option<i64>,
    pub read_returned_lines: Option<i64>,
    pub read_returned_bytes: Option<i64>,
    pub read_limit_kind: ReadLimitKind,
    pub read_lines_applicability: CapApplicability,
    pub read_lines_limit: Option<i64>,
    pub read_lines_observed: Option<i64>,
    pub read_lines_disposition: CapDisposition,
    pub read_bytes_applicability: CapApplicability,
    pub read_bytes_limit: Option<i64>,
    pub read_bytes_observed: Option<i64>,
    pub read_bytes_disposition: CapDisposition,
    pub read_tokens_applicability: CapApplicability,
    pub read_tokens_limit: Option<i64>,
    pub read_tokens_observed: Option<i64>,
    pub read_tokens_disposition: CapDisposition,
}

/// Content-free location class. The path itself is never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PathScope {
    Workspace,
    Tmp,
    Home,
    Other,
}
use xai_grok_tools::implementations::codex::CodexReadFileTool;
use xai_grok_tools::implementations::grok_build::{ReadFileTool, SearchReplaceTool};
use xai_grok_tools::implementations::grok_build_concise::{
    ReadFileConciseTool, SearchReplaceConciseTool,
};
use xai_grok_tools::implementations::opencode::OpenCodeWriteTool;
use xai_grok_tools::registry::types::{FinalizedToolset, RegisteredToolIdentity};
use xai_grok_tools::types::output::{GrepSearchOutput, ToolOutput};
use xai_grok_tools::types::resources::resolve_model_path;
use xai_grok_tools::types::source_summary::ToolSourceSummary;
use xai_grok_tools::types::tool::ToolNamespace;
use xai_grok_tools::types::tool_call_origin::ToolCallOrigin;
use xai_tool_protocol::ToolId;
use xai_tool_runtime::Tool;

use crate::session::events::ToolOutcome;

const PATH_KEYS: &[&str] = &["file_path", "target_file", "filePath", "path"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceProjection {
    pub status: ToolSourceStatus,
    pub reason: Option<ToolSourceReason>,
}

/// Grep classification uses `exit_code` only. stdout/stderr are content, and
/// match counts are not incompleteness, so there is no typed partial signal.
pub(crate) fn source_projection(output: Option<&ToolOutput>) -> SourceProjection {
    match output {
        Some(ToolOutput::GrepSearch(grep)) => grep_source(grep),
        _ => SourceProjection {
            status: ToolSourceStatus::Unknown,
            reason: Some(ToolSourceReason::NotInstrumented),
        },
    }
}

fn grep_source(grep: &GrepSearchOutput) -> SourceProjection {
    match grep.exit_code {
        0 => SourceProjection {
            status: ToolSourceStatus::Succeeded,
            reason: None,
        },
        1 => SourceProjection {
            status: ToolSourceStatus::Empty,
            reason: None,
        },
        _ => SourceProjection {
            status: ToolSourceStatus::Failed,
            reason: Some(ToolSourceReason::SearchUnclassifiedExit),
        },
    }
}

pub(crate) fn coarse_span_outcome(legacy: &'static str, source: &SourceProjection) -> &'static str {
    if source.status.is_failure() && legacy == "success" {
        "error"
    } else {
        legacy
    }
}

pub(crate) fn product_outcome(legacy: ToolOutcome, source: &SourceProjection) -> ToolOutcome {
    if source.status.is_failure() && legacy == ToolOutcome::Success {
        ToolOutcome::Error
    } else {
        legacy
    }
}

pub(crate) struct PreparedToolFacts<'a> {
    pub requested_model: Option<&'a str>,
    pub invocation_id: &'a str,
    pub tool_id: &'a str,
    pub tool_version: Option<&'a str>,
    pub args: &'a serde_json::Value,
}

pub(crate) struct ToolExecutionInput<'a> {
    pub prepared: PreparedToolFacts<'a>,
    pub output: Option<&'a ToolOutput>,
    pub legacy_outcome: &'static str,
    pub legacy_success: bool,
    pub result_size: i64,
    pub cwd: &'a Path,
    pub display_cwd: Option<&'a Path>,
    pub summary: Option<&'a ToolSourceSummary>,
    pub origin: Option<&'a ToolCallOrigin>,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolCallProjection {
    pub requested_model: Option<String>,
    pub model_id: Option<ProductModelId>,
    pub invocation_id: InvocationId,
    pub tool_id: CanonicalToolId,
    pub tool_version: Option<ToolContractVersion>,
    pub source: SourceProjection,
    pub path_scope: Option<PathScope>,
    pub invocation_source: Option<InvocationSource>,
    pub output_limit: Option<ToolOutputLimit>,
    pub read: Option<ReadProfile>,
}

/// Outcome fields are declared `Empty` because `record` on an undeclared field is dropped.
fn declare_tool_execution_span(
    parent: &tracing::Span,
    session_id: &str,
    tool_name: &str,
    hook_tool_name: &str,
    tool_call_id: &str,
    raw_arguments_len: usize,
    retry: bool,
) -> tracing::Span {
    let mcp_server =
        crate::session::mcp_servers::parse_mcp_tool_name(hook_tool_name).map(|(server, _)| server);
    let span = tracing::info_span!(
        parent: parent,
        "tool.execution",
        session_id = %session_id,
        tool_name = %tool_name,
        model_id = tracing::field::Empty,
        invocation_id = tracing::field::Empty,
        tool_id = tracing::field::Empty,
        tool_version = tracing::field::Empty,
        source_status = tracing::field::Empty,
        source_reason = tracing::field::Empty,
        invocation_source = tracing::field::Empty,
        output_limit = tracing::field::Empty,
        read_file_role = tracing::field::Empty,
        read_skill_match = tracing::field::Empty,
        read_skill_source = tracing::field::Empty,
        read_selection = tracing::field::Empty,
        read_limit_kind = tracing::field::Empty,
        read_lines_applicability = tracing::field::Empty,
        read_lines_limit = tracing::field::Empty,
        read_lines_observed = tracing::field::Empty,
        read_lines_disposition = tracing::field::Empty,
        read_bytes_applicability = tracing::field::Empty,
        read_bytes_limit = tracing::field::Empty,
        read_bytes_observed = tracing::field::Empty,
        read_bytes_disposition = tracing::field::Empty,
        read_tokens_applicability = tracing::field::Empty,
        read_tokens_limit = tracing::field::Empty,
        read_tokens_observed = tracing::field::Empty,
        read_tokens_disposition = tracing::field::Empty,
        read_source_bytes = tracing::field::Empty,
        read_returned_lines = tracing::field::Empty,
        read_returned_bytes = tracing::field::Empty,
        // Same value under both names: `tool_call_id` is the join key, `tool_use_id` is kept for existing queries
        tool_use_id = %tool_call_id,
        tool_call_id = %tool_call_id,
        retry,
        server_name = tracing::field::Empty,
        success = tracing::field::Empty,
        outcome = tracing::field::Empty,
        tool_input_size_bytes = raw_arguments_len as i64,
        tool_result_size_bytes = tracing::field::Empty,
    );
    if let Some(id) = mcp_server.as_deref() {
        span.record("server_name", id);
    }
    span
}

#[cfg(feature = "test-support")]
pub fn tool_execution_span(
    parent: &tracing::Span,
    session_id: &str,
    tool_name: &str,
    hook_tool_name: &str,
    tool_call_id: &str,
    raw_arguments_len: usize,
    retry: bool,
) -> tracing::Span {
    declare_tool_execution_span(
        parent,
        session_id,
        tool_name,
        hook_tool_name,
        tool_call_id,
        raw_arguments_len,
        retry,
    )
}

#[cfg(not(feature = "test-support"))]
pub(crate) fn tool_execution_span(
    parent: &tracing::Span,
    session_id: &str,
    tool_name: &str,
    hook_tool_name: &str,
    tool_call_id: &str,
    raw_arguments_len: usize,
    retry: bool,
) -> tracing::Span {
    declare_tool_execution_span(
        parent,
        session_id,
        tool_name,
        hook_tool_name,
        tool_call_id,
        raw_arguments_len,
        retry,
    )
}

/// `path_scope` stays off the span: it is a product enum, not a new span key.
pub(crate) fn record_tool_execution(
    span: tracing::Span,
    input: ToolExecutionInput<'_>,
) -> (ToolCallProjection, bool) {
    let projection = project_tool_call(
        input.prepared,
        input.output,
        input.cwd,
        input.display_cwd,
        input.summary,
        input.origin,
    );
    let success = input.legacy_success && !projection.source.status.is_failure();
    span.record("success", success);
    span.record(
        "outcome",
        coarse_span_outcome(input.legacy_outcome, &projection.source),
    );
    span.record("tool_result_size_bytes", input.result_size);
    // Approved grok ids only, matching product serialization. Unknown stays absent.
    if let Some(model) = projection.model_id.as_ref() {
        span.record("model_id", model.as_str());
    }
    span.record("invocation_id", projection.invocation_id.as_str());
    span.record("tool_id", projection.tool_id.as_str());
    if let Some(version) = projection.tool_version {
        span.record("tool_version", version.as_str());
    }
    span.record("source_status", projection.source.status.as_ref());
    if let Some(reason) = projection.source.reason {
        span.record("source_reason", reason.as_ref());
    }
    super::read_profile::record_read_fields(
        &span,
        projection.invocation_source,
        projection.output_limit,
        projection.read.as_ref(),
    );
    (projection, success)
}

fn project_tool_call(
    prepared: PreparedToolFacts<'_>,
    output: Option<&ToolOutput>,
    cwd: &Path,
    display_cwd: Option<&Path>,
    summary: Option<&ToolSourceSummary>,
    origin: Option<&ToolCallOrigin>,
) -> ToolCallProjection {
    let tool_id =
        CanonicalToolId::from_qualified(prepared.tool_id).unwrap_or_else(CanonicalToolId::opaque);
    let path_scope = path_scope(&tool_id, prepared.args, cwd, display_cwd);
    let (read_source, output_limit, read) = super::read_profile::read_projection(&tool_id, summary);
    ToolCallProjection {
        requested_model: prepared.requested_model.map(str::to_owned),
        model_id: prepared
            .requested_model
            .and_then(ProductModelId::from_requested),
        invocation_id: InvocationId::from_host(prepared.invocation_id)
            .unwrap_or_else(InvocationId::generate),
        tool_id,
        tool_version: prepared
            .tool_version
            .and_then(ToolContractVersion::from_registered),
        source: read_source.unwrap_or_else(|| source_projection(output)),
        path_scope,
        invocation_source: super::read_profile::invocation_source(origin),
        output_limit,
        read,
    }
}

pub(crate) fn tool_identity(
    toolset: &FinalizedToolset,
    client_name: &str,
) -> (String, Option<String>) {
    match toolset.registered_identity(client_name) {
        Some(RegisteredToolIdentity { tool_id, version }) => {
            let id =
                CanonicalToolId::from_qualified(&tool_id).unwrap_or_else(CanonicalToolId::opaque);
            let version = version
                .as_deref()
                .and_then(ToolContractVersion::from_registered)
                .map(|version| version.as_str().to_owned());
            (id.as_str().to_owned(), version)
        }
        None => (CanonicalToolId::OPAQUE.to_owned(), None),
    }
}

fn qualified_id(namespace: ToolNamespace, id: &ToolId) -> String {
    format!("{namespace}:{id}")
}

fn path_scope_ids() -> &'static [String] {
    static IDS: OnceLock<Vec<String>> = OnceLock::new();
    IDS.get_or_init(|| {
        let read_file = ReadFileTool;
        let concise_read = ReadFileConciseTool;
        let codex_read = CodexReadFileTool;
        let search_replace = SearchReplaceTool;
        let concise = SearchReplaceConciseTool;
        let write = OpenCodeWriteTool;
        let mut ids = vec![
            qualified_id(ToolNamespace::GrokBuild, &read_file.id()),
            qualified_id(ToolNamespace::GrokBuildConcise, &concise_read.id()),
            qualified_id(ToolNamespace::Codex, &codex_read.id()),
            qualified_id(ToolNamespace::GrokBuild, &search_replace.id()),
            qualified_id(ToolNamespace::GrokBuildConcise, &concise.id()),
            qualified_id(ToolNamespace::OpenCode, &write.id()),
        ];
        ids.extend(xai_grok_tools::implementations::extra_write_qualified_ids());
        ids
    })
}

fn is_path_scope_tool(tool_id: &CanonicalToolId) -> bool {
    let id = tool_id.as_str();
    path_scope_ids().iter().any(|known| known == id)
}

pub(crate) fn path_scope(
    tool_id: &CanonicalToolId,
    args: &serde_json::Value,
    cwd: &Path,
    display_cwd: Option<&Path>,
) -> Option<PathScope> {
    if !is_path_scope_tool(tool_id) {
        return None;
    }
    let raw = PATH_KEYS.iter().find_map(|key| {
        args.get(*key)
            .and_then(|value| value.as_str())
            .filter(|path| !path.trim().is_empty())
    })?;
    let resolved = resolve_model_path(cwd, display_cwd, raw);
    Some(classify_path(&resolved, cwd))
}

fn classify_path(resolved: &Path, cwd: &Path) -> PathScope {
    let resolved = lexical_normalize(resolved);
    let cwd = lexical_normalize(cwd);
    if is_within(&cwd, &resolved) {
        return PathScope::Workspace;
    }
    if is_tmp(&resolved) {
        return PathScope::Tmp;
    }
    if let Some(home) = resolver_home()
        && is_within(&lexical_normalize(&home), &resolved)
    {
        return PathScope::Home;
    }
    PathScope::Other
}

fn resolver_home() -> Option<PathBuf> {
    let home = resolve_model_path(Path::new("/"), None, "~");
    (home != Path::new("/~")).then_some(home)
}

fn is_tmp(path: &Path) -> bool {
    is_within(&lexical_normalize(Path::new("/tmp")), path)
        || is_within(&lexical_normalize(Path::new("/private/tmp")), path)
        || is_within(&lexical_normalize(&std::env::temp_dir()), path)
}

fn is_within(root: &Path, path: &Path) -> bool {
    let mut root = root.components();
    let mut path = path.components();
    loop {
        match (root.next(), path.next()) {
            (None, _) => return true,
            (Some(left), Some(right)) if left == right => {}
            _ => return false,
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn requested_model_snapshot(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::qualified_id;
    use super::*;
    use crate::session::events::ToolOutcome;
    use std::path::Path;
    use xai_grok_tools::implementations::codex::CodexReadFileTool;
    use xai_grok_tools::implementations::grok_build::{GrepTool, ReadFileTool, SearchReplaceTool};
    use xai_grok_tools::implementations::grok_build_concise::{
        ReadFileConciseTool, SearchReplaceConciseTool,
    };
    use xai_grok_tools::implementations::opencode::OpenCodeWriteTool;
    use xai_grok_tools::types::output::{GrepSearchOutput, TextOutput};
    use xai_grok_tools::types::resources::resolve_model_path;
    use xai_grok_tools::types::tool::ToolNamespace;
    use xai_tool_runtime::Tool;

    fn registered(namespace: ToolNamespace, tool: &impl Tool) -> CanonicalToolId {
        CanonicalToolId::from_qualified(&qualified_id(namespace, &tool.id())).expect("qualified id")
    }

    fn projected(
        requested_model: Option<&str>,
        tool_id: &str,
        tool_version: Option<&str>,
        args: &serde_json::Value,
        output: Option<&ToolOutput>,
        cwd: &Path,
        legacy_success: bool,
    ) -> (ToolCallProjection, bool) {
        record_tool_execution(
            tracing::Span::none(),
            ToolExecutionInput {
                prepared: PreparedToolFacts {
                    requested_model,
                    invocation_id: "018f6b6c-7b3a-7c3a-8c3a-000000000001",
                    tool_id,
                    tool_version,
                    args,
                },
                output,
                legacy_outcome: "success",
                legacy_success,
                result_size: 8,
                cwd,
                display_cwd: None,
                summary: None,
                origin: None,
            },
        )
    }

    fn grep_result(exit_code: i32) -> ToolOutput {
        ToolOutput::GrepSearch(GrepSearchOutput {
            stdout: b"CANARY_STDOUT /tmp/secret-project/main.rs".to_vec(),
            stderr: b"CANARY_STDERR".to_vec(),
            exit_code,
            match_count: 4,
            file_matches: Vec::new(),
        })
    }

    #[test]
    fn grep_projection_uses_exit_code_and_does_not_invent_span_reasons() {
        let cases = [
            (0, ToolSourceStatus::Succeeded, None),
            (1, ToolSourceStatus::Empty, None),
            (
                2,
                ToolSourceStatus::Failed,
                Some(ToolSourceReason::SearchUnclassifiedExit),
            ),
            (
                -1,
                ToolSourceStatus::Failed,
                Some(ToolSourceReason::SearchUnclassifiedExit),
            ),
        ];
        assert_eq!(
            ToolSourceReason::SearchUnclassifiedExit.as_ref(),
            "search.unclassified_exit"
        );
        assert_eq!(ToolSourceStatus::Failed.as_ref(), "failed");
        for (exit_code, status, reason) in cases {
            let projected = source_projection(Some(&grep_result(exit_code)));
            assert_eq!(projected.status, status);
            assert_eq!(projected.reason, reason);
            assert_ne!(
                projected
                    .reason
                    .map(|reason| -> &'static str { reason.into() }),
                Some("timeout")
            );
            assert_ne!(
                projected
                    .reason
                    .map(|reason| -> &'static str { reason.into() }),
                Some("early_stop")
            );
            assert_ne!(
                projected
                    .reason
                    .map(|reason| -> &'static str { reason.into() }),
                Some("spawn_failure")
            );
        }
        assert!(!grep_result(-1).is_error());
        assert!(grep_result(2).is_error());
        let failed = source_projection(Some(&grep_result(-1)));
        assert_eq!(coarse_span_outcome("success", &failed), "error");
        assert_eq!(
            product_outcome(ToolOutcome::Success, &failed),
            ToolOutcome::Error
        );
        assert_eq!(
            product_outcome(ToolOutcome::PermissionRejected, &failed),
            ToolOutcome::PermissionRejected
        );
        let empty = source_projection(Some(&grep_result(1)));
        assert_eq!(coarse_span_outcome("success", &empty), "success");
        let other = source_projection(Some(&ToolOutput::Text(TextOutput::from(
            "CANARY_BODY".to_owned(),
        ))));
        assert_eq!(other.status, ToolSourceStatus::Unknown);
        assert_eq!(other.reason, Some(ToolSourceReason::NotInstrumented));
        let cwd = Path::new("/opt/repo");
        let (failed_projection, failed_success) = projected(
            Some("grok-4.6"),
            "GrokBuild:grep",
            Some("current"),
            &serde_json::json!({}),
            Some(&grep_result(-1)),
            cwd,
            true,
        );
        assert!(!failed_success);
        assert_eq!(failed_projection.source.status, ToolSourceStatus::Failed);
        assert_eq!(
            failed_projection.source.reason,
            Some(ToolSourceReason::SearchUnclassifiedExit)
        );
        assert_eq!(
            product_outcome(ToolOutcome::Success, &failed_projection.source),
            ToolOutcome::Error
        );
        let (empty_projection, empty_success) = projected(
            Some("grok-4.6"),
            "GrokBuild:grep",
            Some("current"),
            &serde_json::json!({}),
            Some(&grep_result(1)),
            cwd,
            true,
        );
        assert!(empty_success);
        assert_eq!(empty_projection.source.status, ToolSourceStatus::Empty);
        assert_eq!(
            product_outcome(ToolOutcome::Success, &empty_projection.source),
            ToolOutcome::Success
        );
    }

    #[test]
    fn path_scope_follows_the_registered_id_not_the_client_name() {
        let cwd = Path::new("/opt/repo");
        let read_file = registered(ToolNamespace::GrokBuild, &ReadFileTool);
        let concise_read = registered(ToolNamespace::GrokBuildConcise, &ReadFileConciseTool);
        let codex_read = registered(ToolNamespace::Codex, &CodexReadFileTool);
        let search_replace = registered(ToolNamespace::GrokBuild, &SearchReplaceTool);
        let concise = registered(ToolNamespace::GrokBuildConcise, &SearchReplaceConciseTool);
        let write = registered(ToolNamespace::OpenCode, &OpenCodeWriteTool);
        let grep = registered(ToolNamespace::GrokBuild, &GrepTool);
        let opaque = CanonicalToolId::opaque();
        let cases = [
            (&read_file, "/tmp/pr.md", Some(PathScope::Tmp)),
            (&concise_read, "/tmp/pr.md", Some(PathScope::Tmp)),
            (&codex_read, "/tmp/pr.md", Some(PathScope::Tmp)),
            (&search_replace, "/private/tmp/pr.md", Some(PathScope::Tmp)),
            (&concise, "/tmp/pr.md", Some(PathScope::Tmp)),
            (&write, "/tmp-other/x", Some(PathScope::Other)),
            (&read_file, "src/a.rs", Some(PathScope::Workspace)),
            (&write, "../outside.rs", Some(PathScope::Other)),
            (&grep, "/tmp/pr.md", None),
            (&read_file, "   ", None),
            (&opaque, "/tmp/pr.md", None),
        ];
        for (tool_id, path, expected) in cases {
            assert_eq!(
                path_scope(tool_id, &serde_json::json!({"path": path}), cwd, None),
                expected,
                "{} {path}",
                tool_id.as_str()
            );
        }
        assert_eq!(PathScope::Tmp.as_ref(), "tmp");
        assert_eq!(PathScope::Workspace.as_ref(), "workspace");
        assert_eq!(PathScope::Home.as_ref(), "home");
        assert_eq!(PathScope::Other.as_ref(), "other");
        for id in xai_grok_tools::implementations::extra_write_qualified_ids() {
            let write_id = CanonicalToolId::from_qualified(&id).expect("compiled write id");
            assert_eq!(
                path_scope(
                    &write_id,
                    &serde_json::json!({"file_path": "/tmp/pr.md"}),
                    cwd,
                    None
                ),
                Some(PathScope::Tmp)
            );
        }
        let temp_child = std::env::temp_dir().join("pr.md");
        assert_eq!(
            path_scope(
                &write,
                &serde_json::json!({"file_path": temp_child.to_string_lossy()}),
                cwd,
                None
            ),
            Some(PathScope::Tmp)
        );
        assert_eq!(
            path_scope(
                &read_file,
                &serde_json::json!({"target_file": "a.rs"}),
                &std::env::temp_dir(),
                None
            ),
            Some(PathScope::Workspace)
        );
        let home = resolve_model_path(Path::new("/"), None, "~");
        assert_ne!(home, Path::new("/~"));
        let home_file = home.join("notes.md");
        assert_eq!(
            path_scope(
                &write,
                &serde_json::json!({"file_path": home_file.to_string_lossy()}),
                cwd,
                None
            ),
            Some(PathScope::Home)
        );
        let read_id = read_file.as_str();
        let args = serde_json::json!({"target_file": "src/a.rs"});
        let (projection, _) = projected(
            Some("grok-4.6"),
            read_id,
            Some("current"),
            &args,
            None,
            cwd,
            true,
        );
        assert_eq!(projection.path_scope, Some(PathScope::Workspace));
        assert_eq!(
            projection.model_id.as_ref().map(ProductModelId::as_str),
            Some("grok-4.6")
        );
        assert_eq!(projection.tool_id.as_str(), read_id);
        assert_eq!(
            projection.invocation_id.as_str(),
            "018f6b6c-7b3a-7c3a-8c3a-000000000001"
        );
        assert_eq!(
            projection.tool_version.map(ToolContractVersion::as_str),
            Some("current")
        );
        assert_eq!(projection.requested_model.as_deref(), Some("grok-4.6"));
        let (renamed, _) = projected(
            Some("grok-4.6"),
            read_id,
            Some("current"),
            &serde_json::json!({"target_file": "/tmp/pr.md"}),
            None,
            cwd,
            true,
        );
        assert_eq!(renamed.path_scope, Some(PathScope::Tmp));
        assert_eq!(renamed.tool_id.as_str(), "GrokBuild:read_file");
        let (opaque_projection, _) = projected(
            Some("grok-4.6"),
            "opaque",
            None,
            &serde_json::json!({"target_file": "/tmp/pr.md"}),
            None,
            cwd,
            true,
        );
        assert_eq!(opaque_projection.path_scope, None);
        assert_eq!(opaque_projection.tool_id.as_str(), CanonicalToolId::OPAQUE);
        let (custom_projection, _) = projected(
            Some("/tmp/secret-project"),
            "search_code",
            Some("nope"),
            &serde_json::json!({}),
            None,
            cwd,
            true,
        );
        assert_eq!(custom_projection.model_id, None);
        assert_eq!(
            custom_projection.tool_id.as_str(),
            CanonicalToolId::OPAQUE
        );
        assert_eq!(custom_projection.tool_version, None);
        assert_eq!(
            custom_projection.requested_model.as_deref(),
            Some("/tmp/secret-project")
        );
    }
}
