use crate::config::Config;
use crate::gc;
use crate::manifest::{RuntimeManifest, IMAGES_DIR_NAME};
use crate::service::error::AvocadoError;
use crate::service::types::{RuntimeEntry, RuntimeExtensionInfo};
use crate::update::UpdateOutcome;
use crate::{staging, update};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

/// A streaming operation: a channel receiver for log messages and a join handle for the result.
type StreamHandle = (
    mpsc::Receiver<String>,
    thread::JoinHandle<Result<(), AvocadoError>>,
);

/// Convert a RuntimeManifest + active flag to a RuntimeEntry.
pub fn manifest_to_entry(manifest: &RuntimeManifest, active: bool) -> RuntimeEntry {
    RuntimeEntry {
        id: manifest.id.clone(),
        manifest_version: manifest.manifest_version,
        built_at: manifest.built_at.clone(),
        name: manifest.runtime.name.clone(),
        version: manifest.runtime.version.clone(),
        extensions: manifest
            .extensions
            .iter()
            .map(|e| RuntimeExtensionInfo {
                name: e.name.clone(),
                version: e.version.clone(),
                image_id: e.image_id.clone(),
                image_type: e.image_type.clone(),
                sha256: e.sha256.clone(),
            })
            .collect(),
        active,
        os_build_id: manifest
            .os_bundle
            .as_ref()
            .and_then(|b| b.os_build_id.clone()),
        initramfs_build_id: manifest
            .os_bundle
            .as_ref()
            .and_then(|b| b.initramfs_build_id.clone()),
    }
}

/// List all available runtimes.
pub fn list_runtimes(config: &Config) -> Result<Vec<RuntimeEntry>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);

    Ok(runtimes
        .iter()
        .map(|(m, active)| manifest_to_entry(m, *active))
        .collect())
}

// ── Streaming service functions ──────────────────────────────────────────────

/// A streaming handle that emits one message and completes. Used where there is
/// no work to stream, so no extensions are touched.
fn message_streaming(message: &str) -> StreamHandle {
    let msg = message.to_string();
    let (tx, rx) = mpsc::sync_channel(4);
    let handle = thread::spawn(move || {
        let _ = tx.send(msg);
        Ok(())
    });
    (rx, handle)
}

/// A streaming handle that sends one message, triggers a reboot, and completes.
fn reboot_streaming(message: &str) -> StreamHandle {
    let msg = message.to_string();
    let (tx, rx) = mpsc::sync_channel(4);
    let handle = thread::spawn(move || {
        let _ = tx.send(msg);
        let _ = std::process::Command::new("reboot").status();
        Ok(())
    });
    (rx, handle)
}

/// Add a runtime from a TUF repository URL with streaming output.
/// Performs the TUF update synchronously, then streams the refresh operation.
pub fn add_from_url_streaming(
    url: &str,
    auth_token: Option<&str>,
    artifacts_url: Option<&str>,
    config: &Config,
) -> Result<StreamHandle, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let outcome = update::perform_update(
        url,
        base_path,
        auth_token,
        artifacts_url,
        config.stream_os_to_partition(),
        false,
        config.get_spot_check_bytes(),
    )?;

    match outcome {
        UpdateOutcome::RebootRequired => Ok(reboot_streaming(
            "OS update applied. Rebooting to activate new OS...",
        )),
        // Nothing changed, so nothing to refresh. Refreshing here would tear down
        // and re-merge every extension — including the one the caller is running
        // from — for no reason.
        UpdateOutcome::AlreadyCurrent => Ok(message_streaming(
            "Runtime already at target version, nothing to do.",
        )),
        UpdateOutcome::Activated => {
            if config.auto_gc() {
                let _ = garbage_collect(config);
            }
            Ok(super::ext::refresh_extensions_streaming(config))
        }
    }
}

/// Add a runtime from a local manifest file with streaming output.
/// Performs staging synchronously, then streams the refresh operation.
pub fn add_from_manifest_streaming(
    manifest_path: &str,
    config: &Config,
) -> Result<StreamHandle, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let manifest_content =
        std::fs::read_to_string(manifest_path).map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Failed to read manifest: {e}"),
        })?;

    let manifest: RuntimeManifest =
        serde_json::from_str(&manifest_content).map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Invalid manifest.json: {e}"),
        })?;

    staging::validate_manifest_images(&manifest, base_path)?;
    staging::stage_manifest(&manifest, &manifest_content, base_path, false)?;

    // Best-effort spot hash cache generation
    if let Ok(cache) =
        staging::generate_spot_hashes(&manifest, base_path, config.get_spot_check_bytes())
    {
        let runtime_dir = base_path.join("runtimes").join(&manifest.id);
        let _ = cache.save(&runtime_dir);
    }

    staging::activate_runtime(&manifest.id, base_path)?;
    if config.auto_gc() {
        let _ = garbage_collect(config);
    }
    Ok(super::ext::refresh_extensions_streaming(config))
}

/// Activate a staged runtime by ID (or prefix) with streaming output.
/// If the runtime requires a different OS, applies the OS update and reboots.
/// Otherwise activates immediately and streams the refresh operation.
pub fn activate_runtime_streaming(
    id_prefix: &str,
    config: &Config,
) -> Result<Option<StreamHandle>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);

    let (matched, is_active) = resolve_runtime_with_active(id_prefix, &runtimes)?;
    if is_active {
        return Ok(None); // Already active, nothing to do
    }

    if runtime_requires_os_change(matched, base_path)? {
        return Ok(Some(reboot_streaming(
            "OS change required. Rebooting to activate new OS...",
        )));
    }

    // Pre-flight: verify target runtime's images before tearing down current extensions
    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    staging::verify_runtime_integrity(
        matched,
        base_path,
        &runtime_dir,
        config.get_spot_check_bytes(),
        false,
    )?;

    staging::activate_runtime(&matched.id, base_path)?;
    Ok(Some(super::ext::refresh_extensions_streaming(config)))
}

// ── Batch service functions ──────────────────────────────────────────────────

/// Add a runtime from a TUF repository URL.
/// Returns log messages from the refresh operation.
pub fn add_from_url(
    url: &str,
    auth_token: Option<&str>,
    artifacts_url: Option<&str>,
    config: &Config,
) -> Result<Vec<String>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let outcome = update::perform_update(
        url,
        base_path,
        auth_token,
        artifacts_url,
        config.stream_os_to_partition(),
        false,
        config.get_spot_check_bytes(),
    )?;

    match outcome {
        UpdateOutcome::RebootRequired => {
            println!("  OS update applied. Rebooting to activate new OS...");
            let _ = std::process::Command::new("reboot").status();
            Ok(vec![
                "OS update applied. Rebooting to activate new OS.".to_string()
            ])
        }
        // Nothing changed, so nothing to refresh. See the streaming path above.
        UpdateOutcome::AlreadyCurrent => Ok(vec![
            "Runtime already at target version, nothing to do.".to_string(),
        ]),
        UpdateOutcome::Activated => {
            let result = super::ext::refresh_extensions(config);
            if config.auto_gc() {
                let _ = garbage_collect(config);
            }
            result
        }
    }
}

/// Add a runtime from a local manifest file.
/// Returns log messages from the refresh operation.
pub fn add_from_manifest(
    manifest_path: &str,
    config: &Config,
) -> Result<Vec<String>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let manifest_content =
        std::fs::read_to_string(manifest_path).map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Failed to read manifest: {e}"),
        })?;

    let manifest: RuntimeManifest =
        serde_json::from_str(&manifest_content).map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Invalid manifest.json: {e}"),
        })?;

    staging::validate_manifest_images(&manifest, base_path)?;
    staging::stage_manifest(&manifest, &manifest_content, base_path, false)?;

    // Best-effort spot hash cache generation
    if let Ok(cache) =
        staging::generate_spot_hashes(&manifest, base_path, config.get_spot_check_bytes())
    {
        let runtime_dir = base_path.join("runtimes").join(&manifest.id);
        let _ = cache.save(&runtime_dir);
    }

    staging::activate_runtime(&manifest.id, base_path)?;
    let result = super::ext::refresh_extensions(config);
    if config.auto_gc() {
        let _ = garbage_collect(config);
    }
    result
}

/// Remove a staged runtime by ID (or prefix).
pub fn remove_runtime(id_prefix: &str, config: &Config) -> Result<(), AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);

    let matched = resolve_runtime(id_prefix, &runtimes)?;
    staging::remove_runtime(&matched.id, base_path)?;
    Ok(())
}

/// Activate a staged runtime by ID (or prefix).
/// Activate a staged runtime by ID (or prefix).
/// If the runtime requires a different OS, applies the OS update and reboots.
/// Returns log messages from the refresh operation.
pub fn activate_runtime(id_prefix: &str, config: &Config) -> Result<Vec<String>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);

    let (matched, is_active) = resolve_runtime_with_active(id_prefix, &runtimes)?;
    if is_active {
        return Ok(Vec::new()); // Already active, nothing to do
    }

    if runtime_requires_os_change(matched, base_path)? {
        println!("  OS change required. Rebooting to activate new OS...");
        let _ = std::process::Command::new("reboot").status();
        return Ok(vec![
            "OS change required. Rebooting to activate new OS.".to_string()
        ]);
    }

    // Pre-flight: verify target runtime's images before tearing down current extensions
    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    staging::verify_runtime_integrity(
        matched,
        base_path,
        &runtime_dir,
        config.get_spot_check_bytes(),
        false,
    )?;

    staging::activate_runtime(&matched.id, base_path)?;
    super::ext::refresh_extensions(config)
}

/// Inspect a runtime's details by ID (or prefix).
/// If `id_prefix` is `None`, inspects the active runtime.
pub fn inspect_runtime(
    id_prefix: Option<&str>,
    config: &Config,
) -> Result<RuntimeEntry, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);

    let (matched, is_active) = match id_prefix {
        Some(prefix) => resolve_runtime_with_active(prefix, &runtimes)?,
        None => runtimes
            .iter()
            .find(|(_, active)| *active)
            .map(|(m, _)| (m, true))
            .ok_or_else(|| AvocadoError::RuntimeNotFound {
                id: "active".to_string(),
            })?,
    };
    Ok(manifest_to_entry(matched, is_active))
}

/// Check if activating a runtime requires an OS change (different os_build_id,
/// or a different initramfs than the active runtime's).
/// If so, applies the OS update from the on-disk image and sets up the pending
/// runtime marker for verification on next boot.
/// Returns true if a reboot is required (caller should reboot, not refresh).
fn runtime_requires_os_change(
    manifest: &RuntimeManifest,
    base_dir: &Path,
) -> Result<bool, AvocadoError> {
    let os_bundle = match &manifest.os_bundle {
        Some(b) => b,
        None => return Ok(false),
    };
    let expected_id = match &os_bundle.os_build_id {
        Some(id) => id,
        None => return Ok(false),
    };

    // Check if the running rootfs already matches
    let already_matches = crate::os_update::verify_os_release(&crate::os_update::VerifyConfig {
        verify_type: "os-release".to_string(),
        field: "AVOCADO_OS_BUILD_ID".to_string(),
        expected: expected_id.clone(),
    })
    .unwrap_or(false);

    if crate::os_update::os_bundle_satisfied(os_bundle, base_dir) {
        return Ok(false);
    }

    // OS differs — apply the update from the on-disk image
    let aos_path = base_dir
        .join(IMAGES_DIR_NAME)
        .join(format!("{}.raw", os_bundle.image_id));

    if !aos_path.exists() {
        return Err(AvocadoError::StagingFailed {
            reason: format!("OS bundle image not found: {}", aos_path.display()),
        });
    }

    if already_matches {
        println!(
            "  OS change required: the initramfs differs from the active runtime's (target initramfs_build_id={})",
            os_bundle.initramfs_build_id.as_deref().unwrap_or("-")
        );
    } else {
        println!(
            "  OS change required: current rootfs does not match target AVOCADO_OS_BUILD_ID={}",
            expected_id
        );
    }
    println!("  Applying OS update from {}...", aos_path.display());

    crate::os_update::apply_os_update(&aos_path, base_dir, false).map_err(|e| {
        AvocadoError::StagingFailed {
            reason: format!("OS update failed: {e}"),
        }
    })?;

    // Mark the runtime as pending — it will be promoted to active on next boot
    // after the OS build ID is verified.
    crate::os_update::set_pending_runtime_id(&manifest.id, base_dir).map_err(|e| {
        AvocadoError::StagingFailed {
            reason: format!("Failed to set pending runtime: {e}"),
        }
    })?;

    Ok(true)
}

// ── Garbage collection ─────────────────────────────────────────────────────

/// Run garbage collection to remove old runtimes and unreferenced images.
pub fn garbage_collect(config: &Config) -> Result<gc::GcResult, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let retention = config.runtime_retention();
    gc::collect_garbage(base_path, retention).map_err(|e| e.into())
}

// ── Metadata service functions ──────────────────────────────────────────────

/// Set a metadata key-value pair on a runtime.
pub fn metadata_set(
    id_prefix: &str,
    key: &str,
    value: &str,
    config: &Config,
) -> Result<(), AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);
    let matched = resolve_runtime(id_prefix, &runtimes)?;

    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    let mut metadata = crate::metadata::RuntimeMetadata::load(&runtime_dir);
    metadata.entries.insert(key.to_string(), value.to_string());
    metadata
        .save(&runtime_dir)
        .map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Failed to save metadata: {e}"),
        })?;
    Ok(())
}

/// Get a metadata value by key.
pub fn metadata_get(id_prefix: &str, key: &str, config: &Config) -> Result<String, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);
    let matched = resolve_runtime(id_prefix, &runtimes)?;

    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    let metadata = crate::metadata::RuntimeMetadata::load(&runtime_dir);
    metadata
        .entries
        .get(key)
        .cloned()
        .ok_or_else(|| AvocadoError::MetadataKeyNotFound {
            id: matched.id.clone(),
            key: key.to_string(),
        })
}

/// List all metadata for a runtime.
pub fn metadata_list(
    id_prefix: &str,
    config: &Config,
) -> Result<Vec<(String, String)>, AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);
    let matched = resolve_runtime(id_prefix, &runtimes)?;

    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    let metadata = crate::metadata::RuntimeMetadata::load(&runtime_dir);
    let mut entries: Vec<(String, String)> = metadata.entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Delete a metadata key from a runtime.
pub fn metadata_delete(id_prefix: &str, key: &str, config: &Config) -> Result<(), AvocadoError> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let runtimes = RuntimeManifest::list_all(base_path);
    let matched = resolve_runtime(id_prefix, &runtimes)?;

    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    let mut metadata = crate::metadata::RuntimeMetadata::load(&runtime_dir);
    if metadata.entries.remove(key).is_none() {
        return Err(AvocadoError::MetadataKeyNotFound {
            id: matched.id.clone(),
            key: key.to_string(),
        });
    }
    metadata
        .save(&runtime_dir)
        .map_err(|e| AvocadoError::StagingFailed {
            reason: format!("Failed to save metadata: {e}"),
        })?;
    Ok(())
}

/// Resolve a runtime ID prefix to a unique RuntimeManifest.
fn resolve_runtime<'a>(
    id_prefix: &str,
    runtimes: &'a [(RuntimeManifest, bool)],
) -> Result<&'a RuntimeManifest, AvocadoError> {
    let (matched, _) = resolve_runtime_with_active(id_prefix, runtimes)?;
    Ok(matched)
}

/// Resolve a runtime ID prefix, returning the manifest and its active status.
fn resolve_runtime_with_active<'a>(
    id_prefix: &str,
    runtimes: &'a [(RuntimeManifest, bool)],
) -> Result<(&'a RuntimeManifest, bool), AvocadoError> {
    let matches: Vec<&(RuntimeManifest, bool)> = runtimes
        .iter()
        .filter(|(m, _)| m.id.starts_with(id_prefix))
        .collect();

    match matches.len() {
        0 => Err(AvocadoError::RuntimeNotFound {
            id: id_prefix.to_string(),
        }),
        1 => Ok((&matches[0].0, matches[0].1)),
        _ => {
            let candidates: Vec<String> = matches
                .iter()
                .map(|(m, _)| m.id[..8.min(m.id.len())].to_string())
                .collect();
            Err(AvocadoError::AmbiguousRuntimeId {
                id: id_prefix.to_string(),
                candidates,
            })
        }
    }
}

#[cfg(test)]
mod initramfs_change_tests {
    use super::*;
    use crate::manifest::OsBundleRef;
    use crate::os_update::initramfs_differs_from_active;

    fn bundle(initramfs: Option<&str>) -> OsBundleRef {
        OsBundleRef {
            image_id: "img".into(),
            sha256: "00".into(),
            os_build_id: Some("os-1".into()),
            initramfs_build_id: initramfs.map(str::to_string),
        }
    }

    fn active_with(initramfs: Option<&str>) -> RuntimeManifest {
        let mut m: RuntimeManifest = serde_json::from_str(
            r#"{"manifest_version":1,"id":"active","runtime":{"name":"dev","version":"1"},"built_at":"2026-01-01T00:00:00Z","extensions":[]}"#,
        )
        .expect("minimal manifest");
        m.os_bundle = Some(bundle(initramfs));
        m
    }

    #[test]
    fn same_rootfs_different_initramfs_is_an_os_change() {
        let active = active_with(Some("initrd-a"));
        assert!(initramfs_differs_from_active(
            &bundle(Some("initrd-b")),
            Some(&active)
        ));
        assert!(!initramfs_differs_from_active(
            &bundle(Some("initrd-a")),
            Some(&active)
        ));
    }

    #[test]
    fn unknown_sides_never_force_a_reboot() {
        let active = active_with(None);
        assert!(!initramfs_differs_from_active(
            &bundle(Some("initrd-b")),
            Some(&active)
        ));
        assert!(!initramfs_differs_from_active(
            &bundle(None),
            Some(&active_with(Some("x")))
        ));
        assert!(!initramfs_differs_from_active(
            &bundle(Some("initrd-b")),
            None
        ));
    }
}
