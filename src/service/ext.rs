use crate::commands::ext;
use crate::config::Config;
use crate::output::OutputManager;
use crate::service::error::AvocadoError;
use crate::service::types::{DisableResult, EnableResult, ExtensionInfo, SetEnabledResult};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

/// List all available extensions from the extensions directory.
/// List the extensions the device actually has, by name.
///
/// The extension image pool stores each `.raw` under its content-addressed
/// image id, so listing that directory named the extensions by UUID -- and
/// listed every image, including the rootfs/initramfs/kernel/os_bundle that are
/// not extensions at all. The active runtime manifest is the authority on which
/// images are extensions and what they are called (`runtime inspect` already
/// reads it), so resolve names and versions from there.
///
/// Directory-form extensions under the extensions dir -- HITL mounts and
/// loose dev extensions, which carry a real name -- are still listed as before,
/// and never duplicate a manifest entry.
pub fn list_extensions(config: &Config) -> Result<Vec<ExtensionInfo>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // From the active runtime manifest: the real extension names + versions,
    // each pointing at its image in the pool.
    if let Some(manifest) = crate::manifest::RuntimeManifest::load_active(base_path) {
        let images_dir = base_path.join(crate::manifest::IMAGES_DIR_NAME);
        for ext in &manifest.extensions {
            let path = match &ext.image_id {
                Some(id) => images_dir.join(format!("{id}.raw")).display().to_string(),
                None => String::new(),
            };
            seen.insert(ext.name.clone());
            result.push(ExtensionInfo {
                name: ext.name.clone(),
                version: Some(ext.version.clone()),
                path,
                is_sysext: true,
                is_confext: false,
                is_directory: false,
            });
        }
    }

    // Directory-form extensions (HITL mounts, dev directories) carry their own
    // name; keep listing them, but do not repeat one the manifest already named.
    let extensions_path = config.get_extensions_dir();
    match fs::read_dir(&extensions_path) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if seen.insert(name.to_string()) {
                        result.push(ExtensionInfo {
                            name: name.to_string(),
                            version: None,
                            path: path.display().to_string(),
                            is_sysext: true,
                            is_confext: false,
                            is_directory: true,
                        });
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(AvocadoError::ConfigurationError {
                message: format!("Cannot read extensions directory '{extensions_path}': {e}"),
            })
        }
    }

    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

// ── Streaming service functions ──────────────────────────────────────────────

/// Merge extensions with streaming output.
/// Returns a receiver that yields log messages as they are produced,
/// and a join handle for the worker thread.
pub fn merge_extensions_streaming(
    config: &Config,
) -> (
    mpsc::Receiver<String>,
    thread::JoinHandle<Result<(), AvocadoError>>,
) {
    let (tx, rx) = mpsc::sync_channel(4);
    let config = config.clone();
    let handle = thread::spawn(move || {
        let output = OutputManager::new_streaming(tx);
        ext::merge_extensions_internal(&config, &output).map_err(AvocadoError::from)
    });
    (rx, handle)
}

/// Unmerge extensions with streaming output.
pub fn unmerge_extensions_streaming(
    unmount: bool,
) -> (
    mpsc::Receiver<String>,
    thread::JoinHandle<Result<(), AvocadoError>>,
) {
    let (tx, rx) = mpsc::sync_channel(4);
    let handle = thread::spawn(move || {
        let output = OutputManager::new_streaming(tx);
        ext::unmerge_extensions_internal_with_options(true, unmount, &output)
            .map_err(AvocadoError::from)
    });
    (rx, handle)
}

/// Refresh extensions (unmerge then merge) with streaming output.
pub fn refresh_extensions_streaming(
    config: &Config,
) -> (
    mpsc::Receiver<String>,
    thread::JoinHandle<Result<(), AvocadoError>>,
) {
    let (tx, rx) = mpsc::sync_channel(4);
    let config = config.clone();
    let handle = thread::spawn(move || {
        let output = OutputManager::new_streaming(tx);

        // Same gate as the CLI refresh: refuse before the unmerge, so a manifest
        // this build cannot honor leaves the running extensions in place.
        if let Err(e) = ext::refresh_preflight(&config) {
            output.error(
                "Extension Refresh",
                &format!("Refusing to refresh (extensions left as they are): {e}"),
            );
            return Err(AvocadoError::from(e));
        }

        // First unmerge (skip depmod since we'll call it after merge, don't unmount loops —
        // the caller may be running from a loop-mounted extension like avocado-connect)
        ext::unmerge_extensions_internal_with_options(false, false, &output)
            .map_err(AvocadoError::from)?;

        // Invalidate NFS caches for any HITL-mounted extensions
        ext::invalidate_hitl_caches(&output);

        // Then merge (this will call depmod via post-merge processing)
        ext::merge_extensions_internal(&config, &output).map_err(AvocadoError::from)
    });
    (rx, handle)
}

// ── Batch service functions (used by non-streaming clients and tests) ────────

/// Merge extensions using systemd-sysext and systemd-confext.
/// Returns log messages produced during the operation.
pub fn merge_extensions(config: &Config) -> Result<Vec<String>, AvocadoError> {
    let (rx, handle) = merge_extensions_streaming(config);
    let messages: Vec<String> = rx.into_iter().collect();
    handle.join().unwrap_or_else(|_| {
        Err(AvocadoError::MergeFailed {
            reason: "internal panic".into(),
        })
    })?;
    Ok(messages)
}

/// Unmerge extensions using systemd-sysext and systemd-confext.
/// Returns log messages produced during the operation.
pub fn unmerge_extensions(unmount: bool) -> Result<Vec<String>, AvocadoError> {
    let (rx, handle) = unmerge_extensions_streaming(unmount);
    let messages: Vec<String> = rx.into_iter().collect();
    handle.join().unwrap_or_else(|_| {
        Err(AvocadoError::UnmergeFailed {
            reason: "internal panic".into(),
        })
    })?;
    Ok(messages)
}

/// Refresh extensions (unmerge then merge).
/// Returns log messages produced during the operation.
pub fn refresh_extensions(config: &Config) -> Result<Vec<String>, AvocadoError> {
    let (rx, handle) = refresh_extensions_streaming(config);
    let messages: Vec<String> = rx.into_iter().collect();
    handle.join().unwrap_or_else(|_| {
        Err(AvocadoError::MergeFailed {
            reason: "internal panic".into(),
        })
    })?;
    Ok(messages)
}

/// Enable extensions for a specific OS release version.
pub fn enable_extensions(
    os_release_version: Option<&str>,
    extensions: &[&str],
    config: &Config,
) -> Result<EnableResult, AvocadoError> {
    let version_id = match os_release_version {
        Some(v) => v.to_string(),
        None => ext::read_os_version_id(),
    };

    let extensions_dir = config.get_extensions_dir();

    // Determine os-releases directory
    let os_releases_dir = config.get_os_releases_dir(&version_id);

    // Create directory
    fs::create_dir_all(&os_releases_dir).map_err(|e| AvocadoError::ConfigurationError {
        message: format!("Failed to create os-releases directory '{os_releases_dir}': {e}"),
    })?;

    // Sync parent directory
    let _ = ext::sync_directory(
        Path::new(&os_releases_dir)
            .parent()
            .unwrap_or(Path::new("/")),
    );

    let mut enabled = 0;
    let mut failed = 0;

    for ext_name in extensions {
        let ext_dir_path = format!("{extensions_dir}/{ext_name}");
        let ext_raw_path = format!("{extensions_dir}/{ext_name}.raw");

        let source_path = if Path::new(&ext_dir_path).exists() {
            ext_dir_path
        } else if Path::new(&ext_raw_path).exists() {
            ext_raw_path
        } else {
            failed += 1;
            continue;
        };

        let target_path = format!(
            "{}/{}",
            os_releases_dir,
            Path::new(&source_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
        );

        // Remove existing symlink
        if Path::new(&target_path).exists() && fs::remove_file(&target_path).is_err() {
            failed += 1;
            continue;
        }

        // Create symlink
        if unix_fs::symlink(&source_path, &target_path).is_err() {
            failed += 1;
        } else {
            enabled += 1;
        }
    }

    // Sync to disk
    if enabled > 0 {
        ext::sync_directory(Path::new(&os_releases_dir)).map_err(AvocadoError::from)?;
    }

    if failed > 0 {
        return Err(AvocadoError::MergeFailed {
            reason: format!("{enabled} succeeded, {failed} failed"),
        });
    }

    Ok(EnableResult { enabled, failed })
}

/// Disable extensions for a specific OS release version.
pub fn disable_extensions(
    os_release_version: Option<&str>,
    extensions: Option<&[&str]>,
    all: bool,
    config: &Config,
) -> Result<DisableResult, AvocadoError> {
    let version_id = match os_release_version {
        Some(v) => v.to_string(),
        None => ext::read_os_version_id(),
    };

    let os_releases_dir = config.get_os_releases_dir(&version_id);

    if !Path::new(&os_releases_dir).exists() {
        return Err(AvocadoError::ConfigurationError {
            message: format!("OS releases directory '{os_releases_dir}' does not exist"),
        });
    }

    let mut disabled = 0;
    let mut failed = 0;

    if all {
        let entries =
            fs::read_dir(&os_releases_dir).map_err(|e| AvocadoError::ConfigurationError {
                message: format!("Failed to read os-releases directory: {e}"),
            })?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                match fs::remove_file(&path) {
                    Ok(_) => disabled += 1,
                    Err(_) => failed += 1,
                }
            }
        }
    } else if let Some(ext_names) = extensions {
        for ext_name in ext_names {
            let symlink_dir = format!("{os_releases_dir}/{ext_name}");
            let symlink_raw = format!("{os_releases_dir}/{ext_name}.raw");
            let mut found = false;

            if Path::new(&symlink_dir).exists() {
                match fs::remove_file(&symlink_dir) {
                    Ok(_) => {
                        disabled += 1;
                        found = true;
                    }
                    Err(_) => {
                        failed += 1;
                        found = true;
                    }
                }
            }

            if Path::new(&symlink_raw).exists() {
                match fs::remove_file(&symlink_raw) {
                    Ok(_) => {
                        if !found {
                            disabled += 1;
                        }
                        found = true;
                    }
                    Err(_) => {
                        failed += 1;
                        found = true;
                    }
                }
            }

            if !found {
                failed += 1;
            }
        }
    }

    // Sync to disk
    if disabled > 0 {
        let _ = ext::sync_directory(Path::new(&os_releases_dir));
    }

    if failed > 0 {
        return Err(AvocadoError::UnmergeFailed {
            reason: format!("{disabled} succeeded, {failed} failed"),
        });
    }

    Ok(DisableResult { disabled, failed })
}

/// Show extension status.
pub fn status_extensions(
    config: &Config,
) -> Result<Vec<crate::varlink::org_avocado_Extensions::ExtensionStatus>, AvocadoError> {
    ext::collect_extension_status(config).map_err(AvocadoError::from)
}

/// Override the build-time `enabled` default for one or more extensions.
/// Writes to `<active_runtime_dir>/overrides.json`. Names may be the bare
/// extension name (`microclaw`) or the versioned form shown by `ext list`
/// (`microclaw-0.1.57`) — the versioned form is normalized against the
/// active manifest before being recorded. An override that would match
/// the manifest's build-time default is cleared rather than written, so
/// `overrides.json` doesn't accumulate redundant entries.
pub fn set_extensions_enabled(
    names: &[&str],
    enabled: bool,
    config: &Config,
) -> Result<SetEnabledResult, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = std::path::Path::new(&base_dir);
    let manifest = crate::manifest::RuntimeManifest::load_active(base_path).ok_or_else(|| {
        AvocadoError::ConfigurationError {
            message: "No active runtime manifest. Provision a runtime first.".into(),
        }
    })?;

    let active_dir = base_path.join(crate::manifest::ACTIVE_LINK_NAME);
    let mut overrides = crate::overrides::RuntimeOverrides::load(&active_dir);

    let known: std::collections::HashSet<&str> = manifest
        .extensions
        .iter()
        .map(|e| e.name.as_str())
        .collect();

    let mut updated = 0usize;
    let mut missing = 0usize;

    for name in names {
        // Accept either the bare extension name or the versioned form
        // shown by `ext list` — normalize the latter against the manifest.
        let resolved = if known.contains(name) {
            name.to_string()
        } else {
            manifest
                .extensions
                .iter()
                .find(|e| format!("{}-{}", e.name, e.version) == *name)
                .map(|e| e.name.clone())
                .unwrap_or_else(|| name.to_string())
        };

        if !known.contains(resolved.as_str()) {
            missing += 1;
        }

        let manifest_default = manifest
            .extensions
            .iter()
            .find(|e| e.name == resolved)
            .map(|e| e.enabled)
            .unwrap_or(true);
        if manifest_default == enabled {
            overrides.set_enabled(&resolved, None);
        } else {
            overrides.set_enabled(&resolved, Some(enabled));
        }
        updated += 1;
    }

    overrides
        .save(&active_dir)
        .map_err(|e| AvocadoError::ConfigurationError {
            message: format!("Failed to write overrides: {e}"),
        })?;

    Ok(SetEnabledResult { updated, missing })
}
