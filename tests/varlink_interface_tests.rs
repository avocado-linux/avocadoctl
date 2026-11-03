//! Integration tests that exercise the full varlink client → daemon → service path.
//!
//! Each test starts a real `avocadoctl serve` daemon on a temporary Unix socket,
//! then invokes CLI commands with `--socket` pointing at it.  This proves that:
//!   - The CLI routes requests through the daemon (not direct service calls)
//!   - The daemon serialises concurrent callers on the socket
//!   - Error messages are correct when the daemon is not running

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ── helpers ──────────────────────────────────────────────────────────────────

fn get_binary_path() -> PathBuf {
    let mut path = std::env::current_dir().expect("Failed to get current directory");
    path.push("target");
    path.push("debug");
    path.push("avocadoctl");
    path
}

fn fixtures_path() -> PathBuf {
    std::env::current_dir().expect("cwd").join("tests/fixtures")
}

/// A running `avocadoctl serve` process bound to a temp socket.
/// Killed automatically when dropped.
struct TestDaemon {
    child: Child,
    socket_path: PathBuf,
    /// Keep temp dir alive so the socket directory isn't removed while the daemon runs.
    _temp_dir: TempDir,
}

impl TestDaemon {
    fn start() -> Self {
        Self::start_with_env(&[])
    }

    /// Start a daemon with `AVOCADO_TEST_MODE=1` and the test fixtures on PATH.
    /// Blocks until the socket file appears (up to 5 s) or panics.
    fn start_with_env(envs: &[(&str, &Path)]) -> Self {
        let temp_dir = TempDir::new().expect("temp dir");
        let socket_path = temp_dir.path().join("avocadoctl-test.sock");
        let socket_address = format!("unix:{}", socket_path.display());

        let original_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", fixtures_path().display(), original_path);

        let child = Command::new(get_binary_path())
            .args(["serve", "--address", &socket_address])
            .env("AVOCADO_TEST_MODE", "1")
            .env("PATH", &test_path)
            .envs(envs.iter().copied())
            .spawn()
            .expect("Failed to spawn daemon");

        // Wait for the socket to be created (up to 5 s)
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if socket_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            socket_path.exists(),
            "Daemon socket should appear within 5 s at {}",
            socket_path.display()
        );

        TestDaemon {
            child,
            socket_path,
            _temp_dir: temp_dir,
        }
    }

    fn socket_address(&self) -> String {
        format!("unix:{}", self.socket_path.display())
    }

    /// Run a CLI command routed through this daemon.
    fn run(&self, args: &[&str]) -> std::process::Output {
        let socket = self.socket_address();

        let mut all_args = vec!["--socket", socket.as_str()];
        all_args.extend_from_slice(args);

        // Do NOT set AVOCADO_TEST_MODE here: the client must use the varlink path.
        // The daemon (started above with AVOCADO_TEST_MODE=1) handles the actual
        // service calls with mock executables.
        Command::new(get_binary_path())
            .args(&all_args)
            .output()
            .expect("Failed to execute avocadoctl")
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// The daemon starts and the socket is reachable.
#[test]
fn test_daemon_starts_and_accepts_connections() {
    let daemon = TestDaemon::start();
    // If TestDaemon::start() returned, the socket exists and the daemon is running.
    assert!(daemon.socket_path.exists(), "socket should exist");
}

/// `ext list` routed through the daemon returns extension data (or "no extensions").
#[test]
fn test_ext_list_via_daemon() {
    let daemon = TestDaemon::start();
    let output = daemon.run(&["ext", "list"]);

    assert!(
        output.status.success(),
        "ext list via daemon should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Either shows extensions or a "No extensions found" message — both are valid.
    let valid = stdout.contains("Extension") || stdout.contains("No extensions found");
    assert!(
        valid,
        "ext list should produce a table or empty message; got: {stdout}"
    );
}

/// `ext list` with an extensions directory populates the table.
#[test]
fn test_ext_list_with_extensions_via_daemon() {
    let temp_dir = TempDir::new().expect("temp dir");
    let ext_dir = temp_dir.path().join("images");
    fs::create_dir_all(&ext_dir).expect("create ext dir");
    fs::create_dir(ext_dir.join("my-app")).expect("create extension dir");
    fs::create_dir(ext_dir.join("base-tools")).expect("create extension dir");

    let socket_path = temp_dir.path().join("avocadoctl.sock");
    let socket_address = format!("unix:{}", socket_path.display());
    let original_path = std::env::var("PATH").unwrap_or_default();
    let test_path = format!("{}:{}", fixtures_path().display(), original_path);

    let mut child = Command::new(get_binary_path())
        .args(["serve", "--address", &socket_address])
        .env("AVOCADO_TEST_MODE", "1")
        .env("AVOCADO_EXTENSIONS_PATH", ext_dir.to_str().unwrap())
        .env("PATH", &test_path)
        .spawn()
        .expect("spawn daemon");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(socket_path.exists(), "socket should appear");

    // Client must NOT have AVOCADO_TEST_MODE set so it routes through varlink.
    let output = Command::new(get_binary_path())
        .args(["--socket", &socket_address, "ext", "list"])
        .output()
        .expect("run cli");

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        output.status.success(),
        "ext list should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("my-app"),
        "Should list my-app extension; got: {stdout}"
    );
    assert!(
        stdout.contains("base-tools"),
        "Should list base-tools extension; got: {stdout}"
    );
}

/// `ext merge` routed through the daemon calls the mock systemd-sysext.
#[test]
fn test_ext_merge_via_daemon() {
    let daemon = TestDaemon::start();
    let output = daemon.run(&["ext", "merge"]);

    // In AVOCADO_TEST_MODE, merge uses mock-systemd-sysext which succeeds.
    assert!(
        output.status.success(),
        "ext merge via daemon should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `ext status` routed through the daemon returns status data.
#[test]
fn test_ext_status_via_daemon() {
    let daemon = TestDaemon::start();
    let output = daemon.run(&["ext", "status"]);

    assert!(
        output.status.success(),
        "ext status via daemon should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn synthetic_kab(qualifier: &str) -> Vec<u8> {
    let identifier = b"5f0c2a8e-1d3b-4e6f-8a9b-0c1d2e3f4a5b".as_slice();
    let kab_type = b"kos.layer".as_slice();
    let name = b"app".as_slice();
    let certs = b"".as_slice();
    let version = b"0.1.0".as_slice();
    let lens = [
        identifier.len(),
        kab_type.len(),
        qualifier.len(),
        name.len(),
        certs.len(),
        version.len(),
    ];

    let mut kab = b"KAB\0\x01\x00".to_vec();
    kab.extend_from_slice(&((36 + lens.iter().sum::<usize>()) as u16).to_be_bytes());
    for len in lens {
        kab.extend_from_slice(&(len as u16).to_be_bytes());
    }
    kab.extend_from_slice(&[0u8; 16]);
    for part in [
        identifier,
        kab_type,
        qualifier.as_bytes(),
        name,
        version,
        certs,
    ] {
        kab.extend_from_slice(part);
    }

    let payload = b"layer";
    let offset = kab.len() as u32;
    kab.extend_from_slice(payload);

    let mut directory = Vec::new();
    directory.extend_from_slice(&9u16.to_be_bytes());
    directory.extend_from_slice(b"layer.img");
    directory.extend_from_slice(&offset.to_be_bytes());
    directory.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    directory.extend_from_slice(&[0u8; 7]);
    kab.extend_from_slice(&directory);

    kab.extend_from_slice(&0u16.to_be_bytes());
    kab.extend_from_slice(&1u16.to_be_bytes());
    kab.extend_from_slice(&(directory.len() as u32).to_be_bytes());
    kab.extend_from_slice(&0x1122_3344u32.to_be_bytes());
    kab.extend_from_slice(&[0u8; 256]);
    kab
}

#[test]
fn test_status_via_daemon_shows_kab_qualifiers() {
    let temp_dir = TempDir::new().expect("temp dir");
    let base = temp_dir.path().join("base");
    let runtime_dir = base.join("runtimes").join("rt-1");
    let images = base.join("images");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&images).unwrap();

    let kab_id = "5f0c2a8e-1d3b-4e6f-8a9b-0c1d2e3f4a5b";
    let raw_id = "0b9d3c4e-2f1a-5b6c-8d7e-9f0a1b2c3d4e";
    fs::write(
        images.join(format!("{kab_id}.kab")),
        synthetic_kab("cm4-arm64"),
    )
    .unwrap();
    fs::write(images.join(format!("{raw_id}.raw")), b"raw").unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 1,
        "id": "rt-1",
        "built_at": "2026-11-03T00:00:00Z",
        "runtime": { "name": "dev", "version": "0.1.0" },
        "extensions": [
            { "name": "app", "version": "0.1.0", "image_id": kab_id, "image_type": "kab" },
            { "name": "plain", "version": "0.1.0", "image_id": raw_id }
        ]
    });
    fs::write(runtime_dir.join("manifest.json"), manifest.to_string()).unwrap();
    std::os::unix::fs::symlink("runtimes/rt-1", base.join("active")).unwrap();

    let daemon = TestDaemon::start_with_env(&[
        ("AVOCADO_BASE_DIR", base.as_path()),
        ("TMPDIR", temp_dir.path()),
    ]);

    let output = daemon.run(&["status"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "status via daemon should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = |prefix: &str| -> Vec<String> {
        stdout
            .lines()
            .find(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} row in:\n{stdout}"))
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    assert_eq!(row("Extension")[3], "Qualifier", "{stdout}");
    assert_eq!(row("app-0.1.0")[3], "cm4-arm64", "{stdout}");
    assert_eq!(row("plain-0.1.0")[3], "-", "{stdout}");

    let output = daemon.run(&["-o", "json", "ext", "status"]);
    let list: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ext status -o json prints JSON");
    let qualifier = |name: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .map(|e| e["qualifier"].clone())
    };
    assert_eq!(qualifier("app"), Some(serde_json::json!("cm4-arm64")));
    assert_eq!(qualifier("plain"), Some(serde_json::Value::Null));
}

/// Top-level `merge` alias is routed through the daemon.
#[test]
fn test_merge_alias_via_daemon() {
    let daemon = TestDaemon::start();
    let output = daemon.run(&["merge"]);

    assert!(
        output.status.success(),
        "merge alias via daemon should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// When no daemon is running, the CLI prints a helpful "Daemon Not Running" error.
#[test]
fn test_no_daemon_shows_helpful_error() {
    let temp_dir = TempDir::new().expect("temp dir");
    let nonexistent_socket = format!("unix:{}/nonexistent.sock", temp_dir.path().display());

    let output = Command::new(get_binary_path())
        .args(["--socket", &nonexistent_socket, "ext", "list"])
        .output()
        .expect("run cli");

    assert!(
        !output.status.success(),
        "Should fail when daemon is not running"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Daemon Not Running") || stderr.contains("Cannot connect"),
        "Should show daemon-not-running error; got: {stderr}"
    );
}

/// Two concurrent CLI invocations both succeed — the daemon serialises them.
#[test]
fn test_concurrent_requests_serialised_by_daemon() {
    let daemon = TestDaemon::start();
    let socket = daemon.socket_address();

    // Spawn two merge requests at the same time.
    // Clients must NOT set AVOCADO_TEST_MODE — they must route through varlink.
    let mut handles = Vec::new();
    for _ in 0..2 {
        let socket_clone = socket.clone();
        let bin = get_binary_path();
        handles.push(std::thread::spawn(move || {
            Command::new(bin)
                .args(["--socket", &socket_clone, "ext", "merge"])
                .output()
                .expect("run cli")
        }));
    }

    for handle in handles {
        let output = handle.join().expect("thread panicked");
        assert!(
            output.status.success(),
            "Concurrent merge should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
