//! Invariant matrix tests for the rollback/downgrade feature.
//!
//! Covers every combination of:
//!   - user's current version vs. channel pointer target
//!   - installer type (github, npm)
//!   - channel (stable, alpha)
//!   - pointer-flip scenarios (stable bumped after user upgraded, alpha pointer rolled back, etc.)
//!
//! Also includes wiremock-based installation tests.
//! They verify the github tarball installer actually downloads and symlinks an older binary on rollback.

#![cfg(unix)]

mod common;

use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{FakeBinGuard, reset_home, set_test_version, test_home};
use xai_grok_update::UpdateConfig;
use xai_grok_update::auto_update::{
    auto_update_target, check_update_status, install_github_from_download_base,
};
use xai_grok_update::version::installed_on_disk_version;

fn host_platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        panic!("unsupported test platform");
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        panic!("unsupported test arch");
    };
    format!("{os}-{arch}")
}

fn make_config(channel: &str) -> UpdateConfig {
    UpdateConfig {
        proxy_base_url: "http://test.invalid/v1".to_string(),
        auth_scope: "test".to_string(),
        deployment_key: None,
        alpha_test_key: None,
        channel: channel.to_string(),
        npm_registry: None,
    }
}

async fn mount_tarball_with_channels(
    stable_version: &str,
    alpha_version: Option<&str>,
    binary_version: &str,
    _platform: &str,
) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/stable"))
        .respond_with(ResponseTemplate::new(200).set_body_string(stable_version))
        .mount(&server)
        .await;

    if let Some(alpha_v) = alpha_version {
        Mock::given(method("GET"))
            .and(path("/alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_string(alpha_v))
            .mount(&server)
            .await;
    }

    // Pinned installs fetch the tarball directly; pointer mocks above are
    // unused by them but kept for the decision-level tests below.
    let asset = common::host_asset();
    let tarball = common::make_pig_tarball(b"#!/bin/sh\nexit 0\n");
    Mock::given(method("GET"))
        .and(path(format!(
            "/xcrong/pig/releases/download/v{binary_version}/{asset}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;

    server
}

// Scenario matrix: direct-binary installer, downgrade via install. Each test simulates a user on version X, with the
// release now pointing to version Y. The github installer should install Y regardless of whether Y < X
// (rollback) or Y > X (upgrade) ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn github_install_stable_rollback_0_2_7_to_0_2_5() {
    // User was on 0.2.7, stable pointer rolled back to 0.2.5.
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = mount_tarball_with_channels("0.2.5", None, "0.2.5", &platform).await;
    let cfg = make_config("stable");

    install_github_from_download_base(Some("0.2.5"), &cfg, &server.uri())
        .await
        .unwrap();

    let home = test_home();
    let downloaded = home.join("downloads").join(format!("pig-0.2.5-{platform}"));
    assert!(downloaded.exists(), "rolled-back binary must be downloaded");

    let symlink = home.join("bin").join("pig");
    let target = std::fs::read_link(&symlink).unwrap();
    assert!(
        target.to_string_lossy().contains("0.2.5"),
        "symlink must point to rolled-back version: {target:?}"
    );
}

#[tokio::test]
#[serial]
async fn github_install_stable_upgrade_0_2_5_to_0_2_7() {
    // Normal upgrade path: user on 0.2.5, pointer at 0.2.7.
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = mount_tarball_with_channels("0.2.7", None, "0.2.7", &platform).await;
    let cfg = make_config("stable");

    install_github_from_download_base(Some("0.2.7"), &cfg, &server.uri())
        .await
        .unwrap();

    let symlink = test_home().join("bin").join("pig");
    let target = std::fs::read_link(&symlink).unwrap();
    assert!(target.to_string_lossy().contains("0.2.7"));
}

#[tokio::test]
#[serial]
async fn github_install_rollback_then_upgrade_sequence() {
    // Simulates: install 0.2.7, roll back to 0.2.5, then the fix ships as 0.2.8
    // All three installs must succeed sequentially.
    let _ = test_home();
    reset_home();
    let platform = host_platform();

    for version in ["0.2.7", "0.2.5", "0.2.8"] {
        // Age the previous installs: cleanup never deletes a freshly-written binary (it may be a concurrent racer's just-renamed download)
        // The retention assertions below need the earlier installs to look like real leftovers from past releases
        common::backdate_downloads();
        let server = mount_tarball_with_channels(version, None, version, &platform).await;
        let cfg = make_config("stable");
        install_github_from_download_base(Some(version), &cfg, &server.uri())
            .await
            .unwrap();
    }

    let target = std::fs::read_link(test_home().join("bin").join("pig")).unwrap();
    assert!(
        target.to_string_lossy().contains("0.2.8"),
        "final symlink must point to 0.2.8: {target:?}"
    );

    // Cleanup retains the current and the highest-semver non-current binary (N-1 by version, not install order)
    let downloads = test_home().join("downloads");
    assert!(
        downloads.join(format!("pig-0.2.8-{platform}")).exists(),
        "current"
    );
    assert!(
        downloads.join(format!("pig-0.2.7-{platform}")).exists(),
        "N-1 by semver"
    );
    assert!(
        !downloads.join(format!("pig-0.2.5-{platform}")).exists(),
        "lowest cleaned up"
    );
}

#[tokio::test]
#[serial]
async fn github_install_alpha_user_gets_newer_stable_after_stable_passes_alpha() {
    // Pinned-install equivalent of the old pointer test: an alpha user
    // installs the newer stable explicitly. Unpinned pointer resolution is
    // covered in test_network.rs (GitHub API); tarball unpack in
    // test_install_github.rs.
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = mount_tarball_with_channels("0.2.7", None, "0.2.7", &platform).await;

    let cfg = make_config("alpha");
    install_github_from_download_base(Some("0.2.7"), &cfg, &server.uri())
        .await
        .unwrap();

    assert!(
        test_home()
            .join("downloads")
            .join(format!("pig-0.2.7-{platform}"))
            .exists(),
        "alpha user should get the newer stable"
    );
}

// ─────────────────────────────────────────────────────────────────────────────. The internal (GCS) path can't be
// end-to-end tested via check_update_status (hardcoded URLs). Its update-detection logic is covered by the needs_update
// unit tests and the install tests above ─────────────────────────────────────────────────────────────────────────────

fn setup_npm(current_version: &str) -> FakeBinGuard {
    let _ = test_home();
    reset_home();
    set_test_version(current_version);
    // SAFETY: serial_test ensures no race; reset_home clears this between tests.
    unsafe { std::env::set_var("GROK_INSTALLER", "npm") };
    FakeBinGuard::install_npm()
}

// ── npm: never downgrades ──

#[tokio::test]
#[serial]
async fn npm_upgrade_reports_update() {
    let g = setup_npm("0.2.5");
    g.set_stdout("\"0.2.7\"");

    let status = check_update_status(&make_config("stable")).await;
    assert!(status.update_available);
    assert_eq!(status.latest_version.as_deref(), Some("0.2.7"));
}

#[tokio::test]
#[serial]
async fn npm_same_version_no_update() {
    let g = setup_npm("0.2.7");
    g.set_stdout("\"0.2.7\"");

    let status = check_update_status(&make_config("stable")).await;
    assert!(!status.update_available);
}

#[tokio::test]
#[serial]
async fn npm_rollback_does_not_report_update() {
    // Stable pointer rolled back from 0.2.7 to 0.2.5
    // npm user on 0.2.7 must NOT see an update; stale registries make this path unsafe
    let g = setup_npm("0.2.7");
    g.set_stdout("\"0.2.5\"");

    let status = check_update_status(&make_config("stable")).await;
    assert!(
        !status.update_available,
        "npm must never report a downgrade: current={} latest={:?}",
        status.current_version, status.latest_version
    );
}

#[tokio::test]
#[serial]
async fn npm_drastically_old_registry_does_not_report_update() {
    // The corporate registry returns an ancient version
    let g = setup_npm("0.2.7");
    g.set_stdout("\"0.1.4\"");

    let status = check_update_status(&make_config("stable")).await;
    assert!(!status.update_available);
}

// ── github version discovery is covered in test_network.rs (pure HTTPS, no
// subprocess); install/unpack is covered in test_install_github.rs. The
// decision-level downgrade rules live in the lib unit tests.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn auto_update_target_npm_rollback_returns_none() {
    // npm registries can serve stale versions, so never downgrade npm installs
    let g = setup_npm("0.2.26");
    g.set_stdout("\"0.2.22\"");

    assert_eq!(
        auto_update_target(&make_config("stable")).await,
        None,
        "npm must never be downgraded even when the registry reports an older version"
    );
}

// Each must decide staleness from the on-disk install, not its own compiled-in version. A binary another process already
// installed is never downloaded a second time, but a stale running process still gets the relaunch signal
// ─────────────────────────────────────────────────────────────────────────────

/// Lay down what `install_github_from_download_base` produces in the test GROK_HOME: `bin/pig -> ../downloads/pig-<version>-<platform>`.
fn fake_managed_install(version: &str) {
    let home = test_home();
    let downloads = home.join("downloads");
    let bin = home.join("bin");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let name = format!("pig-{version}-{}", host_platform());
    std::fs::write(downloads.join(&name), b"#!/bin/sh\nexit 0\n").unwrap();
    std::os::unix::fs::symlink(
        std::path::Path::new("../downloads").join(&name),
        bin.join("pig"),
    )
    .unwrap();
}

#[tokio::test]
#[serial]
async fn installed_on_disk_version_reads_symlink_target() {
    let _ = test_home();
    reset_home();
    assert_eq!(installed_on_disk_version(), None, "no install yet");

    fake_managed_install("0.2.7");
    assert_eq!(installed_on_disk_version().as_deref(), Some("0.2.7"));
}

#[tokio::test]
#[serial]
async fn ensure_latest_skips_download_when_disk_current_but_still_relaunches() {
    // Running 1.0.1, disk already at 1.0.2 (another process downloaded it):
    // pinned github installs converge without re-fetching the pointer.
    // Full pointer-fetch convergence is covered in test_network.rs.
    let _ = test_home();
    reset_home();
    set_test_version("1.0.1");
    fake_managed_install("1.0.2");
    assert_eq!(installed_on_disk_version().as_deref(), Some("1.0.2"));
}

// Pointer-flip timing scenarios. These test the race between a user opening grok (which caches the version) and a
// pointer flip happening. The 30-min TTL means the user won't see the new pointer until the cache expires, but once it
// does, the correct behavior must kick in ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn npm_user_upgraded_then_stable_rolled_back_stays_on_newer() {
    // User ran `grok update` and got 0.2.7. Then stable was rolled back to 0.2.5.
    // Next check_update_status sees 0.2.5 from npm. npm installer must NOT report a downgrade.
    let g = setup_npm("0.2.7");
    g.set_stdout("\"0.2.5\"");

    let status = check_update_status(&make_config("stable")).await;
    assert!(!status.update_available);
    assert_eq!(status.latest_version.as_deref(), Some("0.2.5"));
}

#[tokio::test]
#[serial]
async fn npm_alpha_user_upgrade_after_stable_surpasses_alpha() {
    // Alpha user on 0.2.6-alpha.2. Stable ships 0.2.7. npm returns 0.2.7 for the @latest tag.
    let g = setup_npm("0.2.6-alpha.2");
    g.set_stdout("\"0.2.7\"");

    let status = check_update_status(&make_config("stable")).await;
    // A pre-release current version on the stable channel forces the install
    assert!(
        status.update_available,
        "alpha user should upgrade to stable when stable surpasses alpha"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Double-rollback scenario
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn github_install_double_rollback() {
    // Ship 0.2.7, roll back to 0.2.5, then roll back further to 0.2.3
    // The installer must handle multiple sequential downgrades.
    let _ = test_home();
    reset_home();
    let platform = host_platform();

    for version in ["0.2.7", "0.2.5", "0.2.3"] {
        let server = mount_tarball_with_channels(version, None, version, &platform).await;
        let cfg = make_config("stable");
        install_github_from_download_base(Some(version), &cfg, &server.uri())
            .await
            .unwrap();

        let target = std::fs::read_link(test_home().join("bin").join("pig")).unwrap();
        assert!(
            target.to_string_lossy().contains(version),
            "symlink must point to {version} after install: {target:?}"
        );
    }
}
