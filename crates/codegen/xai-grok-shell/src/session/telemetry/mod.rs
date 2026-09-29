//! Session-level span helpers and tool-call classification for local observability.

mod compaction;
mod permission;
mod read_profile;
mod tool_call;

pub(crate) use compaction::CompactionTrigger;

pub(crate) use read_profile::model_origin;
#[cfg(not(feature = "test-support"))]
pub(crate) use tool_call::tool_execution_span;
pub(crate) use tool_call::{
    CanonicalToolId, InvocationId, InvocationSource, PathScope, PreparedToolFacts,
    ProductModelId, ToolCallProjection, ToolContractVersion, ToolExecutionInput, ToolOutputLimit,
    coarse_span_outcome, product_outcome, record_tool_execution, requested_model_snapshot,
    tool_identity,
};
#[cfg(feature = "test-support")]
pub use tool_call::tool_execution_span;
pub(crate) use permission::permission_mode_label;

use xai_grok_tools::implementations::skills::types::SkillScope;

/// Plugin id is `plugin_source`; SkillScope on a plugin skill is install location.
pub(crate) fn skill_source(scope: SkillScope, plugin_name: Option<&str>) -> &'static str {
    if plugin_name.is_some() {
        return "plugin";
    }
    match scope {
        SkillScope::Local => "local",
        SkillScope::Repo => "repo",
        SkillScope::User => "user",
        SkillScope::Server => "server",
        SkillScope::Bundled => "bundled",
        SkillScope::Plugin => "plugin",
    }
}

/// Canonicalizes both paths; one that cannot be canonicalized (synthetic paths like `chat-product://`) matches only when identical.
pub(crate) fn is_same_skill_file(
    skill_path: &std::path::Path,
    read_path: &std::path::Path,
) -> bool {
    if skill_path == read_path {
        return true;
    }
    match (
        dunce::canonicalize(skill_path),
        dunce::canonicalize(read_path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod is_same_skill_file_tests {
    use super::is_same_skill_file;
    use std::path::Path;

    #[test]
    fn matches_identical_paths() {
        assert!(is_same_skill_file(
            Path::new("/home/u/.grok/skills/review/SKILL.md"),
            Path::new("/home/u/.grok/skills/review/SKILL.md")
        ));
    }

    #[test]
    fn rejects_a_different_skill() {
        assert!(!is_same_skill_file(
            Path::new("/home/u/.grok/skills/review/SKILL.md"),
            Path::new("/home/u/.grok/skills/design/SKILL.md")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn matches_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let skill = real.join("SKILL.md");
        std::fs::write(&skill, "---\nname: x\n---\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(is_same_skill_file(&skill, &link.join("SKILL.md")));
    }

    #[test]
    fn synthetic_product_path_does_not_match_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        std::fs::write(&skill, "body").unwrap();

        assert!(!is_same_skill_file(
            Path::new("chat-product://commit"),
            &skill
        ));
    }
}

#[cfg(test)]
mod skill_source_tests {
    use super::skill_source;
    use xai_grok_tools::implementations::skills::types::SkillScope;

    #[test]
    fn plugin_name_overrides_install_location_scope() {
        assert_eq!("plugin", skill_source(SkillScope::User, Some("acme")));
        assert_eq!("bundled", skill_source(SkillScope::Bundled, None));
        assert_eq!("plugin", skill_source(SkillScope::Plugin, None));
    }
}
