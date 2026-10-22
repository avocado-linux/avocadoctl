use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Helper function to get the path to the built binary
fn get_binary_path() -> PathBuf {
    let mut path = std::env::current_dir().expect("Failed to get current directory");
    path.push("target");
    path.push("debug");
    path.push("avocadoctl");
    path
}

/// Build a test-mode command with the mock tools from tests/fixtures on PATH
/// and a private TMPDIR, like the other integration tests.
fn test_mode_command(args: &[&str], temp_dir: &TempDir) -> Command {
    let fixtures_path = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let mut cmd = Command::new(get_binary_path());
    cmd.args(args)
        .env("AVOCADO_TEST_MODE", "1")
        .env(
            "PATH",
            format!("{}:{original_path}", fixtures_path.display()),
        )
        .env("TMPDIR", temp_dir.path());
    cmd
}

/// Whether `pid` has SIGPIPE in its ignored signal mask (Linux /proc).
fn ignores_sigpipe(pid: u32) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read /proc status");
    let mask = status
        .lines()
        .find_map(|l| l.strip_prefix("SigIgn:"))
        .expect("SigIgn line in /proc status");
    let mask = u64::from_str_radix(mask.trim(), 16).expect("SigIgn is hex");
    mask & (1 << (libc::SIGPIPE - 1)) != 0
}

fn wait_for(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `avocadoctl status | head -1`: the reader goes away before the CLI is done
/// writing. The CLI must stop quietly, as other Unix tools do, and not panic
/// with "failed printing to stdout: Broken pipe".
///
/// The read end is closed before the process starts, so the first write always
/// fails. That is the same error `head` causes, without the timing race.
#[test]
fn test_cli_output_to_closed_pipe_does_not_panic() {
    let commands: &[&[&str]] = &[
        &["status"],
        &["-o", "json", "status"],
        &["ext", "status"],
        &["ext", "list"],
        &[],
    ];
    for args in commands {
        let temp_dir = TempDir::new().expect("Failed to create temp directory");
        let (reader, writer) = std::io::pipe().expect("create pipe");
        drop(reader);
        let output = test_mode_command(args, &temp_dir)
            .stdout(writer)
            .stderr(Stdio::piped())
            .output()
            .expect("Failed to execute avocadoctl");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("panicked"),
            "avocadoctl {args:?} panicked on a closed pipe: {stderr}"
        );
        assert!(
            output.status.success() || output.status.signal() == Some(libc::SIGPIPE),
            "avocadoctl {args:?} should exit 0 or stop on SIGPIPE, got {:?}",
            output.status
        );
    }
}

/// The varlink daemon keeps SIGPIPE ignored. Its stdout and stderr go to the
/// journal, and a write there that fails must give an error, not a signal
/// that kills the daemon.
#[test]
fn test_serve_keeps_sigpipe_ignored() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let socket = temp_dir.path().join("avocadoctl.sock");
    let address = format!("unix:{}", socket.display());
    let child = test_mode_command(&["serve", "--address", &address], &temp_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start avocadoctl serve");
    wait_for(&socket, Duration::from_secs(10));

    let ignored = ignores_sigpipe(child.id());
    stop(child);
    assert!(ignored, "avocadoctl serve must keep SIGPIPE ignored");
}

/// The HITL watchdog runs in the background for as long as a mount lives, so
/// it keeps SIGPIPE ignored like the daemon.
#[test]
fn test_hitl_watchdog_keeps_sigpipe_ignored() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let child = test_mode_command(
        &[
            "hitl",
            "watchdog",
            "--server-ip",
            "127.0.0.1",
            "--server-port",
            "12049",
            "--extension",
            "test-ext",
        ],
        &temp_dir,
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("Failed to start avocadoctl hitl watchdog");
    // The watchdog sleeps before its first check; give main() time to run.
    std::thread::sleep(Duration::from_millis(500));

    let ignored = ignores_sigpipe(child.id());
    stop(child);
    assert!(
        ignored,
        "avocadoctl hitl watchdog must keep SIGPIPE ignored"
    );
}
