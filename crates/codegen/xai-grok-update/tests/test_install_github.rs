//! End-to-end tests for the direct-binary (`github`) installer: pig GitHub
//! Release tarballs unpacked into the managed `downloads/` + `bin/pig` layout.
//!
//! Wires together a wiremock-mocked download base and an isolated `GROK_HOME`
//! tempdir to verify the full install pipeline: resolve version, download the
//! tarball, unpack the `pig` binary, chmod, atomic symlink, cleanup, persist
//! installer config.
//!
//! The function reads `grok_home()` (a process-wide `OnceLock`).
//! All tests in this binary therefore share one `GROK_HOME` and run serially via `#[serial]`.

#![cfg(unix)]

mod common;

use serial_test::serial;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{
    host_asset, host_platform, make_pig_tarball, make_update_config, reset_home,
    small_good_artifact, test_home,
};
use xai_grok_update::auto_update::install_github_from_download_base;

async fn mount_github(version: &str) -> MockServer {
    let server = MockServer::start().await;
    let asset = host_asset();
    let tarball = make_pig_tarball(&small_good_artifact());

    // Release tarball download.
    Mock::given(method("GET"))
        .and(path(format!(
            "/xcrong/pig/releases/download/v{version}/{asset}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;

    server
}

// ─────────────────────────────────────────────────────────────────────────────
// Happy-path
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn install_github_pinned_version_writes_binary_and_symlink() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = mount_github("1.0.1").await;
    let cfg = make_update_config("stable");

    install_github_from_download_base(Some("1.0.1"), &cfg, &server.uri())
        .await
        .unwrap();

    let home = test_home();
    let downloaded = home.join("downloads").join(format!("pig-1.0.1-{platform}"));
    assert!(downloaded.exists(), "binary downloaded: {downloaded:?}");
    assert_eq!(std::fs::read(&downloaded).unwrap(), small_good_artifact());

    let symlink = home.join("bin").join("pig");
    assert!(symlink.is_symlink(), "pig symlink created");
    let target = std::fs::read_link(&symlink).unwrap();
    assert_eq!(
        target.file_name().unwrap(),
        format!("pig-1.0.1-{platform}").as_str()
    );

    // Legacy upstream links are cleaned so `pig` is the only managed binary.
    assert!(!home.join("bin").join("grok").exists());
    assert!(!home.join("bin").join("agent").exists());

    // `pig-latest` tracks the new binary.
    let latest = home.join("downloads").join("pig-latest");
    assert!(latest.is_symlink(), "pig-latest created");
}

#[tokio::test]
#[serial]
async fn install_github_replaces_previous_version() {
    let _ = test_home();
    reset_home();
    let platform = host_platform();
    let server = MockServer::start().await;
    let asset = host_asset();
    for version in ["1.0.1", "1.0.2"] {
        let tarball = make_pig_tarball(&small_good_artifact());
        Mock::given(method("GET"))
            .and(path(format!(
                "/xcrong/pig/releases/download/v{version}/{asset}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
            .mount(&server)
            .await;
    }
    let cfg = make_update_config("stable");

    install_github_from_download_base(Some("1.0.1"), &cfg, &server.uri())
        .await
        .unwrap();
    install_github_from_download_base(Some("1.0.2"), &cfg, &server.uri())
        .await
        .unwrap();

    let home = test_home();
    let link = home.join("bin").join("pig");
    let target = std::fs::read_link(&link).unwrap();
    assert_eq!(
        target.file_name().unwrap(),
        format!("pig-1.0.2-{platform}").as_str()
    );
}

#[tokio::test]
#[serial]
async fn install_github_rejects_invalid_version() {
    let _ = test_home();
    reset_home();
    let server = mount_github("1.0.1").await;
    let cfg = make_update_config("stable");

    let err = install_github_from_download_base(Some("not-a-version"), &cfg, &server.uri())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("invalid version"));
}

#[tokio::test]
#[serial]
async fn install_github_fails_without_pig_entry() {
    let _ = test_home();
    reset_home();
    let server = MockServer::start().await;
    let asset = host_asset();
    // Tarball containing the wrong entry name.
    let tarball = {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        {
            let mut archive = tar::Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_cksum();
            archive
                .append_data(&mut header, "not-pig", b"xyz" as &[u8])
                .unwrap();
            archive.finish().unwrap();
        }
        gz.finish().unwrap()
    };
    Mock::given(method("GET"))
        .and(path(format!(
            "/xcrong/pig/releases/download/v1.0.1/{asset}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball))
        .mount(&server)
        .await;
    let cfg = make_update_config("stable");

    let err = install_github_from_download_base(Some("1.0.1"), &cfg, &server.uri())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("pig"));
}
