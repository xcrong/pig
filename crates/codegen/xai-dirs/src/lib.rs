//! Home-directory resolution generally: USERPROFILE-first `home_dir`, plus
//! pig-home (`$PIG_HOME`, `$GROK_HOME` fallback, or `<home>/.config/pig`).
//! Shared by `xai-grok-config` and `xai-fast-worktree`.
//!
//! Brand mapping (pig fork): the historic `grok_home` / `GrokHomeSource` /
//! `$GROK_HOME` identifiers are kept verbatim so upstream merges stay trivial;
//! they now resolve to the pig home. `$GROK_HOME` still works as a fallback so
//! existing checkouts migrate without manual steps.
//!
//! Which function to call:
//! - [`grok_home`]: the usual choice, a cached, created path to build on.
//!   On first creation from the default location it one-time migrates a legacy
//!   `<home>/.grok` tree into place.
//! - [`user_grok_home`]: `None` instead of a cwd fallback when no home resolves.
//! - [`default_grok_home`]: the `<home>/.config/pig` default, ignoring both
//!   `$PIG_HOME` and `$GROK_HOME`, so callers can detect an override.
//! - [`resolve_grok_home`]: a fresh, uncached resolve.
//! - [`resolve_grok_home_with_source`]: [`resolve_grok_home`] plus where the path came from.
//! - [`home_dir`]: the home directory itself, for sibling dot dirs (`~/.claude`, `~/.agents`, ...).
//!
//! TODO: collapse these getters by threading the path through config as an
//! explicit value.

#![deny(clippy::indexing_slicing)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where a resolved home came from, so "why did pig pick this
/// directory?" is answerable in diagnostics without re-reading the
/// environment at the asking site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokHomeSource {
    /// A non-empty `$PIG_HOME` (or legacy `$GROK_HOME`) override.
    EnvOverride,
    /// `<home>/.config/pig` derived from the home directory.
    HomeDefault,
}

/// The user's home directory via [`std::env::home_dir`]: `HOME` on Unix, `USERPROFILE` on Windows.
/// Not `dirs::home_dir()`: on Windows `dirs` ignores a redirected `USERPROFILE`.
/// Every home-anchored path must come from this one function.
#[allow(deprecated, clippy::disallowed_methods)] // the one sanctioned std::env::home_dir call
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

/// `<home>/.config/pig`, canonicalized via `dunce` (not `std::fs::canonicalize`,
/// which yields Windows `\\?\` verbatim paths).
fn pig_home_in(home: &Path) -> PathBuf {
    dunce::canonicalize(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .join(".config")
        .join("pig")
}

/// Legacy `<home>/.grok` location, used only as a one-time migration source.
/// Canonicalized the same way so the existence check matches what older
/// builds created.
fn legacy_grok_home_in(home: &Path) -> PathBuf {
    dunce::canonicalize(home)
        .unwrap_or_else(|_| home.to_path_buf())
        .join(".grok")
}

/// `$PIG_HOME` verbatim when non-empty, else legacy `$GROK_HOME` verbatim when
/// non-empty, else `<home>/.config/pig`.
/// Used as-is (not canonicalized) so literal prefix checks and symlink guards still see original components.
fn resolve_grok_home_from(
    pig_home_env: Option<&OsStr>,
    grok_home_env: Option<&OsStr>,
    os_home: Option<&Path>,
) -> Option<(PathBuf, GrokHomeSource)> {
    if let Some(env) = pig_home_env.filter(|env| !env.is_empty()) {
        return Some((PathBuf::from(env), GrokHomeSource::EnvOverride));
    }
    if let Some(env) = grok_home_env.filter(|env| !env.is_empty()) {
        return Some((PathBuf::from(env), GrokHomeSource::EnvOverride));
    }
    os_home.map(|home| (pig_home_in(home), GrokHomeSource::HomeDefault))
}

/// Resolve the grok home from the environment (fresh, no cache); `None` if neither resolves.
pub fn resolve_grok_home() -> Option<PathBuf> {
    resolve_grok_home_with_source().map(|(home, _)| home)
}

/// [`resolve_grok_home`] plus the [`GrokHomeSource`] the path came from.
pub fn resolve_grok_home_with_source() -> Option<(PathBuf, GrokHomeSource)> {
    resolve_grok_home_from(
        std::env::var_os("PIG_HOME").as_deref(),
        std::env::var_os("GROK_HOME").as_deref(),
        home_dir().as_deref(),
    )
}

/// The default `<home>/.config/pig`, used when neither `$PIG_HOME` nor
/// `$GROK_HOME` is set.
pub fn default_grok_home() -> PathBuf {
    pig_home_in(&home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

/// Recursively copy a directory tree (regular files and dirs; other file
/// types are skipped with a debug log). Returns the copied file count.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut files = 0u64;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            files += copy_dir_recursive(&entry.path(), &to)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &to)?;
            files += 1;
        } else {
            tracing::debug!(path = %entry.path().display(), "skipping special file during home migration");
        }
    }
    Ok(files)
}

/// Whether a legacy-to-pig migration should run: pure default resolution,
/// pig home absent, legacy home present. Explicit `$PIG_HOME`/`$GROK_HOME`
/// overrides are always respected as-is and never trigger a copy.
fn should_migrate_legacy_home(
    pig_home_env: Option<&OsStr>,
    grok_home_env: Option<&OsStr>,
    legacy_home_exists: bool,
) -> bool {
    legacy_home_exists
        && pig_home_env.is_none_or(|env| env.is_empty())
        && grok_home_env.is_none_or(|env| env.is_empty())
}

/// One-time migration: copy a legacy `<home>/.grok` tree into the pig home
/// before it is created. Never overwrites: runs only when the pig home does
/// not exist yet. Failures warn and fall through to a fresh home.
fn maybe_migrate_legacy_home(pig_home: &Path) {
    if pig_home.exists() {
        return;
    }
    let legacy = home_dir().map(|home| legacy_grok_home_in(&home));
    let legacy_exists = legacy.as_ref().is_some_and(|path| path.exists());
    if !should_migrate_legacy_home(
        std::env::var_os("PIG_HOME").as_deref(),
        std::env::var_os("GROK_HOME").as_deref(),
        legacy_exists,
    ) {
        return;
    }
    let legacy = legacy.expect("migration gated on legacy_home_exists");
    tracing::info!(from = %legacy.display(), to = %pig_home.display(), "migrating legacy grok home to pig home");
    if let Err(err) = copy_dir_recursive(&legacy, pig_home) {
        tracing::warn!(%err, "legacy home migration failed; starting from a fresh pig home");
    }
}

/// The grok home, created if missing and cached for the process; falls back to
/// [`default_grok_home`] when neither `$PIG_HOME`/`$GROK_HOME` nor a home resolves.
/// On first creation from the default location, a legacy `<home>/.grok` tree
/// is migrated into place (see [`maybe_migrate_legacy_home`]).
pub fn grok_home() -> PathBuf {
    static GROK_HOME: OnceLock<PathBuf> = OnceLock::new();
    GROK_HOME
        .get_or_init(|| {
            let (home, source) = resolve_grok_home_with_source()
                .unwrap_or_else(|| (default_grok_home(), GrokHomeSource::HomeDefault));
            if source == GrokHomeSource::HomeDefault {
                maybe_migrate_legacy_home(&home);
            }
            if let Err(err) = std::fs::create_dir_all(&home) {
                tracing::warn!(path = %home.display(), %err, "failed to create grok home");
            }
            home
        })
        .clone()
}

/// Like [`grok_home`], but `None` when no home resolves (no cwd fallback).
pub fn user_grok_home() -> Option<PathBuf> {
    resolve_grok_home().is_some().then(grok_home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsString;

    #[test]
    fn pig_env_wins_over_grok_env_and_os_home() {
        let resolved = resolve_grok_home_from(
            Some(OsStr::new("/pig/home")),
            Some(OsStr::new("/custom/home")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            resolved,
            Some((PathBuf::from("/pig/home"), GrokHomeSource::EnvOverride))
        );
    }

    #[test]
    fn env_wins_over_os_home() {
        let resolved = resolve_grok_home_from(
            None,
            Some(OsStr::new("/custom/home")),
            Some(Path::new("/home/u")),
        );
        assert_eq!(
            resolved,
            Some((PathBuf::from("/custom/home"), GrokHomeSource::EnvOverride))
        );
    }

    #[test]
    fn default_is_pig_home_not_legacy_grok() {
        let resolved = resolve_grok_home_from(None, None, Some(Path::new("/home/u")));
        let (path, source) = resolved.expect("os home resolves");
        assert_eq!(source, GrokHomeSource::HomeDefault);
        assert_eq!(path, PathBuf::from("/home/u/.config/pig"));
    }

    #[test]
    fn env_used_verbatim_even_when_it_exists() {
        // A real, existing dir whose canonical form differs (macOS symlinks
        // `/var` -> `/private/var`): the env value must come back unchanged.
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_grok_home_from(Some(tmp.path().as_os_str()), None, None);
        assert_eq!(
            resolved,
            Some((tmp.path().to_path_buf(), GrokHomeSource::EnvOverride))
        );
    }

    #[test]
    fn empty_env_falls_through_to_os_home() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_grok_home_from(
            Some(&OsString::new()),
            Some(&OsString::new()),
            Some(tmp.path()),
        );
        assert_eq!(
            resolved,
            Some((
                dunce::canonicalize(tmp.path())
                    .unwrap()
                    .join(".config")
                    .join("pig"),
                GrokHomeSource::HomeDefault
            ))
        );
    }

    #[test]
    fn default_grok_home_has_no_verbatim_prefix() {
        // The reason we canonicalize via dunce: std::fs::canonicalize yields
        // `\\?\` verbatim paths on Windows that break git and byte-exact
        // comparisons. No-op assertion on Unix.
        let home = default_grok_home();
        assert!(!home.to_string_lossy().starts_with(r"\\?\"));
        assert!(home.ends_with(Path::new(".config/pig")));
    }

    #[test]
    fn none_when_nothing_resolves() {
        assert_eq!(
            resolve_grok_home_from(
                /* pig_home_env */ None, /* grok_home_env */ None,
                /* os_home */ None
            ),
            None
        );
    }

    #[test]
    fn migration_runs_only_on_pure_default_with_legacy_present() {
        assert!(should_migrate_legacy_home(None, None, true));
        assert!(!should_migrate_legacy_home(None, None, false));
        assert!(!should_migrate_legacy_home(
            Some(OsStr::new("/p")),
            None,
            true
        ));
        assert!(!should_migrate_legacy_home(
            None,
            Some(OsStr::new("/g")),
            true
        ));
        // Empty env values count as unset everywhere else; migration runs.
        assert!(should_migrate_legacy_home(
            Some(&OsString::new()),
            Some(&OsString::new()),
            true
        ));
    }

    #[test]
    fn migration_copies_legacy_tree() {
        let legacy = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(legacy.path().join("sessions")).unwrap();
        std::fs::write(legacy.path().join("config.toml"), "x = 1\n").unwrap();
        std::fs::write(legacy.path().join("sessions/a.json"), "{}\n").unwrap();
        let dst = tempfile::tempdir().unwrap().path().join("pig-home");
        let files = copy_dir_recursive(legacy.path(), &dst).unwrap();
        assert_eq!(files, 2);
        assert_eq!(
            std::fs::read_to_string(dst.join("config.toml")).unwrap(),
            "x = 1\n"
        );
        assert!(dst.join("sessions/a.json").exists());
    }
}
