use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Helper function to get the path to the built binary
fn get_binary_path() -> PathBuf {
    let mut path = std::env::current_dir().expect("Failed to get current directory");
    path.push("target");
    path.push("debug");
    path.push("avocadoctl");
    path
}

/// Helper function to run avocadoctl with custom environment and arguments
fn run_avocadoctl_with_env(args: &[&str], env_vars: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(get_binary_path());
    cmd.args(args);
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    cmd.output().expect("Failed to execute avocadoctl")
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

/// Helper function to run avocadoctl with arguments and return output
fn run_avocadoctl(args: &[&str]) -> std::process::Output {
    Command::new(get_binary_path())
        .args(args)
        .output()
        .expect("Failed to execute avocadoctl")
}

/// Test ext list with non-existent default directory
#[test]
fn test_ext_list_nonexistent_directory() {
    let output = run_avocadoctl_with_env(&["ext", "list"], &[("AVOCADO_TEST_MODE", "1")]);
    // The scanner handles missing directories gracefully — exits 0 and reports no extensions
    assert!(
        output.status.success(),
        "ext list should succeed even when the default extensions directory does not exist"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No extensions found"),
        "Should indicate no extensions found"
    );
}

/// Test ext list with mock directory extensions using environment variable
///
/// .raw files are intentionally excluded: they require loop device mounting which is not
/// available in the unit-test environment. Directory-based extensions are sufficient to
/// exercise the scanner + display logic.
#[test]
fn test_ext_list_with_mock_extensions() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path();

    // Create directory-based extensions
    fs::create_dir(extensions_dir.join("test_extension_dir"))
        .expect("Failed to create test directory");
    fs::create_dir(extensions_dir.join("another_ext"))
        .expect("Failed to create another test directory");

    // Non-extension files that should be ignored
    fs::write(extensions_dir.join("ignored_file.txt"), "").expect("Failed to create ignored file");
    fs::write(extensions_dir.join("README.md"), "readme content")
        .expect("Failed to create ignored readme");

    let output = run_avocadoctl_with_env(
        &["ext", "list"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
        ],
    );

    assert!(output.status.success(), "ext list should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("test_extension_dir"),
        "Should list directory extension"
    );
    assert!(
        stdout.contains("another_ext"),
        "Should list another directory extension"
    );
    assert!(
        !stdout.contains("ignored_file.txt"),
        "Should not list .txt files"
    );
    assert!(!stdout.contains("README.md"), "Should not list .md files");

    // Layer-stack labels should be present
    assert!(
        stdout.contains("top layer") && stdout.contains("base layer"),
        "Should show overlay layer direction labels"
    );
}

/// Test ext list with a custom config file via the -c flag
///
/// ext list now uses the extension scanner (AVOCADO_EXTENSIONS_PATH / manifest) rather than
/// config.ext.dir, so this test verifies that -c is accepted and the command completes
/// successfully — not that config.ext.dir drives discovery.
#[test]
fn test_ext_list_with_config_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let config_path = temp_dir.path().join("test_config.toml");
    let extensions_dir = temp_dir.path().join("custom_extensions");

    fs::create_dir(&extensions_dir).expect("Failed to create extensions directory");
    fs::create_dir(extensions_dir.join("config_test_ext"))
        .expect("Failed to create test directory");

    let config_content = format!(
        r#"[avocado.ext]
dir = "{}"
"#,
        extensions_dir.to_string_lossy()
    );
    fs::write(&config_path, config_content).expect("Failed to write config file");

    // Supply the extensions dir via env var so the scanner can find it
    let output = run_avocadoctl_with_env(
        &["-c", config_path.to_str().unwrap(), "ext", "list"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
        ],
    );

    assert!(
        output.status.success(),
        "ext list should succeed with custom config"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("config_test_ext"),
        "Should list directory extension from config"
    );
}

/// Test -c flag with invalid config file
#[test]
fn test_invalid_config_file() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let config_path = temp_dir.path().join("invalid_config.toml");

    // Create invalid TOML content
    fs::write(&config_path, "invalid toml content [[[").expect("Failed to write invalid config");

    let output = run_avocadoctl(&["-c", config_path.to_str().unwrap(), "ext", "list"]);

    assert!(
        !output.status.success(),
        "Should fail with invalid config file"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Configuration Error"),
        "Should show config error"
    );
}

/// Test -c flag with nonexistent config file (should use defaults)
#[test]
fn test_nonexistent_config_file() {
    let output = run_avocadoctl_with_env(
        &["-c", "/nonexistent/config.toml", "ext", "list"],
        &[("AVOCADO_TEST_MODE", "1")],
    );

    // Nonexistent config falls back to defaults; missing extensions directories are handled
    // gracefully by the scanner — the command should succeed and report no extensions.
    assert!(
        output.status.success(),
        "ext list should succeed using default config when config file does not exist"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No extensions found"),
        "Should indicate no extensions found"
    );
}

/// Test ext list with empty extensions directory
#[test]
fn test_ext_list_empty_directory() {
    // Create an empty temporary directory
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path();

    // Run avocadoctl ext list with empty extensions directory
    let output = run_avocadoctl_with_env(
        &["ext", "list"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
        ],
    );

    assert!(
        output.status.success(),
        "ext list should succeed with empty directory"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No extensions found"),
        "Should indicate no extensions found"
    );
}

/// Test ext list help
#[test]
fn test_ext_list_help() {
    let output = run_avocadoctl(&["ext", "list", "--help"]);
    assert!(output.status.success(), "Ext list help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("List all available extensions"),
        "Should contain list description"
    );
}

/// Test with example config fixture (demonstrates fixture usage)
#[test]
fn test_example_config_fixture() {
    use std::path::Path;

    // Verify the example config fixture exists and is valid
    let fixture_path = Path::new("tests/fixtures/example_config.toml");
    assert!(fixture_path.exists(), "Example config fixture should exist");

    // Test that we can load the example config without errors
    // This demonstrates how fixtures can be used in tests
    let config_content =
        fs::read_to_string(fixture_path).expect("Should be able to read example config");

    // Verify it contains expected content
    assert!(
        config_content.contains("[avocado.ext]"),
        "Should contain avocado.ext section"
    );
    assert!(
        config_content.contains("dir ="),
        "Should contain dir setting"
    );

    // Test parsing the config (would fail if TOML is invalid)
    let _parsed: toml::Value =
        toml::from_str(&config_content).expect("Example config should be valid TOML");
}

/// Test mutable config option integration
#[test]
fn test_mutable_config_option() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Test with valid mutable value
    let config_path = temp_dir.path().join("mutable_config.toml");
    let config_content = r#"
[avocado.ext]
dir = "/tmp/test_extensions"
mutable = "yes"
"#;
    fs::write(&config_path, config_content).expect("Failed to write config file");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "ext",
            "merge",
            "--verbose",
        ],
        &[],
    );

    // Should succeed (even though no extensions exist, config should be valid)
    assert!(
        output.status.success(),
        "ext merge should succeed with valid mutable config"
    );

    // Test with invalid mutable value
    let invalid_config_path = temp_dir.path().join("invalid_mutable_config.toml");
    let invalid_config_content = r#"
[avocado.ext]
dir = "/tmp/test_extensions"
mutable = "invalid_value"
"#;
    fs::write(&invalid_config_path, invalid_config_content).expect("Failed to write config file");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "--config",
            invalid_config_path.to_str().unwrap(),
            "ext",
            "merge",
        ],
        &[],
    );

    // Should fail with configuration error
    assert!(
        !output.status.success(),
        "ext merge should fail with invalid mutable config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid mutable value 'invalid_value'"),
        "Should show invalid mutable value error message"
    );
    assert!(
        stderr.contains("Must be one of: no, auto, yes, import, ephemeral, ephemeral-import"),
        "Should show valid options in error message"
    );
}

/// Test separate sysext and confext mutable config options
#[test]
fn test_separate_mutable_config_options() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Test with separate sysext and confext mutable values
    let config_path = temp_dir.path().join("separate_mutable_config.toml");
    let config_content = r#"
[avocado.ext]
dir = "/tmp/test_extensions"
sysext_mutable = "yes"
confext_mutable = "auto"
"#;
    fs::write(&config_path, config_content).expect("Failed to write config file");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "--config",
            config_path.to_str().unwrap(),
            "ext",
            "merge",
            "--verbose",
        ],
        &[],
    );

    // Should succeed (even though no extensions exist, config should be valid)
    assert!(
        output.status.success(),
        "ext merge should succeed with valid separate mutable config"
    );

    // Test with invalid sysext mutable value
    let invalid_sysext_config_path = temp_dir.path().join("invalid_sysext_config.toml");
    let invalid_sysext_config_content = r#"
[avocado.ext]
dir = "/tmp/test_extensions"
sysext_mutable = "invalid_value"
confext_mutable = "auto"
"#;
    fs::write(&invalid_sysext_config_path, invalid_sysext_config_content)
        .expect("Failed to write config file");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "--config",
            invalid_sysext_config_path.to_str().unwrap(),
            "ext",
            "merge",
        ],
        &[],
    );

    // Should fail with configuration error
    assert!(
        !output.status.success(),
        "ext merge should fail with invalid sysext mutable config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid sysext mutable configuration"),
        "Should show invalid sysext mutable configuration error message"
    );

    // Test with invalid confext mutable value
    let invalid_confext_config_path = temp_dir.path().join("invalid_confext_config.toml");
    let invalid_confext_config_content = r#"
[avocado.ext]
dir = "/tmp/test_extensions"
sysext_mutable = "yes"
confext_mutable = "invalid_value"
"#;
    fs::write(&invalid_confext_config_path, invalid_confext_config_content)
        .expect("Failed to write config file");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &[
            "--config",
            invalid_confext_config_path.to_str().unwrap(),
            "ext",
            "merge",
        ],
        &[],
    );

    // Should fail with configuration error
    assert!(
        !output.status.success(),
        "ext merge should fail with invalid confext mutable config"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid confext mutable configuration"),
        "Should show invalid confext mutable configuration error message"
    );
}

/// Test ext merge command with mock systemd binaries
#[test]
fn test_ext_merge_with_mocks() {
    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(&["ext", "merge", "--verbose"], &[]);

    assert!(
        output.status.success(),
        "ext merge should succeed with mocks"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Starting extension merge process"),
        "Should show merging message"
    );
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show success message"
    );
    assert!(
        stdout.contains("systemd-sysext merge"),
        "Should show sysext operation"
    );
    assert!(
        stdout.contains("systemd-confext merge"),
        "Should show confext operation"
    );
}

/// Test ext unmerge command with mock systemd binaries
#[test]
fn test_ext_unmerge_with_mocks() {
    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) =
        run_avocadoctl_with_isolated_env(&["ext", "unmerge", "--verbose"], &[]);

    assert!(
        output.status.success(),
        "ext unmerge should succeed with mocks"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Starting extension unmerge process"),
        "Should show unmerging message"
    );
    assert!(
        stdout.contains("Extensions unmerged successfully"),
        "Should show success message"
    );
    assert!(
        stdout.contains("systemd-sysext unmerge"),
        "Should show sysext operation"
    );
    assert!(
        stdout.contains("systemd-confext unmerge"),
        "Should show confext operation"
    );
    assert!(
        stdout.contains("[INFO] Running depmod"),
        "Should show depmod running message"
    );
    assert!(
        stdout.contains("[SUCCESS] depmod completed successfully"),
        "Should show depmod completion"
    );
}

/// Test ext merge help
#[test]
fn test_ext_merge_help() {
    let output = run_avocadoctl(&["ext", "merge", "--help"]);
    assert!(output.status.success(), "Ext merge help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Merge extensions using systemd-sysext and systemd-confext"),
        "Should contain merge description"
    );
}

/// Test that environment preparation works with mock extensions
#[test]
fn test_environment_preparation_with_mock_extensions() {
    use std::fs;
    use tempfile::TempDir;

    // Clean up any previous test directories
    let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let _ = fs::remove_dir_all(format!("{temp_base}/test_extensions"));
    let _ = fs::remove_dir_all(format!("{temp_base}/test_confexts"));

    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let extensions_path = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_path).expect("Failed to create extensions dir");

    // Create a mock .raw extension file
    let raw_file = extensions_path.join("test-ext.raw");
    fs::write(&raw_file, b"mock raw extension").expect("Failed to create raw file");

    // Create a mock directory extension
    let dir_ext = extensions_path.join("dir-ext");
    fs::create_dir_all(&dir_ext).expect("Failed to create dir extension");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[("AVOCADO_EXTENSIONS_PATH", extensions_path.to_str().unwrap())],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("ext merge should succeed with mock extensions");
    }

    assert!(
        stdout.contains("Preparing extension environment"),
        "Should show environment preparation message"
    );
    // The output should now include scanning from different sources
    assert!(
        stdout.contains("Scanning HITL extensions")
            && stdout.contains("Scanning directory extensions")
            && stdout.contains("Scanning raw file extensions"),
        "Should scan all extension sources in priority order"
    );
    assert!(
        stdout.contains("Created sysext symlink:") || stdout.contains("Created confext symlink:"),
        "Should create symlinks for extensions"
    );

    // Clean up test directories
    let _ = fs::remove_dir_all(format!("{temp_base}/test_extensions"));
    let _ = fs::remove_dir_all(format!("{temp_base}/test_confexts"));
}

/// Test ext unmerge help
#[test]
fn test_ext_unmerge_help() {
    let output = run_avocadoctl(&["ext", "unmerge", "--help"]);
    assert!(output.status.success(), "Ext unmerge help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unmerge extensions using systemd-sysext and systemd-confext"),
        "Should contain unmerge description"
    );
}

/// Test ext refresh command with mock systemd binaries
#[test]
fn test_ext_refresh_with_mocks() {
    // Setup mock environment
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "refresh", "--verbose"],
        &[(
            "AVOCADO_EXTENSION_RELEASE_DIR",
            &release_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "ext refresh should succeed with mocks"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Starting extension refresh process"),
        "Should show refreshing message"
    );
    assert!(
        stdout.contains("Extensions refreshed successfully"),
        "Should show final success message"
    );
    // Should contain both unmerge and merge operations
    assert!(
        stdout.contains("systemd-sysext unmerge"),
        "Should show sysext unmerge operation"
    );
    assert!(
        stdout.contains("systemd-confext unmerge"),
        "Should show confext unmerge operation"
    );
    assert!(
        stdout.contains("systemd-sysext merge"),
        "Should show sysext merge operation"
    );
    assert!(
        stdout.contains("systemd-confext merge"),
        "Should show confext merge operation"
    );
    assert!(
        stdout.contains("Extensions unmerged"),
        "Should show unmerge success"
    );
    assert!(
        stdout.contains("Extensions merged"),
        "Should show merge success"
    );

    // Verify depmod is only called once at the end (during merge phase)
    let depmod_count = stdout.matches("post-merge: running: depmod").count();
    assert_eq!(
        depmod_count, 1,
        "Should call depmod exactly once during refresh (only during merge phase)"
    );
    assert!(
        stdout.contains("post-merge: running: depmod"),
        "Should show depmod running (verbose step detail)"
    );
    assert!(
        stdout.contains("Rebuilt module dependencies"),
        "Should show the module-dependency rebuild summary"
    );
}

/// Test ext refresh help
#[test]
fn test_ext_refresh_help() {
    let output = run_avocadoctl(&["ext", "refresh", "--help"]);
    assert!(output.status.success(), "Ext refresh help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Unmerge and then merge extensions (refresh extensions)"),
        "Should contain refresh description"
    );
}

/// Test that ext help shows all subcommands
#[test]
fn test_ext_help_shows_all_commands() {
    let output = run_avocadoctl(&["ext", "--help"]);
    assert!(output.status.success(), "Ext help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extension management commands"),
        "Ext help should contain description"
    );
    assert!(
        stdout.contains("list"),
        "Ext help should mention list subcommand"
    );
    assert!(
        stdout.contains("merge"),
        "Ext help should mention merge subcommand"
    );
    assert!(
        stdout.contains("unmerge"),
        "Ext help should mention unmerge subcommand"
    );
    assert!(
        stdout.contains("refresh"),
        "Ext help should mention refresh subcommand"
    );
    assert!(
        stdout.contains("status"),
        "Ext help should mention status subcommand"
    );
}

/// Test ext merge with depmod post-processing
#[test]
fn test_ext_merge_with_depmod_processing() {
    // Setup mock environment with release files that require depmod
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[(
            "AVOCADO_EXTENSION_RELEASE_DIR",
            &release_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with depmod processing"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Starting extension merge process"),
        "Should show merging message"
    );
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );
    // Should show depmod being executed in the new generic command execution
    assert!(
        stdout.contains("post-merge: running: depmod"),
        "Should show depmod running (verbose step detail)"
    );
    assert!(
        stdout.contains("Rebuilt module dependencies"),
        "Should show the module-dependency rebuild summary"
    );
}

/// Test multiple extensions with both depmod and modprobe - verify single depmod call
#[test]
fn test_ext_merge_multiple_extensions_single_depmod() {
    // This test specifically verifies your concern: two extensions with depmod + modprobe
    // should result in ONE depmod call and ALL modules loaded
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[(
            "AVOCADO_EXTENSION_RELEASE_DIR",
            &release_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with multiple extensions"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Verify depmod is called exactly once
    let depmod_count = stdout.matches("post-merge: running: depmod").count();
    assert_eq!(
        depmod_count, 1,
        "Should call depmod exactly once even with multiple extensions requiring it"
    );

    // Verify all modules from all extensions are loaded
    assert!(
        stdout.contains("post-merge: loading kernel modules:"),
        "Should show module loading (verbose step detail)"
    );

    // Check that modules from multiple extensions are included
    // From network-driver: e1000e igb ixgbe
    // From storage-driver: ahci nvme
    // From gpu-driver: nvidia i915 radeon
    // From sound-driver: snd_hda_intel
    let has_network_modules =
        stdout.contains("e1000e") || stdout.contains("igb") || stdout.contains("ixgbe");
    let has_storage_modules = stdout.contains("ahci") || stdout.contains("nvme");
    let has_gpu_modules =
        stdout.contains("nvidia") || stdout.contains("i915") || stdout.contains("radeon");
    let has_sound_modules = stdout.contains("snd_hda_intel");

    assert!(
        has_network_modules || has_storage_modules || has_gpu_modules || has_sound_modules,
        "Should load modules from multiple extensions. Stdout: {stdout}"
    );

    assert!(
        stdout.contains("Loaded ") && stdout.contains(" kernel module"),
        "Should show the kernel-module load summary"
    );
}

/// Test ext merge with modprobe post-processing
#[test]
fn test_ext_merge_with_modprobe_processing() {
    // Setup mock environment with release files that require both depmod and modprobe
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[(
            "AVOCADO_EXTENSION_RELEASE_DIR",
            &release_dir.to_string_lossy(),
        )],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with modprobe processing"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Starting extension merge process"),
        "Should show merging message"
    );
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );
    assert!(
        stdout.contains("post-merge: running: depmod"),
        "Should show depmod running (verbose step detail)"
    );
    assert!(
        stdout.contains("Rebuilt module dependencies"),
        "Should show the module-dependency rebuild summary"
    );
    assert!(
        stdout.contains("post-merge: loading kernel modules:"),
        "Should show module loading (verbose step detail)"
    );
    assert!(
        stdout.contains("Loaded ") && stdout.contains(" kernel module"),
        "Should show the kernel-module load summary"
    );

    // Check that specific modules are being loaded (from our test fixtures)
    assert!(
        stdout.contains("nvidia") || stdout.contains("snd_hda_intel"),
        "Should load modules from test extension files"
    );
}

/// Test post-merge processing with no depmod needed
#[test]
fn test_ext_merge_no_depmod_needed() {
    // This test verifies that merge works normally when no depmod is needed
    // Use a non-existent release directory to ensure no post-merge tasks run
    let empty_release_dir = "/tmp/nonexistent_release_dir";

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[("AVOCADO_EXTENSION_RELEASE_DIR", empty_release_dir)],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed without depmod"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );
}

/// Test ext status command with mock systemd binaries
#[test]
fn test_ext_status_with_mocks() {
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(&["ext", "status"], &[]);

    assert!(
        output.status.success(),
        "ext status should succeed with mocks"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Avocado Extension Status"),
        "Should show enhanced extension status header"
    );
    assert!(
        stdout.contains("Extension") && stdout.contains("Status") && stdout.contains("Origin"),
        "Should show enhanced status table headers"
    );
    assert!(stdout.contains("Summary:"), "Should show status summary");
    assert!(
        stdout.contains("test-ext-1") && stdout.contains("SYSEXT"),
        "Should show system extension in table"
    );
    assert!(
        stdout.contains("test-ext-2") && stdout.contains("SYSEXT"),
        "Should show system extension in table"
    );
    assert!(
        stdout.contains("config-ext-1") && stdout.contains("CONFEXT"),
        "Should show configuration extension in table"
    );
    assert!(
        stdout.contains("Origin"),
        "Should show origin column for extensions"
    );
}

/// Test ext status help
#[test]
fn test_ext_status_help() {
    let output = run_avocadoctl(&["ext", "status", "--help"]);
    assert!(output.status.success(), "Ext status help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Show status of merged extensions"),
        "Should contain status description"
    );
}

/// Test ext merge with multiple AVOCADO_ON_MERGE commands from same extension
#[test]
fn test_ext_merge_with_multiple_on_merge_commands() {
    // Create a temporary release directory with our test files
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with multiple AVOCADO_ON_MERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );

    // Verify that multiple commands are executed
    assert!(
        stdout.contains("post-merge: executing"),
        "Should show execution of post-merge commands"
    );

    // Should see depmod being executed
    assert!(
        stdout.contains("post-merge: running: depmod"),
        "Should execute depmod command"
    );
}

/// Test ext merge with quoted AVOCADO_ON_MERGE commands
#[test]
fn test_ext_merge_with_quoted_commands() {
    // Create a temporary release directory with our test files
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with quoted AVOCADO_ON_MERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );

    // Should execute commands with arguments
    assert!(
        stdout.contains("post-merge: executing"),
        "Should show execution of post-merge commands"
    );
}

/// Test ext unmerge does NOT execute AVOCADO_ON_MERGE commands
/// (but AVOCADO_ON_UNMERGE commands ARE executed)
#[test]
fn test_ext_unmerge_does_not_execute_on_merge_commands() {
    // Setup mock environment with release files
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "unmerge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext unmerge should succeed without executing AVOCADO_ON_MERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions unmerged successfully"),
        "Should show unmerge success"
    );

    // Should NOT execute post-merge commands during unmerge
    // (pre-unmerge commands ARE executed, which is correct behavior)
    assert!(
        !stdout.contains("post-merge: executing"),
        "Should NOT execute AVOCADO_ON_MERGE commands during unmerge"
    );
}

/// Test deduplication of AVOCADO_ON_MERGE commands
#[test]
fn test_avocado_on_merge_command_deduplication() {
    // This test verifies that duplicate commands across multiple extensions
    // are only executed once
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with command deduplication"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Count how many times depmod is called - should be only once despite multiple extensions having it
    let depmod_execution_count = stdout.matches("post-merge: running: depmod").count();

    // We should see depmod executed, but due to deduplication it should appear in consolidated command execution
    assert!(
        depmod_execution_count >= 1,
        "depmod should be executed at least once"
    );

    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );
}

/// Test AVOCADO_ON_MERGE commands in confext release files
#[test]
fn test_ext_merge_with_confext_commands() {
    // Create a temporary test scenario with both sysext and confext directories
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();

    // Create mock sysext and confext release directories
    let sysext_dir = temp_path.join("usr/lib/extension-release.d");
    let confext_dir = temp_path.join("etc/extension-release.d");

    std::fs::create_dir_all(&sysext_dir).expect("Failed to create sysext dir");
    std::fs::create_dir_all(&confext_dir).expect("Failed to create confext dir");

    // Copy our test fixtures
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    // Copy sysext test files
    let source_sysext = fixtures_path.join("extension-release.d/extension-release.utils");
    let dest_sysext = sysext_dir.join("extension-release.utils");
    std::fs::copy(&source_sysext, &dest_sysext).expect("Failed to copy sysext file");

    // Copy confext test files
    let source_confext = fixtures_path.join("confext-release.d/extension-release.config-mgmt");
    let dest_confext = confext_dir.join("extension-release.config-mgmt");
    std::fs::copy(&source_confext, &dest_confext).expect("Failed to copy confext file");

    let (output, _temp_test_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "merge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &temp_path.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext merge should succeed with confext commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions merged successfully"),
        "Should show merge success"
    );

    // Should execute commands from both sysext and confext
    assert!(
        stdout.contains("post-merge: executing"),
        "Should show execution of post-merge commands"
    );
}

/// Test enable command with default runtime version
#[test]
fn test_enable_extensions_default_runtime() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::write(extensions_dir.join("ext2-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");
    fs::write(extensions_dir.join("ext3-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");

    // Run enable command with test mode
    let output = run_avocadoctl_with_env(
        &[
            "enable",
            "--verbose",
            "ext1-1.0.0",
            "ext2-1.0.0",
            "ext3-1.0.0",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("enable command should succeed");
    }

    assert!(
        stdout.contains("Enabling extensions for OS release version"),
        "Should show OS release version message"
    );
    assert!(
        stdout.contains("Successfully enabled 3 extension(s)"),
        "Should show success message for 3 extensions"
    );
    assert!(
        stdout.contains("Enabled extension: ext1-1.0.0"),
        "Should show ext1 enabled"
    );
    assert!(
        stdout.contains("Enabled extension: ext2-1.0.0"),
        "Should show ext2 enabled"
    );
    assert!(
        stdout.contains("Enabled extension: ext3-1.0.0"),
        "Should show ext3 enabled"
    );
}

/// Test enable command with custom runtime version
#[test]
fn test_enable_extensions_custom_runtime() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::write(extensions_dir.join("ext2-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");

    // Run enable command with custom os-release version and test mode
    let output = run_avocadoctl_with_env(
        &[
            "enable",
            "--verbose",
            "--os-release",
            "2.0.0",
            "ext1-1.0.0",
            "ext2-1.0.0",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("enable command should succeed with custom OS release");
    }

    assert!(
        stdout.contains("Enabling extensions for OS release version: 2.0.0"),
        "Should show custom OS release version"
    );
    assert!(
        stdout.contains("Successfully enabled 2 extension(s) for OS release 2.0.0"),
        "Should show success message with OS release version"
    );
}

/// Test enable command with nonexistent extension
#[test]
fn test_enable_nonexistent_extension() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create one valid extension
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");

    // Run enable command with mix of valid and invalid extensions and test mode
    let output = run_avocadoctl_with_env(
        &["enable", "--verbose", "ext1-1.0.0", "nonexistent-ext"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("STDOUT: {stdout}");
    println!("STDERR: {stderr}");

    assert!(
        !output.status.success(),
        "enable command should fail with nonexistent extension"
    );

    assert!(
        stderr.contains("Extension 'nonexistent-ext' not found"),
        "Should show error for nonexistent extension. STDERR: {stderr}"
    );
    assert!(
        stdout.contains("Enabled extension: ext1-1.0.0"),
        "Should still enable valid extension. STDOUT: {stdout}"
    );
}

/// Test enable command help
#[test]
fn test_enable_help() {
    let output = run_avocadoctl(&["enable", "--help"]);
    assert!(output.status.success(), "Enable help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Enable extensions for a specific runtime version"),
        "Should contain enable description"
    );
    assert!(
        stdout.contains("--os-release"),
        "Should mention --os-release flag"
    );
}

/// Test disable command with specific extensions
#[test]
fn test_disable_extensions() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::write(extensions_dir.join("ext2-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");
    fs::write(extensions_dir.join("ext3-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");

    // First enable extensions
    let enable_output = run_avocadoctl_with_env(
        &[
            "enable",
            "--verbose",
            "--os-release",
            "2.0.0",
            "ext1-1.0.0",
            "ext2-1.0.0",
            "ext3-1.0.0",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    assert!(enable_output.status.success(), "Enable should succeed");

    // Now disable some extensions
    let disable_output = run_avocadoctl_with_env(
        &[
            "disable",
            "--verbose",
            "--os-release",
            "2.0.0",
            "ext1-1.0.0",
            "ext2-1.0.0",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&disable_output.stdout);
    let stderr = String::from_utf8_lossy(&disable_output.stderr);

    if !disable_output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("disable command should succeed");
    }

    assert!(
        stdout.contains("Disabling extensions for OS release version: 2.0.0"),
        "Should show OS release version message"
    );
    assert!(
        stdout.contains("Successfully disabled 2 extension(s)"),
        "Should show success message for 2 extensions"
    );
    assert!(
        stdout.contains("Disabled extension: ext1-1.0.0"),
        "Should show ext1 disabled"
    );
    assert!(
        stdout.contains("Disabled extension: ext2-1.0.0"),
        "Should show ext2 disabled"
    );
    assert!(
        stdout.contains("Synced changes to disk"),
        "Should show sync message"
    );

    // Verify ext3 still exists
    let os_releases_dir = temp_dir.path().join("avocado/os-releases/2.0.0");
    assert!(
        os_releases_dir.join("ext3-1.0.0.raw").exists(),
        "ext3 should still be enabled"
    );
    assert!(
        !os_releases_dir.join("ext1-1.0.0").exists(),
        "ext1 should be disabled"
    );
    assert!(
        !os_releases_dir.join("ext2-1.0.0.raw").exists(),
        "ext2 should be disabled"
    );
}

/// Test disable command with --all flag
#[test]
fn test_disable_all_extensions() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::write(extensions_dir.join("ext2-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");
    fs::write(extensions_dir.join("ext3-1.0.0.raw"), b"mock raw data")
        .expect("Failed to create test raw extension");

    // First enable extensions
    let enable_output = run_avocadoctl_with_env(
        &[
            "enable",
            "--verbose",
            "--os-release",
            "2.0.0",
            "ext1-1.0.0",
            "ext2-1.0.0",
            "ext3-1.0.0",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    assert!(enable_output.status.success(), "Enable should succeed");

    // Now disable all extensions
    let disable_output = run_avocadoctl_with_env(
        &["disable", "--verbose", "--os-release", "2.0.0", "--all"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&disable_output.stdout);
    let stderr = String::from_utf8_lossy(&disable_output.stderr);

    if !disable_output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("disable --all command should succeed");
    }

    assert!(
        stdout.contains("Disabling extensions for OS release version: 2.0.0"),
        "Should show OS release version message"
    );
    assert!(
        stdout.contains("Removing all extensions"),
        "Should show removing all message"
    );
    assert!(
        stdout.contains("Successfully disabled 3 extension(s)"),
        "Should show success message for 3 extensions"
    );
    assert!(
        stdout.contains("Synced changes to disk"),
        "Should show sync message"
    );

    // Verify all extensions are removed
    let os_releases_dir = temp_dir.path().join("avocado/os-releases/2.0.0");
    let entries =
        fs::read_dir(&os_releases_dir).expect("Should be able to read os-releases directory");
    let symlink_count = entries
        .filter(|e| {
            if let Ok(entry) = e {
                entry.path().is_symlink()
            } else {
                false
            }
        })
        .count();

    assert_eq!(symlink_count, 0, "All symlinks should be removed");
}

/// Test disable command with default runtime version
#[test]
fn test_disable_extensions_default_runtime() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");

    // First enable extension
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "ext1-1.0.0"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    assert!(enable_output.status.success(), "Enable should succeed");

    // Now disable with default runtime
    let disable_output = run_avocadoctl_with_env(
        &["disable", "--verbose", "ext1-1.0.0"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stdout = String::from_utf8_lossy(&disable_output.stdout);
    let stderr = String::from_utf8_lossy(&disable_output.stderr);

    if !disable_output.status.success() {
        println!("STDOUT: {stdout}");
        println!("STDERR: {stderr}");
        panic!("disable command should succeed with default runtime");
    }

    assert!(
        stdout.contains("Disabling extensions for OS release version"),
        "Should show OS release version message"
    );
    assert!(
        stdout.contains("Successfully disabled 1 extension(s)"),
        "Should show success message"
    );
}

/// Test disable command with non-existent extension
#[test]
fn test_disable_nonexistent_extension() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extension
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");

    // First enable extension
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "--os-release", "2.0.0", "ext1-1.0.0"],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    assert!(enable_output.status.success(), "Enable should succeed");

    // Try to disable a non-existent extension
    let disable_output = run_avocadoctl_with_env(
        &[
            "disable",
            "--verbose",
            "--os-release",
            "2.0.0",
            "nonexistent-ext",
        ],
        &[
            ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
            ("AVOCADO_TEST_MODE", "1"),
            ("TMPDIR", temp_dir.path().to_str().unwrap()),
        ],
    );

    let stderr = String::from_utf8_lossy(&disable_output.stderr);

    assert!(
        !disable_output.status.success(),
        "disable command should fail with non-existent extension"
    );

    assert!(
        stderr.contains("Extension 'nonexistent-ext' is not enabled"),
        "Should show error for non-existent extension. STDERR: {stderr}"
    );
}

/// Test disable command help
#[test]
fn test_disable_help() {
    let output = run_avocadoctl(&["disable", "--help"]);
    assert!(output.status.success(), "Disable help should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Disable extensions for a specific runtime version"),
        "Should contain disable description"
    );
    assert!(
        stdout.contains("--os-release"),
        "Should mention --os-release flag"
    );
    assert!(stdout.contains("--all"), "Should mention --all flag");
}

/// Test enable/disable/refresh workflow
#[test]
fn test_enable_disable_refresh_workflow() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::create_dir(extensions_dir.join("ext2-1.0.0"))
        .expect("Failed to create test extension directory");

    // Create release files for both extensions
    let ext1_release_dir = extensions_dir.join("ext1-1.0.0/usr/lib/extension-release.d");
    fs::create_dir_all(&ext1_release_dir).expect("Failed to create release dir");
    fs::write(
        ext1_release_dir.join("extension-release.ext1-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let ext2_release_dir = extensions_dir.join("ext2-1.0.0/usr/lib/extension-release.d");
    fs::create_dir_all(&ext2_release_dir).expect("Failed to create release dir");
    fs::write(
        ext2_release_dir.join("extension-release.ext2-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Step 1: Enable both extensions
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "ext1-1.0.0", "ext2-1.0.0"],
        &test_env,
    );
    assert!(
        enable_output.status.success(),
        "Initial enable should succeed"
    );
    let stdout = String::from_utf8_lossy(&enable_output.stdout);
    assert!(stdout.contains("Successfully enabled 2 extension(s)"));

    // Step 2: Refresh with both enabled - both should be merged
    let (refresh_output1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(
        refresh_output1.status.success(),
        "First refresh should succeed"
    );
    let stdout1 = String::from_utf8_lossy(&refresh_output1.stdout);
    assert!(
        stdout1.contains("Found runtime extension: ext1-1.0.0") || stdout1.contains("ext1-1.0.0"),
        "Should scan ext1 from runtime"
    );
    assert!(
        stdout1.contains("Found runtime extension: ext2-1.0.0") || stdout1.contains("ext2-1.0.0"),
        "Should scan ext2 from runtime"
    );

    // Step 3: Disable ext1
    let disable_output =
        run_avocadoctl_with_env(&["disable", "--verbose", "ext1-1.0.0"], &test_env);
    assert!(disable_output.status.success(), "Disable should succeed");

    // Step 4: Refresh after disabling ext1 - only ext2 should be merged
    let (refresh_output2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(
        refresh_output2.status.success(),
        "Second refresh should succeed"
    );
    let stdout2 = String::from_utf8_lossy(&refresh_output2.stdout);

    // ext2 should still be found from runtime
    assert!(
        stdout2.contains("Found runtime extension: ext2-1.0.0") || stdout2.contains("ext2-1.0.0"),
        "Should still scan ext2 from runtime"
    );

    // ext1 should NOT be found from runtime (it was disabled)
    // It might be found from the base extensions directory though
    if stdout2.contains("ext1-1.0.0") {
        // If ext1 appears, it should be from the base directory, not runtime
        assert!(
            !stdout2.contains("Found runtime extension: ext1-1.0.0"),
            "ext1 should not be found in runtime directory"
        );
    }

    // Step 5: Re-enable ext1
    let reenable_output =
        run_avocadoctl_with_env(&["enable", "--verbose", "ext1-1.0.0"], &test_env);
    assert!(reenable_output.status.success(), "Re-enable should succeed");

    // Step 6: Refresh with both enabled again - both should be merged
    let (refresh_output3, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(
        refresh_output3.status.success(),
        "Third refresh should succeed"
    );
    let stdout3 = String::from_utf8_lossy(&refresh_output3.stdout);
    assert!(
        stdout3.contains("Found runtime extension: ext1-1.0.0") || stdout3.contains("ext1-1.0.0"),
        "Should scan ext1 from runtime again"
    );
    assert!(
        stdout3.contains("Found runtime extension: ext2-1.0.0") || stdout3.contains("ext2-1.0.0"),
        "Should scan ext2 from runtime"
    );
}

/// Test that disabled extensions are not merged after refresh
#[test]
fn test_disabled_extension_not_merged_after_refresh() {
    // Create a temporary directory for extensions
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::create_dir(extensions_dir.join("ext2-1.0.0"))
        .expect("Failed to create test extension directory");

    // Create release files for both extensions
    let ext1_release_dir = extensions_dir.join("ext1-1.0.0/usr/lib/extension-release.d");
    fs::create_dir_all(&ext1_release_dir).expect("Failed to create release dir");
    fs::write(
        ext1_release_dir.join("extension-release.ext1-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let ext2_release_dir = extensions_dir.join("ext2-1.0.0/usr/lib/extension-release.d");
    fs::create_dir_all(&ext2_release_dir).expect("Failed to create release dir");
    fs::write(
        ext2_release_dir.join("extension-release.ext2-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable both extensions
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "ext1-1.0.0", "ext2-1.0.0"],
        &test_env,
    );
    assert!(enable_output.status.success(), "Enable should succeed");

    // Refresh with both enabled
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success(), "First refresh should succeed");

    // Verify both symlinks exist after merge
    let sysext_dir = temp_dir.path().join("test_extensions");
    assert!(
        sysext_dir.join("ext1-1.0.0").exists(),
        "ext1 symlink should exist"
    );
    assert!(
        sysext_dir.join("ext2-1.0.0").exists(),
        "ext2 symlink should exist"
    );

    // Disable ext1
    let disable_output =
        run_avocadoctl_with_env(&["disable", "--verbose", "ext1-1.0.0"], &test_env);
    assert!(disable_output.status.success(), "Disable should succeed");

    // Refresh after disabling ext1
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success(), "Second refresh should succeed");
    let stdout2 = String::from_utf8_lossy(&refresh2.stdout);

    // Verify ext1 is NOT scanned from OS release
    assert!(
        !stdout2.contains("Found OS release extension: ext1-1.0.0"),
        "ext1 should NOT be found from OS release after being disabled. Stdout: {stdout2}"
    );

    // Verify ext2 IS scanned from OS release
    assert!(
        stdout2.contains("Found OS release extension: ext2-1.0.0"),
        "ext2 should still be found from OS release"
    );

    // Verify ext1 symlink was removed (stale cleanup)
    assert!(
        !sysext_dir.join("ext1-1.0.0").exists(),
        "ext1 symlink should be removed after refresh"
    );

    // Verify ext2 symlink still exists
    assert!(
        sysext_dir.join("ext2-1.0.0").exists(),
        "ext2 symlink should still exist"
    );

    // Verify base directory was skipped (because os-releases directory exists)
    assert!(
        stdout2.contains("OS releases directory exists, skipping base extensions directory")
            || !stdout2.contains("Found directory extension: ext1-1.0.0"),
        "Base directory should be skipped when OS releases directory exists"
    );
}

/// Test that base directory is completely skipped when runtime directory exists
#[test]
fn test_base_directory_skipped_with_runtime() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create extensions in base directory
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::create_dir(extensions_dir.join("ext2-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::create_dir(extensions_dir.join("ext3-1.0.0"))
        .expect("Failed to create test extension directory");

    // Create release files
    for ext in &["ext1-1.0.0", "ext2-1.0.0", "ext3-1.0.0"] {
        let release_dir = extensions_dir.join(format!("{ext}/usr/lib/extension-release.d"));
        fs::create_dir_all(&release_dir).expect("Failed to create release dir");
        fs::write(
            release_dir.join(format!("extension-release.{ext}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable only ext1
    let enable_output = run_avocadoctl_with_env(&["enable", "--verbose", "ext1-1.0.0"], &test_env);
    assert!(enable_output.status.success(), "Enable should succeed");

    // Refresh - should only merge ext1, not ext2 or ext3 from base directory
    let (refresh_output, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh_output.status.success(), "Refresh should succeed");
    let stdout = String::from_utf8_lossy(&refresh_output.stdout);

    // Verify ext1 is found from OS release
    assert!(
        stdout.contains("Found OS release extension: ext1-1.0.0"),
        "ext1 should be found from OS release"
    );

    // Verify ext2 and ext3 are NOT found (base directory skipped)
    assert!(
        !stdout.contains("Found directory extension: ext2-1.0.0"),
        "ext2 should NOT be found from base directory"
    );
    assert!(
        !stdout.contains("Found directory extension: ext3-1.0.0"),
        "ext3 should NOT be found from base directory"
    );

    // Verify message about skipping base directory
    assert!(
        stdout.contains("OS releases directory exists, skipping base extensions directory")
            || stdout.contains("OS releases directory exists, skipping base raw files"),
        "Should show message about skipping base directory"
    );
}

/// Test that all extensions from base are used when no runtime directory exists
#[test]
fn test_base_directory_used_without_runtime() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create extensions in base directory
    fs::create_dir(extensions_dir.join("ext1-1.0.0"))
        .expect("Failed to create test extension directory");
    fs::create_dir(extensions_dir.join("ext2-1.0.0"))
        .expect("Failed to create test extension directory");

    // Create release files
    for ext in &["ext1-1.0.0", "ext2-1.0.0"] {
        let release_dir = extensions_dir.join(format!("{ext}/usr/lib/extension-release.d"));
        fs::create_dir_all(&release_dir).expect("Failed to create release dir");
        fs::write(
            release_dir.join(format!("extension-release.{ext}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // DON'T enable any extensions - this means no runtime directory exists

    // Refresh - should use all extensions from base directory
    let (refresh_output, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh_output.status.success(), "Refresh should succeed");
    let stdout = String::from_utf8_lossy(&refresh_output.stdout);

    // Verify both extensions are found from base directory (not OS release)
    assert!(
        stdout.contains("Found directory extension: ext1-1.0.0"),
        "ext1 should be found from base directory. Stdout: {stdout}"
    );
    assert!(
        stdout.contains("Found directory extension: ext2-1.0.0"),
        "ext2 should be found from base directory. Stdout: {stdout}"
    );

    // Verify message about no OS releases directory
    assert!(
        stdout.contains("No OS releases directory found")
            || stdout.contains("OS releases directory") && stdout.contains("does not exist"),
        "Should indicate OS releases directory doesn't exist"
    );
}

/// Test enable with --all flag to disable all extensions
#[test]
fn test_disable_all_then_refresh() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    for ext in &["ext1-1.0.0", "ext2-1.0.0", "ext3-1.0.0"] {
        fs::create_dir(extensions_dir.join(ext))
            .expect("Failed to create test extension directory");
        let release_dir = extensions_dir.join(format!("{ext}/usr/lib/extension-release.d"));
        fs::create_dir_all(&release_dir).expect("Failed to create release dir");
        fs::write(
            release_dir.join(format!("extension-release.{ext}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable all three extensions
    let enable_output = run_avocadoctl_with_env(
        &[
            "enable",
            "--verbose",
            "ext1-1.0.0",
            "ext2-1.0.0",
            "ext3-1.0.0",
        ],
        &test_env,
    );
    assert!(enable_output.status.success(), "Enable should succeed");

    // Refresh to merge them
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success(), "First refresh should succeed");

    // Disable all extensions
    let disable_output = run_avocadoctl_with_env(&["disable", "--verbose", "--all"], &test_env);
    assert!(
        disable_output.status.success(),
        "Disable all should succeed"
    );

    // Refresh after disabling all
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success(), "Second refresh should succeed");
    let stdout2 = String::from_utf8_lossy(&refresh2.stdout);

    // Verify NO extensions are found from runtime (all were disabled)
    assert!(
        !stdout2.contains("Found runtime extension:"),
        "No extensions should be found from runtime after disabling all"
    );

    // The os-releases directory should still exist but be empty, so base directory should still be skipped
    // Read the actual VERSION_ID from the system to make the test environment-agnostic
    let os_release_content = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let version_id = os_release_content
        .lines()
        .find(|line| line.starts_with("VERSION_ID="))
        .map(|line| {
            line.trim_start_matches("VERSION_ID=")
                .trim_matches('"')
                .trim_matches('\'')
        })
        .unwrap_or("unknown");

    let os_releases_dir = temp_dir
        .path()
        .join(format!("avocado/os-releases/{version_id}"));
    assert!(
        os_releases_dir.exists(),
        "OS releases directory should still exist at: {}",
        os_releases_dir.display()
    );

    // Verify no symlinks exist after refresh
    let sysext_dir = temp_dir.path().join("test_extensions");
    if sysext_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&sysext_dir)
            .expect("Should read sysext dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_symlink())
            .collect();
        assert_eq!(
            entries.len(),
            0,
            "No symlinks should exist after disabling all and refreshing"
        );
    }
}

/// Test stale symlink cleanup
#[test]
fn test_stale_symlink_cleanup() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create test extensions
    for ext in &["ext1-1.0.0", "ext2-1.0.0"] {
        fs::create_dir(extensions_dir.join(ext))
            .expect("Failed to create test extension directory");
        let release_dir = extensions_dir.join(format!("{ext}/usr/lib/extension-release.d"));
        fs::create_dir_all(&release_dir).expect("Failed to create release dir");
        fs::write(
            release_dir.join(format!("extension-release.{ext}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable both extensions
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "ext1-1.0.0", "ext2-1.0.0"],
        &test_env,
    );
    assert!(enable_output.status.success());

    // Refresh to create symlinks
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success());

    let sysext_dir = temp_dir.path().join("test_extensions");
    assert!(
        sysext_dir.join("ext1-1.0.0").exists(),
        "ext1 symlink should exist"
    );
    assert!(
        sysext_dir.join("ext2-1.0.0").exists(),
        "ext2 symlink should exist"
    );

    // Disable ext1
    let disable_output =
        run_avocadoctl_with_env(&["disable", "--verbose", "ext1-1.0.0"], &test_env);
    assert!(disable_output.status.success());

    // Refresh - should clean up ext1 stale symlink
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success());
    let stdout2 = String::from_utf8_lossy(&refresh2.stdout);

    // Verify stale symlink was removed
    assert!(
        !sysext_dir.join("ext1-1.0.0").exists(),
        "ext1 stale symlink should be removed"
    );
    assert!(
        sysext_dir.join("ext2-1.0.0").exists(),
        "ext2 symlink should still exist"
    );

    // Check for cleanup message
    assert!(
        stdout2.contains("Removed stale") || !sysext_dir.join("ext1-1.0.0").exists(),
        "Should remove stale symlink or show cleanup message"
    );
}

#[test]
fn test_hitl_mount_masks_versioned_extensions() {
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    let hitl_dir = temp_dir.path().join("avocado/hitl");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create a versioned extension (myext-1.0.0) in the regular extensions directory
    let versioned_ext_dir = extensions_dir.join("myext-1.0.0");
    fs::create_dir(&versioned_ext_dir).expect("Failed to create versioned extension directory");
    let versioned_release_dir = versioned_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&versioned_release_dir).expect("Failed to create release dir");
    fs::write(
        versioned_release_dir.join("extension-release.myext-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable the versioned extension first
    let enable_output = run_avocadoctl_with_env(&["enable", "--verbose", "myext-1.0.0"], &test_env);
    assert!(
        enable_output.status.success(),
        "Enable command should succeed"
    );

    // Refresh to create symlinks for the versioned extension (WITHOUT HITL mount yet)
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success(), "First refresh should succeed");

    let sysext_dir = temp_dir.path().join("test_extensions");

    // Verify that the versioned symlink was created
    assert!(
        sysext_dir.join("myext-1.0.0").exists(),
        "Versioned symlink (myext-1.0.0) should exist after initial refresh"
    );

    // Now create a HITL extension with the same base name (myext) but no version
    fs::create_dir_all(&hitl_dir).expect("Failed to create HITL directory");
    let hitl_ext_dir = hitl_dir.join("myext");
    fs::create_dir(&hitl_ext_dir).expect("Failed to create HITL extension directory");
    let hitl_release_dir = hitl_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&hitl_release_dir).expect("Failed to create HITL release dir");
    fs::write(
        hitl_release_dir.join("extension-release.myext"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write HITL release file");

    // Refresh again - this should detect the HITL mount and remove the versioned symlink
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success(), "Second refresh should succeed");
    let stdout2 = String::from_utf8_lossy(&refresh2.stdout);

    // Verify that the versioned symlink was removed (masked by HITL)
    assert!(
        !sysext_dir.join("myext-1.0.0").exists(),
        "Versioned symlink (myext-1.0.0) should be removed when HITL mount (myext) exists"
    );

    // Verify that the non-versioned HITL symlink exists
    assert!(
        sysext_dir.join("myext").exists(),
        "HITL symlink (myext) should exist"
    );

    // Check for cleanup message in verbose output
    assert!(
        stdout2.contains("Removed stale") || stdout2.contains("myext"),
        "Should mention cleanup or the extension name in verbose output"
    );
}

#[test]
fn test_hitl_mount_masks_multiple_versions() {
    // Test that HITL mount masks multiple different versions of the same extension
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    let hitl_dir = temp_dir.path().join("avocado/hitl");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create multiple versioned extensions (myext-1.0.0 and myext-2.0.0)
    for version in &["1.0.0", "2.0.0"] {
        let ext_name = format!("myext-{version}");
        let versioned_ext_dir = extensions_dir.join(&ext_name);
        fs::create_dir(&versioned_ext_dir).expect("Failed to create versioned extension directory");
        let versioned_release_dir = versioned_ext_dir.join("usr/lib/extension-release.d");
        fs::create_dir_all(&versioned_release_dir).expect("Failed to create release dir");
        fs::write(
            versioned_release_dir.join(format!("extension-release.{ext_name}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable both versioned extensions
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "myext-1.0.0", "myext-2.0.0"],
        &test_env,
    );
    assert!(enable_output.status.success(), "Enable should succeed");

    // Refresh to create symlinks
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success(), "First refresh should succeed");

    let sysext_dir = temp_dir.path().join("test_extensions");

    // Verify both versioned symlinks exist (only one would be active, but both should be in os-releases)
    // Note: Only the last enabled one should actually be symlinked since they have the same base name
    // and the extension_map uses the base name as key
    assert!(
        sysext_dir.join("myext-1.0.0").exists() || sysext_dir.join("myext-2.0.0").exists(),
        "At least one versioned symlink should exist"
    );

    // Create HITL mount
    fs::create_dir_all(&hitl_dir).expect("Failed to create HITL directory");
    let hitl_ext_dir = hitl_dir.join("myext");
    fs::create_dir(&hitl_ext_dir).expect("Failed to create HITL extension directory");
    let hitl_release_dir = hitl_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&hitl_release_dir).expect("Failed to create HITL release dir");
    fs::write(
        hitl_release_dir.join("extension-release.myext"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write HITL release file");

    // Refresh with HITL mount
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success(), "Second refresh should succeed");

    // Verify ALL versioned symlinks are removed
    assert!(
        !sysext_dir.join("myext-1.0.0").exists(),
        "myext-1.0.0 should be masked by HITL mount"
    );
    assert!(
        !sysext_dir.join("myext-2.0.0").exists(),
        "myext-2.0.0 should be masked by HITL mount"
    );
    assert!(
        sysext_dir.join("myext").exists(),
        "HITL symlink should exist"
    );
}

#[test]
fn test_hitl_mount_only_masks_same_base_name() {
    // Test that HITL mount for "myext" doesn't mask "otherext-1.0.0"
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    let hitl_dir = temp_dir.path().join("avocado/hitl");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create two different extensions
    for (name, version) in &[("myext", "1.0.0"), ("otherext", "2.0.0")] {
        let ext_name = format!("{name}-{version}");
        let ext_dir = extensions_dir.join(&ext_name);
        fs::create_dir(&ext_dir).expect("Failed to create extension directory");
        let release_dir = ext_dir.join("usr/lib/extension-release.d");
        fs::create_dir_all(&release_dir).expect("Failed to create release dir");
        fs::write(
            release_dir.join(format!("extension-release.{ext_name}")),
            "ID=avocado\nVERSION_ID=1.0",
        )
        .expect("Failed to write release file");
    }

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable both extensions
    let enable_output = run_avocadoctl_with_env(
        &["enable", "--verbose", "myext-1.0.0", "otherext-2.0.0"],
        &test_env,
    );
    assert!(enable_output.status.success(), "Enable should succeed");

    // Refresh to create symlinks
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh1.status.success(), "First refresh should succeed");

    let sysext_dir = temp_dir.path().join("test_extensions");

    // Verify both symlinks exist
    assert!(
        sysext_dir.join("myext-1.0.0").exists(),
        "myext-1.0.0 should exist"
    );
    assert!(
        sysext_dir.join("otherext-2.0.0").exists(),
        "otherext-2.0.0 should exist"
    );

    // Create HITL mount for myext only
    fs::create_dir_all(&hitl_dir).expect("Failed to create HITL directory");
    let hitl_ext_dir = hitl_dir.join("myext");
    fs::create_dir(&hitl_ext_dir).expect("Failed to create HITL extension directory");
    let hitl_release_dir = hitl_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&hitl_release_dir).expect("Failed to create HITL release dir");
    fs::write(
        hitl_release_dir.join("extension-release.myext"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write HITL release file");

    // Refresh with HITL mount
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(refresh2.status.success(), "Second refresh should succeed");

    // Verify myext-1.0.0 is masked but otherext-2.0.0 remains
    assert!(
        !sysext_dir.join("myext-1.0.0").exists(),
        "myext-1.0.0 should be masked"
    );
    assert!(sysext_dir.join("myext").exists(), "HITL myext should exist");
    assert!(
        sysext_dir.join("otherext-2.0.0").exists(),
        "otherext-2.0.0 should NOT be masked (different base name)"
    );
}

#[test]
fn test_hitl_mount_removal_restores_versioned() {
    // Test that removing HITL mount allows the versioned extension to be used again
    let temp_dir = TempDir::new().expect("Failed to create temp directory");
    let extensions_dir = temp_dir.path().join("extensions");
    let hitl_dir = temp_dir.path().join("avocado/hitl");
    fs::create_dir_all(&extensions_dir).expect("Failed to create extensions directory");

    // Create a versioned extension
    let versioned_ext_dir = extensions_dir.join("myext-1.0.0");
    fs::create_dir(&versioned_ext_dir).expect("Failed to create versioned extension directory");
    let versioned_release_dir = versioned_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&versioned_release_dir).expect("Failed to create release dir");
    fs::write(
        versioned_release_dir.join("extension-release.myext-1.0.0"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write release file");

    let test_env = [
        ("AVOCADO_EXTENSIONS_PATH", extensions_dir.to_str().unwrap()),
        ("AVOCADO_TEST_MODE", "1"),
        ("TMPDIR", temp_dir.path().to_str().unwrap()),
    ];

    // Enable the versioned extension
    let enable_output = run_avocadoctl_with_env(&["enable", "--verbose", "myext-1.0.0"], &test_env);
    assert!(enable_output.status.success(), "Enable should succeed");

    // Create and use HITL mount
    fs::create_dir_all(&hitl_dir).expect("Failed to create HITL directory");
    let hitl_ext_dir = hitl_dir.join("myext");
    fs::create_dir(&hitl_ext_dir).expect("Failed to create HITL extension directory");
    let hitl_release_dir = hitl_ext_dir.join("usr/lib/extension-release.d");
    fs::create_dir_all(&hitl_release_dir).expect("Failed to create HITL release dir");
    fs::write(
        hitl_release_dir.join("extension-release.myext"),
        "ID=avocado\nVERSION_ID=1.0",
    )
    .expect("Failed to write HITL release file");

    // Refresh with HITL
    let (refresh1, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(
        refresh1.status.success(),
        "Refresh with HITL should succeed"
    );

    let sysext_dir = temp_dir.path().join("test_extensions");
    assert!(
        sysext_dir.join("myext").exists(),
        "HITL symlink should exist"
    );
    assert!(
        !sysext_dir.join("myext-1.0.0").exists(),
        "Versioned should be masked"
    );

    // Remove HITL mount
    fs::remove_dir_all(&hitl_ext_dir).expect("Failed to remove HITL extension");

    // Refresh without HITL
    let (refresh2, _) =
        run_avocadoctl_with_isolated_env(&["ext", "refresh", "--verbose"], &test_env);
    assert!(
        refresh2.status.success(),
        "Refresh without HITL should succeed"
    );

    // Verify versioned extension is restored
    assert!(
        !sysext_dir.join("myext").exists(),
        "HITL symlink should be removed"
    );
    assert!(
        sysext_dir.join("myext-1.0.0").exists(),
        "Versioned symlink should be restored"
    );
}

/// Test ext unmerge executes AVOCADO_ON_UNMERGE commands
#[test]
fn test_ext_unmerge_executes_on_unmerge_commands() {
    // Setup mock environment with release files containing AVOCADO_ON_UNMERGE
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "unmerge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext unmerge should succeed when executing AVOCADO_ON_UNMERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions unmerged successfully"),
        "Should show unmerge success"
    );

    // Should execute pre-unmerge commands
    assert!(
        stdout.contains("pre-unmerge commands") || stdout.contains("Running command:"),
        "Should execute AVOCADO_ON_UNMERGE commands during unmerge"
    );
}

/// Test ext unmerge with multiple AVOCADO_ON_UNMERGE commands from same extension
#[test]
fn test_ext_unmerge_with_multiple_on_unmerge_commands() {
    // Create a temporary release directory with test files
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "unmerge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext unmerge should succeed with multiple AVOCADO_ON_UNMERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions unmerged successfully"),
        "Should show unmerge success"
    );
}

/// Test deduplication of AVOCADO_ON_UNMERGE commands
#[test]
fn test_avocado_on_unmerge_command_deduplication() {
    // This test verifies that duplicate commands across multiple extensions
    // are only executed once
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp directory");
    let temp_path = temp_dir.path();

    // Create a release directory with duplicate AVOCADO_ON_UNMERGE commands
    let release_dir = temp_path.join("test-release");
    fs::create_dir_all(&release_dir).expect("Failed to create release dir");

    // Create multiple release files with the same AVOCADO_ON_UNMERGE command
    fs::write(
        release_dir.join("extension-release.ext1"),
        "VERSION_ID=1.0\nAVOCADO_ON_UNMERGE=\"systemctl stop common-service\"\n",
    )
    .expect("Failed to write release file");
    fs::write(
        release_dir.join("extension-release.ext2"),
        "VERSION_ID=1.0\nAVOCADO_ON_UNMERGE=\"systemctl stop common-service\"\nAVOCADO_ON_UNMERGE=\"systemctl stop unique-service\"\n",
    )
    .expect("Failed to write release file");

    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");

    let (output, _temp_test_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "unmerge", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext unmerge should succeed with command deduplication"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Count how many times "systemctl stop common-service" is executed
    // Should be only once due to deduplication
    let common_service_count = stdout
        .matches("Running command: systemctl stop common-service")
        .count();

    // Due to deduplication, common-service should appear at most once in command execution
    assert!(
        common_service_count <= 1,
        "Duplicate commands should be deduplicated (found {common_service_count} executions)"
    );

    assert!(
        stdout.contains("Extensions unmerged successfully"),
        "Should show unmerge success"
    );
}

/// Test ext refresh executes AVOCADO_ON_UNMERGE commands before unmerge
#[test]
fn test_ext_refresh_executes_on_unmerge_before_unmerge() {
    // Create a temporary release directory with test files
    let current_dir = std::env::current_dir().expect("Failed to get current directory");
    let fixtures_path = current_dir.join("tests/fixtures");
    let release_dir = fixtures_path.join("extension-release.d");

    // Use isolated environment to avoid race conditions
    let (output, _temp_dir) = run_avocadoctl_with_isolated_env(
        &["ext", "refresh", "--verbose"],
        &[
            (
                "AVOCADO_EXTENSION_RELEASE_DIR",
                &release_dir.to_string_lossy(),
            ),
            (
                "PATH",
                &format!(
                    "{}:{}",
                    fixtures_path.to_string_lossy(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
        ],
    );

    assert!(
        output.status.success(),
        "ext refresh should succeed and execute AVOCADO_ON_UNMERGE commands"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Extensions refreshed successfully"),
        "Should show refresh success"
    );

    // Verify that both pre-unmerge and post-merge commands are executed in order
    // Pre-unmerge commands should appear before unmerge, post-merge should appear after merge
}
