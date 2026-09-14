use std::process::Command;
use tempfile::TempDir;

/// Helper function to run avocadoctl with environment variables
fn run_avocadoctl_with_env(args: &[&str], env_vars: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new("./target/debug/avocadoctl");
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    cmd.args(args)
        .output()
        .expect("Failed to execute avocadoctl")
}

/// Helper function to run avocadoctl
fn run_avocadoctl(args: &[&str]) -> std::process::Output {
    Command::new("./target/debug/avocadoctl")
        .args(args)
        .output()
        .expect("Failed to execute avocadoctl")
}

/// Helper function to run avocadoctl with an isolated test environment
/// This creates unique temporary directories to avoid race conditions between parallel tests
fn run_avocadoctl_with_isolated_env(
    args: &[&str],
    additional_env_vars: &[(&str, &str)],
) -> (std::process::Output, TempDir) {
    // Create a unique temporary directory for this test
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path().to_string_lossy();

    // Set up isolated environment variables
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    let mut env_vars = vec![
        ("AVOCADO_TEST_MODE", "1"),
        ("PATH", new_path.as_str()),
        ("TMPDIR", temp_path.as_ref()),
    ];

    // Add additional environment variables
    env_vars.extend(additional_env_vars);

    let output = run_avocadoctl_with_env(args, &env_vars);
    (output, temp_dir)
}

/// Test hitl help command
#[test]
fn test_hitl_help() {
    let output = run_avocadoctl(&["hitl", "--help"]);
    assert!(output.status.success(), "Hitl help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hardware-in-the-loop (HITL) testing commands"),
        "Should contain HITL description"
    );
    assert!(stdout.contains("mount"), "Should mention mount subcommand");
}

/// Test hitl mount help command
#[test]
fn test_hitl_mount_help() {
    let output = run_avocadoctl(&["hitl", "mount", "--help"]);
    assert!(output.status.success(), "Hitl mount help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Mount NFS extensions from a remote server"),
        "Should contain mount description"
    );
    assert!(
        stdout.contains("--server-ip"),
        "Should mention server-ip option"
    );
    assert!(
        stdout.contains("--server-port"),
        "Should mention server-port option"
    );
    assert!(
        stdout.contains("--extension"),
        "Should mention extension option"
    );
    assert!(
        stdout.contains("-s, --server-ip"),
        "Should show short option for server-ip"
    );
    assert!(
        stdout.contains("-p, --server-port"),
        "Should show short option for server-port"
    );
    assert!(
        stdout.contains("-e, --extension"),
        "Should show short option for extension"
    );
}

/// Test hitl mount command with mock
#[test]
fn test_hitl_mount_with_mocks() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    // Create a temporary directory to simulate /var/lib/avocado/extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path();
    let temp_path = temp_dir.path().to_string_lossy();

    // Add fixtures path to PATH so mock binaries can be found
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    let output = run_avocadoctl_with_env(
        &[
            "hitl",
            "mount",
            "--server-ip",
            "192.168.1.10",
            "--server-port",
            "12049",
            "--extension",
            "foo",
            "--extension",
            "avocado-dev",
            "--verbose",
        ],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_path),
            (
                "AVOCADO_EXTENSIONS_PATH",
                &temp_extensions_dir.to_string_lossy(),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "Hitl mount should succeed with mocks: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Mounting extensions from 192.168.1.10:12049"),
        "Should show mounting message"
    );
    assert!(
        stdout.contains("Setting up extension: foo"),
        "Should show setup for foo extension"
    );
    assert!(
        stdout.contains("Setting up extension: avocado-dev"),
        "Should show setup for avocado-dev extension"
    );
    assert!(
        stdout.contains("All extensions mounted successfully"),
        "Should show success message"
    );
    assert!(
        stdout.contains("Refreshing extensions to apply mounted changes"),
        "Should show extension refresh message"
    );
    assert!(
        stdout.contains("Starting extension refresh process"),
        "Should call ext refresh"
    );
    assert!(
        stdout.contains("Extensions refreshed successfully"),
        "Should complete extension refresh"
    );
    // Not asserted: the verbose per-source scan lines ("Scanning HITL
    // extensions in ..."). They come from inside the refresh, which now runs
    // through the service layer whose output is collected rather than
    // printed; the success line above is what proves the refresh ran.
}

/// Test hitl mount with short options
#[test]
fn test_hitl_mount_short_options() {
    // Create a temporary directory to simulate /var/lib/avocado/extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path();

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "hitl",
            "mount",
            "-s",
            "192.168.1.20",
            "-p",
            "2049",
            "-e",
            "test-ext",
            "-v",
        ],
        &[(
            "AVOCADO_EXTENSIONS_PATH",
            &temp_extensions_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "Hitl mount with short options should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Mounting extensions from 192.168.1.20:2049"),
        "Should show correct server and port"
    );
    assert!(
        stdout.contains("Setting up extension: test-ext"),
        "Should show setup for test-ext extension"
    );
}

/// Test hitl mount missing required arguments
#[test]
fn test_hitl_mount_missing_args() {
    let output = run_avocadoctl(&["hitl", "mount"]);
    assert!(
        !output.status.success(),
        "Hitl mount should fail without required arguments"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("required") || stderr.contains("missing"),
        "Should show error about missing required arguments"
    );
}

/// Test hitl mount with default port
#[test]
fn test_hitl_mount_default_port() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    // Create a temporary directory to simulate /var/lib/avocado/extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path();
    let temp_path = temp_dir.path().to_string_lossy();

    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    let output = run_avocadoctl_with_env(
        &[
            "hitl",
            "mount",
            "--server-ip",
            "192.168.1.30",
            "--extension",
            "default-port-test",
            "--verbose",
        ],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_path),
            (
                "AVOCADO_EXTENSIONS_PATH",
                &temp_extensions_dir.to_string_lossy(),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "Hitl mount should succeed with default port"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Mounting extensions from 192.168.1.30:12049"),
        "Should use default port 12049"
    );
}

/// Test hitl unmount help command
#[test]
fn test_hitl_unmount_help() {
    let output = run_avocadoctl(&["hitl", "unmount", "--help"]);
    assert!(output.status.success(), "Hitl unmount help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unmount NFS extensions"),
        "Should contain unmount description"
    );
    assert!(
        stdout.contains("--extension"),
        "Should mention extension option"
    );
    assert!(
        stdout.contains("-e, --extension"),
        "Should show short option for extension"
    );
}

/// Test hitl unmount command with mock
#[test]
fn test_hitl_unmount_with_mocks() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    // Create a temporary directory to simulate /var/lib/avocado/extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path();
    let temp_path = temp_dir.path().to_string_lossy();

    // Add fixtures path to PATH so mock binaries can be found
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    let output = run_avocadoctl_with_env(
        &[
            "hitl",
            "unmount",
            "--extension",
            "foo",
            "--extension",
            "avocado-dev",
            "--verbose",
        ],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_path),
            (
                "AVOCADO_EXTENSIONS_PATH",
                &temp_extensions_dir.to_string_lossy(),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "Hitl unmount should succeed with mocks: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unmounting 2 extension(s)"),
        "Should show unmounting message"
    );
    assert!(
        stdout.contains("Unmerging extensions"),
        "Should show unmerge step"
    );
    assert!(
        stdout.contains("Unmounting extension: foo"),
        "Should show unmount for foo extension"
    );
    assert!(
        stdout.contains("Unmounting extension: avocado-dev"),
        "Should show unmount for avocado-dev extension"
    );
    assert!(
        stdout.contains("All extensions unmounted successfully"),
        "Should show success message"
    );
    assert!(
        stdout.contains("Starting extension merge process"),
        "Should show merge step at the end"
    );
}

/// Test hitl unmount with short options
#[test]
fn test_hitl_unmount_short_options() {
    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path();

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["hitl", "unmount", "-e", "foo", "--verbose"],
        &[(
            "AVOCADO_EXTENSIONS_PATH",
            &temp_extensions_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "Hitl unmount should succeed with short options"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unmounting 1 extension(s)"),
        "Should show unmounting single extension"
    );
}

/// Test that main help shows hitl command
#[test]
fn test_main_help_shows_hitl() {
    let output = run_avocadoctl(&["--help"]);
    assert!(output.status.success(), "Main help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hitl"),
        "Main help should mention hitl command"
    );
}

/// Test that hitl help shows both mount and unmount
#[test]
fn test_hitl_help_shows_both_subcommands() {
    let output = run_avocadoctl(&["hitl", "--help"]);
    assert!(output.status.success(), "Hitl help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mount"), "Should mention mount subcommand");
    assert!(
        stdout.contains("unmount"),
        "Should mention unmount subcommand"
    );
}

/// Test that failed HITL mount operations clean up directories
#[test]
fn test_hitl_mount_failure_cleanup() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_extensions_dir = temp_dir.path().join("avocado/hitl");

    // Create a failing mock-systemd-mount script in a temp directory
    let temp_bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir_all(&temp_bin_dir).expect("Failed to create temp bin directory");

    let mock_mount_fail_path = temp_bin_dir.join("mock-systemd-mount");
    std::fs::write(
        &mock_mount_fail_path,
        r#"#!/bin/bash
# Mock systemd-mount command that fails
echo "Failed to mount 10.0.2.2:/test-extension: No such file or directory" >&2
exit 1
"#,
    )
    .expect("Failed to write failing mock-systemd-mount");

    // Make it executable
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&mock_mount_fail_path)
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&mock_mount_fail_path, perms).unwrap();

    // Add temp bin path to PATH (before fixtures so our failing mock takes precedence)
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!(
        "{}:{}:{}",
        temp_bin_dir.to_string_lossy(),
        fixtures_path.to_string_lossy(),
        original_path
    );

    let output = run_avocadoctl_with_env(
        &["hitl", "mount", "-s", "10.0.2.2", "-e", "test-extension"],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_dir.path().to_string_lossy()),
        ],
    );

    // The mount should fail
    assert!(
        !output.status.success(),
        "Hitl mount should fail with mock failure"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Failed to mount extension test-extension"),
        "Should show mount failure message"
    );

    // Verify the directory was cleaned up - it should not exist
    let extension_dir = temp_extensions_dir.join("test-extension");
    assert!(
        !extension_dir.exists(),
        "Extension directory should be cleaned up after mount failure"
    );
}

/// Test that HITL mount creates service drop-ins when extension has AVOCADO_ENABLE_SERVICES
#[test]
fn test_hitl_mount_creates_service_dropins() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");

    // Create extension directory with metadata containing AVOCADO_ENABLE_SERVICES
    let extension_dir = temp_dir.path().join("avocado/hitl/test-ext");
    let release_dir = extension_dir.join("usr/lib/extension-release.d");
    std::fs::create_dir_all(&release_dir).expect("Failed to create release directory");

    let release_file = release_dir.join("extension-release.test-ext");
    std::fs::write(
        &release_file,
        r#"ID=extension-release.test-ext
VERSION_ID=1.0
DESCRIPTION="Test Extension with Services"
AVOCADO_ENABLE_SERVICES="nginx prometheus"
"#,
    )
    .expect("Failed to write release file");

    // Run a mock mount that just succeeds (the directory is already created)
    let output = run_avocadoctl_with_env(
        &[
            "hitl",
            "mount",
            "-s",
            "10.0.2.2",
            "-e",
            "test-ext",
            "--verbose",
        ],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_dir.path().to_string_lossy()),
        ],
    );

    assert!(output.status.success(), "Hitl mount should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Found 2 enabled service(s)"),
        "Should detect enabled services. Got: {stdout}"
    );
    assert!(
        stdout.contains("nginx") && stdout.contains("prometheus"),
        "Should list the services. Got: {stdout}"
    );
    assert!(
        stdout.contains("Created drop-in"),
        "Should create drop-ins. Got: {stdout}"
    );

    // Verify drop-in files were created
    let systemd_dir = temp_dir.path().join("run/systemd/system");
    let nginx_dropin = systemd_dir.join("nginx.service.d/10-hitl-test-ext.conf");
    let prometheus_dropin = systemd_dir.join("prometheus.service.d/10-hitl-test-ext.conf");

    assert!(
        nginx_dropin.exists(),
        "Nginx drop-in should exist at {nginx_dropin:?}"
    );
    assert!(
        prometheus_dropin.exists(),
        "Prometheus drop-in should exist at {prometheus_dropin:?}"
    );

    // Verify drop-in content
    let nginx_content =
        std::fs::read_to_string(&nginx_dropin).expect("Failed to read nginx drop-in");
    assert!(
        nginx_content.contains("[Unit]"),
        "Drop-in should have [Unit] section"
    );
    assert!(
        nginx_content.contains("RequiresMountsFor="),
        "Drop-in should have RequiresMountsFor"
    );
    assert!(
        nginx_content.contains("BindsTo="),
        "Drop-in should have BindsTo"
    );
    assert!(
        nginx_content.contains("After="),
        "Drop-in should have After"
    );
}

/// Test that HITL unmount cleans up service drop-ins
#[test]
fn test_hitl_unmount_cleans_service_dropins() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");

    // Create extension directory with metadata
    let extension_dir = temp_dir.path().join("avocado/hitl/cleanup-ext");
    let release_dir = extension_dir.join("usr/lib/extension-release.d");
    std::fs::create_dir_all(&release_dir).expect("Failed to create release directory");

    let release_file = release_dir.join("extension-release.cleanup-ext");
    std::fs::write(
        &release_file,
        r#"ID=extension-release.cleanup-ext
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="redis"
"#,
    )
    .expect("Failed to write release file");

    // Pre-create the drop-in file to simulate a previous mount
    let systemd_dir = temp_dir.path().join("run/systemd/system");
    let dropin_dir = systemd_dir.join("redis.service.d");
    std::fs::create_dir_all(&dropin_dir).expect("Failed to create drop-in directory");
    let dropin_file = dropin_dir.join("10-hitl-cleanup-ext.conf");
    std::fs::write(
        &dropin_file,
        "[Unit]\nRequiresMountsFor=/run/avocado/hitl/cleanup-ext\n",
    )
    .expect("Failed to write drop-in");

    assert!(dropin_file.exists(), "Drop-in should exist before unmount");

    // Run unmount
    let output = run_avocadoctl_with_env(
        &["hitl", "unmount", "-e", "cleanup-ext", "--verbose"],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_dir.path().to_string_lossy()),
        ],
    );

    assert!(output.status.success(), "Hitl unmount should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Removed drop-in"),
        "Should remove drop-ins. Got: {stdout}"
    );

    // Verify drop-in file was removed
    assert!(
        !dropin_file.exists(),
        "Drop-in file should be removed after unmount"
    );
}

/// Test that ext refresh invalidates HITL NFS caches
/// This verifies that when HITL-mounted extensions exist, refresh will
/// attempt to invalidate the NFS cache for each mount before merging.
#[test]
fn test_ext_refresh_invalidates_hitl_caches() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    // Create a temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path().to_string_lossy().to_string();

    // Create HITL mount directory with a mock extension
    let hitl_dir = temp_dir.path().join("avocado/hitl");
    let extension_dir = hitl_dir.join("my-hitl-ext");
    let release_dir = extension_dir.join("usr/lib/extension-release.d");
    std::fs::create_dir_all(&release_dir).expect("Failed to create release directory");

    let release_file = release_dir.join("extension-release.my-hitl-ext");
    std::fs::write(
        &release_file,
        r#"ID=avocado
VERSION_ID=1.0
"#,
    )
    .expect("Failed to write release file");

    // Create extensions directory (required for merge)
    let extensions_dir = temp_dir.path().join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create os-releases directory (required for enable/disable)
    let os_releases_dir = temp_dir.path().join("os-releases");
    std::fs::create_dir_all(&os_releases_dir).expect("Failed to create os-releases directory");

    // Run refresh with verbose output
    let output = run_avocadoctl_with_env(
        &["ext", "refresh", "--verbose"],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_path),
            ("AVOCADO_TEST_TMPDIR", &temp_path),
            ("AVOCADO_EXTENSIONS_DIR", &extensions_dir.to_string_lossy()),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // In test mode, we should see the cache invalidation message
    assert!(
        stdout.contains("Invalidating NFS cache for extension: my-hitl-ext")
            || stdout.contains("Skipping remount in test mode"),
        "Should attempt to invalidate HITL cache. stdout: {stdout}, stderr: {stderr}"
    );
}

/// Test that ext refresh works normally when no HITL mounts exist
#[test]
fn test_ext_refresh_no_hitl_mounts() {
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", fixtures_path.to_string_lossy(), original_path);

    // Create a temporary directory WITHOUT any HITL mounts
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path().to_string_lossy().to_string();

    // Create extensions directory (required for merge)
    let extensions_dir = temp_dir.path().join("extensions");
    std::fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Run refresh
    let output = run_avocadoctl_with_env(
        &["ext", "refresh", "--verbose"],
        &[
            ("AVOCADO_TEST_MODE", "1"),
            ("PATH", &new_path),
            ("TMPDIR", &temp_path),
            ("AVOCADO_TEST_TMPDIR", &temp_path),
            ("AVOCADO_EXTENSIONS_DIR", &extensions_dir.to_string_lossy()),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should NOT have any HITL cache invalidation messages
    assert!(
        !stdout.contains("Invalidating NFS cache"),
        "Should not attempt cache invalidation when no HITL mounts exist. stdout: {stdout}"
    );

    // But refresh should still succeed
    assert!(
        stdout.contains("Extensions refreshed successfully")
            || stdout.contains("Extensions merged"),
        "Refresh should complete successfully. stdout: {stdout}"
    );
}
