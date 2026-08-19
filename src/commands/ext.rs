use crate::commands::image_adaptor::{
    self, aggregate_failures, analyze_mounted_extension, extension_mount_point,
    unmount_all_persistent_mounts, ImageAdaptor, ImageType, ImageTypeTag, KabAdaptor, RawAdaptor,
};
use crate::config::Config;
use crate::output::OutputManager;
use clap::{Arg, ArgMatches, Command};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

// Re-export SystemdError so that service/error.rs From impl continues to work
pub use image_adaptor::SystemdError;

/// Represents an extension and its type(s)
#[derive(Debug, Clone)]
struct Extension {
    name: String,
    version: Option<String>, // Version extracted from filename (e.g., "1.0.0" from "app-1.0.0.raw")
    path: PathBuf,
    is_sysext: bool,
    is_confext: bool,
    image_type: ImageTypeTag,
    /// Merge priority index derived from manifest ordering.
    /// Used to compute a numerical prefix for deterministic systemd merge order.
    /// None for extensions discovered outside the manifest (legacy behavior).
    merge_index: Option<usize>,
}

/// Print a colored info message
fn print_colored_info(message: &str) {
    // Use auto-detection but fallback gracefully
    let color_choice =
        if std::env::var("NO_COLOR").is_ok() || std::env::var("AVOCADO_TEST_MODE").is_ok() {
            ColorChoice::Never
        } else {
            ColorChoice::Auto
        };

    let mut stdout = StandardStream::stdout(color_choice);
    let mut color_spec = ColorSpec::new();
    color_spec.set_fg(Some(Color::Blue)).set_bold(true);

    if stdout.set_color(&color_spec).is_ok() && color_choice != ColorChoice::Never {
        let _ = write!(&mut stdout, "[INFO]");
        let _ = stdout.reset();
        println!(" {message}");
    } else {
        // Fallback for environments without color support
        println!("[INFO] {message}");
    }
}

// Scope / initrd utilities are in image_adaptor — import locally for convenience.
use image_adaptor::is_running_in_initrd;
use image_adaptor::is_scope_enabled_for_current_environment;

/// Read the running rootfs's AVOCADO_OS_BUILD_ID from the appropriate os-release file.
/// Returns None if the field is not present (e.g. initial provisioned rootfs).
fn read_running_os_build_id() -> Option<String> {
    let paths: &[&str] = if is_running_in_initrd() {
        &["/sysroot/etc/os-release", "/sysroot/usr/lib/os-release"]
    } else {
        &["/etc/os-release", "/usr/lib/os-release"]
    };

    for path in paths {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Some(value) =
                crate::os_update::parse_os_release_field(&contents, "AVOCADO_OS_BUILD_ID")
            {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Create the ext subcommand definition
pub fn create_command() -> Command {
    Command::new("ext")
        .about("Extension management commands")
        .subcommand(Command::new("list").about("List all available extensions"))
        .subcommand(
            Command::new("merge")
                .about("Merge extensions using systemd-sysext and systemd-confext"),
        )
        .subcommand(
            Command::new("unmerge")
                .about("Unmerge extensions using systemd-sysext and systemd-confext")
                .arg(
                    Arg::new("unmount")
                        .long("unmount")
                        .help("Also unmount all persistent loops for .raw extensions")
                        .action(clap::ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("refresh").about("Unmerge and then merge extensions (refresh extensions)"),
        )
        .subcommand(Command::new("status").about("Show status of merged extensions"))
        .subcommand(
            Command::new("enable")
                .about("Mark one or more extensions as enabled (writes to overrides.json)")
                .arg(
                    Arg::new("names")
                        .help("Extension name(s) to enable")
                        .num_args(1..)
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("disable")
                .about("Mark one or more extensions as disabled (writes to overrides.json)")
                .arg(
                    Arg::new("names")
                        .help("Extension name(s) to disable")
                        .num_args(1..)
                        .required(true),
                ),
        )
}

/// Handle ext command and its subcommands
pub fn handle_command(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    match matches.subcommand() {
        Some(("list", _)) => {
            list_extensions(config, output);
        }
        Some(("merge", _)) => {
            merge_extensions(config, output);
        }
        Some(("unmerge", unmerge_matches)) => {
            let unmount = unmerge_matches.get_flag("unmount");
            unmerge_extensions(unmount, output);
        }
        Some(("refresh", _)) => {
            refresh_extensions(config, output);
        }
        Some(("status", _)) => {
            status_extensions(config, output);
        }
        Some(("enable", sub)) => {
            let names: Vec<String> = sub
                .get_many::<String>("names")
                .map(|vs| vs.cloned().collect())
                .unwrap_or_default();
            set_extensions_enabled(&names, true, output);
        }
        Some(("disable", sub)) => {
            let names: Vec<String> = sub
                .get_many::<String>("names")
                .map(|vs| vs.cloned().collect())
                .unwrap_or_default();
            set_extensions_enabled(&names, false, output);
        }
        _ => {
            println!("Use 'avocadoctl ext --help' for available extension commands");
        }
    }
}

/// CLI-facing wrapper around `service::ext::set_extensions_enabled` that
/// formats success / failure for the terminal. Used only by the
/// `AVOCADO_TEST_MODE` direct dispatch path — the production path goes
/// through varlink so the daemon owns serialization across callers.
pub fn set_extensions_enabled(names: &[String], enabled: bool, output: &OutputManager) {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    match crate::service::ext::set_extensions_enabled(&refs, enabled) {
        Ok(result) => {
            let verb = if enabled { "enabled" } else { "disabled" };
            output.success(
                "Extension Override",
                &format!(
                    "{verb}: {} ({} updated, {} missing)",
                    names.join(", "),
                    result.updated,
                    result.missing
                ),
            );
            output.info(
                "Extension Override",
                "Run `avocadoctl ext refresh` to apply.",
            );
        }
        Err(e) => {
            output.error("Extension Override", &e.to_string());
            std::process::exit(1);
        }
    }
}

/// List all extensions from disk images, annotating which are currently mounted/active.
fn list_extensions(_config: &Config, output: &OutputManager) {
    output.info("Extension List", "Listing available extensions");

    let available = match scan_extensions_from_all_sources_with_verbosity(output.is_verbose()) {
        Ok(exts) => exts,
        Err(e) => {
            eprintln!("Error scanning extensions: {e}");
            std::process::exit(1);
        }
    };

    if available.is_empty() {
        println!("No extensions found.");
        return;
    }

    // Collect mounted names for correlation (strip order prefix, ignore errors)
    let mounted_sysext: std::collections::HashSet<String> =
        get_mounted_systemd_extensions("systemd-sysext")
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .collect();
    let mounted_confext: std::collections::HashSet<String> =
        get_mounted_systemd_extensions("systemd-confext")
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .collect();

    // Sort descending by merge_index (highest priority / top layer first).
    // Extensions without a merge_index sort to the bottom.
    let mut sorted = available;
    sorted.sort_by(|a, b| {
        b.merge_index
            .cmp(&a.merge_index)
            .then_with(|| a.name.cmp(&b.name))
    });

    // Compute column width
    let name_width = sorted
        .iter()
        .map(|e| {
            if let Some(ver) = &e.version {
                e.name.len() + 1 + ver.len()
            } else {
                e.name.len()
            }
        })
        .max()
        .unwrap_or(9)
        .max(9);

    println!("  (high priority / top layer)");
    println!(
        "{:<6}{:<nw$} {:<12} {:<8}",
        "Order",
        "Extension",
        "Type",
        "Status",
        nw = name_width
    );
    println!("{}", "=".repeat(6 + name_width + 1 + 12 + 1 + 8));

    for ext in &sorted {
        let versioned_name = if let Some(ver) = &ext.version {
            format!("{}-{}", ext.name, ver)
        } else {
            ext.name.clone()
        };

        let order_str = ext
            .merge_index
            .map(|i| format!("#{i:02}"))
            .unwrap_or_else(|| "-".to_string());

        let mut types = Vec::new();
        if ext.is_sysext {
            types.push("sys");
        }
        if ext.is_confext {
            types.push("conf");
        }
        let type_str = if types.is_empty() {
            "?".to_string()
        } else {
            types.join("+")
        };

        let in_sysext = mounted_sysext.contains(&versioned_name);
        let in_confext = mounted_confext.contains(&versioned_name);
        let status = match (in_sysext, in_confext) {
            (true, true) => "MERGED",
            (true, false) => "SYSEXT",
            (false, true) => "CONFEXT",
            (false, false) => "READY",
        };

        println!("{order_str:<6}{versioned_name:<name_width$} {type_str:<12} {status}");
    }

    println!("  (low priority / base layer)");

    // Manifest-listed extensions that the scan filtered out because they
    // are effectively disabled. Surfaced separately so the user can see
    // them and know they exist to enable, rather than having them
    // silently vanish from `ext list`.
    let base_dir = crate::manifest::RuntimeManifest::base_dir();
    let base_path = Path::new(&base_dir);
    if let Some(manifest) = crate::manifest::RuntimeManifest::load_active(base_path) {
        let active_dir = base_path.join(crate::manifest::ACTIVE_LINK_NAME);
        let overrides = crate::overrides::RuntimeOverrides::load(&active_dir);
        let scanned: std::collections::HashSet<&str> =
            sorted.iter().map(|e| e.name.as_str()).collect();
        let disabled: Vec<&crate::manifest::ManifestExtension> = manifest
            .extensions
            .iter()
            .filter(|m| {
                !scanned.contains(m.name.as_str())
                    && !crate::overrides::effective_enabled(m, &overrides)
            })
            .collect();
        if !disabled.is_empty() {
            println!();
            println!("Disabled (present in manifest, not activated):");
            for m in &disabled {
                let reason = match overrides.enabled_override(&m.name) {
                    Some(false) => "user override",
                    None => "manifest default",
                    Some(true) => continue, // shouldn't happen given filter
                };
                println!("  {}-{}  ({reason})", m.name, m.version);
            }
        }
    }

    println!();
    println!("Total: {} active extension(s)", sorted.len());
}

/// Merge extensions using systemd-sysext and systemd-confext
pub fn merge_extensions(config: &Config, output: &OutputManager) {
    match merge_extensions_internal(config, output) {
        Ok(_) => {
            output.success("Extension Merge", "Extensions merged successfully");
        }
        Err(e) => {
            output.error(
                "Extension Merge",
                &format!("Failed to merge extensions: {e}"),
            );
            std::process::exit(1);
        }
    }
}

/// Internal merge function that returns a Result
pub(crate) fn merge_extensions_internal(
    config: &Config,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    // Check for pending OS update — verify the new OS booted correctly.
    // If a runtime_id is set, the runtime hasn't been activated yet and depends
    // on OS verification. On success, promote the pending runtime to active.
    // On failure, rollback the boot slot and keep the current active runtime.
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    if let Some(pending) = crate::os_update::read_pending_update() {
        let mut verified = true;

        // Verify rootfs os-release (/sysroot/etc/os-release when in initrd)
        if let Some(ref verify) = pending.verify {
            match crate::os_update::verify_os_release(verify) {
                Ok(true) => {
                    output.step(
                        "OS Update",
                        &format!("Verified rootfs — {}={}", verify.field, verify.expected),
                    );
                }
                Ok(false) => {
                    output.error(
                        "OS Update",
                        &format!(
                            "Rootfs {} mismatch — expected '{}'",
                            verify.field, verify.expected
                        ),
                    );
                    verified = false;
                }
                Err(e) => {
                    output.error("OS Update", &format!("Rootfs verification error: {e}"));
                    verified = false;
                }
            }
        }

        // Verify initrd identity (/etc/initrd-release when in initrd)
        if is_running_in_initrd() {
            if let Some(ref verify_initramfs) = pending.verify_initramfs {
                match crate::os_update::verify_os_release_initrd(verify_initramfs) {
                    Ok(true) => {
                        output.step(
                            "OS Update",
                            &format!(
                                "Verified initramfs — {}={}",
                                verify_initramfs.field, verify_initramfs.expected
                            ),
                        );
                    }
                    Ok(false) => {
                        output.error(
                            "OS Update",
                            &format!(
                                "Initramfs {} mismatch — expected '{}'",
                                verify_initramfs.field, verify_initramfs.expected
                            ),
                        );
                        verified = false;
                    }
                    Err(e) => {
                        output.error("OS Update", &format!("Initramfs verification error: {e}"));
                        verified = false;
                    }
                }
            }
        }

        if verified {
            output.step("OS Update", "Verification passed, clearing pending marker");
            // Promote pending runtime to active if one is set
            if let Some(ref runtime_id) = pending.runtime_id {
                match crate::staging::activate_runtime(runtime_id, base_path) {
                    Ok(()) => {
                        output.step(
                            "OS Update",
                            &format!("Activated pending runtime: {runtime_id}"),
                        );
                    }
                    Err(e) => {
                        output.error(
                            "OS Update",
                            &format!("Failed to activate pending runtime {runtime_id}: {e}"),
                        );
                    }
                }
            }
        } else {
            output.error("OS Update", "Pending update verification failed");
            // Rollback boot slot to previous OS
            if let Err(e) = crate::os_update::rollback_os_update(&pending, false) {
                output.error("OS Update", &format!("Rollback failed: {e}"));
            }
            if pending.runtime_id.is_some() {
                output.step(
                    "OS Update",
                    "Keeping current active runtime (OS update did not succeed)",
                );
            }
        }
        // Always clear pending marker to avoid re-checking on subsequent boots
        crate::os_update::clear_pending_update().ok();
    }

    // Verify rootfs matches what the active runtime expects.
    // If the runtime's os_bundle.os_build_id doesn't match the running rootfs,
    // try to fall back to a previous runtime that is compatible.
    // Never refuse to merge extensions — always make a best effort.
    if let Some(manifest) = crate::manifest::RuntimeManifest::load_active(base_path) {
        // Spot-check extension image integrity before merging
        let spot_bytes = config.get_spot_check_bytes();
        if let Err(e) = crate::staging::verify_spot_hashes(
            &manifest,
            base_path,
            spot_bytes,
            output.is_verbose(),
        ) {
            output.error(
                "Extension Merge",
                &format!("Image integrity spot check failed:\n{e}"),
            );
            return Err(SystemdError::ConfigurationError {
                message: format!("Image integrity spot check failed: {e}"),
            });
        }

        if let Some(ref os_bundle) = manifest.os_bundle {
            if let Some(ref expected_id) = os_bundle.os_build_id {
                match read_running_os_build_id() {
                    Some(ref running_id) if running_id == expected_id => {
                        // Rootfs matches — proceed normally
                    }
                    Some(ref running_id) => {
                        // Mismatch — try to fall back to a compatible runtime
                        output.error(
                            "Extension Merge",
                            &format!(
                                "Rootfs mismatch: active runtime expects AVOCADO_OS_BUILD_ID={} but running rootfs has {}",
                                expected_id, running_id
                            ),
                        );

                        let all_runtimes = crate::manifest::RuntimeManifest::list_all(base_path);
                        let fallback = all_runtimes.iter().find(|(rt, is_active)| {
                            !is_active
                                && match &rt.os_bundle {
                                    Some(bundle) => match &bundle.os_build_id {
                                        Some(rt_id) => rt_id == running_id,
                                        None => true,
                                    },
                                    None => true,
                                }
                        });

                        if let Some((fallback_rt, _)) = fallback {
                            output.step(
                                "Extension Merge",
                                &format!(
                                    "Falling back to runtime {} {} ({})",
                                    fallback_rt.runtime.name,
                                    fallback_rt.runtime.version,
                                    fallback_rt.id
                                ),
                            );
                            if let Err(e) =
                                crate::staging::activate_runtime(&fallback_rt.id, base_path)
                            {
                                output.error(
                                    "Extension Merge",
                                    &format!("Failed to activate fallback runtime: {e}"),
                                );
                            }
                        } else {
                            output.error(
                                "Extension Merge",
                                "No compatible runtime found — proceeding with current runtime (best effort)",
                            );
                        }
                    }
                    None => {
                        // AVOCADO_OS_BUILD_ID not present in os-release — skip check
                        // (initial provisioned rootfs or pre-avocado-cli image)
                    }
                }
            }
        }
    }

    let environment_info = if is_running_in_initrd() {
        "initrd environment"
    } else {
        "system environment"
    };
    output.info(
        "Extension Merge",
        &format!("Starting extension merge process in {environment_info}"),
    );

    // Prepare the environment by setting up symlinks and get the list of enabled extensions
    let enabled_extensions = prepare_extension_environment_with_output(output)?;

    // Get the mutability settings from config (separate for sysext and confext)
    let sysext_mutability = match config.get_sysext_mutable() {
        Ok(value) => value,
        Err(e) => {
            output.error(
                "Configuration Error",
                &format!("Invalid sysext mutable configuration: {e}"),
            );
            return Err(SystemdError::ConfigurationError {
                message: e.to_string(),
            });
        }
    };
    let sysext_mutable_arg = format!("--mutable={sysext_mutability}");

    let confext_mutability = match config.get_confext_mutable() {
        Ok(value) => value,
        Err(e) => {
            output.error(
                "Configuration Error",
                &format!("Invalid confext mutable configuration: {e}"),
            );
            return Err(SystemdError::ConfigurationError {
                message: e.to_string(),
            });
        }
    };
    let confext_mutable_arg = format!("--mutable={confext_mutability}");

    // Merge system extensions
    let sysext_result = run_systemd_command(
        "systemd-sysext",
        &["merge", &sysext_mutable_arg, "--json=short"],
    )?;
    handle_systemd_output("systemd-sysext merge", &sysext_result, output)?;

    // Merge configuration extensions
    let confext_result = run_systemd_command(
        "systemd-confext",
        &["merge", &confext_mutable_arg, "--json=short"],
    )?;
    handle_systemd_output("systemd-confext merge", &confext_result, output)?;

    // Process post-merge tasks for enabled extensions, with daemon-reload
    // happening after depmod/ldconfig/modprobe but before service commands.
    // This ensures kernel modules and shared libraries are available when
    // systemd re-evaluates units during daemon-reload.
    process_post_merge_tasks_for_extensions(&enabled_extensions, output)?;

    Ok(())
}

/// Unmerge extensions using systemd-sysext and systemd-confext
pub fn unmerge_extensions(unmount: bool, output: &OutputManager) {
    match unmerge_extensions_internal(unmount, output) {
        Ok(_) => {
            output.success("Extension Unmerge", "Extensions unmerged successfully");
        }
        Err(e) => {
            output.error(
                "Extension Unmerge",
                &format!("Failed to unmerge extensions: {e}"),
            );
            std::process::exit(1);
        }
    }
}

/// Internal unmerge function that returns a Result for use in refresh
fn unmerge_extensions_internal(unmount: bool, output: &OutputManager) -> Result<(), SystemdError> {
    unmerge_extensions_internal_with_depmod(true, unmount, output)
}

/// Internal unmerge function with optional depmod control
fn unmerge_extensions_internal_with_depmod(
    call_depmod: bool,
    unmount: bool,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    unmerge_extensions_internal_with_options(call_depmod, unmount, output)
}

/// Internal unmerge function with all options
pub(crate) fn unmerge_extensions_internal_with_options(
    call_depmod: bool,
    unmount: bool,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    let environment_info = if is_running_in_initrd() {
        "initrd environment"
    } else {
        "system environment"
    };
    output.info(
        "Extension Unmerge",
        &format!("Starting extension unmerge process in {environment_info}"),
    );

    // Execute AVOCADO_ON_UNMERGE commands before unmerging extensions
    // These commands are executed while extensions are still merged
    if let Err(e) = process_pre_unmerge_tasks(output) {
        output.progress(&format!(
            "Warning: Failed to process pre-unmerge tasks: {e}"
        ));
        // Continue with unmerge even if pre-unmerge tasks fail
    }

    // Unmerge system extensions
    let sysext_result = run_systemd_command("systemd-sysext", &["unmerge", "--json=short"])?;
    handle_systemd_output("systemd-sysext unmerge", &sysext_result, output)?;

    // Unmerge configuration extensions
    let confext_result = run_systemd_command("systemd-confext", &["unmerge", "--json=short"])?;
    handle_systemd_output("systemd-confext unmerge", &confext_result, output)?;

    // Clean up extension-release bind mounts and staging directories
    // Must happen after systemd unmerge but before loop unmount
    cleanup_extension_release_staging(output)?;

    // Clean up all symlinks to ensure fresh state for next merge
    cleanup_extension_symlinks(output)?;

    // Run depmod after unmerge if requested
    if call_depmod {
        run_depmod(output)?;
    }

    // Unmount persistent loops if requested
    if unmount {
        unmount_all_persistent_mounts()?;
    }

    Ok(())
}

/// Direct access functions for top-level command aliases
///
/// Merge extensions - direct access for top-level alias
pub fn merge_extensions_direct(output: &OutputManager) {
    // Use default config for direct access
    let config = Config::default();
    merge_extensions(&config, output);
}

/// Unmerge extensions - direct access for top-level alias
pub fn unmerge_extensions_direct(unmount: bool, output: &OutputManager) {
    unmerge_extensions(unmount, output);
}

/// Refresh extensions - direct access for top-level alias
pub fn refresh_extensions_direct(output: &OutputManager) {
    // Use default config for direct access
    let config = Config::default();
    refresh_extensions(&config, output);
}

/// Enable extensions for a specific OS release version
pub fn enable_extensions(
    os_release_version: Option<&str>,
    extensions: &[&str],
    config: &Config,
    output: &OutputManager,
) {
    // Warn if an active runtime manifest is present
    let base_dir = config.get_avocado_base_dir();
    if crate::manifest::RuntimeManifest::load_active(std::path::Path::new(&base_dir)).is_some() {
        eprintln!("Warning: An active runtime manifest is present. The manifest takes precedence over symlink-based extension discovery during merge/refresh.");
    }

    // Determine the OS release version to use
    let version_id = if let Some(version) = os_release_version {
        version.to_string()
    } else {
        read_os_version_id()
    };

    output.info(
        "Enable Extensions",
        &format!("Enabling extensions for OS release version: {version_id}"),
    );

    // Get the extensions directory from config
    let extensions_dir = config.get_extensions_dir();

    // Determine os-releases directory based on test mode
    let os_releases_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/os-releases/{version_id}")
    } else {
        format!("/var/lib/avocado/os-releases/{version_id}")
    };

    // Create the os-releases directory if it doesn't exist
    if let Err(e) = fs::create_dir_all(&os_releases_dir) {
        output.error(
            "Enable Extensions",
            &format!("Failed to create os-releases directory '{os_releases_dir}': {e}"),
        );
        std::process::exit(1);
    }

    // Sync the parent directory to ensure the os-releases directory is persisted
    if let Err(e) = sync_directory(
        Path::new(&os_releases_dir)
            .parent()
            .unwrap_or(Path::new("/")),
    ) {
        output.progress(&format!("Warning: Failed to sync parent directory: {e}"));
    }

    output.step(
        "Enable",
        &format!("Created os-releases directory: {os_releases_dir}"),
    );

    // Process each extension
    let mut success_count = 0;
    let mut error_count = 0;

    for ext_name in extensions {
        // Check if extension exists - try both directory and .raw file
        let ext_dir_path = format!("{extensions_dir}/{ext_name}");
        let ext_raw_path = format!("{extensions_dir}/{ext_name}.raw");

        let source_path = if Path::new(&ext_dir_path).exists() {
            ext_dir_path
        } else if Path::new(&ext_raw_path).exists() {
            ext_raw_path
        } else {
            output.error(
                "Enable Extensions",
                &format!("Extension '{ext_name}' not found in {extensions_dir}"),
            );
            error_count += 1;
            continue;
        };

        // Create symlink in os-releases directory
        let target_path = format!(
            "{}/{}",
            os_releases_dir,
            Path::new(&source_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
        );

        // Remove existing symlink if it exists
        if Path::new(&target_path).exists() {
            if let Err(e) = fs::remove_file(&target_path) {
                output.error(
                    "Enable Extensions",
                    &format!("Failed to remove existing symlink '{target_path}': {e}"),
                );
                error_count += 1;
                continue;
            }
        }

        // Create the symlink
        if let Err(e) = unix_fs::symlink(&source_path, &target_path) {
            output.error(
                "Enable Extensions",
                &format!("Failed to create symlink for '{ext_name}': {e}"),
            );
            error_count += 1;
        } else {
            output.progress(&format!("Enabled extension: {ext_name}"));
            success_count += 1;
        }
    }

    // Sync the os-releases directory to ensure all symlinks are persisted to disk
    if success_count > 0 {
        if let Err(e) = sync_directory(Path::new(&os_releases_dir)) {
            output.error(
                "Enable Extensions",
                &format!("Failed to sync os-releases directory to disk: {e}"),
            );
            std::process::exit(1);
        }
        output.progress("Synced changes to disk");
    }

    // Summary
    if error_count > 0 {
        output.error(
            "Enable Extensions",
            &format!("Completed with errors: {success_count} succeeded, {error_count} failed"),
        );
        std::process::exit(1);
    } else {
        output.success(
            "Enable Extensions",
            &format!(
                "Successfully enabled {success_count} extension(s) for OS release {version_id}"
            ),
        );
    }
}

/// Sync a directory to ensure all changes are persisted to disk
pub(crate) fn sync_directory(dir_path: &Path) -> Result<(), SystemdError> {
    // Open the directory
    let dir = fs::File::open(dir_path).map_err(|e| SystemdError::CommandFailed {
        command: format!("open directory {}", dir_path.display()),
        source: e,
    })?;

    // Sync the directory to disk
    // This ensures directory entries (like new symlinks) are persisted
    dir.sync_all().map_err(|e| SystemdError::CommandFailed {
        command: format!("sync directory {}", dir_path.display()),
        source: e,
    })?;

    Ok(())
}

/// Disable extensions for a specific OS release version
pub fn disable_extensions(
    os_release_version: Option<&str>,
    extensions: Option<&[&str]>,
    all: bool,
    config: &Config,
    output: &OutputManager,
) {
    // Warn if an active runtime manifest is present
    let base_dir = config.get_avocado_base_dir();
    if crate::manifest::RuntimeManifest::load_active(std::path::Path::new(&base_dir)).is_some() {
        eprintln!("Warning: An active runtime manifest is present. The manifest takes precedence over symlink-based extension discovery during merge/refresh.");
    }

    // Determine the OS release version to use
    let version_id = if let Some(version) = os_release_version {
        version.to_string()
    } else {
        read_os_version_id()
    };

    output.info(
        "Disable Extensions",
        &format!("Disabling extensions for OS release version: {version_id}"),
    );

    // Determine os-releases directory based on test mode
    let os_releases_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/os-releases/{version_id}")
    } else {
        format!("/var/lib/avocado/os-releases/{version_id}")
    };

    // Check if os-releases directory exists
    if !Path::new(&os_releases_dir).exists() {
        output.error(
            "Disable Extensions",
            &format!("OS releases directory '{os_releases_dir}' does not exist"),
        );
        std::process::exit(1);
    }

    let mut success_count = 0;
    let mut error_count = 0;

    if all {
        // Disable all extensions by removing all symlinks in the os-releases directory
        output.step("Disable", "Removing all extensions");

        match fs::read_dir(&os_releases_dir) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) => {
                            let path = entry.path();
                            // Only remove symlinks, not regular files or directories
                            if path.is_symlink() {
                                if let Some(file_name) = path.file_name() {
                                    if let Some(name_str) = file_name.to_str() {
                                        match fs::remove_file(&path) {
                                            Ok(_) => {
                                                output.progress(&format!(
                                                    "Disabled extension: {name_str}"
                                                ));
                                                success_count += 1;
                                            }
                                            Err(e) => {
                                                output.error(
                                                    "Disable Extensions",
                                                    &format!("Failed to remove symlink '{name_str}': {e}"),
                                                );
                                                error_count += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            output.error(
                                "Disable Extensions",
                                &format!("Failed to read directory entry: {e}"),
                            );
                            error_count += 1;
                        }
                    }
                }
            }
            Err(e) => {
                output.error(
                    "Disable Extensions",
                    &format!("Failed to read os-releases directory '{os_releases_dir}': {e}"),
                );
                std::process::exit(1);
            }
        }
    } else if let Some(ext_names) = extensions {
        // Disable specific extensions
        for ext_name in ext_names {
            // Check for both directory and .raw file symlinks
            let symlink_dir = format!("{os_releases_dir}/{ext_name}");
            let symlink_raw = format!("{os_releases_dir}/{ext_name}.raw");

            let mut found = false;

            // Try to remove directory symlink
            if Path::new(&symlink_dir).exists() {
                match fs::remove_file(&symlink_dir) {
                    Ok(_) => {
                        output.progress(&format!("Disabled extension: {ext_name}"));
                        success_count += 1;
                        found = true;
                    }
                    Err(e) => {
                        output.error(
                            "Disable Extensions",
                            &format!("Failed to remove symlink for '{ext_name}': {e}"),
                        );
                        error_count += 1;
                        found = true;
                    }
                }
            }

            // Try to remove .raw symlink
            if Path::new(&symlink_raw).exists() {
                match fs::remove_file(&symlink_raw) {
                    Ok(_) => {
                        if !found {
                            output.progress(&format!("Disabled extension: {ext_name}"));
                            success_count += 1;
                        }
                        found = true;
                    }
                    Err(e) => {
                        output.error(
                            "Disable Extensions",
                            &format!("Failed to remove .raw symlink for '{ext_name}': {e}"),
                        );
                        error_count += 1;
                        found = true;
                    }
                }
            }

            if !found {
                output.error(
                    "Disable Extensions",
                    &format!("Extension '{ext_name}' is not enabled for OS release {version_id}"),
                );
                error_count += 1;
            }
        }
    } else {
        // This should not happen due to clap validation, but handle it anyway
        output.error(
            "Disable Extensions",
            "No extensions specified. Use --all to disable all extensions or specify extension names.",
        );
        std::process::exit(1);
    }

    // Sync the os-releases directory to ensure all removals are persisted to disk
    if success_count > 0 {
        if let Err(e) = sync_directory(Path::new(&os_releases_dir)) {
            output.error(
                "Disable Extensions",
                &format!("Failed to sync os-releases directory to disk: {e}"),
            );
            std::process::exit(1);
        }
        output.progress("Synced changes to disk");
    }

    // Summary
    if error_count > 0 {
        output.error(
            "Disable Extensions",
            &format!("Completed with errors: {success_count} succeeded, {error_count} failed"),
        );
        std::process::exit(1);
    } else {
        output.success(
            "Disable Extensions",
            &format!(
                "Successfully disabled {success_count} extension(s) for OS release {version_id}"
            ),
        );
    }
}

/// Invalidate NFS caches for HITL-mounted extensions
///
/// When extensions are mounted via NFS from a HITL server, the client may have
/// stale cached data after the host rebuilds the extension. This function forces
/// a remount of each HITL mount to invalidate the NFS client cache, ensuring
/// fresh data is fetched from the server on the next access.
pub(crate) fn invalidate_hitl_caches(output: &OutputManager) {
    let hitl_dir = std::path::Path::new("/run/avocado/hitl");

    // Skip if not in test mode and no HITL directory exists
    if std::env::var("AVOCADO_TEST_MODE").is_err() && !hitl_dir.exists() {
        return;
    }

    // In test mode, use the test directory
    let hitl_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("AVOCADO_TEST_TMPDIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        std::path::PathBuf::from(format!("{temp_base}/avocado/hitl"))
    } else {
        hitl_dir.to_path_buf()
    };

    if !hitl_dir.exists() {
        return;
    }

    let entries = match std::fs::read_dir(&hitl_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let extension_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");

            output.step(
                "HITL",
                &format!("Invalidating NFS cache for extension: {extension_name}"),
            );

            // Skip actual remount in test mode
            if std::env::var("AVOCADO_TEST_MODE").is_ok() {
                output.progress(&format!(
                    "Skipping remount in test mode for: {}",
                    path.display()
                ));
                continue;
            }

            // Remount to invalidate NFS client cache
            let result = std::process::Command::new("mount")
                .args(["-o", "remount"])
                .arg(&path)
                .output();

            match result {
                Ok(output_result) => {
                    if !output_result.status.success() {
                        let stderr = String::from_utf8_lossy(&output_result.stderr);
                        output.progress(&format!(
                            "Warning: Failed to remount {}: {}",
                            path.display(),
                            stderr.trim()
                        ));
                    }
                }
                Err(e) => {
                    output.progress(&format!(
                        "Warning: Could not execute remount for {}: {}",
                        path.display(),
                        e
                    ));
                }
            }
        }
    }
}

/// Refresh extensions (unmerge then merge)
pub fn refresh_extensions(config: &Config, output: &OutputManager) {
    let environment_info = if is_running_in_initrd() {
        "initrd environment"
    } else {
        "system environment"
    };
    output.info(
        "Extension Refresh",
        &format!("Starting extension refresh process in {environment_info}"),
    );

    // First unmerge (skip depmod since we'll call it after merge, don't unmount loops —
    // the caller may be running from a loop-mounted extension like avocado-connect)
    if let Err(e) = unmerge_extensions_internal_with_options(false, false, output) {
        output.error(
            "Extension Refresh",
            &format!("Failed to unmerge extensions: {e}"),
        );
        std::process::exit(1);
    }
    output.step("Refresh", "Extensions unmerged");

    // Invalidate NFS caches for any HITL-mounted extensions
    // This ensures fresh data is fetched from the server after a host rebuild
    invalidate_hitl_caches(output);

    // Then merge (this will call depmod via post-merge processing)
    if let Err(e) = merge_extensions_internal(config, output) {
        output.error(
            "Extension Refresh",
            &format!("Failed to merge extensions: {e}"),
        );
        std::process::exit(1);
    }
    output.step("Refresh", "Extensions merged");

    output.success("Extension Refresh", "Extensions refreshed successfully");
}

/// Show status of merged extensions
pub fn status_extensions(config: &Config, output: &OutputManager) {
    match show_enhanced_status(config, output) {
        Ok(_) => {}
        Err(e) => {
            if output.is_json() {
                println!(
                    "{}",
                    serde_json::json!({"error": format!("Failed to show status: {e}")})
                );
                return;
            }
            output.error("Extension Status", &format!("Failed to show status: {e}"));
            show_legacy_status(output);
        }
    }
}

/// Collect extension status data for the varlink Status RPC.
///
/// This gathers the same data as `show_enhanced_status` but returns it as
/// structured `ExtensionStatus` values instead of printing to stdout.
pub(crate) fn collect_extension_status(
    config: &Config,
) -> Result<Vec<crate::varlink::org_avocado_Extensions::ExtensionStatus>, SystemdError> {
    use crate::varlink::org_avocado_Extensions::ExtensionStatus;

    let base_dir = config.get_avocado_base_dir();
    let base_path = std::path::Path::new(&base_dir);
    let active_manifest = crate::manifest::RuntimeManifest::load_active(base_path);
    let manifest_extensions = active_manifest
        .as_ref()
        .map(|m| m.extensions.as_slice())
        .unwrap_or(&[]);

    let available_extensions = scan_extensions_from_all_sources_with_verbosity(false)?;
    let mounted_sysext = get_mounted_systemd_extensions("systemd-sysext")?;
    let mounted_confext = get_mounted_systemd_extensions("systemd-confext")?;

    // Collect all unique extension names (with versions if present)
    let mut all_names = std::collections::HashSet::new();
    for ext in &available_extensions {
        if let Some(ver) = &ext.version {
            all_names.insert(format!("{}-{}", ext.name, ver));
        } else {
            all_names.insert(ext.name.clone());
        }
    }
    for ext in &mounted_sysext {
        all_names.insert(ext.name.clone());
    }
    for ext in &mounted_confext {
        all_names.insert(ext.name.clone());
    }

    let mut result: Vec<ExtensionStatus> = all_names
        .into_iter()
        .map(|ext_name| {
            let available_ext = available_extensions.iter().find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == ext_name
                } else {
                    e.name == ext_name
                }
            });

            let is_sysext_mounted = mounted_sysext.iter().any(|e| e.name == ext_name);
            let is_confext_mounted = mounted_confext.iter().any(|e| e.name == ext_name);
            let is_merged = is_sysext_mounted || is_confext_mounted;

            let (is_sysext, is_confext) = if let Some(ext) = available_ext {
                (ext.is_sysext, ext.is_confext)
            } else {
                (is_sysext_mounted, is_confext_mounted)
            };

            let origin = available_ext.map(get_extension_origin_short);

            let image_id_str = lookup_extension_short_id(&ext_name, manifest_extensions);
            let image_id = if image_id_str == "-" {
                None
            } else {
                Some(image_id_str)
            };

            let (name, version) = if let Some(ext) = available_ext {
                (ext.name.clone(), ext.version.clone())
            } else {
                (ext_name, None)
            };

            ExtensionStatus {
                name,
                version,
                isSysext: is_sysext,
                isConfext: is_confext,
                isMerged: is_merged,
                origin,
                imageId: image_id,
                imageType: available_ext.and_then(|e| match e.image_type {
                    ImageTypeTag::Kab => Some("kab".to_string()),
                    _ => None,
                }),
            }
        })
        .collect();

    // Sort descending by merge_index (highest priority / top layer first).
    // Extensions without a merge_index sort to the bottom, then alphabetically.
    result.sort_by(|a, b| {
        let versioned_a = match &a.version {
            Some(v) => format!("{}-{}", a.name, v),
            None => a.name.clone(),
        };
        let versioned_b = match &b.version {
            Some(v) => format!("{}-{}", b.name, v),
            None => b.name.clone(),
        };
        let idx_a = available_extensions
            .iter()
            .find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == versioned_a
                } else {
                    e.name == versioned_a
                }
            })
            .and_then(|e| e.merge_index);
        let idx_b = available_extensions
            .iter()
            .find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == versioned_b
                } else {
                    e.name == versioned_b
                }
            })
            .and_then(|e| e.merge_index);
        idx_b.cmp(&idx_a).then_with(|| a.name.cmp(&b.name))
    });

    Ok(result)
}

/// Show enhanced status with extension origins and HITL information
pub(crate) fn show_enhanced_status(
    config: &Config,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    // Load active manifest
    let base_dir = config.get_avocado_base_dir();
    let base_path = std::path::Path::new(&base_dir);
    let active_manifest = crate::manifest::RuntimeManifest::load_active(base_path);
    let manifest_extensions = active_manifest
        .as_ref()
        .map(|m| m.extensions.as_slice())
        .unwrap_or(&[]);

    // Get our view of available extensions
    let available_extensions =
        scan_extensions_from_all_sources_with_verbosity(output.is_verbose())?;

    // Get systemd's view of mounted extensions
    let mounted_sysext = get_mounted_systemd_extensions("systemd-sysext")?;
    let mounted_confext = get_mounted_systemd_extensions("systemd-confext")?;

    if output.is_json() {
        let runtime_json = match &active_manifest {
            Some(m) => {
                let mut rj = serde_json::json!({
                    "name": m.runtime.name,
                    "version": m.runtime.version,
                    "id": m.id,
                    "built_at": m.built_at,
                    "manifest_version": m.manifest_version,
                });
                if let Some(ref os_bundle) = m.os_bundle {
                    rj["os_bundle"] = serde_json::json!({
                        "image_id": os_bundle.image_id,
                        "sha256": os_bundle.sha256,
                        "os_build_id": os_bundle.os_build_id,
                        "initramfs_build_id": os_bundle.initramfs_build_id,
                    });
                }
                rj
            }
            None => serde_json::Value::Null,
        };

        let extensions_json: Vec<serde_json::Value> = build_extension_json_list(
            &available_extensions,
            &mounted_sysext,
            &mounted_confext,
            manifest_extensions,
        );

        let status_json = serde_json::json!({
            "runtime": runtime_json,
            "extensions": extensions_json,
        });
        println!("{}", serde_json::to_string_pretty(&status_json).unwrap());
        return Ok(());
    }

    output.status_header("Avocado Extension Status");

    // Display active runtime info
    display_active_runtime(config, output);

    // Create comprehensive status
    display_extension_status(
        &available_extensions,
        &mounted_sysext,
        &mounted_confext,
        manifest_extensions,
    )?;

    Ok(())
}

/// Display the active runtime configuration
fn display_active_runtime(config: &Config, output: &OutputManager) {
    let base_dir = config.get_avocado_base_dir();
    let base_path = std::path::Path::new(&base_dir);

    match crate::manifest::RuntimeManifest::load_active(base_path) {
        Some(manifest) => {
            let short_id = if manifest.id.len() >= 8 {
                &manifest.id[..8]
            } else {
                &manifest.id
            };
            println!("Active Runtime:");
            println!(
                "  {} {} ({short_id})",
                manifest.runtime.name, manifest.runtime.version
            );
            println!("  Built: {}", manifest.built_at);
            println!("  Extensions: {}", manifest.extensions.len());
            if let Some(ref os_bundle) = manifest.os_bundle {
                if let Some(ref id) = os_bundle.os_build_id {
                    println!("  OS Build ID (manifest): {id}");
                }
                if let Some(ref id) = os_bundle.initramfs_build_id {
                    println!("  Initramfs Build ID:     {id}");
                }
            }
            // Show the running system's AVOCADO_OS_BUILD_ID for comparison
            let os_release_path = if is_running_in_initrd() {
                "/etc/os-release-initrd"
            } else {
                "/etc/os-release"
            };
            if let Ok(contents) = std::fs::read_to_string(os_release_path) {
                for line in contents.lines() {
                    if let Some(value) = line.strip_prefix("AVOCADO_OS_BUILD_ID=") {
                        let label = if is_running_in_initrd() {
                            "Initramfs Build ID (running)"
                        } else {
                            "OS Build ID (running)"
                        };
                        println!("  {label}:  {}", value.trim_matches('"'));
                        break;
                    }
                }
            }
            if output.is_verbose() {
                println!("  Build ID: {}", manifest.id);
                for ext in &manifest.extensions {
                    let id_display = ext.image_id.as_deref().unwrap_or("?");
                    println!("    - {} {} ({})", ext.name, ext.version, id_display);
                }
            }
            println!();
        }
        None => {
            println!("Active Runtime: none (using legacy extension discovery)");
            println!();
        }
    }
}

/// Legacy status display for fallback
fn show_legacy_status(output: &OutputManager) {
    output.status("Legacy status display not yet implemented");
    println!("Extension Status");
    println!("================");
    println!();

    // Get system extensions status
    println!("System Extensions (/opt, /usr):");
    println!("--------------------------------");
    match run_systemd_command("systemd-sysext", &["status"]) {
        Ok(output) => {
            if output.trim().is_empty() {
                println!("No system extensions currently merged.");
            } else {
                format_status_output(&output);
            }
        }
        Err(e) => {
            eprintln!("Error getting system extensions status: {e}");
        }
    }

    println!();

    // Get configuration extensions status
    println!("Configuration Extensions (/etc):");
    println!("---------------------------------");
    match run_systemd_command("systemd-confext", &["status"]) {
        Ok(output) => {
            if output.trim().is_empty() {
                println!("No configuration extensions currently merged.");
            } else {
                format_status_output(&output);
            }
        }
        Err(e) => {
            eprintln!("Error getting configuration extensions status: {e}");
        }
    }
}

/// Structure to represent mounted extension info from systemd
#[derive(Debug, Clone)]
struct MountedExtension {
    name: String,
    #[allow(dead_code)] // May be used in future for hierarchy-specific logic
    hierarchy: String,
}

/// Strip a numeric order prefix (e.g. "00-", "03-") from an extension name.
/// These prefixes are added by avocadoctl to enforce systemd merge ordering.
fn strip_order_prefix(name: &str) -> &str {
    let end = name.bytes().take_while(|b| b.is_ascii_digit()).count();
    if end > 0 && name.as_bytes().get(end) == Some(&b'-') {
        &name[end + 1..]
    } else {
        name
    }
}

/// Get mounted extensions from systemd using JSON format
fn get_mounted_systemd_extensions(command: &str) -> Result<Vec<MountedExtension>, SystemdError> {
    let mut mounted = Vec::new();

    let output = run_systemd_command(command, &["status", "--json=short"])?;
    if output.trim().is_empty() {
        return Ok(mounted);
    }

    // Parse JSON output
    let json_data: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| SystemdError::CommandFailed {
            command: format!("{command} status --json=short"),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;

    // Handle both single object and array formats
    let hierarchies = if json_data.is_array() {
        json_data.as_array().unwrap()
    } else {
        std::slice::from_ref(&json_data)
    };

    for hierarchy_obj in hierarchies {
        let hierarchy = hierarchy_obj["hierarchy"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        // Handle extensions field - can be string "none" or array of strings
        if let Some(extensions) = hierarchy_obj["extensions"].as_array() {
            // Array of extension names — strip any "NN-" ordering prefix before storing
            for ext in extensions {
                if let Some(ext_name) = ext.as_str() {
                    mounted.push(MountedExtension {
                        name: strip_order_prefix(ext_name).to_string(),
                        hierarchy: hierarchy.clone(),
                    });
                }
            }
        } else if let Some(ext_str) = hierarchy_obj["extensions"].as_str() {
            // Single string - skip if it's "none"
            if ext_str != "none" {
                mounted.push(MountedExtension {
                    name: strip_order_prefix(ext_str).to_string(),
                    hierarchy: hierarchy.clone(),
                });
            }
        }
    }

    Ok(mounted)
}

/// Build a JSON representation of all extensions for machine-readable output
fn build_extension_json_list(
    available: &[Extension],
    mounted_sysext: &[MountedExtension],
    mounted_confext: &[MountedExtension],
    manifest_extensions: &[crate::manifest::ManifestExtension],
) -> Vec<serde_json::Value> {
    let mut all_extensions = std::collections::HashSet::new();

    for ext in available {
        if let Some(ver) = &ext.version {
            all_extensions.insert(format!("{}-{}", ext.name, ver));
        } else {
            all_extensions.insert(ext.name.clone());
        }
    }
    for ext in mounted_sysext {
        all_extensions.insert(ext.name.clone());
    }
    for ext in mounted_confext {
        all_extensions.insert(ext.name.clone());
    }

    let mut sorted: Vec<_> = all_extensions.into_iter().collect();
    sorted.sort();

    sorted
        .iter()
        .map(|ext_name| {
            let available_ext = available.iter().find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == *ext_name
                } else {
                    e.name == *ext_name
                }
            });

            let is_sysext = mounted_sysext.iter().any(|e| e.name == *ext_name);
            let is_confext = mounted_confext.iter().any(|e| e.name == *ext_name);

            let status = match (is_sysext, is_confext) {
                (true, true) => "MERGED",
                (true, false) => "SYSEXT",
                (false, true) => "CONFEXT",
                (false, false) => {
                    if available_ext.is_some() {
                        "READY"
                    } else {
                        "UNKNOWN"
                    }
                }
            };

            let mut types = Vec::new();
            if let Some(ext) = available_ext {
                if ext.is_sysext {
                    types.push("sys");
                }
                if ext.is_confext {
                    types.push("conf");
                }
            }

            let origin = available_ext
                .map(get_extension_origin_short)
                .unwrap_or_else(|| "?".to_string());

            let short_id = lookup_extension_short_id(ext_name, manifest_extensions);

            let order = available_ext.and_then(|e| e.merge_index);

            serde_json::json!({
                "name": ext_name,
                "order": order,
                "id": if short_id == "-" { serde_json::Value::Null } else { serde_json::Value::String(short_id) },
                "status": status,
                "type": if types.is_empty() { vec!["?"] } else { types },
                "origin": origin,
            })
        })
        .collect()
}

/// Display comprehensive extension status
fn display_extension_status(
    available: &[Extension],
    mounted_sysext: &[MountedExtension],
    mounted_confext: &[MountedExtension],
    manifest_extensions: &[crate::manifest::ManifestExtension],
) -> Result<(), SystemdError> {
    // Collect all unique extension names (with versions if present)
    let mut all_extensions = std::collections::HashSet::new();

    // For available extensions, use versioned name if available
    for ext in available {
        if let Some(ver) = &ext.version {
            all_extensions.insert(format!("{}-{}", ext.name, ver));
        } else {
            all_extensions.insert(ext.name.clone());
        }
    }

    // Add mounted extensions (these already include versions in their names)
    for ext in mounted_sysext {
        all_extensions.insert(ext.name.clone());
    }
    for ext in mounted_confext {
        all_extensions.insert(ext.name.clone());
    }

    if all_extensions.is_empty() {
        println!("No extensions found or mounted.");
        return Ok(());
    }

    // Sort descending by merge_index (highest priority / top layer first).
    // Extensions without a merge_index sort to the bottom.
    let mut sorted_extensions: Vec<_> = all_extensions.into_iter().collect();
    sorted_extensions.sort_by(|a, b| {
        let idx_a = available
            .iter()
            .find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == *a
                } else {
                    e.name == *a
                }
            })
            .and_then(|e| e.merge_index);
        let idx_b = available
            .iter()
            .find(|e| {
                if let Some(ver) = &e.version {
                    format!("{}-{}", e.name, ver) == *b
                } else {
                    e.name == *b
                }
            })
            .and_then(|e| e.merge_index);
        // Descending by index; None sorts last
        idx_b.cmp(&idx_a).then_with(|| a.cmp(b))
    });

    // Compute dynamic column width from the longest extension name
    let name_width = sorted_extensions
        .iter()
        .map(|n| n.len())
        .max()
        .unwrap_or(9)
        .max(9); // at least as wide as "Extension"

    let total_width = 6 + name_width + 1 + 10 + 1 + 10 + 1 + 12 + 1 + 10;

    // Display header — top-of-stack indicator makes the overlay direction explicit
    println!("  (high priority / top layer)");
    println!(
        "{:<6}{:<nw$} {:<10} {:<10} {:<12} Origin",
        "Order",
        "Extension",
        "ID",
        "Status",
        "Type",
        nw = name_width
    );
    println!("{}", "=".repeat(total_width));

    for ext_name in &sorted_extensions {
        display_extension_info(
            ext_name,
            available,
            mounted_sysext,
            mounted_confext,
            manifest_extensions,
            name_width,
        );
    }

    println!("  (low priority / base layer)");

    // Display summary
    println!();
    display_status_summary(available, mounted_sysext, mounted_confext);

    Ok(())
}

/// Display information for a single extension
fn display_extension_info(
    ext_name: &str,
    available: &[Extension],
    mounted_sysext: &[MountedExtension],
    mounted_confext: &[MountedExtension],
    manifest_extensions: &[crate::manifest::ManifestExtension],
    name_width: usize,
) {
    // Find extension in available list (match by full versioned name or base name)
    let available_ext = available.iter().find(|e| {
        if let Some(ver) = &e.version {
            format!("{}-{}", e.name, ver) == ext_name
        } else {
            e.name == ext_name
        }
    });

    let sysext_mount = mounted_sysext.iter().find(|e| e.name == ext_name);
    let confext_mount = mounted_confext.iter().find(|e| e.name == ext_name);

    // Determine status
    let status = match (sysext_mount.is_some(), confext_mount.is_some()) {
        (true, true) => "MERGED",
        (true, false) => "SYSEXT",
        (false, true) => "CONFEXT",
        (false, false) => {
            if available_ext.is_some() {
                "READY"
            } else {
                "UNKNOWN"
            }
        }
    };

    // Determine types
    let mut types = Vec::new();
    if let Some(ext) = available_ext {
        if ext.is_sysext {
            types.push("sys");
        }
        if ext.is_confext {
            types.push("conf");
        }
    }
    let type_str = if types.is_empty() {
        "?".to_string()
    } else {
        let base = types.join("+");
        if available_ext.is_some_and(|e| e.image_type == ImageTypeTag::Kab) {
            format!("kab:{base}")
        } else {
            base
        }
    };

    // Determine origin
    let origin = if let Some(ext) = available_ext {
        get_extension_origin_short(ext)
    } else {
        "?".to_string()
    };

    // Look up short image ID from manifest extensions
    let short_id = lookup_extension_short_id(ext_name, manifest_extensions);

    // Show merge order if available
    let order_str = if let Some(ext) = available_ext {
        if let Some(idx) = ext.merge_index {
            format!("#{idx:02}")
        } else {
            "-".to_string()
        }
    } else {
        "-".to_string()
    };

    println!(
        "{order_str:<6}{ext_name:<name_width$} {short_id:<10} {status:<10} {type_str:<12} {origin}"
    );
}

/// Look up the short image ID (first 8 chars) for an extension by matching
/// the versioned name (e.g. "app-0.2.0") against manifest extension entries.
fn lookup_extension_short_id(
    ext_name: &str,
    manifest_extensions: &[crate::manifest::ManifestExtension],
) -> String {
    let matched = manifest_extensions.iter().find(|me| {
        let versioned = format!("{}-{}", me.name, me.version);
        versioned == ext_name || me.name == ext_name
    });
    match matched {
        Some(me) => match &me.image_id {
            Some(id) if id.len() >= 8 => id[..8].to_string(),
            Some(id) => id.clone(),
            None => "-".to_string(),
        },
        None => "-".to_string(),
    }
}

/// Get short extension origin description (for 80-column display)
fn get_extension_origin_short(ext: &Extension) -> String {
    let path_str = ext.path.to_string_lossy();

    if path_str.contains("/hitl") {
        "HITL".to_string()
    } else {
        match ext.image_type {
            ImageTypeTag::Directory => "Dir".to_string(),
            ImageTypeTag::Kab => {
                if let Some(filename) = ext.path.file_name() {
                    format!("KAB:{}", filename.to_string_lossy())
                } else {
                    "KAB".to_string()
                }
            }
            ImageTypeTag::Raw => {
                if let Some(filename) = ext.path.file_name() {
                    format!("Loop:{}", filename.to_string_lossy())
                } else {
                    "Loop".to_string()
                }
            }
        }
    }
}

/// Display status summary
fn display_status_summary(
    available: &[Extension],
    mounted_sysext: &[MountedExtension],
    mounted_confext: &[MountedExtension],
) {
    let hitl_count = available
        .iter()
        .filter(|e| e.path.to_string_lossy().contains("/hitl"))
        .count();
    let directory_count = available
        .iter()
        .filter(|e| {
            e.image_type == ImageTypeTag::Directory && !e.path.to_string_lossy().contains("/hitl")
        })
        .count();
    let loop_count = available
        .iter()
        .filter(|e| e.image_type != ImageTypeTag::Directory)
        .count();

    let unique_sysext: std::collections::HashSet<&str> =
        mounted_sysext.iter().map(|e| e.name.as_str()).collect();
    let unique_confext: std::collections::HashSet<&str> =
        mounted_confext.iter().map(|e| e.name.as_str()).collect();

    println!("Summary:");
    println!("  Available Extensions: {} total", available.len());
    println!("    - HITL mounted: {hitl_count}");
    println!("    - Local directories: {directory_count}");
    println!("    - Loop devices: {loop_count}");
    println!("  Mounted Extensions:");
    println!("    - System extensions: {}", unique_sysext.len());
    println!("    - Configuration extensions: {}", unique_confext.len());

    if hitl_count > 0 {
        print_colored_info("HITL extensions are active - development mode");
    }
}

/// Format status output from systemd commands
fn format_status_output(output: &str) {
    let lines: Vec<&str> = output.lines().collect();

    // Skip the header line if present and process the data
    let data_lines: Vec<&str> = lines
        .iter()
        .skip_while(|line| line.starts_with("HIERARCHY") || line.trim().is_empty())
        .copied()
        .collect();

    if data_lines.is_empty() {
        println!("No extensions currently merged.");
        return;
    }

    for line in data_lines {
        if line.trim().is_empty() {
            continue;
        }

        // Parse the line format: HIERARCHY EXTENSIONS SINCE
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            let hierarchy = parts[0];
            let extensions = parts[1];
            let since = parts[2..].join(" ");

            println!("  {hierarchy} -> {extensions} (since {since})");
        } else {
            // Fallback: just print the line as-is
            println!("  {line}");
        }
    }
}

/// Prepare the extension environment by setting up symlinks with output manager
fn prepare_extension_environment_with_output(
    output: &OutputManager,
) -> Result<Vec<Extension>, SystemdError> {
    output.step("Environment", "Preparing extension environment");

    // Verify clean state by ensuring no stale symlinks exist
    verify_clean_extension_environment(output)?;

    // Scan for available extensions from multiple sources
    let extensions = scan_extensions_from_all_sources_with_verbosity(output.is_verbose())?;

    if extensions.is_empty() {
        output.progress("No extensions found in any source location");
        return Ok(Vec::new());
    }

    // Create target directories
    create_target_directories()?;

    // Track which extensions are actually enabled and linked
    let mut enabled_extensions = Vec::new();

    // Create symlinks for sysext and confext extensions, using prefixed names for ordering
    for extension in &extensions {
        let mut extension_enabled = false;
        let prefixed_name = compute_prefixed_name(extension);

        // Stage extension-release files with prefixed name if ordering is active
        if extension.merge_index.is_some() {
            let original_name = if let Some(ver) = &extension.version {
                format!("{}-{}", extension.name, ver)
            } else {
                extension.name.clone()
            };
            // Only stage if the prefixed name differs from the original
            if prefixed_name != original_name {
                stage_extension_release(extension, &prefixed_name, output.is_verbose())?;
            }
        }

        if extension.is_sysext {
            create_sysext_symlink_with_verbosity(extension, &prefixed_name, output.is_verbose())?;
            extension_enabled = true;
        }
        if extension.is_confext {
            create_confext_symlink_with_verbosity(extension, &prefixed_name, output.is_verbose())?;
            extension_enabled = true;
        }

        // Only add to enabled list if at least one type was linked
        if extension_enabled {
            enabled_extensions.push(extension.clone());
        }
    }

    // Important: After creating symlinks for enabled extensions, ensure no stale symlinks remain
    // This handles the case where an extension was previously enabled but is now disabled
    cleanup_stale_extension_symlinks(&enabled_extensions, output)?;

    output.progress("Extension environment prepared successfully");
    Ok(enabled_extensions)
}

/// Remove any symlinks in /run/extensions and /run/confexts that are NOT in the enabled list
/// This ensures disabled extensions are not merged
fn cleanup_stale_extension_symlinks(
    enabled_extensions: &[Extension],
    output: &OutputManager,
) -> Result<(), SystemdError> {
    let sysext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_extensions")
    } else {
        "/run/extensions".to_string()
    };

    let confext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_confexts")
    } else {
        "/run/confexts".to_string()
    };

    // Build a set of expected symlink names (using prefixed names when ordering is active)
    let mut expected_names = std::collections::HashSet::new();
    // Also track base names without versions for masking logic
    let mut non_versioned_base_names = std::collections::HashSet::new();

    for ext in enabled_extensions {
        // Use the same prefixed name that was used when creating the symlink
        let prefixed = compute_prefixed_name(ext);
        expected_names.insert(prefixed);

        // Track non-versioned extensions (e.g., HITL mounts) for masking
        if ext.version.is_none() && ext.merge_index.is_none() {
            non_versioned_base_names.insert(ext.name.clone());
        }
    }

    // Clean up sysext directory
    if Path::new(&sysext_dir).exists() {
        if let Ok(entries) = fs::read_dir(&sysext_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_symlink() {
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        // Remove .raw suffix if present for comparison
                        let name_without_raw = file_name.strip_suffix(".raw").unwrap_or(file_name);

                        // Check if this symlink should be removed
                        let should_remove = if !expected_names.contains(file_name)
                            && !expected_names.contains(name_without_raw)
                        {
                            // Not in expected list, should be removed
                            true
                        } else {
                            // Check if this is a versioned symlink that should be masked by a non-versioned one
                            // e.g., "myext-1.0.0" should be removed if "myext" (HITL mount) exists
                            if let Some(last_dash) = name_without_raw.rfind('-') {
                                let base_name = &name_without_raw[..last_dash];
                                let potential_version = &name_without_raw[last_dash + 1..];
                                // Check if this looks like a version (contains digits or dots)
                                if potential_version
                                    .chars()
                                    .any(|c| c.is_ascii_digit() || c == '.')
                                {
                                    // This is a versioned symlink, check if we have a non-versioned version
                                    non_versioned_base_names.contains(base_name)
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if should_remove {
                            if let Err(e) = fs::remove_file(&path) {
                                output.progress(&format!(
                        "Warning: Failed to remove stale sysext symlink {file_name}: {e}"
                    ));
                            } else {
                                output.progress(&format!(
                                    "Removed stale sysext symlink: {file_name}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Clean up confext directory
    if Path::new(&confext_dir).exists() {
        if let Ok(entries) = fs::read_dir(&confext_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_symlink() {
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        // Remove .raw suffix if present for comparison
                        let name_without_raw = file_name.strip_suffix(".raw").unwrap_or(file_name);

                        // Check if this symlink should be removed
                        let should_remove = if !expected_names.contains(file_name)
                            && !expected_names.contains(name_without_raw)
                        {
                            // Not in expected list, should be removed
                            true
                        } else {
                            // Check if this is a versioned symlink that should be masked by a non-versioned one
                            // e.g., "myext-1.0.0" should be removed if "myext" (HITL mount) exists
                            if let Some(last_dash) = name_without_raw.rfind('-') {
                                let base_name = &name_without_raw[..last_dash];
                                let potential_version = &name_without_raw[last_dash + 1..];
                                // Check if this looks like a version (contains digits or dots)
                                if potential_version
                                    .chars()
                                    .any(|c| c.is_ascii_digit() || c == '.')
                                {
                                    // This is a versioned symlink, check if we have a non-versioned version
                                    non_versioned_base_names.contains(base_name)
                                } else {
                                    false
                                }
                            } else {
                                false
                            }
                        };

                        if should_remove {
                            if let Err(e) = fs::remove_file(&path) {
                                output.progress(&format!(
                        "Warning: Failed to remove stale confext symlink {file_name}: {e}"
                    ));
                            } else {
                                output.progress(&format!(
                                    "Removed stale confext symlink: {file_name}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Read VERSION_ID from /etc/os-release
pub(crate) fn read_os_version_id() -> String {
    let os_release_path = "/etc/os-release";

    if let Ok(contents) = fs::read_to_string(os_release_path) {
        for line in contents.lines() {
            if line.starts_with("VERSION_ID=") {
                // Parse VERSION_ID value, removing quotes if present
                let value = line.trim_start_matches("VERSION_ID=");
                let value = value.trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
    }

    // Return default if VERSION_ID not found or file doesn't exist
    "unknown".to_string()
}

/// Scan all extension sources in priority order with verbosity control
fn scan_extensions_from_all_sources_with_verbosity(
    verbose: bool,
) -> Result<Vec<Extension>, SystemdError> {
    let mut extensions = Vec::new();
    let mut extension_map = std::collections::HashMap::new();

    // Define search paths in priority order: HITL → Runtime/<VERSION_ID> → Directory → Loop-mounted
    let hitl_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/hitl")
    } else {
        "/run/avocado/hitl".to_string()
    };

    // Read OS VERSION_ID for runtime-specific extensions
    let version_id = read_os_version_id();

    // Fallback to the images directory where extension images are installed
    let extensions_dir = std::env::var("AVOCADO_EXTENSIONS_PATH")
        .unwrap_or_else(|_| "/var/lib/avocado/images".to_string());

    // 1. First priority: HITL mounted extensions
    if verbose {
        println!("Scanning HITL extensions in {hitl_dir}");
    }
    if let Ok(hitl_extensions) = scan_directory_extensions(&hitl_dir) {
        for ext in hitl_extensions {
            if verbose {
                println!(
                    "Found HITL extension: {} at {}",
                    ext.name,
                    ext.path.display()
                );
            }
            extension_map.insert(ext.name.clone(), ext);
        }
    }

    // 2. Second priority: Active runtime manifest
    // If a manifest exists, use it to determine extensions and skip legacy os-releases scanning
    let base_dir = crate::manifest::RuntimeManifest::base_dir();
    let base_path = Path::new(&base_dir);
    let active_manifest = crate::manifest::RuntimeManifest::load_active(base_path);
    let used_manifest = if let Some(ref manifest) = active_manifest {
        if verbose {
            println!(
                "Found active runtime manifest: {} {} ({})",
                manifest.runtime.name,
                manifest.runtime.version,
                &manifest.id[..8.min(manifest.id.len())]
            );
        }

        // Per-runtime user overrides sit alongside the manifest. The
        // `active` symlink resolves to runtimes/<id>/, so overrides.json
        // (when present) lives at the same path.
        let active_dir = base_path.join(crate::manifest::ACTIVE_LINK_NAME);
        let overrides = crate::overrides::RuntimeOverrides::load(&active_dir);

        let ext_count = manifest.extensions.len();
        for (index, mext) in manifest.extensions.iter().enumerate() {
            // Skip extensions the user (or the build) has marked disabled.
            // `effective_enabled` is the single policy point — never read
            // `mext.enabled` directly outside of it.
            if !crate::overrides::effective_enabled(mext, &overrides) {
                if verbose {
                    println!(
                        "Skipping disabled extension '{}' (manifest={}, override={:?})",
                        mext.name,
                        mext.enabled,
                        overrides.enabled_override(&mext.name)
                    );
                }
                continue;
            }
            // Inverted index: manifest[0] = highest priority = highest prefix number
            let merge_idx = ext_count - 1 - index;

            // If HITL version exists, let it inherit the manifest's merge priority
            if let Some(existing) = extension_map.get_mut(&mext.name) {
                existing.merge_index = Some(merge_idx);
                if verbose {
                    println!(
                        "HITL extension {} inherits manifest priority #{:02}",
                        mext.name, merge_idx
                    );
                }
                continue;
            }

            // Resolve the on-disk path for this extension image
            let raw_path = mext.resolve_path(base_path);
            if raw_path.exists() {
                if raw_path.is_dir() {
                    if let Ok(dir_exts) =
                        scan_directory_extensions(raw_path.to_str().unwrap_or_default())
                    {
                        for mut ext in dir_exts {
                            if !extension_map.contains_key(&ext.name) {
                                ext.merge_index = Some(merge_idx);
                                if verbose {
                                    println!(
                                        "Found manifest extension: {} at {} (priority #{:02})",
                                        ext.name,
                                        ext.path.display(),
                                        merge_idx
                                    );
                                }
                                extension_map.insert(ext.name.clone(), ext);
                            }
                        }
                    }
                } else {
                    // Image file extension — adaptor selected by manifest image_type
                    let adaptor = ImageType::from_manifest(&mext.image_type);
                    match analyze_image_extension(
                        &mext.name,
                        &Some(mext.version.clone()),
                        &raw_path,
                        &adaptor,
                        verbose,
                    ) {
                        Ok(mut ext) => {
                            ext.merge_index = Some(merge_idx);
                            if verbose {
                                println!(
                                    "Found manifest extension: {} at {} (priority #{:02})",
                                    ext.name,
                                    ext.path.display(),
                                    merge_idx
                                );
                            }
                            extension_map.insert(ext.name.clone(), ext);
                        }
                        Err(e) => {
                            eprintln!(
                                "Warning: Failed to analyze manifest extension '{}': {e}",
                                mext.name
                            );
                        }
                    }
                }
            } else if verbose {
                let display_name = mext.image_id.as_deref().unwrap_or(&mext.name);
                eprintln!(
                    "Warning: Extension image '{}' from manifest not found at {}",
                    display_name,
                    raw_path.display()
                );
            }
        }

        true
    } else {
        if verbose {
            println!("No active runtime manifest found, using legacy extension discovery");
        }
        false
    };

    // Legacy extension discovery: only used when no manifest is present
    if !used_manifest {
        // 2b. Legacy: OS release-specific extensions (/var/lib/avocado/os-releases/<VERSION_ID>)
        let os_releases_extensions_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
            let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
            format!("{temp_base}/avocado/os-releases/{version_id}")
        } else {
            format!("/var/lib/avocado/os-releases/{version_id}")
        };

        if verbose {
            println!(
            "Scanning OS release extensions in {os_releases_extensions_dir} (VERSION_ID: {version_id})"
        );
        }

        if !Path::new(&os_releases_extensions_dir).exists() {
            if verbose {
                println!(
                    "OS releases directory {os_releases_extensions_dir} does not exist, skipping"
                );
            }
            if std::env::var("AVOCADO_TEST_MODE").is_err() {
                eprintln!("Warning: No extensions are enabled for VERSION_ID '{version_id}'. Directory not found: {os_releases_extensions_dir}");
            }
        } else {
            if let Ok(os_releases_extensions) =
                scan_directory_extensions(&os_releases_extensions_dir)
            {
                for ext in os_releases_extensions {
                    if !extension_map.contains_key(&ext.name) {
                        if verbose {
                            println!(
                                "Found OS release extension: {} at {}",
                                ext.name,
                                ext.path.display()
                            );
                        }
                        extension_map.insert(ext.name.clone(), ext);
                    } else if verbose {
                        println!(
                            "Skipping runtime extension {} (higher priority version preferred)",
                            ext.name
                        );
                    }
                }
            }

            if let Ok(os_releases_raw_files) = scan_raw_files(&os_releases_extensions_dir) {
                for (ext_name, ext_version, ext_path) in os_releases_raw_files {
                    use std::collections::hash_map::Entry;
                    match extension_map.entry(ext_name.clone()) {
                        Entry::Vacant(entry) => {
                            let adaptor = ImageType::Raw(RawAdaptor);
                            if let Ok(ext) = analyze_image_extension(
                                &ext_name,
                                &ext_version,
                                &ext_path,
                                &adaptor,
                                verbose,
                            ) {
                                if verbose {
                                    println!(
                                        "Found OS release raw extension: {} at {}",
                                        ext.name,
                                        ext.path.display()
                                    );
                                }
                                entry.insert(ext);
                            }
                        }
                        Entry::Occupied(_) => {
                            if verbose {
                                println!(
                        "Skipping OS release raw extension {ext_name} (higher priority version preferred)"
                    );
                            }
                        }
                    }
                }
            }
        }

        let os_releases_dir_exists = Path::new(&os_releases_extensions_dir).exists();

        if verbose {
            println!("Scanning directory extensions in {extensions_dir}");
        }

        if !os_releases_dir_exists {
            if verbose {
                println!("No OS releases directory found, scanning base extensions directory");
            }
            if let Ok(dir_extensions) = scan_directory_extensions(&extensions_dir) {
                for ext in dir_extensions {
                    if !extension_map.contains_key(&ext.name) {
                        if verbose {
                            println!(
                                "Found directory extension: {} at {}",
                                ext.name,
                                ext.path.display()
                            );
                        }
                        extension_map.insert(ext.name.clone(), ext);
                    } else if verbose {
                        println!(
                            "Skipping directory extension {} (HITL or runtime version preferred)",
                            ext.name
                        );
                    }
                }
            }
        } else if verbose {
            println!("OS releases directory exists, skipping base extensions directory (use enable/disable to manage extensions)");
        }

        if verbose {
            println!("Scanning raw file extensions in {extensions_dir}");
        }

        if !os_releases_dir_exists {
            if verbose {
                println!("No OS releases directory found, scanning base raw files");
            }
            let raw_files = scan_raw_files(&extensions_dir)?;

            let mut available_loop_names: Vec<String> = Vec::new();

            for ext in extension_map.values() {
                if let Some(ver) = &ext.version {
                    available_loop_names.push(format!("{}-{}", ext.name, ver));
                } else {
                    available_loop_names.push(ext.name.clone());
                }
            }

            for (name, version, _path) in &raw_files {
                if let Some(ver) = version {
                    available_loop_names.push(format!("{name}-{ver}"));
                } else {
                    available_loop_names.push(name.clone());
                }
            }

            // Cleaning up loops for extensions we are *not* about to mount must
            // never block mounting the ones we are: this scan backs `ext list`,
            // `ext status` and `prepare_extension_environment`, so propagating a
            // failure here (a single loop the kernel refuses to release, for an
            // extension that no longer exists) would abort every one of them.
            if let Err(e) = cleanup_stale_mounts(&available_loop_names) {
                eprintln!("Warning: stale mount cleanup incomplete: {e}");
            }

            for (ext_name, ext_version, path) in raw_files {
                match extension_map.entry(ext_name.clone()) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        if verbose {
                            println!("Found raw file extension: {ext_name} at {}", path.display());
                        }
                        let adaptor = ImageType::Raw(RawAdaptor);
                        let extension = analyze_image_extension(
                            &ext_name,
                            &ext_version,
                            &path,
                            &adaptor,
                            verbose,
                        )?;
                        entry.insert(extension);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {
                        if verbose {
                            println!(
                            "Skipping raw file extension {ext_name} (higher priority version preferred)"
                        );
                        }
                    }
                }
            }
        } else if verbose {
            println!("OS releases directory exists, skipping base raw files (use enable/disable to manage extensions)");
        }
    } // end !used_manifest

    // Convert map to vector
    extensions.extend(extension_map.into_values());
    Ok(extensions)
}

/// Scan a single directory for directory-based extensions
fn scan_directory_extensions(dir_path: &str) -> Result<Vec<Extension>, SystemdError> {
    let mut extensions = Vec::new();

    if !Path::new(dir_path).exists() {
        return Ok(extensions);
    }

    let entries = fs::read_dir(dir_path).map_err(|e| SystemdError::CommandFailed {
        command: "scan_directory_extensions".to_string(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| SystemdError::CommandFailed {
            command: "scan_directory_extensions".to_string(),
            source: e,
        })?;

        let path = entry.path();

        if path.is_dir() {
            if let Some(file_name) = path.file_name() {
                if let Some(name_str) = file_name.to_str() {
                    let extension = analyze_directory_extension(name_str, &path)?;
                    extensions.push(extension);
                }
            }
        }
    }

    Ok(extensions)
}

/// Scan a directory for raw file extensions
fn scan_raw_files(dir_path: &str) -> Result<Vec<(String, Option<String>, PathBuf)>, SystemdError> {
    let mut raw_files = Vec::new();

    if !Path::new(dir_path).exists() {
        return Ok(raw_files);
    }

    let entries = fs::read_dir(dir_path).map_err(|e| SystemdError::CommandFailed {
        command: "scan_raw_files".to_string(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| SystemdError::CommandFailed {
            command: "scan_raw_files".to_string(),
            source: e,
        })?;

        let path = entry.path();

        if path.is_file() {
            if let Some(file_name) = path.file_name() {
                if let Some(name_str) = file_name.to_str() {
                    if name_str.ends_with(".raw") {
                        // Strip .raw suffix to get the extension name (with version)
                        let ext_name_with_version =
                            name_str.strip_suffix(".raw").unwrap_or(name_str);

                        // Extract base extension name and version
                        // Extension name pattern: <name>-<version>.raw -> extract <name> and <version>
                        let (ext_name, ext_version) =
                            if let Some(last_dash) = ext_name_with_version.rfind('-') {
                                // Check if what follows the last dash looks like a version (contains digits or dots)
                                let potential_version = &ext_name_with_version[last_dash + 1..];
                                if potential_version
                                    .chars()
                                    .any(|c| c.is_ascii_digit() || c == '.')
                                {
                                    // This looks like a version, split name and version
                                    let name = &ext_name_with_version[..last_dash];
                                    let version = potential_version;
                                    (name.to_string(), Some(version.to_string()))
                                } else {
                                    // No version pattern found, use full name without version
                                    (ext_name_with_version.to_string(), None)
                                }
                            } else {
                                // No dash found, use full name without version
                                (ext_name_with_version.to_string(), None)
                            };

                        raw_files.push((ext_name, ext_version, path));
                    }
                }
            }
        }
    }

    Ok(raw_files)
}

/// Analyze an image file extension using the given adaptor for mount/unmount.
/// This unified function replaces the former `analyze_raw_extension_with_loop` and
/// `analyze_kab_extension` functions.
fn analyze_image_extension(
    name: &str,
    version: &Option<String>,
    path: &Path,
    adaptor: &ImageType,
    verbose: bool,
) -> Result<Extension, SystemdError> {
    if verbose {
        println!("Analyzing image extension: {name}");
    }

    let mount_name = if let Some(ver) = version {
        format!("{name}-{ver}")
    } else {
        name.to_string()
    };

    let mount_point = if adaptor.is_mounted(&mount_name) {
        if adaptor.needs_remount(&mount_name, path) {
            if verbose {
                println!("Backing file changed for {mount_name}, remounting...");
            }
            // The remount below can still succeed after a partial teardown, so
            // this is not fatal - but it is reported unconditionally. Hiding it
            // behind --verbose is how a loop the kernel would not release went
            // unnoticed until it had accumulated on the board.
            if let Err(e) = adaptor.unmount(&mount_name, verbose) {
                eprintln!("Warning: failed to unmount stale {mount_name}: {e}");
            }
            adaptor.mount(&mount_name, path, verbose)?
        } else {
            if verbose {
                println!("Using existing mount for {mount_name}");
            }
            PathBuf::from(extension_mount_point(&mount_name))
        }
    } else {
        adaptor.mount(&mount_name, path, verbose)?
    };

    let (sysext_enabled, confext_enabled, _detected_version) =
        analyze_mounted_extension(name, version, &mount_point);

    Ok(Extension {
        name: name.to_string(),
        version: version.clone(),
        path: mount_point,
        is_sysext: sysext_enabled,
        is_confext: confext_enabled,
        image_type: adaptor.type_tag(),
        merge_index: None,
    })
}

/// Analyze a directory extension to determine if it's sysext, confext, or both
fn analyze_directory_extension(name: &str, path: &Path) -> Result<Extension, SystemdError> {
    let (sysext_enabled, confext_enabled, detected_version) =
        analyze_mounted_extension(name, &None, path);

    Ok(Extension {
        name: name.to_string(),
        version: detected_version,
        path: path.to_path_buf(),
        is_sysext: sysext_enabled,
        is_confext: confext_enabled,
        image_type: ImageTypeTag::Directory,
        merge_index: None,
    })
}

/// Staging base directory for extension-release overrides used to control merge ordering.
const EXT_RELEASE_STAGING_DIR: &str = "/run/avocado/ext-release-staging";

/// Compute the prefixed symlink name for an extension based on its merge index.
/// When a merge_index is set, returns "NN-name" or "NN-name-version".
/// Without a merge_index (legacy), returns "name" or "name-version".
fn compute_prefixed_name(extension: &Extension) -> String {
    let base_name = if let Some(ver) = &extension.version {
        format!("{}-{}", extension.name, ver)
    } else {
        extension.name.clone()
    };

    if let Some(index) = extension.merge_index {
        format!("{index:02}-{base_name}")
    } else {
        base_name
    }
}

/// Stage extension-release files with a prefixed name so systemd recognizes the renamed extension.
///
/// For each extension that needs ordering, this:
/// 1. Creates a staging directory with copies of the original extension-release.d contents
/// 2. Adds a new extension-release file named to match the prefixed symlink name
/// 3. Bind mounts the staging directory over the original extension-release.d
///
/// This allows systemd-sysext/confext to find extension-release.{prefixed-name} even though
/// the extension image was built with extension-release.{original-name}.
fn stage_extension_release(
    extension: &Extension,
    prefixed_name: &str,
    verbose: bool,
) -> Result<(), SystemdError> {
    let staging_base = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/ext-release-staging")
    } else {
        EXT_RELEASE_STAGING_DIR.to_string()
    };

    // Determine the original extension-release name (without prefix)
    let original_name = if let Some(ver) = &extension.version {
        format!("{}-{}", extension.name, ver)
    } else {
        extension.name.clone()
    };

    // Handle sysext release directory
    if extension.is_sysext {
        let original_release_dir = extension.path.join("usr/lib/extension-release.d");
        if original_release_dir.exists() {
            let staging_dir = PathBuf::from(&staging_base)
                .join(prefixed_name)
                .join("sysext");
            fs::create_dir_all(&staging_dir).map_err(|e| SystemdError::CommandFailed {
                command: "create_dir_all (sysext staging)".to_string(),
                source: e,
            })?;

            // Copy all existing files from original release dir
            if let Ok(entries) = fs::read_dir(&original_release_dir) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        let dest = staging_dir.join(entry.file_name());
                        fs::copy(entry.path(), &dest).map_err(|e| SystemdError::CommandFailed {
                            command: format!("copy extension-release file {:?}", entry.file_name()),
                            source: e,
                        })?;
                    }
                }
            }

            // Create the prefixed release file by copying content from original
            let original_release =
                original_release_dir.join(format!("extension-release.{original_name}"));
            // Also try without version if versioned doesn't exist
            let original_release = if original_release.exists() {
                original_release
            } else {
                original_release_dir.join(format!("extension-release.{}", extension.name))
            };

            let prefixed_release = staging_dir.join(format!("extension-release.{prefixed_name}"));
            if original_release.exists() && !prefixed_release.exists() {
                fs::copy(&original_release, &prefixed_release).map_err(|e| {
                    SystemdError::CommandFailed {
                        command: "copy prefixed extension-release (sysext)".to_string(),
                        source: e,
                    }
                })?;
            }

            // Bind mount staging dir over original release dir
            run_bind_mount(
                staging_dir.to_str().unwrap_or_default(),
                original_release_dir.to_str().unwrap_or_default(),
                verbose,
            )?;
        }
    }

    // Handle confext release directory
    if extension.is_confext {
        let original_release_dir = extension.path.join("etc/extension-release.d");
        if original_release_dir.exists() {
            let staging_dir = PathBuf::from(&staging_base)
                .join(prefixed_name)
                .join("confext");
            fs::create_dir_all(&staging_dir).map_err(|e| SystemdError::CommandFailed {
                command: "create_dir_all (confext staging)".to_string(),
                source: e,
            })?;

            // Copy all existing files from original release dir
            if let Ok(entries) = fs::read_dir(&original_release_dir) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        let dest = staging_dir.join(entry.file_name());
                        fs::copy(entry.path(), &dest).map_err(|e| SystemdError::CommandFailed {
                            command: format!("copy extension-release file {:?}", entry.file_name()),
                            source: e,
                        })?;
                    }
                }
            }

            let original_release =
                original_release_dir.join(format!("extension-release.{original_name}"));
            let original_release = if original_release.exists() {
                original_release
            } else {
                original_release_dir.join(format!("extension-release.{}", extension.name))
            };

            let prefixed_release = staging_dir.join(format!("extension-release.{prefixed_name}"));
            if original_release.exists() && !prefixed_release.exists() {
                fs::copy(&original_release, &prefixed_release).map_err(|e| {
                    SystemdError::CommandFailed {
                        command: "copy prefixed extension-release (confext)".to_string(),
                        source: e,
                    }
                })?;
            }

            run_bind_mount(
                staging_dir.to_str().unwrap_or_default(),
                original_release_dir.to_str().unwrap_or_default(),
                verbose,
            )?;
        }
    }

    Ok(())
}

/// Execute a bind mount, or simulate in test mode.
fn run_bind_mount(source: &str, target: &str, verbose: bool) -> Result<(), SystemdError> {
    if verbose {
        println!("Bind mounting {source} -> {target}");
    }

    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        // In test mode, skip actual mount syscall
        return Ok(());
    }

    let output = ProcessCommand::new("mount")
        .args(["--bind", source, target])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: "mount --bind".to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: format!("mount --bind {source} {target}"),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    Ok(())
}

/// Create target directories for symlinks
fn create_target_directories() -> Result<(), SystemdError> {
    let (sysext_dir, confext_dir) = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        // In test mode, use temporary directories
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        (
            format!("{temp_base}/test_extensions"),
            format!("{temp_base}/test_confexts"),
        )
    } else {
        ("/run/extensions".to_string(), "/run/confexts".to_string())
    };

    // Create /run/extensions (or test equivalent) if it doesn't exist
    if !Path::new(&sysext_dir).exists() {
        fs::create_dir_all(&sysext_dir).map_err(|e| SystemdError::CommandFailed {
            command: "create_dir_all".to_string(),
            source: e,
        })?;
    }

    // Create /run/confexts (or test equivalent) if it doesn't exist
    if !Path::new(&confext_dir).exists() {
        fs::create_dir_all(&confext_dir).map_err(|e| SystemdError::CommandFailed {
            command: "create_dir_all".to_string(),
            source: e,
        })?;
    }

    Ok(())
}

/// Create a symlink for a sysext extension with verbosity control.
/// The `symlink_name` parameter is the (possibly prefixed) name to use for the symlink.
fn create_sysext_symlink_with_verbosity(
    extension: &Extension,
    symlink_name: &str,
    verbose: bool,
) -> Result<(), SystemdError> {
    let sysext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_extensions")
    } else {
        "/run/extensions".to_string()
    };

    let target_path = format!("{sysext_dir}/{symlink_name}");

    // Remove existing symlink or file if it exists
    if Path::new(&target_path).exists() {
        let path = Path::new(&target_path);

        // Try to remove as file first (works for symlinks and regular files)
        if fs::remove_file(&target_path).is_err() {
            // If that fails, it might be a directory
            if path.is_dir() {
                fs::remove_dir_all(&target_path).map_err(|e| SystemdError::CommandFailed {
                    command: "remove_dir_all".to_string(),
                    source: e,
                })?;
            }
        }
    }

    // Create symlink
    unix_fs::symlink(&extension.path, &target_path).map_err(|e| SystemdError::CommandFailed {
        command: "symlink".to_string(),
        source: e,
    })?;

    if verbose {
        println!(
            "Created sysext symlink: {} -> {}",
            target_path,
            extension.path.display()
        );
    }
    Ok(())
}

/// Create a symlink for a confext extension with verbosity control.
/// The `symlink_name` parameter is the (possibly prefixed) name to use for the symlink.
fn create_confext_symlink_with_verbosity(
    extension: &Extension,
    symlink_name: &str,
    verbose: bool,
) -> Result<(), SystemdError> {
    let confext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_confexts")
    } else {
        "/run/confexts".to_string()
    };

    let target_path = format!("{confext_dir}/{symlink_name}");

    // Remove existing symlink or file if it exists
    if Path::new(&target_path).exists() {
        let path = Path::new(&target_path);

        // Try to remove as file first (works for symlinks and regular files)
        if fs::remove_file(&target_path).is_err() {
            // If that fails, it might be a directory
            if path.is_dir() {
                fs::remove_dir_all(&target_path).map_err(|e| SystemdError::CommandFailed {
                    command: "remove_dir_all".to_string(),
                    source: e,
                })?;
            }
        }
    }

    // Create symlink
    unix_fs::symlink(&extension.path, &target_path).map_err(|e| SystemdError::CommandFailed {
        command: "symlink".to_string(),
        source: e,
    })?;

    if verbose {
        println!(
            "Created confext symlink: {} -> {}",
            target_path,
            extension.path.display()
        );
    }
    Ok(())
}

/// Cleanup stale loop refs and KAB loops for extensions that no longer exist.
fn cleanup_stale_mounts(available_extensions: &[String]) -> Result<(), SystemdError> {
    // Skip cleanup in test mode to avoid interfering with system loops
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        return Ok(());
    }

    // Both halves of the cleanup always run and their failures are collected,
    // so one loop the kernel refuses to release cannot abort the sweep and
    // leave the remaining stale loops - raw and KAB alike - attached.
    let mut attempted = 0;
    let mut failures = Vec::new();

    // Clean up stale raw loop refs
    let loop_ref_dir = "/dev/disk/by-loop-ref";
    if Path::new(loop_ref_dir).exists() {
        let entries = fs::read_dir(loop_ref_dir).map_err(|e| SystemdError::CommandFailed {
            command: "read_dir".to_string(),
            source: e,
        })?;

        let raw = RawAdaptor;
        for entry in entries.flatten() {
            if let Some(loop_name) = entry.file_name().to_str() {
                if !available_extensions.contains(&loop_name.to_string()) {
                    println!("Cleaning up stale raw loop for: {loop_name}");
                    attempted += 1;
                    if let Err(e) = raw.unmount(loop_name, false) {
                        eprintln!("Warning: failed to clean up stale raw loop {loop_name}: {e}");
                        failures.push(format!("{loop_name}: {e}"));
                    }
                }
            }
        }
    }

    // Clean up stale KAB offset loops
    let kab_loops_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/kab-loops")
    } else {
        "/run/avocado/kab-loops".to_string()
    };

    if Path::new(&kab_loops_dir).exists() {
        if let Ok(entries) = fs::read_dir(&kab_loops_dir) {
            let kab = KabAdaptor;
            for entry in entries.flatten() {
                if let Some(loop_name) = entry.file_name().to_str() {
                    if !available_extensions.contains(&loop_name.to_string()) {
                        println!("Cleaning up stale KAB loop for: {loop_name}");
                        attempted += 1;
                        if let Err(e) = kab.unmount(loop_name, false) {
                            eprintln!(
                                "Warning: failed to clean up stale KAB loop {loop_name}: {e}"
                            );
                            failures.push(format!("{loop_name}: {e}"));
                        }
                    }
                }
            }
        }
    }

    aggregate_failures(attempted, failures)
}

/// Clean up all extension symlinks to ensure fresh state for merge
/// Clean up extension-release bind mounts and staging directories.
/// Scans /proc/mounts for bind mounts within extension paths and unmounts them,
/// then removes the staging directory tree.
fn cleanup_extension_release_staging(output: &OutputManager) -> Result<(), SystemdError> {
    let staging_base = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/ext-release-staging")
    } else {
        EXT_RELEASE_STAGING_DIR.to_string()
    };

    if !Path::new(&staging_base).exists() {
        return Ok(());
    }

    if std::env::var("AVOCADO_TEST_MODE").is_err() {
        // Unmount bind mounts over extension-release.d directories.
        // These are bind mounts from the staging dir onto the extension's release dir.
        let ext_mount_base = "/run/avocado/extensions";
        if let Ok(mounts_content) = fs::read_to_string("/proc/mounts") {
            for line in mounts_content.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mount_point = parts[1];
                    if mount_point.starts_with(ext_mount_base)
                        && mount_point.contains("extension-release.d")
                    {
                        let result = ProcessCommand::new("umount")
                            .arg(mount_point)
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped())
                            .output();

                        match result {
                            Ok(o) if o.status.success() => {
                                if output.is_verbose() {
                                    output
                                        .progress(&format!("Unmounted bind mount: {mount_point}"));
                                }
                            }
                            _ => {
                                output.progress(&format!(
                                    "Warning: Failed to unmount bind mount: {mount_point}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }

    // Remove staging directories
    if let Err(e) = fs::remove_dir_all(&staging_base) {
        output.progress(&format!(
            "Warning: Failed to remove staging directory {staging_base}: {e}"
        ));
    } else if output.is_verbose() {
        output.progress("Cleaned up extension-release staging directories");
    }

    Ok(())
}

fn cleanup_extension_symlinks(output: &OutputManager) -> Result<(), SystemdError> {
    output.step("Cleanup", "Removing old extension symlinks");

    // Clean up sysext symlinks
    let sysext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_extensions")
    } else {
        "/run/extensions".to_string()
    };

    cleanup_symlinks_in_directory(&sysext_dir, output)?;

    // Clean up confext symlinks
    let confext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_confexts")
    } else {
        "/run/confexts".to_string()
    };

    cleanup_symlinks_in_directory(&confext_dir, output)?;

    output.progress("Extension symlinks cleaned up");
    Ok(())
}

/// Clean up all symlinks in a specific directory
fn cleanup_symlinks_in_directory(
    directory: &str,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    if !Path::new(directory).exists() {
        return Ok(());
    }

    let entries = fs::read_dir(directory).map_err(|e| SystemdError::CommandFailed {
        command: "read_dir".to_string(),
        source: e,
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            if let Err(e) = fs::remove_file(&path) {
                output.progress(&format!(
                    "Warning: Failed to remove symlink {}: {}",
                    path.display(),
                    e
                ));
            } else {
                output.progress(&format!("Removed symlink: {}", path.display()));
            }
        }
    }

    Ok(())
}

/// Verify that extension directories are clean before merge
fn verify_clean_extension_environment(output: &OutputManager) -> Result<(), SystemdError> {
    let sysext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_extensions")
    } else {
        "/run/extensions".to_string()
    };

    let confext_dir = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/test_confexts")
    } else {
        "/run/confexts".to_string()
    };

    // Check for stale symlinks in sysext directory
    if let Some(stale_symlinks) = check_for_stale_symlinks(&sysext_dir)? {
        output.progress(&format!(
            "Warning: Found {} stale symlinks in {}, cleaning up",
            stale_symlinks.len(),
            sysext_dir
        ));
        cleanup_symlinks_in_directory(&sysext_dir, output)?;
    }

    // Check for stale symlinks in confext directory
    if let Some(stale_symlinks) = check_for_stale_symlinks(&confext_dir)? {
        output.progress(&format!(
            "Warning: Found {} stale symlinks in {}, cleaning up",
            stale_symlinks.len(),
            confext_dir
        ));
        cleanup_symlinks_in_directory(&confext_dir, output)?;
    }

    Ok(())
}

/// Check for stale symlinks in a directory
fn check_for_stale_symlinks(directory: &str) -> Result<Option<Vec<String>>, SystemdError> {
    if !Path::new(directory).exists() {
        return Ok(None);
    }

    let entries = fs::read_dir(directory).map_err(|e| SystemdError::CommandFailed {
        command: "read_dir".to_string(),
        source: e,
    })?;

    let mut stale_symlinks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_symlink() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                stale_symlinks.push(name.to_string());
            }
        }
    }

    if stale_symlinks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(stale_symlinks))
    }
}

/// Scan release files for only the enabled extensions
fn scan_release_files_for_enabled_extensions(
    enabled_extensions: &[Extension],
) -> Result<(Vec<String>, Vec<String>), SystemdError> {
    let mut on_merge_commands = Vec::new();
    let mut modprobe_modules = Vec::new();

    // Handle test mode with custom release directory (for backwards compatibility)
    if let Ok(custom_dir) = std::env::var("AVOCADO_EXTENSION_RELEASE_DIR") {
        return scan_custom_release_directory(&custom_dir);
    }

    for extension in enabled_extensions {
        // Scan release files from each enabled extension mount point
        scan_extension_release_files(extension, &mut on_merge_commands, &mut modprobe_modules)?;
    }

    Ok((on_merge_commands, modprobe_modules))
}

/// Scan release files from a custom directory (test mode)
fn scan_custom_release_directory(
    custom_dir: &str,
) -> Result<(Vec<String>, Vec<String>), SystemdError> {
    let mut on_merge_commands = Vec::new();
    let mut modprobe_modules = Vec::new();

    let custom_path = Path::new(custom_dir);
    let mut dirs: Vec<(String, Option<&str>)> = Vec::new();

    // Check if it's a single directory with release files (legacy behavior)
    if custom_path.join("extension-release.d").exists() {
        dirs.push((custom_dir.to_string(), None));
    } else {
        // Look for sysext and confext subdirectories
        let sysext_dir = custom_path.join("usr/lib/extension-release.d");
        let confext_dir = custom_path.join("etc/extension-release.d");

        if sysext_dir.exists() {
            dirs.push((
                sysext_dir.to_string_lossy().to_string(),
                Some("SYSEXT_SCOPE"),
            ));
        }
        if confext_dir.exists() {
            dirs.push((
                confext_dir.to_string_lossy().to_string(),
                Some("CONFEXT_SCOPE"),
            ));
        }

        // If neither subdirectory structure exists, use the custom dir directly
        if dirs.is_empty() {
            dirs.push((custom_dir.to_string(), None));
        }
    }

    for (release_dir, scope_key) in &dirs {
        scan_directory_for_release_files(
            release_dir,
            &mut on_merge_commands,
            &mut modprobe_modules,
            *scope_key,
        );
    }

    Ok((on_merge_commands, modprobe_modules))
}

/// Scan release files from a specific extension's trusted mount point.
/// Only processes sysext release files if the extension is enabled as sysext for the
/// current scope, and confext release files if enabled as confext for the current scope.
/// Also verifies scope from the release file content as defense in depth.
fn scan_extension_release_files(
    extension: &Extension,
    on_merge_commands: &mut Vec<String>,
    modprobe_modules: &mut Vec<String>,
) -> Result<(), SystemdError> {
    if extension.is_sysext {
        // Check for sysext release file - try both versioned and non-versioned
        let sysext_release_path = extension
            .path
            .join("usr/lib/extension-release.d")
            .join(format!("extension-release.{}", extension.name));

        if sysext_release_path.exists() {
            if let Ok(content) = fs::read_to_string(&sysext_release_path) {
                if is_scope_enabled_for_current_environment(&content, "SYSEXT_SCOPE") {
                    let mut commands = parse_avocado_on_merge_commands(&content);
                    on_merge_commands.append(&mut commands);

                    let mut modules = parse_avocado_modprobe(&content);
                    modprobe_modules.append(&mut modules);
                }
            }
        } else {
            // Try to find versioned release file
            let sysext_dir = extension.path.join("usr/lib/extension-release.d");
            if sysext_dir.exists() {
                if let Ok(entries) = fs::read_dir(&sysext_dir) {
                    for entry in entries.flatten() {
                        let filename = entry.file_name();
                        let filename_str = filename.to_string_lossy();
                        if filename_str
                            .starts_with(&format!("extension-release.{}-", extension.name))
                        {
                            if let Ok(content) = fs::read_to_string(entry.path()) {
                                if is_scope_enabled_for_current_environment(
                                    &content,
                                    "SYSEXT_SCOPE",
                                ) {
                                    let mut commands = parse_avocado_on_merge_commands(&content);
                                    on_merge_commands.append(&mut commands);

                                    let mut modules = parse_avocado_modprobe(&content);
                                    modprobe_modules.append(&mut modules);
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    if extension.is_confext {
        // Check for confext release file - try both versioned and non-versioned
        let confext_release_path = extension
            .path
            .join("etc/extension-release.d")
            .join(format!("extension-release.{}", extension.name));

        if confext_release_path.exists() {
            if let Ok(content) = fs::read_to_string(&confext_release_path) {
                if is_scope_enabled_for_current_environment(&content, "CONFEXT_SCOPE") {
                    let mut commands = parse_avocado_on_merge_commands(&content);
                    on_merge_commands.append(&mut commands);

                    let mut modules = parse_avocado_modprobe(&content);
                    modprobe_modules.append(&mut modules);
                }
            }
        } else {
            // Try to find versioned release file
            let confext_dir = extension.path.join("etc/extension-release.d");
            if confext_dir.exists() {
                if let Ok(entries) = fs::read_dir(&confext_dir) {
                    for entry in entries.flatten() {
                        let filename = entry.file_name();
                        let filename_str = filename.to_string_lossy();
                        if filename_str
                            .starts_with(&format!("extension-release.{}-", extension.name))
                        {
                            if let Ok(content) = fs::read_to_string(entry.path()) {
                                if is_scope_enabled_for_current_environment(
                                    &content,
                                    "CONFEXT_SCOPE",
                                ) {
                                    let mut commands = parse_avocado_on_merge_commands(&content);
                                    on_merge_commands.append(&mut commands);

                                    let mut modules = parse_avocado_modprobe(&content);
                                    modprobe_modules.append(&mut modules);
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Scan extension release files for AVOCADO_ENABLE_SERVICES
/// This is used by HITL to determine which services need mount dependencies
pub fn scan_extension_for_enable_services(
    extension_path: &Path,
    extension_name: &str,
) -> Vec<String> {
    let mut services = Vec::new();

    // Check for sysext release file - try both versioned and non-versioned
    let sysext_release_path = extension_path
        .join("usr/lib/extension-release.d")
        .join(format!("extension-release.{extension_name}"));

    if sysext_release_path.exists() {
        if let Ok(content) = fs::read_to_string(&sysext_release_path) {
            let mut svc = parse_avocado_enable_services(&content);
            for s in svc.drain(..) {
                if !services.contains(&s) {
                    services.push(s);
                }
            }
        }
    } else {
        // Try to find versioned release file
        let sysext_dir = extension_path.join("usr/lib/extension-release.d");
        if sysext_dir.exists() {
            if let Ok(entries) = fs::read_dir(&sysext_dir) {
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(&format!("extension-release.{extension_name}-")) {
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            let mut svc = parse_avocado_enable_services(&content);
                            for s in svc.drain(..) {
                                if !services.contains(&s) {
                                    services.push(s);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // Check for confext release file - try both versioned and non-versioned
    let confext_release_path = extension_path
        .join("etc/extension-release.d")
        .join(format!("extension-release.{extension_name}"));

    if confext_release_path.exists() {
        if let Ok(content) = fs::read_to_string(&confext_release_path) {
            let mut svc = parse_avocado_enable_services(&content);
            for s in svc.drain(..) {
                if !services.contains(&s) {
                    services.push(s);
                }
            }
        }
    } else {
        // Try to find versioned release file
        let confext_dir = extension_path.join("etc/extension-release.d");
        if confext_dir.exists() {
            if let Ok(entries) = fs::read_dir(&confext_dir) {
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(&format!("extension-release.{extension_name}-")) {
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            let mut svc = parse_avocado_enable_services(&content);
                            for s in svc.drain(..) {
                                if !services.contains(&s) {
                                    services.push(s);
                                }
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    services
}

/// Scan a directory for release files (used in test mode).
/// Only includes commands from release files whose scope matches the current environment.
fn scan_directory_for_release_files(
    release_dir: &str,
    on_merge_commands: &mut Vec<String>,
    modprobe_modules: &mut Vec<String>,
    scope_key: Option<&str>,
) {
    if !Path::new(release_dir).exists() {
        return;
    }

    if let Ok(entries) = fs::read_dir(release_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Some(key) = scope_key {
                        if !is_scope_enabled_for_current_environment(&content, key) {
                            continue;
                        }
                    }
                    let mut commands = parse_avocado_on_merge_commands(&content);
                    on_merge_commands.append(&mut commands);

                    let mut modules = parse_avocado_modprobe(&content);
                    modprobe_modules.append(&mut modules);
                }
            }
        }
    }
}

/// Process post-merge tasks for only the enabled extensions
/// Commands that must run before daemon-reload so that kernel modules
/// and shared libraries are available when systemd re-evaluates units.
const PRE_DAEMON_RELOAD_COMMANDS: &[&str] = &["depmod", "ldconfig"];

/// Check if a command should run before daemon-reload
fn is_pre_daemon_reload_command(command: &str) -> bool {
    let first_word = command.split_whitespace().next().unwrap_or("");
    PRE_DAEMON_RELOAD_COMMANDS.contains(&first_word)
}

fn process_post_merge_tasks_for_extensions(
    enabled_extensions: &[Extension],
    output: &OutputManager,
) -> Result<(), SystemdError> {
    let (on_merge_commands, modprobe_modules) =
        scan_release_files_for_enabled_extensions(enabled_extensions)?;

    // Remove duplicates while preserving order
    let mut unique_commands = Vec::new();
    for command in on_merge_commands {
        if !unique_commands.contains(&command) {
            unique_commands.push(command);
        }
    }

    // Split commands into pre-daemon-reload (depmod, ldconfig) and post-daemon-reload
    let (pre_reload, post_reload): (Vec<_>, Vec<_>) = unique_commands
        .into_iter()
        .partition(|cmd| is_pre_daemon_reload_command(cmd));

    // Phase 1: Run depmod/ldconfig so modules and libraries are available
    if !pre_reload.is_empty() {
        run_avocado_on_merge_commands(&pre_reload, output)?;
    }

    // Phase 2: Load kernel modules (requires depmod to have run first)
    if !modprobe_modules.is_empty() {
        run_modprobe(&modprobe_modules, output)?;
    }

    // Phase 3: Reload systemd's unit database now that modules and libraries
    // are available, so units like proc-fs-nfsd.mount can start successfully
    match std::process::Command::new("systemctl")
        .arg("daemon-reload")
        .output()
    {
        Ok(result) if result.status.success() => {
            output.log_info("Reloaded systemd daemon after extension merge");
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            output.log_info(&format!("Warning: daemon-reload failed: {stderr}"));
        }
        Err(e) => {
            output.log_info(&format!("Warning: Failed to run daemon-reload: {e}"));
        }
    }

    // Phase 4: Run remaining post-merge commands (service restarts, etc.)
    if !post_reload.is_empty() {
        run_avocado_on_merge_commands(&post_reload, output)?;
    }

    Ok(())
}

/// Parse all AVOCADO_ON_MERGE commands from release file content
fn parse_avocado_on_merge_commands(content: &str) -> Vec<String> {
    let mut commands = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("AVOCADO_ON_MERGE=") {
            let value = line
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .trim_matches('"')
                .trim();

            if !value.is_empty() {
                commands.push(value.to_string());
            }
        }
    }

    commands
}

/// Parse all AVOCADO_ON_UNMERGE commands from release file content
fn parse_avocado_on_unmerge_commands(content: &str) -> Vec<String> {
    let mut commands = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("AVOCADO_ON_UNMERGE=") {
            let value = line
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .trim_matches('"')
                .trim();

            if !value.is_empty() {
                commands.push(value.to_string());
            }
        }
    }

    commands
}

/// Check if a release file content contains AVOCADO_ON_MERGE=depmod
/// (Kept for backward compatibility with existing tests)
#[allow(dead_code)]
fn check_avocado_on_merge_depmod(content: &str) -> bool {
    let commands = parse_avocado_on_merge_commands(content);
    commands.contains(&"depmod".to_string())
}

/// Scan currently merged extensions for AVOCADO_ON_UNMERGE commands.
/// Only includes commands from extensions whose scope matches the current environment.
fn scan_merged_extensions_for_on_unmerge_commands() -> Result<Vec<String>, SystemdError> {
    let mut on_unmerge_commands = Vec::new();

    // Handle test mode with custom release directory (for backwards compatibility)
    if let Ok(custom_dir) = std::env::var("AVOCADO_EXTENSION_RELEASE_DIR") {
        return scan_custom_release_directory_for_on_unmerge(&custom_dir);
    }

    // When extensions are merged, their release files are overlayed to:
    // - /usr/lib/extension-release.d/ for sysext (scope key: SYSEXT_SCOPE)
    // - /etc/extension-release.d/ for confext (scope key: CONFEXT_SCOPE)
    let release_dirs: [(&str, &str); 2] = [
        ("/usr/lib/extension-release.d", "SYSEXT_SCOPE"),
        ("/etc/extension-release.d", "CONFEXT_SCOPE"),
    ];

    for (release_dir, scope_key) in &release_dirs {
        let path = Path::new(release_dir);
        if !path.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let file_path = entry.path();
                if file_path.is_file() {
                    if let Ok(content) = fs::read_to_string(&file_path) {
                        if !is_scope_enabled_for_current_environment(&content, scope_key) {
                            continue;
                        }
                        let mut commands = parse_avocado_on_unmerge_commands(&content);
                        on_unmerge_commands.append(&mut commands);
                    }
                }
            }
        }
    }

    Ok(on_unmerge_commands)
}

/// Scan a custom release directory for AVOCADO_ON_UNMERGE commands (test mode)
fn scan_custom_release_directory_for_on_unmerge(
    custom_dir: &str,
) -> Result<Vec<String>, SystemdError> {
    let mut on_unmerge_commands = Vec::new();

    let custom_path = Path::new(custom_dir);
    let mut dirs: Vec<(String, Option<&str>)> = Vec::new();

    // Check if it's a single directory with release files (legacy behavior)
    if custom_path.join("extension-release.d").exists() {
        dirs.push((custom_dir.to_string(), None));
    } else {
        // Look for sysext and confext subdirectories
        let sysext_dir = custom_path.join("usr/lib/extension-release.d");
        let confext_dir = custom_path.join("etc/extension-release.d");

        if sysext_dir.exists() {
            dirs.push((
                sysext_dir.to_string_lossy().to_string(),
                Some("SYSEXT_SCOPE"),
            ));
        }
        if confext_dir.exists() {
            dirs.push((
                confext_dir.to_string_lossy().to_string(),
                Some("CONFEXT_SCOPE"),
            ));
        }

        // If neither subdirectory structure exists, use the custom dir directly
        if dirs.is_empty() {
            dirs.push((custom_dir.to_string(), None));
        }
    }

    for (release_dir, scope_key) in &dirs {
        scan_directory_for_on_unmerge_commands(release_dir, &mut on_unmerge_commands, *scope_key);
    }

    Ok(on_unmerge_commands)
}

/// Scan a directory for AVOCADO_ON_UNMERGE commands in release files.
/// Only includes commands from release files whose scope matches the current environment.
fn scan_directory_for_on_unmerge_commands(
    release_dir: &str,
    on_unmerge_commands: &mut Vec<String>,
    scope_key: Option<&str>,
) {
    if !Path::new(release_dir).exists() {
        return;
    }

    if let Ok(entries) = fs::read_dir(release_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Some(key) = scope_key {
                        if !is_scope_enabled_for_current_environment(&content, key) {
                            continue;
                        }
                    }
                    let mut commands = parse_avocado_on_unmerge_commands(&content);
                    on_unmerge_commands.append(&mut commands);
                }
            }
        }
    }
}

/// Process pre-unmerge tasks: execute AVOCADO_ON_UNMERGE commands
fn process_pre_unmerge_tasks(output: &OutputManager) -> Result<(), SystemdError> {
    let on_unmerge_commands = scan_merged_extensions_for_on_unmerge_commands()?;

    // Remove duplicates while preserving order
    let mut unique_commands = Vec::new();
    for command in on_unmerge_commands {
        if !unique_commands.contains(&command) {
            unique_commands.push(command);
        }
    }

    // Execute accumulated AVOCADO_ON_UNMERGE commands
    if !unique_commands.is_empty() {
        run_avocado_on_unmerge_commands(&unique_commands, output)?;
    }

    Ok(())
}

/// Parse AVOCADO_MODPROBE modules from release file content
fn parse_avocado_modprobe(content: &str) -> Vec<String> {
    let mut modules = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("AVOCADO_MODPROBE=") {
            let value = line
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .trim_matches('"')
                .trim();

            // Parse space-separated list of modules
            for module in value.split_whitespace() {
                if !module.is_empty() {
                    modules.push(module.to_string());
                }
            }
            break; // Only process the first AVOCADO_MODPROBE line
        }
    }

    modules
}

/// Parse AVOCADO_ENABLE_SERVICES from release file content
/// Returns a list of systemd service unit names that should depend on the extension's mount
pub fn parse_avocado_enable_services(content: &str) -> Vec<String> {
    let mut services = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("AVOCADO_ENABLE_SERVICES=") {
            let value = line
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .trim_matches('"')
                .trim();

            // Parse space-separated list of services
            for service in value.split_whitespace() {
                if !service.is_empty() && !services.contains(&service.to_string()) {
                    services.push(service.to_string());
                }
            }
        }
    }

    services
}

/// Run the depmod command
fn run_depmod(out: &OutputManager) -> Result<(), SystemdError> {
    out.log_info("Running depmod to update kernel module dependencies...");

    // Check if we're in test mode and should use mock commands
    let command_name = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        "mock-depmod"
    } else {
        "depmod"
    };

    let output = ProcessCommand::new(command_name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: command_name.to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: command_name.to_string(),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    out.log_success("depmod completed successfully.");
    Ok(())
}

/// Run modprobe for a list of modules
fn run_modprobe(modules: &[String], out: &OutputManager) -> Result<(), SystemdError> {
    if modules.is_empty() {
        return Ok(());
    }

    out.log_info(&format!("Loading kernel modules: {}", modules.join(", ")));

    for module in modules {
        // Check if we're in test mode and should use mock commands
        let command_name = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
            "mock-modprobe"
        } else {
            "modprobe"
        };

        let output = ProcessCommand::new(command_name)
            .arg(module)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SystemdError::CommandFailed {
                command: format!("{command_name} {module}"),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!("Warning: Failed to load module {module}: {stderr}");
            // Don't fail the entire operation for individual module failures
            // Just log the warning and continue with other modules
        } else {
            out.log_success(&format!("Module {module} loaded successfully."));
        }
    }

    out.log_success("Module loading completed.");
    Ok(())
}

/// Execute a single command with its arguments
fn execute_single_command(command_str: &str, out: &OutputManager) -> Result<(), SystemdError> {
    // Parse the command string to handle commands with arguments
    // Commands may be quoted or contain spaces
    let parts: Vec<&str> = if command_str.starts_with('"') && command_str.ends_with('"') {
        // Handle quoted commands
        let unquoted = &command_str[1..command_str.len() - 1];
        unquoted.split_whitespace().collect()
    } else {
        // Handle unquoted commands
        command_str.split_whitespace().collect()
    };

    if parts.is_empty() {
        eprintln!("Warning: Empty command in AVOCADO_ON_MERGE, skipping");
        return Ok(());
    }

    let (command_name, args) = parts.split_first().unwrap();

    // Check if we're in test mode and should use mock commands
    let mock_command_name = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        match *command_name {
            "depmod" => "mock-depmod".to_string(),
            "modprobe" => "mock-modprobe".to_string(),
            _ => {
                // For other commands in test mode, prefix with mock- if not already
                if command_name.starts_with("mock-") {
                    command_name.to_string()
                } else {
                    format!("mock-{command_name}")
                }
            }
        }
    } else {
        command_name.to_string()
    };

    let actual_command = &mock_command_name;

    let output = ProcessCommand::new(actual_command)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: command_str.to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("Warning: Command '{command_str}' failed: {stderr}");
        // Log warning but don't fail the entire operation
        // This matches the behavior of modprobe failures
    } else {
        out.log_success(&format!("Command '{command_str}' completed successfully"));
    }

    Ok(())
}

/// Run accumulated AVOCADO_ON_MERGE commands
fn run_avocado_on_merge_commands(
    commands: &[String],
    out: &OutputManager,
) -> Result<(), SystemdError> {
    if commands.is_empty() {
        return Ok(());
    }

    out.log_info(&format!("Executing {} post-merge commands", commands.len()));

    for command_str in commands {
        out.log_info(&format!("Running command: {command_str}"));

        // Check if the command contains shell operators like semicolons
        if command_str.contains(';') {
            // Split the command by semicolons and execute each part sequentially
            let sub_commands: Vec<&str> = command_str.split(';').map(|s| s.trim()).collect();

            for sub_command in sub_commands {
                if !sub_command.is_empty() {
                    out.log_info(&format!("Running sub-command: {sub_command}"));
                    execute_single_command(sub_command, out)?;
                }
            }
        } else {
            // Execute as a single command
            execute_single_command(command_str, out)?;
        }
    }

    out.log_success("Post-merge command execution completed.");
    Ok(())
}

/// Run accumulated AVOCADO_ON_UNMERGE commands
fn run_avocado_on_unmerge_commands(
    commands: &[String],
    out: &OutputManager,
) -> Result<(), SystemdError> {
    if commands.is_empty() {
        return Ok(());
    }

    out.log_info(&format!(
        "Executing {} pre-unmerge commands",
        commands.len()
    ));

    for command_str in commands {
        out.log_info(&format!("Running command: {command_str}"));

        // Check if the command contains shell operators like semicolons
        if command_str.contains(';') {
            // Split the command by semicolons and execute each part sequentially
            let sub_commands: Vec<&str> = command_str.split(';').map(|s| s.trim()).collect();

            for sub_command in sub_commands {
                if !sub_command.is_empty() {
                    out.log_info(&format!("Running sub-command: {sub_command}"));
                    execute_single_command(sub_command, out)?;
                }
            }
        } else {
            // Execute as a single command
            execute_single_command(command_str, out)?;
        }
    }

    out.log_success("Pre-unmerge command execution completed.");
    Ok(())
}

/// Run a systemd command with proper error handling
fn run_systemd_command(command: &str, args: &[&str]) -> Result<String, SystemdError> {
    // Check if we're in test mode and should use mock commands
    let command_name = if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        // In test mode, use mock commands from PATH
        format!("mock-{command}")
    } else {
        command.to_string()
    };

    let output = ProcessCommand::new(&command_name)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: command.to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: command.to_string(),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.to_string())
}

/// Handle and parse systemd command output with proper formatting
fn handle_systemd_output(
    operation: &str,
    output_str: &str,
    output: &OutputManager,
) -> Result<(), SystemdError> {
    if output_str.trim().is_empty() {
        output.progress(&format!(
            "{operation}: No output (operation may have completed with no changes)"
        ));
        return Ok(());
    }

    // Try to parse as JSON for better formatting
    match serde_json::from_str::<Value>(output_str) {
        Ok(json) => {
            output.raw(&format!("{operation}: {json}"));
            Ok(())
        }
        Err(_) => {
            // If not JSON, just print the raw output
            output.raw(&format!("{operation}: {output_str}"));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::image_adaptor::{
        is_confext_enabled_for_current_environment, is_sysext_enabled_for_current_environment,
        parse_scope_from_release_content,
    };
    use crate::config::Config;
    use std::env;
    use std::sync::Mutex;

    // Mutex to serialize tests that modify AVOCADO_EXTENSIONS_PATH environment variable
    static ENV_VAR_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_config_integration() {
        // Test that config is used for extensions directory
        // Lock the mutex to prevent env var interference from other tests
        let _guard = ENV_VAR_MUTEX.lock().unwrap();

        // Ensure no environment variable is set
        let original_value = env::var("AVOCADO_EXTENSIONS_PATH").ok();
        env::remove_var("AVOCADO_EXTENSIONS_PATH");

        let mut config = Config::default();
        config.avocado.ext.dir = "/test/config/path".to_string();

        let extensions_path = config.get_extensions_dir();
        assert_eq!(extensions_path, "/test/config/path");

        // Restore original
        if let Some(val) = original_value {
            env::set_var("AVOCADO_EXTENSIONS_PATH", val);
        }
    }

    #[test]
    fn test_environment_variable_precedence() {
        // Lock the mutex to prevent env var interference from other tests
        let _guard = ENV_VAR_MUTEX.lock().unwrap();

        // Save original environment variable value for restoration
        let original_value = env::var("AVOCADO_EXTENSIONS_PATH").ok();

        // Test that environment variable overrides config
        let mut config = Config::default();
        config.avocado.ext.dir = "/config/path".to_string();

        env::set_var("AVOCADO_EXTENSIONS_PATH", "/env/override/path");
        let extensions_path = config.get_extensions_dir();
        assert_eq!(extensions_path, "/env/override/path");

        // Clean up
        env::remove_var("AVOCADO_EXTENSIONS_PATH");

        // Now should use config value
        let extensions_path = config.get_extensions_dir();
        assert_eq!(extensions_path, "/config/path");

        // Restore original environment variable
        match original_value {
            Some(val) => env::set_var("AVOCADO_EXTENSIONS_PATH", val),
            None => env::remove_var("AVOCADO_EXTENSIONS_PATH"),
        }
    }

    #[test]
    fn test_default_path_when_no_config_or_env() {
        // Ensure no environment variable is set
        env::remove_var("AVOCADO_EXTENSIONS_PATH");

        let config = Config::default();
        let extensions_path = config.get_extensions_dir();
        assert_eq!(extensions_path, "/var/lib/avocado/images");
    }

    #[test]
    fn test_extension_name_extraction() {
        // Test file name extraction logic
        use std::path::Path;

        // Test directory name
        let dir_path = Path::new("/test/path/my_extension");
        if let Some(name) = dir_path.file_name() {
            if let Some(name_str) = name.to_str() {
                assert_eq!(name_str, "my_extension");
            }
        }

        // Test .raw file name
        let raw_path = Path::new("/test/path/my_extension.raw");
        if let Some(name) = raw_path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.ends_with(".raw") {
                    let ext_name = name_str.strip_suffix(".raw").unwrap_or(name_str);
                    assert_eq!(ext_name, "my_extension");
                }
            }
        }
    }

    #[test]
    fn test_create_command() {
        let cmd = create_command();
        assert_eq!(cmd.get_name(), "ext");

        // Check that all subcommands exist
        let subcommands: Vec<_> = cmd.get_subcommands().collect();
        assert_eq!(subcommands.len(), 7);

        let subcommand_names: Vec<&str> = subcommands.iter().map(|cmd| cmd.get_name()).collect();
        assert!(subcommand_names.contains(&"list"));
        assert!(subcommand_names.contains(&"merge"));
        assert!(subcommand_names.contains(&"unmerge"));
        assert!(subcommand_names.contains(&"refresh"));
        assert!(subcommand_names.contains(&"status"));
        assert!(subcommand_names.contains(&"enable"));
        assert!(subcommand_names.contains(&"disable"));
    }

    #[test]
    fn test_extension_preference() {
        // Directory should be preferred over .raw file
        use std::collections::HashMap;

        let mut extension_map = HashMap::new();

        // Simulate adding a .raw file first
        let raw_extension = Extension {
            name: "test_ext".to_string(),
            version: Some("1.0.0".to_string()),
            path: PathBuf::from("/test/test_ext.raw"),
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Raw,
            merge_index: None,
        };
        extension_map.insert("test_ext".to_string(), raw_extension);

        // Now add a directory with the same name (should replace the .raw)
        let dir_extension = Extension {
            name: "test_ext".to_string(),
            version: None,
            path: PathBuf::from("/test/test_ext"),
            is_sysext: true,
            is_confext: true,
            image_type: ImageTypeTag::Directory,
            merge_index: None,
        };
        extension_map.insert("test_ext".to_string(), dir_extension);

        let extension = extension_map.get("test_ext").unwrap();
        assert_eq!(extension.image_type, ImageTypeTag::Directory);
        assert!(extension.is_confext);
    }

    #[test]
    fn test_analyze_directory_extension() {
        // Test with no release files
        let test_path = PathBuf::from("/tmp/test_extension");
        let extension = analyze_directory_extension("test_ext", &test_path).unwrap();

        assert_eq!(extension.name, "test_ext");
        assert!(extension.is_sysext);
        assert!(extension.is_confext);
        assert_eq!(extension.image_type, ImageTypeTag::Directory);
    }

    #[test]
    fn test_symlink_naming() {
        // Test directory extension symlink naming
        let dir_extension = Extension {
            name: "test_ext".to_string(),
            version: None,
            path: PathBuf::from("/test/test_ext"),
            is_sysext: true,
            is_confext: true,
            image_type: ImageTypeTag::Directory,
            merge_index: None,
        };

        // Test loop-mounted raw file extension symlink naming
        let raw_extension = Extension {
            name: "test_ext".to_string(),
            version: Some("1.0.0".to_string()),
            path: PathBuf::from("/run/avocado/extensions/test_ext-1.0.0"), // Points to mounted directory
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Raw,
            merge_index: None,
        };

        // Directory extensions should use just the name (no version)
        let dir_symlink_name = if let Some(ver) = &dir_extension.version {
            format!("{}-{}", dir_extension.name, ver)
        } else {
            dir_extension.name.clone()
        };
        assert_eq!(dir_symlink_name, "test_ext");

        // Raw extensions with version should include version in symlink name
        let raw_symlink_name = if let Some(ver) = &raw_extension.version {
            format!("{}-{}", raw_extension.name, ver)
        } else {
            raw_extension.name.clone()
        };
        assert_eq!(raw_symlink_name, "test_ext-1.0.0");
    }

    #[test]
    fn test_check_avocado_on_merge_depmod() {
        // Test case with AVOCADO_ON_MERGE=depmod
        let content_with_depmod = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
OTHER_KEY=value
"#;
        assert!(check_avocado_on_merge_depmod(content_with_depmod));

        // Test case with AVOCADO_ON_MERGE=depmod with quotes
        let content_with_quoted_depmod = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE="depmod"
OTHER_KEY=value
"#;
        assert!(check_avocado_on_merge_depmod(content_with_quoted_depmod));

        // Test case with different AVOCADO_ON_MERGE value
        let content_with_other_value = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=something_else
OTHER_KEY=value
"#;
        assert!(!check_avocado_on_merge_depmod(content_with_other_value));

        // Test case without AVOCADO_ON_MERGE
        let content_without_key = r#"
VERSION_ID=1.0
OTHER_KEY=value
"#;
        assert!(!check_avocado_on_merge_depmod(content_without_key));

        // Test case with empty content
        assert!(!check_avocado_on_merge_depmod(""));

        // Test case with AVOCADO_ON_MERGE but empty value
        let content_with_empty_value = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=
OTHER_KEY=value
"#;
        assert!(!check_avocado_on_merge_depmod(content_with_empty_value));
    }

    #[test]
    fn test_parse_avocado_modprobe() {
        // Test case with multiple modules
        let content_with_modules = r#"
VERSION_ID=2.0
AVOCADO_MODPROBE="nvidia i915 radeon"
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_with_modules);
        assert_eq!(modules, vec!["nvidia", "i915", "radeon"]);

        // Test case with single module without quotes
        let content_single_module = r#"
VERSION_ID=1.5
AVOCADO_MODPROBE=snd_hda_intel
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_single_module);
        assert_eq!(modules, vec!["snd_hda_intel"]);

        // Test case with no AVOCADO_MODPROBE
        let content_no_modprobe = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_no_modprobe);
        assert!(modules.is_empty());

        // Test case with empty AVOCADO_MODPROBE
        let content_empty_modprobe = r#"
VERSION_ID=1.0
AVOCADO_MODPROBE=""
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_empty_modprobe);
        assert!(modules.is_empty());

        // Test case with extra whitespace
        let content_with_whitespace = r#"
VERSION_ID=1.0
AVOCADO_MODPROBE="  nvidia   i915  radeon  "
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_with_whitespace);
        assert_eq!(modules, vec!["nvidia", "i915", "radeon"]);

        // Test case with mixed quotes and no quotes in different lines (only first should be processed)
        let content_multiple_lines = r#"
VERSION_ID=1.0
AVOCADO_MODPROBE="nvidia i915"
AVOCADO_MODPROBE=should_be_ignored
OTHER_KEY=value
"#;
        let modules = parse_avocado_modprobe(content_multiple_lines);
        assert_eq!(modules, vec!["nvidia", "i915"]);
    }

    #[test]
    fn test_parse_avocado_on_merge_commands_with_equals() {
        // Test case with command containing equals signs in arguments
        let content_with_equals = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE="udevadm trigger --action=add"
AVOCADO_ON_MERGE=command --option=value --other=setting
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_merge_commands(content_with_equals);
        assert_eq!(
            commands,
            vec![
                "udevadm trigger --action=add",
                "command --option=value --other=setting"
            ]
        );

        // Test case with multiple equals signs in same argument
        let content_multiple_equals = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE="systemctl set-property --runtime some.service CPUQuota=50% MemoryLimit=1G"
"#;
        let commands = parse_avocado_on_merge_commands(content_multiple_equals);
        assert_eq!(
            commands,
            vec!["systemctl set-property --runtime some.service CPUQuota=50% MemoryLimit=1G"]
        );

        // Test case ensuring backwards compatibility with simple commands
        let content_simple = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
AVOCADO_ON_MERGE="systemctl restart some-service"
"#;
        let commands = parse_avocado_on_merge_commands(content_simple);
        assert_eq!(commands, vec!["depmod", "systemctl restart some-service"]);
    }

    #[test]
    fn test_parse_avocado_on_merge_commands_with_semicolons() {
        // Test case with semicolon-separated commands
        let content_with_semicolons = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE="systemctl --no-block restart dbus; systemctl --no-block restart avahi-daemon"
AVOCADO_ON_MERGE="command1 --arg=value; command2; command3 --option"
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_merge_commands(content_with_semicolons);
        assert_eq!(
            commands,
            vec![
                "systemctl --no-block restart dbus; systemctl --no-block restart avahi-daemon",
                "command1 --arg=value; command2; command3 --option"
            ]
        );

        // Test case with mixed semicolons and regular commands
        let content_mixed = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
AVOCADO_ON_MERGE="systemctl restart service1; systemctl restart service2"
AVOCADO_ON_MERGE="single-command --arg"
"#;
        let commands = parse_avocado_on_merge_commands(content_mixed);
        assert_eq!(
            commands,
            vec![
                "depmod",
                "systemctl restart service1; systemctl restart service2",
                "single-command --arg"
            ]
        );
    }

    #[test]
    fn test_parse_avocado_enable_services() {
        // Test case with multiple services
        let content_with_services = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="nginx.service prometheus.service"
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_with_services);
        assert_eq!(services, vec!["nginx.service", "prometheus.service"]);

        // Test case with services without .service suffix
        let content_short_names = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="nginx prometheus redis"
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_short_names);
        assert_eq!(services, vec!["nginx", "prometheus", "redis"]);

        // Test case with no AVOCADO_ENABLE_SERVICES
        let content_no_services = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_no_services);
        assert!(services.is_empty());

        // Test case with empty AVOCADO_ENABLE_SERVICES
        let content_empty_services = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES=""
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_empty_services);
        assert!(services.is_empty());

        // Test case with extra whitespace
        let content_with_whitespace = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="  nginx   redis  "
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_with_whitespace);
        assert_eq!(services, vec!["nginx", "redis"]);

        // Test case with multiple AVOCADO_ENABLE_SERVICES lines (all should be processed)
        let content_multiple_lines = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="nginx prometheus"
AVOCADO_ENABLE_SERVICES="redis"
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_multiple_lines);
        assert_eq!(services, vec!["nginx", "prometheus", "redis"]);

        // Test case with duplicates (should be deduplicated)
        let content_with_duplicates = r#"
VERSION_ID=1.0
AVOCADO_ENABLE_SERVICES="nginx redis"
AVOCADO_ENABLE_SERVICES="nginx worker"
OTHER_KEY=value
"#;
        let services = parse_avocado_enable_services(content_with_duplicates);
        assert_eq!(services, vec!["nginx", "redis", "worker"]);
    }

    #[test]
    fn test_parse_scope_from_release_content() {
        // Test case with SYSEXT_SCOPE
        let content_with_sysext_scope = r#"
VERSION_ID=1.0
SYSEXT_SCOPE="initrd system"
OTHER_KEY=value
"#;
        let scopes = parse_scope_from_release_content(content_with_sysext_scope, "SYSEXT_SCOPE");
        assert_eq!(scopes, vec!["initrd", "system"]);

        // Test case with CONFEXT_SCOPE
        let content_with_confext_scope = r#"
VERSION_ID=1.0
CONFEXT_SCOPE=system
OTHER_KEY=value
"#;
        let scopes = parse_scope_from_release_content(content_with_confext_scope, "CONFEXT_SCOPE");
        assert_eq!(scopes, vec!["system"]);

        // Test case with no scope
        let content_no_scope = r#"
VERSION_ID=1.0
OTHER_KEY=value
"#;
        let scopes = parse_scope_from_release_content(content_no_scope, "SYSEXT_SCOPE");
        assert!(scopes.is_empty());

        // Test case with empty scope
        let content_empty_scope = r#"
VERSION_ID=1.0
SYSEXT_SCOPE=""
OTHER_KEY=value
"#;
        let scopes = parse_scope_from_release_content(content_empty_scope, "SYSEXT_SCOPE");
        assert!(scopes.is_empty());

        // Test case with extra whitespace
        let content_with_whitespace = r#"
VERSION_ID=1.0
SYSEXT_SCOPE="  initrd   system  portable  "
OTHER_KEY=value
"#;
        let scopes = parse_scope_from_release_content(content_with_whitespace, "SYSEXT_SCOPE");
        assert_eq!(scopes, vec!["initrd", "system", "portable"]);
    }

    #[test]
    fn test_is_running_in_initrd() {
        // This test can't easily test the actual function since it depends on filesystem state
        // But we can test that the function exists and returns a boolean
        let result = is_running_in_initrd();
        let _ = result; // Just ensure it returns a boolean without crashing
    }

    #[test]
    fn test_sysext_scope_checking() {
        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory structure
        let temp_dir = TempDir::new().unwrap();
        let ext_path = temp_dir.path().join("test_ext");
        let release_dir = ext_path.join("usr/lib/extension-release.d");
        fs::create_dir_all(&release_dir).unwrap();

        // Test case 1: Extension with initrd scope only
        let release_file = release_dir.join("extension-release.test_ext");
        fs::write(&release_file, "VERSION_ID=1.0\nSYSEXT_SCOPE=\"initrd\"\n").unwrap();

        // This test will always return true since we can't mock is_running_in_initrd easily
        // But we can verify the function doesn't crash
        let _result = is_sysext_enabled_for_current_environment(&ext_path, "test_ext");

        // Test case 2: Extension with system scope only
        fs::write(&release_file, "VERSION_ID=1.0\nSYSEXT_SCOPE=\"system\"\n").unwrap();
        let _result = is_sysext_enabled_for_current_environment(&ext_path, "test_ext");

        // Test case 3: Extension with both scopes
        fs::write(
            &release_file,
            "VERSION_ID=1.0\nSYSEXT_SCOPE=\"initrd system\"\n",
        )
        .unwrap();
        let _result = is_sysext_enabled_for_current_environment(&ext_path, "test_ext");

        // Test case 4: Extension with no scope (should default to enabled)
        fs::write(&release_file, "VERSION_ID=1.0\n").unwrap();
        let result = is_sysext_enabled_for_current_environment(&ext_path, "test_ext");
        assert!(result);

        // Test case 5: No release file (should default to enabled)
        fs::remove_file(&release_file).unwrap();
        let result = is_sysext_enabled_for_current_environment(&ext_path, "test_ext");
        assert!(result);
    }

    #[test]
    fn test_confext_scope_checking() {
        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory structure
        let temp_dir = TempDir::new().unwrap();
        let ext_path = temp_dir.path().join("test_ext");
        let release_dir = ext_path.join("etc/extension-release.d");
        fs::create_dir_all(&release_dir).unwrap();

        // Test case 1: Extension with initrd scope only
        let release_file = release_dir.join("extension-release.test_ext");
        fs::write(&release_file, "VERSION_ID=1.0\nCONFEXT_SCOPE=\"initrd\"\n").unwrap();

        // This test will always return true since we can't mock is_running_in_initrd easily
        // But we can verify the function doesn't crash
        let _result = is_confext_enabled_for_current_environment(&ext_path, "test_ext");

        // Test case 2: Extension with no scope (should default to enabled)
        fs::write(&release_file, "VERSION_ID=1.0\n").unwrap();
        let result = is_confext_enabled_for_current_environment(&ext_path, "test_ext");
        assert!(result);

        // Test case 3: No release file (should default to enabled)
        fs::remove_file(&release_file).unwrap();
        let result = is_confext_enabled_for_current_environment(&ext_path, "test_ext");
        assert!(result);
    }

    #[test]
    fn test_config_mutable_integration() {
        // Test that the config mutable options are properly used
        let mut config = Config::default();

        // Test with default values (ephemeral)
        assert_eq!(config.get_sysext_mutable().unwrap(), "ephemeral");
        assert_eq!(config.get_confext_mutable().unwrap(), "ephemeral");

        // Test with separate custom values
        config.avocado.ext.sysext_mutable = Some("yes".to_string());
        config.avocado.ext.confext_mutable = Some("auto".to_string());
        assert_eq!(config.get_sysext_mutable().unwrap(), "yes");
        assert_eq!(config.get_confext_mutable().unwrap(), "auto");

        // Test error handling for invalid values
        config.avocado.ext.sysext_mutable = Some("invalid".to_string());
        let result = config.get_sysext_mutable();
        assert!(result.is_err());

        let error = result.unwrap_err();
        assert!(error
            .to_string()
            .contains("Invalid mutable value 'invalid'"));

        // Test backward compatibility with legacy mutable option
        let mut legacy_config = Config::default();
        legacy_config.avocado.ext.mutable = Some("import".to_string());
        assert_eq!(legacy_config.get_sysext_mutable().unwrap(), "import");
        assert_eq!(legacy_config.get_confext_mutable().unwrap(), "import");
    }

    #[test]
    fn test_parse_avocado_on_unmerge_commands() {
        // Test case with single AVOCADO_ON_UNMERGE command
        let content_single = r#"
VERSION_ID=1.0
AVOCADO_ON_UNMERGE="systemctl stop some-service"
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_unmerge_commands(content_single);
        assert_eq!(commands, vec!["systemctl stop some-service"]);

        // Test case with multiple AVOCADO_ON_UNMERGE commands
        let content_multiple = r#"
VERSION_ID=1.0
AVOCADO_ON_UNMERGE="systemctl stop service1"
AVOCADO_ON_UNMERGE="systemctl stop service2"
AVOCADO_ON_UNMERGE=cleanup-command
"#;
        let commands = parse_avocado_on_unmerge_commands(content_multiple);
        assert_eq!(
            commands,
            vec![
                "systemctl stop service1",
                "systemctl stop service2",
                "cleanup-command"
            ]
        );

        // Test case with no AVOCADO_ON_UNMERGE commands
        let content_none = r#"
VERSION_ID=1.0
AVOCADO_ON_MERGE=depmod
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_unmerge_commands(content_none);
        assert!(commands.is_empty());

        // Test case with empty AVOCADO_ON_UNMERGE
        let content_empty = r#"
VERSION_ID=1.0
AVOCADO_ON_UNMERGE=
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_unmerge_commands(content_empty);
        assert!(commands.is_empty());

        // Test case with empty content
        let commands = parse_avocado_on_unmerge_commands("");
        assert!(commands.is_empty());
    }

    #[test]
    fn test_parse_avocado_on_unmerge_commands_with_equals() {
        // Test case with command containing equals signs in arguments
        let content_with_equals = r#"
VERSION_ID=1.0
AVOCADO_ON_UNMERGE="systemctl set-property --runtime some.service CPUQuota=0%"
AVOCADO_ON_UNMERGE=cleanup --option=value
"#;
        let commands = parse_avocado_on_unmerge_commands(content_with_equals);
        assert_eq!(
            commands,
            vec![
                "systemctl set-property --runtime some.service CPUQuota=0%",
                "cleanup --option=value"
            ]
        );
    }

    #[test]
    fn test_parse_avocado_on_unmerge_commands_with_semicolons() {
        // Test case with semicolon-separated commands
        let content_with_semicolons = r#"
VERSION_ID=1.0
AVOCADO_ON_UNMERGE="systemctl stop service1; systemctl stop service2"
OTHER_KEY=value
"#;
        let commands = parse_avocado_on_unmerge_commands(content_with_semicolons);
        assert_eq!(
            commands,
            vec!["systemctl stop service1; systemctl stop service2"]
        );
    }

    #[test]
    fn test_both_merge_and_unmerge_commands() {
        // Test case with both AVOCADO_ON_MERGE and AVOCADO_ON_UNMERGE commands
        let content = r#"
VERSION_ID=1.0
DESCRIPTION="Extension with both merge and unmerge commands"
AVOCADO_ON_MERGE="systemctl start service"
AVOCADO_ON_MERGE=depmod
AVOCADO_ON_UNMERGE="systemctl stop service"
OTHER_KEY=value
"#;
        let merge_commands = parse_avocado_on_merge_commands(content);
        let unmerge_commands = parse_avocado_on_unmerge_commands(content);

        assert_eq!(merge_commands, vec!["systemctl start service", "depmod"]);
        assert_eq!(unmerge_commands, vec!["systemctl stop service"]);
    }

    #[test]
    fn test_compute_prefixed_name_with_merge_index() {
        let ext = Extension {
            name: "app".to_string(),
            version: Some("1.0.0".to_string()),
            path: PathBuf::from("/test/app"),
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Raw,
            merge_index: Some(2),
        };
        assert_eq!(compute_prefixed_name(&ext), "02-app-1.0.0");
    }

    #[test]
    fn test_compute_prefixed_name_no_version() {
        let ext = Extension {
            name: "networking".to_string(),
            version: None,
            path: PathBuf::from("/test/networking"),
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Directory,
            merge_index: Some(1),
        };
        assert_eq!(compute_prefixed_name(&ext), "01-networking");
    }

    #[test]
    fn test_compute_prefixed_name_no_merge_index() {
        // Legacy extension without ordering — no prefix
        let ext = Extension {
            name: "legacy".to_string(),
            version: Some("0.5.0".to_string()),
            path: PathBuf::from("/test/legacy"),
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Directory,
            merge_index: None,
        };
        assert_eq!(compute_prefixed_name(&ext), "legacy-0.5.0");
    }

    #[test]
    fn test_compute_prefixed_name_inverted_ordering() {
        // Simulate a manifest with 3 extensions: [highest, middle, lowest]
        // manifest[0] = highest priority → merge_index = 2
        // manifest[1] = middle → merge_index = 1
        // manifest[2] = lowest → merge_index = 0
        let n = 3;
        let names = ["highest", "middle", "lowest"];
        let expected = ["02-highest", "01-middle", "00-lowest"];

        for (index, name) in names.iter().enumerate() {
            let ext = Extension {
                name: name.to_string(),
                version: None,
                path: PathBuf::from(format!("/test/{name}")),
                is_sysext: true,
                is_confext: false,
                image_type: ImageTypeTag::Directory,
                merge_index: Some(n - 1 - index),
            };
            assert_eq!(
                compute_prefixed_name(&ext),
                expected[index],
                "manifest[{index}] should get prefix {:02}",
                n - 1 - index
            );
        }
    }

    #[test]
    fn test_hitl_inherits_manifest_priority() {
        // When a HITL extension overrides a manifest extension,
        // it should inherit the same merge_index
        let mut hitl_ext = Extension {
            name: "networking".to_string(),
            version: None,
            path: PathBuf::from("/run/avocado/hitl/networking"),
            is_sysext: true,
            is_confext: false,
            image_type: ImageTypeTag::Directory,
            merge_index: None, // Initially no index (HITL discovery)
        };

        // Simulate the manifest scanning assigning the index
        // For a 3-extension manifest where networking is at position 1:
        let ext_count = 3;
        let manifest_index = 1;
        let merge_idx = ext_count - 1 - manifest_index; // = 1
        hitl_ext.merge_index = Some(merge_idx);

        // The HITL extension now gets the same prefix as the manifest entry
        assert_eq!(compute_prefixed_name(&hitl_ext), "01-networking");
    }
}
