use crate::output::OutputManager;
use clap::{Arg, ArgMatches, Command};
use std::fs;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};

/// Create the hitl subcommand definition
pub fn create_command() -> Command {
    Command::new("hitl")
        .about("Hardware-in-the-loop (HITL) testing commands")
        .subcommand(
            Command::new("mount")
                .about("Mount NFS extensions from a remote server")
                .arg(
                    Arg::new("server-ip")
                        .short('s')
                        .long("server-ip")
                        .value_name("IP")
                        .help("Server IP address")
                        .required(true),
                )
                .arg(
                    Arg::new("server-port")
                        .short('p')
                        .long("server-port")
                        .value_name("PORT")
                        .help("Server port number")
                        .default_value("12049"),
                )
                .arg(
                    Arg::new("extension")
                        .short('e')
                        .long("extension")
                        .value_name("NAME")
                        .help("Extension name to mount (can be specified multiple times)")
                        .action(clap::ArgAction::Append)
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("watchdog")
                .about("Watch a HITL server and fall back to the installed extension if it goes away (started by `hitl mount`)")
                .hide(true)
                .arg(Arg::new("server-ip").long("server-ip").value_name("IP").required(true))
                .arg(Arg::new("server-port").long("server-port").value_name("PORT").required(true))
                .arg(Arg::new("extension").long("extension").value_name("NAME").required(true)),
        )
        .subcommand(
            Command::new("unmount").about("Unmount NFS extensions").arg(
                Arg::new("extension")
                    .short('e')
                    .long("extension")
                    .value_name("NAME")
                    .help("Extension name to unmount (can be specified multiple times)")
                    .action(clap::ArgAction::Append)
                    .required(true),
            ),
        )
}

/// Handle hitl command and its subcommands
pub fn handle_command(matches: &ArgMatches, output: &OutputManager) {
    match matches.subcommand() {
        Some(("mount", mount_matches)) => {
            mount_extensions(mount_matches, output);
        }
        Some(("unmount", unmount_matches)) => {
            unmount_extensions(unmount_matches, output);
        }
        Some(("watchdog", m)) => {
            let ip = m.get_one::<String>("server-ip").expect("required");
            let port = m.get_one::<String>("server-port").expect("required");
            let ext = m.get_one::<String>("extension").expect("required");
            crate::service::hitl::watchdog(ip, port, ext, output);
        }
        _ => {
            println!("Use 'avocadoctl hitl --help' for available HITL commands");
        }
    }
}

/// Mount NFS extensions from a remote server
/// Mount NFS extensions from a remote server.
///
/// Thin: the work is `service::hitl::mount`, shared with the varlink daemon.
/// This path only runs under AVOCADO_TEST_MODE, where the CLI bypasses the
/// daemon so integration tests can use mock executables.
fn mount_extensions(matches: &ArgMatches, output: &OutputManager) {
    let server_ip = matches
        .get_one::<String>("server-ip")
        .expect("server-ip is required");
    let server_port = matches.get_one::<String>("server-port").map(String::as_str);
    let extensions: Vec<String> = matches
        .get_many::<String>("extension")
        .expect("at least one extension is required")
        .cloned()
        .collect();

    output.info(
        "HITL Mount",
        &format!(
            "Mounting {} extension(s) from {server_ip}",
            extensions.len()
        ),
    );
    match crate::service::hitl::mount(server_ip, server_port, &extensions, output) {
        Ok(()) => output.success("HITL Mount", "All extensions mounted successfully"),
        Err(e) => {
            output.error("HITL Mount", &describe(&e));
            std::process::exit(1);
        }
    }
}

/// Name the extension in the way the user typed it, then the reason.
fn describe(e: &crate::service::error::AvocadoError) -> String {
    use crate::service::error::AvocadoError::{MountFailed, UnmountFailed};
    match e {
        MountFailed { extension, reason } => {
            format!("Failed to mount extension {extension}: {reason}")
        }
        UnmountFailed { extension, reason } => {
            format!("Failed to unmount extension {extension}: {reason}")
        }
        other => other.to_string(),
    }
}

/// Unmount NFS extensions and restore the installed ones. See `mount_extensions`.
fn unmount_extensions(matches: &ArgMatches, output: &OutputManager) {
    let extensions: Vec<String> = matches
        .get_many::<String>("extension")
        .expect("at least one extension is required")
        .cloned()
        .collect();

    output.info(
        "HITL Unmount",
        &format!("Unmounting {} extension(s)", extensions.len()),
    );
    match crate::service::hitl::unmount(&extensions, output) {
        Ok(()) => output.success("HITL Unmount", "All extensions unmounted successfully"),
        Err(e) => {
            output.error("HITL Unmount", &describe(&e));
            std::process::exit(1);
        }
    }
}

/// Convert a mount path to a systemd mount unit name
/// e.g., /run/avocado/hitl/my-ext -> run-avocado-hitl-my\x2dext.mount
pub fn systemd_escape_mount_path(path: &str) -> String {
    // Remove leading slash and replace / with -
    let without_leading_slash = path.trim_start_matches('/');
    // Escape dashes in path components (except separators)
    // Systemd mount unit names simply replace / with -
    // No escaping of dashes within path components is needed
    let escaped = without_leading_slash.replace('/', "-");
    format!("{escaped}.mount")
}

/// Create systemd drop-in files for services that depend on the HITL mount
/// This ensures services are stopped before the NFS mount is unmounted during shutdown
pub fn create_service_dropins(
    extension: &str,
    mount_point: &str,
    services: &[String],
    output: &OutputManager,
) -> Result<(), HitlError> {
    if services.is_empty() {
        return Ok(());
    }

    let mount_unit = systemd_escape_mount_path(mount_point);
    output.step(
        "Service Dependencies",
        &format!(
            "Creating drop-ins for {} service(s) to depend on {}",
            services.len(),
            mount_unit
        ),
    );

    // Determine the base directory for drop-ins
    let systemd_run_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        // Use AVOCADO_TEST_TMPDIR if set (to avoid affecting TempDir::new()),
        // otherwise fall back to TMPDIR, then /tmp
        let temp_base = std::env::var("AVOCADO_TEST_TMPDIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/run/systemd/system")
    } else {
        "/run/systemd/system".to_string()
    };

    // Collect service unit names for the mount unit drop-in
    let service_units: Vec<String> = services
        .iter()
        .map(|s| {
            if s.ends_with(".service") {
                s.clone()
            } else {
                format!("{s}.service")
            }
        })
        .collect();

    // Create drop-ins for each service
    for service_unit in &service_units {
        let dropin_dir = format!("{systemd_run_dir}/{service_unit}.d");
        let dropin_file = format!("{dropin_dir}/10-hitl-{extension}.conf");

        // Create the drop-in directory
        if let Err(e) = fs::create_dir_all(&dropin_dir) {
            output.error(
                "Service Dependencies",
                &format!("Failed to create drop-in directory {dropin_dir}: {e}"),
            );
            continue;
        }

        // Create the drop-in content
        // - RequiresMountsFor: Ensures the mount path is available
        // - BindsTo: Binds service lifecycle to mount (stops service when mount stops)
        // - After: Service starts after mount is ready; during shutdown, service stops BEFORE mount
        // - After=remote-fs.target: During shutdown, service stops BEFORE remote-fs.target
        //   This ensures the service is stopped before NFS mounts are unmounted
        let dropin_content = format!(
            "# Auto-generated by avocadoctl hitl mount for extension: {extension}\n\
            [Unit]\n\
            RequiresMountsFor={mount_point}\n\
            BindsTo={mount_unit}\n\
            After={mount_unit}\n\
            After=remote-fs.target\n"
        );

        // Write the drop-in file
        if let Err(e) = fs::write(&dropin_file, &dropin_content) {
            output.error(
                "Service Dependencies",
                &format!("Failed to write drop-in file {dropin_file}: {e}"),
            );
            continue;
        }

        output.progress(&format!("Created drop-in: {dropin_file}"));
    }

    // Create a drop-in for the mount unit to ensure services stop before unmount
    // This is critical for proper shutdown ordering - the mount unit needs to know
    // it should wait for services to stop before unmounting
    let mount_dropin_dir = format!("{systemd_run_dir}/{mount_unit}.d");
    let mount_dropin_file = format!("{mount_dropin_dir}/10-hitl-{extension}-services.conf");

    if let Err(e) = fs::create_dir_all(&mount_dropin_dir) {
        output.error(
            "Service Dependencies",
            &format!("Failed to create mount drop-in directory {mount_dropin_dir}: {e}"),
        );
    } else {
        // Before= ensures the mount unit stops AFTER the services stop
        // (i.e., services stop first, then mount is unmounted)
        let services_list = service_units.join(" ");
        let mount_dropin_content = format!(
            "# Auto-generated by avocadoctl hitl mount for extension: {extension}\n\
            # Ensures services are stopped before this mount is unmounted during shutdown\n\
            [Unit]\n\
            Before={services_list}\n"
        );

        if let Err(e) = fs::write(&mount_dropin_file, &mount_dropin_content) {
            output.error(
                "Service Dependencies",
                &format!("Failed to write mount drop-in file {mount_dropin_file}: {e}"),
            );
        } else {
            output.progress(&format!("Created drop-in: {mount_dropin_file}"));
        }
    }

    Ok(())
}

/// Clean up systemd drop-in files for services when unmounting HITL extensions
pub fn cleanup_service_dropins(
    extension: &str,
    services: &[String],
    output: &OutputManager,
) -> Result<(), HitlError> {
    if services.is_empty() {
        return Ok(());
    }

    output.step(
        "Service Dependencies",
        &format!(
            "Removing drop-ins for {} service(s) from extension {}",
            services.len(),
            extension
        ),
    );

    // Determine the base directory for drop-ins
    let systemd_run_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        // Use AVOCADO_TEST_TMPDIR if set (to avoid affecting TempDir::new()),
        // otherwise fall back to TMPDIR, then /tmp
        let temp_base = std::env::var("AVOCADO_TEST_TMPDIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/run/systemd/system")
    } else {
        "/run/systemd/system".to_string()
    };

    for service in services {
        // Ensure service name ends with .service
        let service_unit = if service.ends_with(".service") {
            service.clone()
        } else {
            format!("{service}.service")
        };

        let dropin_dir = format!("{systemd_run_dir}/{service_unit}.d");
        let dropin_file = format!("{dropin_dir}/10-hitl-{extension}.conf");

        // Remove the drop-in file if it exists
        if Path::new(&dropin_file).exists() {
            if let Err(e) = fs::remove_file(&dropin_file) {
                output.error(
                    "Service Dependencies",
                    &format!("Failed to remove drop-in file {dropin_file}: {e}"),
                );
                continue;
            }
            output.progress(&format!("Removed drop-in: {dropin_file}"));

            // Try to remove the drop-in directory if it's empty
            if let Ok(entries) = fs::read_dir(&dropin_dir) {
                if entries.count() == 0 {
                    let _ = fs::remove_dir(&dropin_dir);
                }
            }
        }
    }

    // Clean up mount unit drop-ins
    // We need to find and remove all mount unit drop-ins for this extension
    // Look for directories matching *.mount.d and files matching 10-hitl-{extension}-services.conf
    if let Ok(entries) = fs::read_dir(&systemd_run_dir) {
        for entry in entries.flatten() {
            let filename = entry.file_name();
            let filename_str = filename.to_string_lossy();
            if filename_str.ends_with(".mount.d") {
                let mount_dropin_file =
                    format!("{systemd_run_dir}/{filename_str}/10-hitl-{extension}-services.conf");
                if Path::new(&mount_dropin_file).exists() {
                    if let Err(e) = fs::remove_file(&mount_dropin_file) {
                        output.error(
                            "Service Dependencies",
                            &format!(
                                "Failed to remove mount drop-in file {mount_dropin_file}: {e}"
                            ),
                        );
                    } else {
                        output.progress(&format!("Removed drop-in: {mount_dropin_file}"));

                        // Try to remove the drop-in directory if it's empty
                        let mount_dropin_dir = format!("{systemd_run_dir}/{filename_str}");
                        if let Ok(dir_entries) = fs::read_dir(&mount_dropin_dir) {
                            if dir_entries.count() == 0 {
                                let _ = fs::remove_dir(&mount_dropin_dir);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Call systemctl daemon-reload to apply drop-in changes
pub fn systemd_daemon_reload(output: &OutputManager) -> Result<(), HitlError> {
    // Skip daemon-reload in test mode
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        output.progress("Skipping daemon-reload in test mode");
        return Ok(());
    }

    output.step(
        "Systemd",
        "Reloading systemd daemon to apply drop-in changes",
    );

    let result = ProcessCommand::new("systemctl")
        .arg("daemon-reload")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| HitlError::Command {
            command: "systemctl daemon-reload".to_string(),
            source: e,
        })?;

    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        output.error("Systemd", &format!("daemon-reload failed: {stderr}"));
        return Err(HitlError::DaemonReload {
            error: stderr.to_string(),
        });
    }

    output.progress("Systemd daemon reloaded successfully");
    Ok(())
}

/// Errors related to HITL operations
#[derive(Debug, thiserror::Error)]
pub enum HitlError {
    #[error("Failed to run command '{command}': {source}")]
    Command {
        command: String,
        source: std::io::Error,
    },

    #[error("Failed to mount extension '{extension}' to '{mount_point}': {error}")]
    Mount {
        extension: String,
        mount_point: String,
        error: String,
    },

    #[error("Failed to unmount '{mount_point}': {error}")]
    Unmount { mount_point: String, error: String },

    #[error("Failed to reload systemd daemon: {error}")]
    DaemonReload { error: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_env::ENV_VAR_MUTEX;

    #[test]
    fn test_create_command() {
        let cmd = create_command();
        assert_eq!(cmd.get_name(), "hitl");

        // mount, unmount, and the hidden watchdog that mount starts
        let subcommands: Vec<_> = cmd.get_subcommands().collect();
        assert_eq!(subcommands.len(), 3);

        let subcommand_names: Vec<&str> = subcommands.iter().map(|cmd| cmd.get_name()).collect();
        assert!(subcommand_names.contains(&"mount"));
        assert!(subcommand_names.contains(&"unmount"));
        assert!(subcommand_names.contains(&"watchdog"));
        let watchdog = subcommands
            .iter()
            .find(|c| c.get_name() == "watchdog")
            .unwrap();
        assert!(
            watchdog.is_hide_set(),
            "watchdog is an implementation detail of mount"
        );
    }

    #[test]
    fn test_mount_command_args() {
        let cmd = create_command();
        let mount_cmd = cmd
            .get_subcommands()
            .find(|subcmd| subcmd.get_name() == "mount")
            .expect("mount subcommand should exist");

        // Check required arguments
        let args: Vec<_> = mount_cmd.get_arguments().collect();
        let arg_names: Vec<&str> = args.iter().map(|arg| arg.get_id().as_str()).collect();

        assert!(arg_names.contains(&"server-ip"));
        assert!(arg_names.contains(&"server-port"));
        assert!(arg_names.contains(&"extension"));
    }

    #[test]
    fn test_unmount_command_args() {
        let cmd = create_command();
        let unmount_cmd = cmd
            .get_subcommands()
            .find(|subcmd| subcmd.get_name() == "unmount")
            .expect("unmount subcommand should exist");

        // Check required arguments
        let args: Vec<_> = unmount_cmd.get_arguments().collect();
        let arg_names: Vec<&str> = args.iter().map(|arg| arg.get_id().as_str()).collect();

        assert!(arg_names.contains(&"extension"));
    }

    #[test]
    fn test_systemd_escape_mount_path() {
        // Test basic path escaping
        assert_eq!(
            systemd_escape_mount_path("/run/avocado/hitl/myext"),
            "run-avocado-hitl-myext.mount"
        );

        // Test path with dashes in component name (dashes are preserved, not escaped)
        assert_eq!(
            systemd_escape_mount_path("/run/avocado/hitl/my-extension"),
            "run-avocado-hitl-my-extension.mount"
        );

        // Test path with multiple dashes
        assert_eq!(
            systemd_escape_mount_path("/run/avocado/hitl/my-cool-ext"),
            "run-avocado-hitl-my-cool-ext.mount"
        );

        // Test path with leading slash removal
        assert_eq!(
            systemd_escape_mount_path("run/avocado/hitl/ext"),
            "run-avocado-hitl-ext.mount"
        );
    }

    #[test]
    fn test_create_and_cleanup_service_dropins() {
        use tempfile::TempDir;

        // Lock the mutex to prevent env var interference from other tests
        let _guard = ENV_VAR_MUTEX.lock().unwrap();

        // Save original environment variable values for restoration
        let original_test_mode = std::env::var("AVOCADO_TEST_MODE").ok();
        let original_test_tmpdir = std::env::var("AVOCADO_TEST_TMPDIR").ok();

        // Set up test environment - create TempDir BEFORE modifying env vars
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path().to_string_lossy().to_string();

        std::env::set_var("AVOCADO_TEST_MODE", "1");
        // Use AVOCADO_TEST_TMPDIR to avoid affecting TempDir::new() in other tests
        std::env::set_var("AVOCADO_TEST_TMPDIR", &temp_path);

        let output = OutputManager::new(true, false);
        let extension = "test-ext";
        let mount_point = &format!("{temp_path}/avocado/hitl/test-ext");
        let services = vec!["nginx".to_string(), "prometheus.service".to_string()];

        // Create drop-ins
        let result = create_service_dropins(extension, mount_point, &services, &output);
        assert!(result.is_ok());

        // Verify service drop-ins were created
        let systemd_dir = format!("{temp_path}/run/systemd/system");
        let nginx_dropin = format!("{systemd_dir}/nginx.service.d/10-hitl-test-ext.conf");
        let prometheus_dropin = format!("{systemd_dir}/prometheus.service.d/10-hitl-test-ext.conf");

        assert!(Path::new(&nginx_dropin).exists());
        assert!(Path::new(&prometheus_dropin).exists());

        // Verify service drop-in content
        let nginx_content = fs::read_to_string(&nginx_dropin).unwrap();
        assert!(nginx_content.contains("[Unit]"));
        assert!(nginx_content.contains("RequiresMountsFor="));
        assert!(nginx_content.contains("BindsTo="));
        assert!(nginx_content.contains("After="));
        assert!(
            nginx_content.contains("After=remote-fs.target"),
            "Service drop-in should have After=remote-fs.target for shutdown ordering"
        );

        // Verify mount unit drop-in was created
        let mount_unit = systemd_escape_mount_path(mount_point);
        let mount_dropin = format!("{systemd_dir}/{mount_unit}.d/10-hitl-test-ext-services.conf");
        assert!(
            Path::new(&mount_dropin).exists(),
            "Mount drop-in should exist at {mount_dropin}"
        );

        // Verify mount drop-in content - should have Before= for all services
        let mount_content = fs::read_to_string(&mount_dropin).unwrap();
        assert!(mount_content.contains("[Unit]"));
        assert!(mount_content.contains("Before="));
        assert!(mount_content.contains("nginx.service"));
        assert!(mount_content.contains("prometheus.service"));

        // Clean up drop-ins
        let result = cleanup_service_dropins(extension, &services, &output);
        assert!(result.is_ok());

        // Verify service drop-ins were removed
        assert!(!Path::new(&nginx_dropin).exists());
        assert!(!Path::new(&prometheus_dropin).exists());

        // Verify mount drop-in was removed
        assert!(
            !Path::new(&mount_dropin).exists(),
            "Mount drop-in should be removed"
        );

        // Restore original environment variables
        match original_test_mode {
            Some(val) => std::env::set_var("AVOCADO_TEST_MODE", val),
            None => std::env::remove_var("AVOCADO_TEST_MODE"),
        }
        match original_test_tmpdir {
            Some(val) => std::env::set_var("AVOCADO_TEST_TMPDIR", val),
            None => std::env::remove_var("AVOCADO_TEST_TMPDIR"),
        }
    }

    #[test]
    fn test_create_service_dropins_empty_services() {
        let output = OutputManager::new(false, false);
        let services: Vec<String> = vec![];

        // Should return Ok without doing anything
        let result = create_service_dropins("test-ext", "/run/test", &services, &output);
        assert!(result.is_ok());
    }
}
