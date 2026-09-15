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
/// Split a legacy raw-image stem `<name>-<version>` into its parts. The version
/// is only taken when the text after the last dash looks like one (digits or
/// dots), matching the legacy raw scanner.
/// Whether `s` is a content-addressed image id (a UUID: 8-4-4-4-12 hex). Those
/// live in the image pool and are described by the manifest; a loose dev
/// extension is named `<name>-<version>`, never a UUID. Used to keep the
/// loose-.raw scan from listing pool images when the extensions dir is the pool.
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn split_name_version(stem: &str) -> (&str, Option<String>) {
    if let Some(dash) = stem.rfind('-') {
        let v = &stem[dash + 1..];
        if v.chars().any(|c| c.is_ascii_digit() || c == '.') {
            return (&stem[..dash], Some(v.to_string()));
        }
    }
    (stem, None)
}

pub fn list_extensions(config: &Config) -> Result<Vec<ExtensionInfo>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // From the active runtime manifest: the real extension names + versions,
    // each pointing at its image in the pool. `list` is read-only and is the
    // command you reach for to diagnose a broken device -- e.g. a
    // version-above-ceiling manifest after an avocadoctl downgrade -- so a
    // manifest that cannot be read must NOT fail it: warn and fall back to
    // directory listing. (merge stays strict via load_active_checked, where a
    // wrong answer merges the wrong files.)
    let active = match crate::manifest::RuntimeManifest::load_active_checked(base_path) {
        Ok(m) => m,
        Err(message) => {
            eprintln!(
                "warning: active runtime manifest could not be read ({message}); \
                 listing directory extensions only"
            );
            None
        }
    };
    if let Some(manifest) = active {
        for ext in &manifest.extensions {
            // The resolver knows the fallback `<name>-<version>` naming and the
            // `.kab` vs `.raw` distinction; reconstructing `<id>.raw` by hand
            // reported a nonexistent path for both cases.
            let resolved = ext.resolve_path(base_path);
            // Seed the bare name, the `<name>-<version>` form (directory-form
            // extensions are named that way), AND the image file's stem. The
            // last one matters when the extensions dir IS the image pool: the
            // loose-.raw scan below would otherwise re-list every pool image by
            // its UUID (the exact thing listing by manifest name replaced).
            seen.insert(ext.name.clone());
            seen.insert(format!("{}-{}", ext.name, ext.version));
            if let Some(stem) = resolved.file_stem().and_then(|s| s.to_str()) {
                seen.insert(stem.to_string());
            }
            let path = resolved.display().to_string();
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
                if path.is_dir() {
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
                } else {
                    // Loose `<name>-<version>.raw` images in the extensions dir
                    // are real extensions: `enable` resolves `{name}.raw` here
                    // whether or not a manifest exists, so `list` must show them
                    // too (deduped against manifest entries by stem) or the two
                    // commands would disagree about what the device has.
                    if let Some(stem) = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".raw"))
                    {
                        // A UUID-stemmed .raw is a content-addressed pool image,
                        // not a loose dev extension; the manifest describes those.
                        if looks_like_uuid(stem) {
                            continue;
                        }
                        let (name, version) = split_name_version(stem);
                        // Dedup on the full `<name>-<version>` stem: foo-1.0 and
                        // foo-2.0 are distinct extensions and must both list.
                        if seen.insert(stem.to_string()) {
                            result.push(ExtensionInfo {
                                name: name.to_string(),
                                version,
                                path: path.display().to_string(),
                                is_sysext: true,
                                is_confext: false,
                                is_directory: false,
                            });
                        }
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

#[cfg(test)]
mod list_tests {
    use super::*;
    use crate::commands::test_env::ENV_VAR_MUTEX;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    /// The active-manifest path: names+versions come from the manifest, the path
    /// is the resolver's, and a versioned directory for the same extension is
    /// deduped away while an unrelated directory extension still lists.
    #[test]
    fn list_extensions_uses_active_manifest_and_dedups_versioned_dir() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();

        let rt = base.join("runtimes").join("uuid-1");
        std::fs::create_dir_all(&rt).unwrap();
        let manifest = serde_json::json!({
            "manifest_version": 1,
            "id": "uuid-1",
            "built_at": "2026-02-18T15:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": [
                { "name": "app", "version": "1.2.3",
                  "image_id": "a1b2c3d4-e5f6-5789-abcd-ef0123456789", "enabled": true }
            ]
        });
        std::fs::write(rt.join("manifest.json"), manifest.to_string()).unwrap();
        unix_fs::symlink("runtimes/uuid-1", base.join("active")).unwrap();
        let images = base.join("images");
        std::fs::create_dir_all(&images).unwrap();
        let img = images.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw");
        std::fs::write(&img, b"x").unwrap();

        let ext_dir = base.join("ext");
        std::fs::create_dir_all(ext_dir.join("app-1.2.3")).unwrap();
        std::fs::create_dir_all(ext_dir.join("hitlmount")).unwrap();
        // A loose .raw NOT in the manifest: `enable` would accept it, so `list`
        // must show it too even though a manifest is present.
        std::fs::write(ext_dir.join("loose-9.9.9.raw"), b"x").unwrap();

        std::env::set_var("AVOCADO_BASE_DIR", base);
        std::env::set_var("AVOCADO_EXTENSIONS_PATH", &ext_dir);
        let result = list_extensions(&Config::default());
        std::env::remove_var("AVOCADO_BASE_DIR");
        std::env::remove_var("AVOCADO_EXTENSIONS_PATH");
        let exts = result.expect("list ok");

        let app = exts.iter().find(|e| e.name == "app").expect("app listed");
        assert_eq!(app.version.as_deref(), Some("1.2.3"));
        assert_eq!(app.path, img.display().to_string());
        assert!(!app.is_directory);
        assert_eq!(
            exts.iter()
                .filter(|e| e.name == "app" || e.name == "app-1.2.3")
                .count(),
            1,
            "versioned dir must not double-list the manifest extension"
        );
        assert!(exts.iter().any(|e| e.name == "hitlmount" && e.is_directory));
        let loose = exts
            .iter()
            .find(|e| e.name == "loose")
            .expect("loose .raw listed with a manifest present");
        assert_eq!(loose.version.as_deref(), Some("9.9.9"));
    }

    /// When the extensions dir IS the image pool (as on some devices), the
    /// loose-.raw scan must not re-list manifest images by their UUID stem.
    #[test]
    fn list_extensions_does_not_relist_pool_images_by_uuid() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let rt = base.join("runtimes").join("uuid-1");
        std::fs::create_dir_all(&rt).unwrap();
        let uuid = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        let manifest = serde_json::json!({
            "manifest_version": 1, "id": "uuid-1", "built_at": "2026-02-18T15:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": [ { "name": "vmm", "version": "0.1.6", "image_id": uuid, "enabled": true } ]
        });
        std::fs::write(rt.join("manifest.json"), manifest.to_string()).unwrap();
        std::os::unix::fs::symlink("runtimes/uuid-1", base.join("active")).unwrap();
        // The extensions dir points AT the image pool (the board's layout).
        let images = base.join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join(format!("{uuid}.raw")), b"x").unwrap();
        // An orphan pool image (not in the manifest) must also be skipped.
        std::fs::write(
            images.join("deadbeef-0000-4000-8000-000000000000.raw"),
            b"x",
        )
        .unwrap();

        std::env::set_var("AVOCADO_BASE_DIR", base);
        std::env::set_var("AVOCADO_EXTENSIONS_PATH", &images);
        let result = list_extensions(&Config::default());
        std::env::remove_var("AVOCADO_BASE_DIR");
        std::env::remove_var("AVOCADO_EXTENSIONS_PATH");
        let exts = result.expect("list ok");

        assert_eq!(
            exts.len(),
            1,
            "pool image must not be double-listed: {exts:?}"
        );
        assert_eq!(exts[0].name, "vmm");
        assert_eq!(exts[0].version.as_deref(), Some("0.1.6"));
    }

    /// With no active manifest, loose `<name>-<version>.raw` images are still
    /// listed (legacy device behavior), not silently dropped.
    #[test]
    fn list_extensions_lists_raw_files_when_no_manifest() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let ext_dir = tmp.path().join("ext");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("foo-0.3.0.raw"), b"x").unwrap();
        std::fs::write(ext_dir.join("foo-0.4.0.raw"), b"x").unwrap();

        std::env::set_var("AVOCADO_BASE_DIR", tmp.path());
        std::env::set_var("AVOCADO_EXTENSIONS_PATH", &ext_dir);
        let result = list_extensions(&Config::default());
        std::env::remove_var("AVOCADO_BASE_DIR");
        std::env::remove_var("AVOCADO_EXTENSIONS_PATH");
        let exts = result.expect("list ok");

        let foos: Vec<_> = exts.iter().filter(|e| e.name == "foo").collect();
        assert_eq!(foos.len(), 2, "both versions must list: {foos:?}");
        let mut versions: Vec<_> = foos.iter().filter_map(|e| e.version.clone()).collect();
        versions.sort();
        assert_eq!(versions, vec!["0.3.0".to_string(), "0.4.0".to_string()]);
    }
}
