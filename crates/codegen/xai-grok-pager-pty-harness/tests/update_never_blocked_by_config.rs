//! `pig update --check` is a recovery command: a config failure must not block the check,
//! and a WinGet install hands off to WinGet.
//!
//! A local server mocks the GitHub Releases API (`GET /repos/xcrong/pig/releases?per_page=100`)
//! and records each request path. The payload shape mirrors
//! `xai-grok-update/tests/common/mod.rs::github_releases_json`
//! (`tag_name`/`prerelease`/`draft`); the harness keeps its own `TcpListener` fixture
//! instead of adding a wiremock dependency. `PIG_GITHUB_API_BASE` /
//! `PIG_GITHUB_DOWNLOAD_BASE` point at the loopback mock and `installer = "github"`.
//! The config test serves the binary's own version, so a healthy `--check` reports
//! "already up to date" (`updateAvailable: false`). A run with a corrupt config must exit 0
//! and still emit `--check --json`; reintroducing a config `?` fails exactly that run.
//! Only the check path is exercised here: no tarball is downloaded and download success is
//! never asserted.
//!
//! A copy of the binary inside a WinGet package dir must exit 0, print the WinGet command, and
//! write no update state, even with a stale `installer` in config or an npm env hint. Without an
//! org version cap it makes no update request; with one it reads the stable releases feed and
//! names the exact allowed version, or no command when nothing is allowed or it is already there.
//! `--check` must not save a channel switch, and a `--version` pin below the org floor must fail
//! before the hand-off.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use xai_grok_pager_pty_harness::pager_binary;

// A child forked while the copy's write fd is open, even pager_binary's cargo build, fails the copy's exec with "Text file busy".
static EXEC_LOCK: Mutex<()> = Mutex::new(());

/// Minimal GitHub Releases payload holding a single stable release for `version`.
/// Same `tag_name`/`prerelease`/`draft` shape as
/// `xai-grok-update/tests/common/mod.rs::github_releases_json`.
fn releases_json(version: &str) -> String {
    serde_json::json!([{"tag_name": format!("v{version}"), "prerelease": false, "draft": false}])
        .to_string()
}

/// Spawn a local server mocking the GitHub Releases API: every request is answered with the
/// current releases payload for the version in `body`, and each request path is recorded.
fn spawn_github_releases_server(
    body: Arc<Mutex<String>>,
    requests: Arc<Mutex<Vec<String>>>,
) -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let serving = listener.try_clone().unwrap();
    std::thread::spawn(move || {
        for stream in serving.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(&stream);
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            if let Some(path) = request_line.split_whitespace().nth(1) {
                requests
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(path.to_owned());
            }
            // Drain headers: unread input at close resets the connection and can drop the reply.
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|n| n > 0) && header != "\r\n" {
                header.clear();
            }
            let version = body.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let payload = releases_json(&version);
            let _ = (&stream).write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                )
                .as_bytes(),
            );
        }
    });
    (listener, base)
}

/// `exe` with an isolated `home`, pointed at the local Releases mock.
/// `installer` selects the update backend: `Some("github")` for the direct-binary check,
/// `None` lets the running exe's own path decide (used for the WinGet package copy).
fn pig_command(exe: &Path, home: &Path, base: &str, installer: Option<&str>) -> Command {
    let mut command = Command::new(exe);
    command
        .env_clear()
        .env("HOME", home)
        .env("GROK_HOME", home)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("PIG_GITHUB_API_BASE", base)
        .env("PIG_GITHUB_DOWNLOAD_BASE", base);
    if let Some(installer) = installer {
        command.env("GROK_INSTALLER", installer);
    }
    xai_tty_utils::detach_std_command(&mut command);
    command
}

/// Run `command` to completion, holding [`EXEC_LOCK`] for the spawn only.
fn output(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[allow(clippy::disallowed_methods)] // waited on right below
    let child = {
        let _exec = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        command.spawn().expect("spawn pig")
    };
    child.wait_with_output().expect("wait for pig")
}

/// Run `pig update --check --json` in a fresh isolated home against the local Releases mock.
fn run_check(base: &str, config_toml: &str, extra_args: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), config_toml).unwrap();
    let exe = {
        let _exec = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        pager_binary().expect("resolve pager binary")
    };
    let mut command = pig_command(&exe, home.path(), base, Some("github"));
    command
        .arg("update")
        .arg("--check")
        .arg("--json")
        .args(extra_args);
    output(command)
}

/// Copies (never links) the binary into a user-scope WinGet package dir, so the running exe's path is the package path.
fn copy_into_winget_package(root: &Path) -> PathBuf {
    let package = root.join(
        "Local/Microsoft/WinGet/Packages/xAI.GrokBuild_Microsoft.Winget.Source_8wekyb3d8bbwe",
    );
    std::fs::create_dir_all(&package).unwrap();
    let exe = package.join(format!("grok{}", std::env::consts::EXE_SUFFIX));
    let _exec = EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::copy(pager_binary().expect("resolve pager binary"), &exe).unwrap();
    exe
}

/// Update artifacts present under `home`, among those the updater writes.
fn update_artifacts(home: &Path) -> Vec<&'static str> {
    ["bin", "downloads", "version.json"]
        .into_iter()
        .filter(|name| home.join(name).exists())
        .collect()
}

/// The valid run proves the environment resolves to success, so a nonzero corrupt run can only mean a config failure aborted the check.
#[test]
fn corrupt_config_never_changes_update_outcome() {
    let body = Arc::new(Mutex::new("0.0.1".to_owned()));
    let (_listener, base) = spawn_github_releases_server(body.clone(), Arc::default());

    // Probe the binary's own version so the mock serves it exactly as the stable release.
    let check = run_check(&base, "[cli]\ninstaller = \"github\"\n", &[]);
    let status: Value = serde_json::from_slice(&check.stdout)
        .unwrap_or_else(|e| panic!("update --check --json must emit JSON: {e}"));
    let current = status["currentVersion"]
        .as_str()
        .expect("currentVersion in update --check --json")
        .to_owned();
    *body.lock().unwrap_or_else(|e| e.into_inner()) = current;

    let valid = run_check(&base, "[cli]\ninstaller = \"github\"\n", &[]);
    assert!(
        valid.status.success(),
        "healthy pig update --check against the local releases mock must exit 0\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&valid.stdout),
        String::from_utf8_lossy(&valid.stderr)
    );
    let status: Value = serde_json::from_slice(&valid.stdout)
        .unwrap_or_else(|e| panic!("healthy --check must emit JSON: {e}"));
    assert_eq!(
        (Some("github"), Some(false), Some("stable")),
        (
            status.get("installer").and_then(Value::as_str),
            status.get("updateAvailable").and_then(Value::as_bool),
            status.get("channel").and_then(Value::as_str),
        ),
        "serving the binary's own version means no update is available"
    );

    let corrupt = run_check(&base, "this is not toml {{{[[[", &[]);
    assert!(
        corrupt.status.success(),
        "a corrupt config.toml must not block pig update --check\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&corrupt.stdout),
        String::from_utf8_lossy(&corrupt.stderr)
    );
    serde_json::from_slice::<Value>(&corrupt.stdout)
        .unwrap_or_else(|e| panic!("corrupt-config --check must still emit JSON: {e}"));
}

#[test]
fn winget_install_update_hands_off_without_update_writes() {
    let body = Arc::new(Mutex::new("999.0.0".to_owned()));
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let (_listener, base) = spawn_github_releases_server(body, requests.clone());
    let package_root = tempfile::tempdir().unwrap();
    let exe = copy_into_winget_package(package_root.path());
    let home = tempfile::tempdir().unwrap();
    let config = "[cli]\ninstaller = \"github\"\nchannel = \"alpha\"\n";
    std::fs::write(home.path().join("config.toml"), config).unwrap();

    // The second run adds an npm hint, which must not outrank the WinGet location.
    let runs: [&[(&str, &str)]; 2] = [
        &[],
        &[("npm_config_user_agent", "npm/10.8.0 node/v22 win32 x64")],
    ];
    for envs in runs {
        let mut command = pig_command(&exe, home.path(), &base, None);
        command.arg("update").envs(envs.iter().copied());
        let update = output(command);
        let stderr = String::from_utf8_lossy(&update.stderr);
        assert!(
            update.status.success(),
            "pig update on a WinGet install must exit 0 (env {envs:?})\nstderr:\n{stderr}"
        );
        assert!(
            stderr.contains("winget upgrade --id xAI.GrokBuild -e")
                && stderr.contains("WinGet ships only the stable channel"),
            "pig update must hand off to WinGet and flag the ignored alpha channel (env {envs:?})\nstderr:\n{stderr}"
        );
        let logged = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(Vec::<String>::new(), logged);
        assert_eq!(Vec::<&str>::new(), update_artifacts(home.path()));
        assert_eq!(
            config,
            std::fs::read_to_string(home.path().join("config.toml")).unwrap()
        );
    }

    let mut pinned = pig_command(&exe, home.path(), &base, None);
    pinned
        .args(["update", "--version", "0.0.1"])
        .env("GROK_REQUIRED_MINIMUM_VERSION", "0.0.2");
    let pinned = output(pinned);
    let stderr = String::from_utf8_lossy(&pinned.stderr);
    assert!(
        !pinned.status.success()
            && stderr.contains("the minimum allowed version is 0.0.2")
            && !stderr.contains("winget install"),
        "a pin below the org floor must fail before the WinGet hand-off\nstderr:\n{stderr}"
    );

    struct CappedCase {
        envs: &'static [(&'static str, &'static str)],
        succeeds: bool,
        expected: &'static str,
        prints_install: bool,
    }
    let install_5 = "winget install --id xAI.GrokBuild -e --version 5.0.0 --force";
    let capped_cases = [
        CappedCase {
            envs: &[
                ("GROK_MAXIMUM_VERSION", "5.0.0"),
                ("GROK_TEST_VERSION", "1.0.0"),
            ],
            succeeds: true,
            expected: install_5,
            prints_install: true,
        },
        CappedCase {
            envs: &[
                ("GROK_REQUIRED_MAXIMUM_VERSION", "5.0.0"),
                ("GROK_TEST_VERSION", "6.0.0"),
            ],
            succeeds: true,
            expected: install_5,
            prints_install: true,
        },
        CappedCase {
            envs: &[
                ("GROK_MAXIMUM_VERSION", "5.0.0"),
                ("GROK_TEST_VERSION", "5.0.0"),
            ],
            succeeds: true,
            expected: "Already up to date (5.0.0).",
            prints_install: false,
        },
        CappedCase {
            envs: &[
                ("GROK_MAXIMUM_VERSION", "5.0.0"),
                ("GROK_TEST_VERSION", "6.0.0"),
            ],
            succeeds: true,
            expected: "Already up to date (6.0.0).",
            prints_install: false,
        },
        CappedCase {
            envs: &[
                ("GROK_MAXIMUM_VERSION", "5.0.0"),
                ("GROK_MINIMUM_VERSION", "6.0.0"),
            ],
            succeeds: true,
            expected: "is not an allowed update",
            prints_install: false,
        },
        CappedCase {
            envs: &[
                ("GROK_MAXIMUM_VERSION", "2000.0.0"),
                ("GROK_REQUIRED_MINIMUM_VERSION", "1000.0.0"),
            ],
            succeeds: false,
            expected: "newer than the latest available release (999.0.0)",
            prints_install: false,
        },
    ];
    // `winget upgrade` would jump past the cap, so a capped org never gets it.
    for case in capped_cases {
        let mut command = pig_command(&exe, home.path(), &base, None);
        command.arg("update").envs(case.envs.iter().copied());
        let update = output(command);
        let stderr = String::from_utf8_lossy(&update.stderr);
        assert!(
            update.status.success() == case.succeeds
                && stderr.contains(case.expected)
                && !stderr.contains("winget upgrade")
                && (case.prints_install || !stderr.contains("winget install")),
            "capped WinGet update (env {:?}) must print {:?}\nstderr:\n{stderr}",
            case.envs,
            case.expected
        );
        assert_eq!(Vec::<&str>::new(), update_artifacts(home.path()));
    }

    let mut check = pig_command(&exe, home.path(), &base, None);
    check.args(["update", "--check", "--json"]);
    let status: Value = serde_json::from_slice(&output(check).stdout)
        .unwrap_or_else(|e| panic!("update --check --json must emit JSON: {e}"));
    assert_eq!(
        (Some("winget"), Some(true), Some("stable")),
        (
            status.get("installer").and_then(Value::as_str),
            status.get("updateAvailable").and_then(Value::as_bool),
            status.get("channel").and_then(Value::as_str),
        )
    );
    let logged = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        !logged.is_empty()
            && logged
                .iter()
                .all(|path| path.starts_with("/repos/xcrong/pig/releases")),
        "a WinGet --check reads only the stable releases feed: {logged:?}"
    );
    assert_eq!(vec!["version.json"], update_artifacts(home.path()));
    assert_eq!(
        config,
        std::fs::read_to_string(home.path().join("config.toml")).unwrap()
    );

    // An explicit installer override outranks the WinGet location.
    let mut explicit = pig_command(&exe, home.path(), &base, Some("github"));
    explicit.args(["update", "--check", "--json"]);
    let status: Value = serde_json::from_slice(&output(explicit).stdout)
        .unwrap_or_else(|e| panic!("update --check --json must emit JSON: {e}"));
    assert_eq!(
        Some("github"),
        status.get("installer").and_then(Value::as_str)
    );
}
