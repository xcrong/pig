//! Covers the HTTPS version-discovery path (`fetch_github_version_from_api`)
//! against a wiremock GitHub API, plus the generic download helpers.
//! Each `MockServer` binds to its own random port and tests don't touch global
//! state, so no `serial_test` is needed (except where env overrides apply).

use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use xai_grok_update::auto_update::{download_silent, download_with_progress};
use xai_grok_update::version::fetch_github_version_from_api;

fn releases_json(entries: &[(&str, bool)]) -> serde_json::Value {
    serde_json::Value::Array(
        entries
            .iter()
            .map(|(tag, prerelease)| {
                serde_json::json!({
                    "tag_name": tag,
                    "prerelease": *prerelease,
                    "draft": false,
                })
            })
            .collect(),
    )
}

async fn mount_releases(server: &MockServer, entries: &[(&str, bool)]) {
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/.*/releases.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(releases_json(entries)))
        .mount(server)
        .await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Version discovery
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn github_stable_returns_newest_non_prerelease() {
    let server = MockServer::start().await;
    mount_releases(&server, &[("v1.0.2-alpha.1", true), ("v1.0.1", false)]).await;

    let v = fetch_github_version_from_api("stable", &server.uri())
        .await
        .unwrap();
    assert_eq!(v, "1.0.1");
}

#[tokio::test]
async fn github_alpha_returns_newest_including_prerelease() {
    let server = MockServer::start().await;
    mount_releases(&server, &[("v1.0.2-alpha.1", true), ("v1.0.1", false)]).await;

    let v = fetch_github_version_from_api("alpha", &server.uri())
        .await
        .unwrap();
    assert_eq!(v, "1.0.2-alpha.1");
}

#[tokio::test]
async fn github_stable_skips_drafts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/.*/releases.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {"tag_name": "v1.0.3", "prerelease": false, "draft": true},
            {"tag_name": "v1.0.1", "prerelease": false, "draft": false},
        ])))
        .mount(&server)
        .await;

    let v = fetch_github_version_from_api("stable", &server.uri())
        .await
        .unwrap();
    assert_eq!(v, "1.0.1");
}

#[tokio::test]
async fn github_enterprise_falls_back_to_stable() {
    let server = MockServer::start().await;
    mount_releases(&server, &[("v1.0.2-alpha.1", true), ("v1.0.1", false)]).await;

    let v = fetch_github_version_from_api("enterprise", &server.uri())
        .await
        .unwrap();
    assert_eq!(v, "1.0.1");
}

#[tokio::test]
async fn github_no_stable_releases_errors() {
    let server = MockServer::start().await;
    mount_releases(&server, &[("v1.0.2-alpha.1", true)]).await;

    let err = fetch_github_version_from_api("stable", &server.uri())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("No stable releases"));
}

#[tokio::test]
async fn github_http_error_surfaces_status() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/.*/releases.*"))
        .respond_with(ResponseTemplate::new(500).set_body_string("backend down"))
        .mount(&server)
        .await;

    let err = fetch_github_version_from_api("stable", &server.uri())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 500"));
}

#[tokio::test]
async fn github_connection_refused_returns_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let url = format!("http://127.0.0.1:{port}");

    let err = fetch_github_version_from_api("stable", &url)
        .await
        .unwrap_err();
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("fetch failed")
            || msg.contains("connection")
            || msg.contains("error sending request")
            || msg.contains("refused"),
        "expected network error, got: {msg}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Downloads (generic single-file helpers used for tarballs)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn download_silent_writes_body_to_dest() {
    use wiremock::matchers::path;
    let server = MockServer::start().await;
    let body = b"binary contents \x00\x01\x02".to_vec();
    Mock::given(method("GET"))
        .and(path("/pig-1.0.1-macos-arm64.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pig-1.0.1-macos-arm64.tar.gz");
    let url = format!("{}/pig-1.0.1-macos-arm64.tar.gz", server.uri());
    download_silent(&url, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[tokio::test]
async fn download_with_progress_writes_body_to_dest() {
    use wiremock::matchers::path;
    let server = MockServer::start().await;
    let body = b"binary contents".to_vec();
    Mock::given(method("GET"))
        .and(path("/pig-1.0.1-macos-arm64.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("pig-1.0.1-macos-arm64.tar.gz");
    let url = format!("{}/pig-1.0.1-macos-arm64.tar.gz", server.uri());
    download_with_progress(&url, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);
}

#[tokio::test]
async fn download_silent_errors_on_404() {
    use wiremock::matchers::path;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing.tar.gz"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("missing.tar.gz");
    let url = format!("{}/missing.tar.gz", server.uri());
    let err = download_silent(&url, &dest).await.unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 404"));
}
