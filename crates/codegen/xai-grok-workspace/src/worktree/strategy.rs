//! Build the strategy report from a dispatch outcome plus the request gate.

use xai_fast_worktree::{ArmSkip, WorktreeReport};
use xai_grok_workspace_types::rpc::worktree::{
    StrategyReport, WorktreeType, is_grove_resolved, transport_for_resolved,
};

pub(super) fn report_from_worktree(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    requested_type: WorktreeType,
    rewrite_reason: Option<&str>,
    report: &WorktreeReport,
) -> StrategyReport {
    let requested = requested_strategy(grove_enabled, grove_gate_source, requested_type);
    let resolved = report.resolved_strategy;
    let grove_transport = report
        .strategy_metadata
        .as_ref()
        .and_then(|m| m.get("grove"))
        .and_then(|g| g.get("transport"))
        .and_then(|v| v.as_str());
    StrategyReport {
        requested_strategy: Some(requested.into()),
        resolved_strategy: Some(resolved.into()),
        transport: transport_for_resolved(resolved, grove_transport).map(str::to_owned),
        // The adopt reply carries no object source; only the clone path knows it.
        source_mode: None,
        fallback_reason: fallback_reason(
            grove_enabled,
            grove_gate_source,
            resolved,
            rewrite_reason,
            &report.skipped,
        ),
        daemon_capability_class: report.daemon_capability_class.map(str::to_owned),
    }
}

fn requested_strategy(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    requested_type: WorktreeType,
) -> &'static str {
    if grove_enabled || grove_gate_source == Some("remote_kill") {
        return "grove";
    }
    match requested_type {
        WorktreeType::Linked => "linked",
        WorktreeType::Standalone => "standalone",
        WorktreeType::Git => "git",
    }
}

fn fallback_reason(
    grove_enabled: bool,
    grove_gate_source: Option<&str>,
    resolved: &str,
    rewrite_reason: Option<&str>,
    skipped: &[ArmSkip],
) -> Option<String> {
    if !grove_enabled {
        return match grove_gate_source {
            Some("remote_kill") => Some("remote Grove is off".into()),
            _ => None,
        };
    }
    if is_grove_resolved(resolved) {
        return None;
    }
    // Only a grove skip explains why grove did not run: a snapshot arm's skip
    // means an earlier arm won and grove was never reached.
    skipped
        .iter()
        .find(|s| s.arm.is_grove())
        .map(ArmSkip::to_string)
        // A pre-dispatch rewrite kept every arm from running, so it is the only
        // account of why grove did not serve this worktree.
        .or_else(|| rewrite_reason.map(str::to_owned))
}

pub(super) fn creating_progress(grove_enabled: bool) -> &'static str {
    if grove_enabled {
        "Creating worktree with Grove..."
    } else {
        "Creating worktree..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use xai_fast_worktree::{CopyReport, WorktreeArm};

    fn report(
        resolved: &'static str,
        skipped: Vec<ArmSkip>,
        daemon: Option<&'static str>,
    ) -> WorktreeReport {
        WorktreeReport {
            worktree_path: PathBuf::from("/wt"),
            commit: "abc".into(),
            unignored_copy: CopyReport::default(),
            ignored_copy: None,
            resolved_strategy: resolved,
            strategy_metadata: None,
            skipped,
            daemon_capability_class: daemon,
        }
    }

    #[test]
    fn grove_success_and_copy_fallback_print_the_right_reason() {
        let ok = report_from_worktree(true, Some("request"), WorktreeType::Linked, None, &{
            let mut r = report("grove-fuse", Vec::new(), Some("current"));
            r.strategy_metadata = Some(serde_json::json!({"grove":{"transport":"fuse"}}));
            r
        });
        assert_eq!(ok.requested_strategy.as_deref(), Some("grove"));
        assert_eq!(ok.resolved_strategy.as_deref(), Some("grove-fuse"));
        assert_eq!(ok.transport.as_deref(), Some("fuse"));
        assert_eq!(ok.source_mode, None);
        assert!(ok.fallback_reason.is_none());
        assert_eq!(ok.summary(), "Requested Grove; using `grove-fuse`.");

        let copy = report_from_worktree(
            false,
            Some("remote_kill"),
            WorktreeType::Linked,
            None,
            &report("copy", Vec::new(), Some("unknown")),
        );
        assert_eq!(copy.requested_strategy.as_deref(), Some("grove"));
        assert_eq!(copy.resolved_strategy.as_deref(), Some("copy"));
        assert_eq!(copy.fallback_reason.as_deref(), Some("remote Grove is off"));
        assert_eq!(
            copy.summary(),
            "Requested Grove; using copy because remote Grove is off."
        );

        let skipped = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "/dev/fuse or fusermount missing",
                )],
                Some("unknown"),
            ),
        );
        assert_eq!(
            skipped.fallback_reason.as_deref(),
            Some("grove-fuse: /dev/fuse or fusermount missing")
        );
        assert!(skipped.summary().contains("grove-fuse:"));

        let old_and_skip = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "/dev/fuse or fusermount missing",
                )],
                Some("old"),
            ),
        );
        assert_eq!(
            old_and_skip.fallback_reason.as_deref(),
            Some("grove-fuse: /dev/fuse or fusermount missing")
        );
        assert!(old_and_skip.summary().contains("grove-fuse:"));
        assert!(!old_and_skip.summary().contains("daemon too old"));

        let overlay = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report("overlay", Vec::new(), Some("old")),
        );
        assert_eq!(overlay.resolved_strategy.as_deref(), Some("overlay"));
        assert_eq!(overlay.daemon_capability_class.as_deref(), Some("old"));
        assert!(overlay.fallback_reason.is_none());
        assert_eq!(overlay.summary(), "Requested Grove; using overlay.");
    }

    #[test]
    fn earlier_arm_skip_lines_are_not_a_grove_fallback_reason() {
        let btrfs_won = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "btrfs",
                vec![ArmSkip::new(WorktreeArm::Overlay, "mount failed: EPERM")],
                Some("current"),
            ),
        );
        assert!(
            btrfs_won.fallback_reason.is_none(),
            "grove never ran, so an overlay error cannot explain its absence: {:?}",
            btrfs_won.fallback_reason
        );
        assert_eq!(btrfs_won.summary(), "Requested Grove; using btrfs.");

        let copy_after_grove = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![
                    ArmSkip::new(WorktreeArm::Overlay, "mount failed: EPERM"),
                    ArmSkip::new(WorktreeArm::GroveFuse, "daemon declined or unreachable"),
                ],
                Some("current"),
            ),
        );
        assert_eq!(
            copy_after_grove.fallback_reason.as_deref(),
            Some("grove-fuse: daemon declined or unreachable")
        );
    }

    #[test]
    fn a_pre_dispatch_rewrite_still_explains_itself() {
        // to git before dispatch: no arm ran, and no arm recorded a skip.
        let rewritten = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            Some(xai_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT),
            &report("git", Vec::new(), None),
        );
        assert_eq!(
            rewritten.summary(),
            "Requested Grove; using git because source is itself a Grove mount."
        );

        // A grove skip line still wins: it is the arm's own account.
        let both = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            Some(xai_fast_worktree::SKIP_SOURCE_IS_GROVE_MOUNT),
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "the source is a jj repo",
                )],
                None,
            ),
        );
        assert_eq!(
            both.fallback_reason.as_deref(),
            Some("grove-fuse: the source is a jj repo")
        );
    }

    #[test]
    fn notice_is_silent_for_an_ordinary_copy_worktree() {
        let plain = report_from_worktree(
            false,
            Some("default"),
            WorktreeType::Linked,
            None,
            &report("copy", Vec::new(), None),
        );
        assert_eq!(plain.summary(), "Using copy.");
        assert_eq!(plain.notice(), None);

        let grove = report_from_worktree(
            true,
            Some("request"),
            WorktreeType::Linked,
            None,
            &report(
                "copy",
                vec![ArmSkip::new(
                    WorktreeArm::GroveFuse,
                    "private mount namespace",
                )],
                None,
            ),
        );
        assert_eq!(
            grove.notice().as_deref(),
            Some("Requested Grove; using copy because grove-fuse: private mount namespace.")
        );
    }
}
