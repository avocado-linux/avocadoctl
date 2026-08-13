use crate::hash::{sha256_file, spot_hash_file};
use crate::manifest::{RuntimeManifest, ACTIVE_LINK_NAME, IMAGES_DIR_NAME, MANIFEST_FILENAME};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

pub const SPOT_HASHES_FILENAME: &str = "spot_hashes.json";

#[derive(Error, Debug)]
pub enum StagingError {
    #[error("Staging failed: {0}")]
    StagingFailed(String),

    #[error("Cannot remove the active runtime. Activate a different runtime first.")]
    RemoveActiveRuntime,

    #[error("Runtime not found: {0}")]
    RuntimeNotFound(String),

    #[error("Missing extension images:\n{0}")]
    MissingImages(String),

    #[error("Image integrity check failed:\n{0}")]
    HashMismatch(String),
}

#[derive(Debug)]
pub struct MissingImage {
    pub extension_name: String,
    pub expected_path: String,
}

#[derive(Debug)]
pub struct ImageHashMismatch {
    pub image_name: String,
    pub path: String,
    pub expected: String,
    pub actual: String,
}

/// Cached spot-check hashes for fast integrity verification at merge time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotHashCache {
    pub version: u32,
    pub spot_check_bytes: u64,
    pub hashes: HashMap<String, String>,
}

impl SpotHashCache {
    /// Load the spot hash cache from a runtime directory. Returns None if missing or corrupt.
    pub fn load(runtime_dir: &Path) -> Option<Self> {
        let path = runtime_dir.join(SPOT_HASHES_FILENAME);
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save the spot hash cache to a runtime directory.
    pub fn save(&self, runtime_dir: &Path) -> Result<(), StagingError> {
        let path = runtime_dir.join(SPOT_HASHES_FILENAME);
        let json = serde_json::to_string_pretty(self).map_err(|e| {
            StagingError::StagingFailed(format!("Failed to serialize spot hash cache: {e}"))
        })?;
        fs::write(&path, json).map_err(|e| {
            StagingError::StagingFailed(format!(
                "Failed to write spot hash cache to {}: {e}",
                path.display()
            ))
        })?;
        Ok(())
    }
}

/// Generate spot-check hashes for all extension and OS bundle images in the manifest.
pub fn generate_spot_hashes(
    manifest: &RuntimeManifest,
    base_dir: &Path,
    spot_check_bytes: u64,
) -> Result<SpotHashCache, StagingError> {
    let mut hashes = HashMap::new();

    for ext in &manifest.extensions {
        let path = ext.resolve_path(base_dir);
        if path.exists() && !path.is_dir() {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let hash = spot_hash_file(&path, spot_check_bytes).map_err(|e| {
                StagingError::StagingFailed(format!("Failed to spot-hash {}: {e}", path.display()))
            })?;
            hashes.insert(filename, hash);
        }
    }

    if let Some(ref os_bundle) = manifest.os_bundle {
        let path = base_dir
            .join(IMAGES_DIR_NAME)
            .join(format!("{}.raw", os_bundle.image_id));
        if path.exists() {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let hash = spot_hash_file(&path, spot_check_bytes).map_err(|e| {
                StagingError::StagingFailed(format!(
                    "Failed to spot-hash OS bundle {}: {e}",
                    path.display()
                ))
            })?;
            hashes.insert(filename, hash);
        }
    }

    Ok(SpotHashCache {
        version: 1,
        spot_check_bytes,
        hashes,
    })
}

/// Verify integrity of a runtime's extension images before activation or merge.
///
/// If a `spot_hashes.json` cache exists in `runtime_dir`, uses the fast spot check.
/// Otherwise falls back to full SHA256 validation against manifest hashes, then
/// generates and saves a spot cache for future checks.
pub fn verify_runtime_integrity(
    manifest: &RuntimeManifest,
    base_dir: &Path,
    runtime_dir: &Path,
    spot_check_bytes: u64,
    verbose: bool,
) -> Result<(), StagingError> {
    if let Some(cache) = SpotHashCache::load(runtime_dir) {
        return verify_with_spot_cache(manifest, base_dir, &cache, verbose);
    }

    // No spot cache — fall back to full SHA256 validation
    if verbose {
        eprintln!("Note: No spot hash cache found — falling back to full SHA256 verification");
    }
    validate_manifest_images(manifest, base_dir)?;

    // Full check passed — generate and save spot cache for next time
    if let Ok(cache) = generate_spot_hashes(manifest, base_dir, spot_check_bytes) {
        let _ = cache.save(runtime_dir);
    }

    Ok(())
}

/// Convenience wrapper that resolves the active runtime directory.
pub fn verify_spot_hashes(
    manifest: &RuntimeManifest,
    base_dir: &Path,
    spot_check_bytes: u64,
    verbose: bool,
) -> Result<(), StagingError> {
    let active_dir = base_dir.join(ACTIVE_LINK_NAME);
    verify_runtime_integrity(manifest, base_dir, &active_dir, spot_check_bytes, verbose)
}

/// Verify images against a loaded spot hash cache.
fn verify_with_spot_cache(
    manifest: &RuntimeManifest,
    base_dir: &Path,
    cache: &SpotHashCache,
    verbose: bool,
) -> Result<(), StagingError> {
    let spot_size = cache.spot_check_bytes;
    let mut mismatches: Vec<ImageHashMismatch> = Vec::new();

    if verbose {
        eprintln!(
            "Verifying {} extension image(s) with spot check ({spot_size} byte head+tail)",
            manifest.extensions.len()
        );
    }

    for ext in &manifest.extensions {
        let path = ext.resolve_path(base_dir);
        if !path.exists() || path.is_dir() {
            continue;
        }
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if let Some(expected) = cache.hashes.get(filename) {
            let actual = spot_hash_file(&path, spot_size).map_err(|e| {
                StagingError::StagingFailed(format!("Failed to spot-hash {}: {e}", path.display()))
            })?;
            if actual != *expected {
                mismatches.push(ImageHashMismatch {
                    image_name: format!("{} {}", ext.name, ext.version),
                    path: path.display().to_string(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
    }

    if let Some(ref os_bundle) = manifest.os_bundle {
        let path = base_dir
            .join(IMAGES_DIR_NAME)
            .join(format!("{}.raw", os_bundle.image_id));
        if path.exists() {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if let Some(expected) = cache.hashes.get(filename) {
                let actual = spot_hash_file(&path, spot_size).map_err(|e| {
                    StagingError::StagingFailed(format!(
                        "Failed to spot-hash OS bundle {}: {e}",
                        path.display()
                    ))
                })?;
                if actual != *expected {
                    mismatches.push(ImageHashMismatch {
                        image_name: format!("os_bundle ({})", os_bundle.image_id),
                        path: path.display().to_string(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
        }
    }

    if !mismatches.is_empty() {
        let details = mismatches
            .iter()
            .map(|h| {
                format!(
                    "  {} ({}): expected {}, got {}",
                    h.image_name, h.path, h.expected, h.actual
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(StagingError::HashMismatch(details));
    }

    Ok(())
}

/// Check that all extension and OS bundle images referenced by the manifest
/// exist on disk and, when SHA256 hashes are present, match their expected values.
pub fn validate_manifest_images(
    manifest: &RuntimeManifest,
    base_dir: &Path,
) -> Result<(), StagingError> {
    let mut missing: Vec<MissingImage> = Vec::new();
    let mut hash_errors: Vec<ImageHashMismatch> = Vec::new();

    for ext in &manifest.extensions {
        let path = ext.resolve_path(base_dir);
        if !path.exists() {
            missing.push(MissingImage {
                extension_name: format!("{} {}", ext.name, ext.version),
                expected_path: path.display().to_string(),
            });
            continue;
        }
        if let Some(ref expected_sha) = ext.sha256 {
            let actual = sha256_file(&path).map_err(|e| {
                StagingError::StagingFailed(format!("Failed to hash {}: {e}", path.display()))
            })?;
            if actual != *expected_sha {
                hash_errors.push(ImageHashMismatch {
                    image_name: format!("{} {}", ext.name, ext.version),
                    path: path.display().to_string(),
                    expected: expected_sha.clone(),
                    actual,
                });
            }
        }
    }

    // Also check os_bundle image if present
    if let Some(ref os_bundle) = manifest.os_bundle {
        let path = base_dir
            .join(IMAGES_DIR_NAME)
            .join(format!("{}.raw", os_bundle.image_id));
        if !path.exists() {
            missing.push(MissingImage {
                extension_name: format!("os_bundle ({})", os_bundle.image_id),
                expected_path: path.display().to_string(),
            });
        } else {
            let actual = sha256_file(&path).map_err(|e| {
                StagingError::StagingFailed(format!(
                    "Failed to hash OS bundle {}: {e}",
                    path.display()
                ))
            })?;
            if actual != os_bundle.sha256 {
                hash_errors.push(ImageHashMismatch {
                    image_name: format!("os_bundle ({})", os_bundle.image_id),
                    path: path.display().to_string(),
                    expected: os_bundle.sha256.clone(),
                    actual,
                });
            }
        }
    }

    if !missing.is_empty() {
        let details = missing
            .iter()
            .map(|m| format!("  {} -> {}", m.extension_name, m.expected_path))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(StagingError::MissingImages(details));
    }

    if !hash_errors.is_empty() {
        let details = hash_errors
            .iter()
            .map(|h| {
                format!(
                    "  {} ({}): expected {}, got {}",
                    h.image_name, h.path, h.expected, h.actual
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(StagingError::HashMismatch(details));
    }

    Ok(())
}

/// Create the runtime directory and write the manifest file.
pub fn stage_manifest(
    manifest: &RuntimeManifest,
    manifest_json: &str,
    base_dir: &Path,
    verbose: bool,
) -> Result<(), StagingError> {
    let runtime_dir = base_dir.join("runtimes").join(&manifest.id);
    fs::create_dir_all(&runtime_dir).map_err(|e| {
        StagingError::StagingFailed(format!("Failed to create runtime directory: {e}"))
    })?;

    fs::write(runtime_dir.join(MANIFEST_FILENAME), manifest_json)
        .map_err(|e| StagingError::StagingFailed(format!("Failed to write manifest: {e}")))?;

    if verbose {
        println!(
            "  Staged runtime: {} {} ({})",
            manifest.runtime.name,
            manifest.runtime.version,
            &manifest.id[..8.min(manifest.id.len())]
        );
    }

    Ok(())
}

/// Copy extension images from a staging directory into the shared image pool.
/// Verifies SHA256 hashes after copying when hashes are present in the manifest.
/// Used by the TUF update path after downloading targets.
pub fn install_images_from_staging(
    manifest: &RuntimeManifest,
    staging_dir: &Path,
    base_dir: &Path,
    skip_os_bundle: bool,
    verbose: bool,
) -> Result<(), StagingError> {
    let images_dir = base_dir.join(IMAGES_DIR_NAME);
    let _ = fs::create_dir_all(&images_dir);

    let mut missing = Vec::new();

    for ext in &manifest.extensions {
        if let Some(ref image_id) = ext.image_id {
            let dest = images_dir.join(format!("{image_id}.raw"));
            if dest.exists() {
                if verbose {
                    println!(
                        "    Image already present: {} {} ({})",
                        ext.name, ext.version, image_id
                    );
                }
                // Verify hash of existing image if sha256 is available
                if let Some(ref expected_sha) = ext.sha256 {
                    verify_installed_hash(&dest, expected_sha, &ext.name)?;
                }
                continue;
            }
            let staged_file = staging_dir.join(format!("{image_id}.raw"));
            if staged_file.exists() {
                fs::copy(&staged_file, &dest).map_err(|e| {
                    StagingError::StagingFailed(format!(
                        "Failed to install image for {}: {e}",
                        ext.name
                    ))
                })?;
                // Verify hash after copy if sha256 is available
                if let Some(ref expected_sha) = ext.sha256 {
                    verify_installed_hash(&dest, expected_sha, &ext.name)?;
                }
                if verbose {
                    println!(
                        "    Installed image: {} {} -> {image_id}.raw",
                        ext.name, ext.version,
                    );
                }
            } else {
                println!(
                    "    WARNING: Image not in staging and not on disk: {} {} ({})",
                    ext.name, ext.version, image_id
                );
                missing.push(format!("{} {} ({})", ext.name, ext.version, image_id));
            }
        }
    }

    // Install os_bundle image if present
    if let Some(ref os_bundle) = manifest.os_bundle {
        if skip_os_bundle {
            println!(
                "    OS bundle skipped (OS already at target version): {}",
                os_bundle.image_id
            );
        } else {
            let image_id = &os_bundle.image_id;
            let dest = images_dir.join(format!("{image_id}.raw"));
            if dest.exists() {
                if verbose {
                    println!("    OS bundle image already present: {image_id}");
                }
                verify_installed_hash(&dest, &os_bundle.sha256, "os_bundle")?;
            } else {
                let staged_file = staging_dir.join(format!("{image_id}.raw"));
                if staged_file.exists() {
                    fs::copy(&staged_file, &dest).map_err(|e| {
                        StagingError::StagingFailed(format!(
                            "Failed to install OS bundle image: {e}"
                        ))
                    })?;
                    verify_installed_hash(&dest, &os_bundle.sha256, "os_bundle")?;
                    if verbose {
                        println!("    Installed OS bundle image: {image_id}");
                    }
                } else {
                    println!(
                        "    WARNING: OS bundle image not in staging and not on disk: {image_id}"
                    );
                    missing.push(format!("os_bundle ({image_id})"));
                }
            }
        }
    }

    if !missing.is_empty() {
        let details = missing.join(", ");
        return Err(StagingError::StagingFailed(format!(
            "{} image(s) missing after staging: {details}",
            missing.len()
        )));
    }

    Ok(())
}

/// Verify the SHA256 hash of an installed image file.
fn verify_installed_hash(
    path: &Path,
    expected: &str,
    image_name: &str,
) -> Result<(), StagingError> {
    let actual = sha256_file(path).map_err(|e| {
        StagingError::StagingFailed(format!("Failed to hash {}: {e}", path.display()))
    })?;
    if actual != expected {
        return Err(StagingError::HashMismatch(format!(
            "  {} ({}): expected {}, got {}",
            image_name,
            path.display(),
            expected,
            actual
        )));
    }
    Ok(())
}

/// Atomically switch the active symlink to point to the given runtime.
///
/// The link is staged under a temporary name and `rename`d into place. Removing
/// the old link first would leave a window where `active` does not exist, and a
/// crash inside that window is unrecoverable in the field: the boot path merges
/// no extensions without it, and sshd is itself an extension.
pub fn activate_runtime(runtime_id: &str, base_dir: &Path) -> Result<(), StagingError> {
    let runtime_dir = base_dir.join("runtimes").join(runtime_id);
    if !runtime_dir.exists() {
        return Err(StagingError::RuntimeNotFound(runtime_id.to_string()));
    }

    #[cfg(unix)]
    {
        let active_link = base_dir.join(ACTIVE_LINK_NAME);
        let active_target = format!("runtimes/{runtime_id}");
        let staged_link = base_dir.join(format!(".{ACTIVE_LINK_NAME}.new"));

        // A staged link left by an interrupted activation is stale by
        // definition — its target is whatever that attempt wanted, not ours.
        let _ = fs::remove_file(&staged_link);

        std::os::unix::fs::symlink(&active_target, &staged_link).map_err(|e| {
            StagingError::StagingFailed(format!("Failed to stage active runtime link: {e}"))
        })?;

        fs::rename(&staged_link, &active_link).map_err(|e| {
            let _ = fs::remove_file(&staged_link);
            StagingError::StagingFailed(format!("Failed to switch active runtime: {e}"))
        })?;

        // The rename is only crash-safe once the directory entry is durable.
        // Best-effort: a failing fsync does not invalidate the swap itself.
        if let Ok(dir) = fs::File::open(base_dir) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// Remove a runtime directory. Fails if the runtime is currently active.
pub fn remove_runtime(runtime_id: &str, base_dir: &Path) -> Result<(), StagingError> {
    let active_id = resolve_active_id(base_dir);
    if active_id.as_deref() == Some(runtime_id) {
        return Err(StagingError::RemoveActiveRuntime);
    }

    let runtime_dir = base_dir.join("runtimes").join(runtime_id);
    if !runtime_dir.exists() {
        return Err(StagingError::RuntimeNotFound(runtime_id.to_string()));
    }

    fs::remove_dir_all(&runtime_dir).map_err(|e| {
        StagingError::StagingFailed(format!("Failed to remove runtime directory: {e}"))
    })?;

    Ok(())
}

fn resolve_active_id(base_dir: &Path) -> Option<String> {
    let active_path = base_dir.join(ACTIVE_LINK_NAME);
    let target = fs::read_link(&active_path).ok()?;
    target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestExtension, RuntimeInfo};
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn make_manifest(id: &str, name: &str, version: &str) -> RuntimeManifest {
        RuntimeManifest {
            manifest_version: 1,
            id: id.to_string(),
            built_at: "2026-02-19T00:00:00Z".to_string(),
            runtime: RuntimeInfo {
                name: name.to_string(),
                version: version.to_string(),
            },
            extensions: vec![ManifestExtension {
                name: "app".to_string(),
                version: "0.1.0".to_string(),
                image_id: Some("a1b2c3d4-e5f6-5789-abcd-ef0123456789".to_string()),
                image_type: None,
                sha256: None,
                enabled: true,
            }],
            os_bundle: None,
        }
    }

    #[test]
    fn test_validate_manifest_images_present() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(
            images_dir.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"),
            b"image data",
        )
        .unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");
        assert!(validate_manifest_images(&manifest, tmp.path()).is_ok());
    }

    #[test]
    fn test_validate_manifest_images_missing() {
        let tmp = TempDir::new().unwrap();
        let manifest = make_manifest("test-id", "dev", "0.1.0");
        let result = validate_manifest_images(&manifest, tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("app 0.1.0"));
        assert!(err.contains("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"));
    }

    #[test]
    fn test_stage_manifest_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let manifest = make_manifest("uuid-123", "dev", "0.1.0");
        let json = serde_json::to_string_pretty(&manifest).unwrap();

        stage_manifest(&manifest, &json, tmp.path(), false).unwrap();

        let manifest_path = tmp
            .path()
            .join("runtimes")
            .join("uuid-123")
            .join("manifest.json");
        assert!(manifest_path.exists());
        let content = fs::read_to_string(manifest_path).unwrap();
        assert!(content.contains("uuid-123"));
    }

    #[test]
    fn test_activate_runtime_creates_symlink() {
        let tmp = TempDir::new().unwrap();
        let runtime_dir = tmp.path().join("runtimes").join("uuid-456");
        fs::create_dir_all(&runtime_dir).unwrap();

        activate_runtime("uuid-456", tmp.path()).unwrap();

        let active = tmp.path().join("active");
        assert!(active.exists());
        let target = fs::read_link(&active).unwrap();
        assert_eq!(target.to_str().unwrap(), "runtimes/uuid-456");
    }

    #[test]
    fn test_activate_runtime_switches_symlink() {
        let tmp = TempDir::new().unwrap();
        let rt1 = tmp.path().join("runtimes").join("uuid-1");
        let rt2 = tmp.path().join("runtimes").join("uuid-2");
        fs::create_dir_all(&rt1).unwrap();
        fs::create_dir_all(&rt2).unwrap();

        activate_runtime("uuid-1", tmp.path()).unwrap();
        activate_runtime("uuid-2", tmp.path()).unwrap();

        let target = fs::read_link(tmp.path().join("active")).unwrap();
        assert_eq!(target.to_str().unwrap(), "runtimes/uuid-2");
    }

    /// `active` must never be observable as missing. Rename-into-place
    /// guarantees that; unlink-then-symlink leaves a window a concurrent reader
    /// — or a crash — can land in, and a boot that lands in it merges nothing.
    #[test]
    fn test_activate_runtime_active_never_observed_missing() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        for id in ["uuid-1", "uuid-2"] {
            fs::create_dir_all(tmp.path().join("runtimes").join(id)).unwrap();
        }
        activate_runtime("uuid-1", tmp.path()).unwrap();

        let active = tmp.path().join("active");
        let stop = Arc::new(AtomicBool::new(false));
        let missing = Arc::new(AtomicBool::new(false));

        let reader = {
            let (active, stop, missing) = (active.clone(), stop.clone(), missing.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if fs::read_link(&active).is_err() {
                        missing.store(true, Ordering::Relaxed);
                    }
                }
            })
        };

        for i in 0..2000 {
            activate_runtime(if i % 2 == 0 { "uuid-2" } else { "uuid-1" }, tmp.path()).unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();

        assert!(
            !missing.load(Ordering::Relaxed),
            "`active` was observed missing mid-activation"
        );
        assert!(!tmp.path().join(".active.new").exists());
    }

    /// A staged link left by an interrupted activation must not divert the next
    /// one to the abandoned target.
    #[test]
    fn test_activate_runtime_discards_stale_staged_link() {
        let tmp = TempDir::new().unwrap();
        for id in ["uuid-1", "uuid-2"] {
            fs::create_dir_all(tmp.path().join("runtimes").join(id)).unwrap();
        }
        activate_runtime("uuid-1", tmp.path()).unwrap();

        // Simulate a crash between staging the link and renaming it.
        unix_fs::symlink("runtimes/uuid-2", tmp.path().join(".active.new")).unwrap();

        activate_runtime("uuid-1", tmp.path()).unwrap();

        assert_eq!(
            fs::read_link(tmp.path().join("active"))
                .unwrap()
                .to_str()
                .unwrap(),
            "runtimes/uuid-1"
        );
        assert!(!tmp.path().join(".active.new").exists());
    }

    #[test]
    fn test_activate_runtime_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let result = activate_runtime("nonexistent", tmp.path());
        assert!(matches!(result, Err(StagingError::RuntimeNotFound(_))));
    }

    #[test]
    fn test_remove_runtime_deletes_directory() {
        let tmp = TempDir::new().unwrap();
        let runtime_dir = tmp.path().join("runtimes").join("uuid-rm");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("manifest.json"), b"{}").unwrap();

        remove_runtime("uuid-rm", tmp.path()).unwrap();
        assert!(!runtime_dir.exists());
    }

    #[test]
    fn test_remove_runtime_rejects_active() {
        let tmp = TempDir::new().unwrap();
        let runtime_dir = tmp.path().join("runtimes").join("uuid-active");
        fs::create_dir_all(&runtime_dir).unwrap();
        unix_fs::symlink("runtimes/uuid-active", tmp.path().join("active")).unwrap();

        let result = remove_runtime("uuid-active", tmp.path());
        assert!(matches!(result, Err(StagingError::RemoveActiveRuntime)));
        assert!(runtime_dir.exists());
    }

    #[test]
    fn test_remove_runtime_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let result = remove_runtime("nonexistent", tmp.path());
        assert!(matches!(result, Err(StagingError::RuntimeNotFound(_))));
    }

    #[test]
    fn test_install_images_from_staging_v2() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(staging.join(format!("{image_id}.raw")), b"image content").unwrap();

        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");
        install_images_from_staging(&manifest, &staging, &base, false, false).unwrap();

        let installed = base.join("images").join(format!("{image_id}.raw"));
        assert!(installed.exists());
    }

    #[test]
    fn test_install_images_skips_existing() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(staging.join(format!("{image_id}.raw")), b"new content").unwrap();

        let base = tmp.path().join("base");
        let images_dir = base.join("images");
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(images_dir.join(format!("{image_id}.raw")), b"old content").unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");
        install_images_from_staging(&manifest, &staging, &base, false, false).unwrap();

        let content = fs::read_to_string(images_dir.join(format!("{image_id}.raw"))).unwrap();
        assert_eq!(content, "old content");
    }

    /// Helper: compute sha256 hex of some bytes for test fixtures.
    fn test_sha256(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(data);
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn make_manifest_with_hash(
        id: &str,
        name: &str,
        version: &str,
        sha256: Option<String>,
    ) -> RuntimeManifest {
        RuntimeManifest {
            manifest_version: 1,
            id: id.to_string(),
            built_at: "2026-02-19T00:00:00Z".to_string(),
            runtime: RuntimeInfo {
                name: name.to_string(),
                version: version.to_string(),
            },
            extensions: vec![ManifestExtension {
                name: "app".to_string(),
                version: "0.1.0".to_string(),
                image_id: Some("a1b2c3d4-e5f6-5789-abcd-ef0123456789".to_string()),
                image_type: None,
                sha256,
                enabled: true,
            }],
            os_bundle: None,
        }
    }

    #[test]
    fn test_validate_manifest_images_hash_ok() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_data = b"correct image data";
        let hash = test_sha256(image_data);
        fs::write(
            images_dir.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"),
            image_data,
        )
        .unwrap();

        let manifest = make_manifest_with_hash("test-id", "dev", "0.1.0", Some(hash));
        assert!(validate_manifest_images(&manifest, tmp.path()).is_ok());
    }

    #[test]
    fn test_validate_manifest_images_hash_mismatch() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        fs::write(
            images_dir.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"),
            b"corrupted data",
        )
        .unwrap();

        let manifest = make_manifest_with_hash(
            "test-id",
            "dev",
            "0.1.0",
            Some("0000000000000000000000000000000000000000000000000000000000000000".to_string()),
        );
        let result = validate_manifest_images(&manifest, tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity check failed"));
        assert!(err.contains("app 0.1.0"));
    }

    #[test]
    fn test_validate_manifest_images_no_hash_skips_check() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(
            images_dir.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"),
            b"any content at all",
        )
        .unwrap();

        let manifest = make_manifest_with_hash("test-id", "dev", "0.1.0", None);
        assert!(validate_manifest_images(&manifest, tmp.path()).is_ok());
    }

    #[test]
    fn test_validate_os_bundle_hash_mismatch() {
        use crate::manifest::OsBundleRef;

        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(
            images_dir.join("deadbeef-1234-5678-abcd-000000000000.raw"),
            b"corrupted os bundle",
        )
        .unwrap();

        let mut manifest = make_manifest("test-id", "dev", "0.1.0");
        fs::write(
            images_dir.join("a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw"),
            b"ext data",
        )
        .unwrap();
        manifest.os_bundle = Some(OsBundleRef {
            image_id: "deadbeef-1234-5678-abcd-000000000000".to_string(),
            sha256: "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            os_build_id: None,
            initramfs_build_id: None,
        });
        let result = validate_manifest_images(&manifest, tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity check failed"));
        assert!(err.contains("os_bundle"));
    }

    #[test]
    fn test_install_images_hash_mismatch_after_copy() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(staging.join(format!("{image_id}.raw")), b"staged content").unwrap();

        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();

        let manifest = make_manifest_with_hash(
            "test-id",
            "dev",
            "0.1.0",
            Some("0000000000000000000000000000000000000000000000000000000000000000".to_string()),
        );
        let result = install_images_from_staging(&manifest, &staging, &base, false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity check failed"));
    }

    #[test]
    fn test_install_images_hash_ok() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();

        let image_data = b"good image content";
        let hash = test_sha256(image_data);
        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(staging.join(format!("{image_id}.raw")), image_data).unwrap();

        let base = tmp.path().join("base");
        fs::create_dir_all(&base).unwrap();

        let manifest = make_manifest_with_hash("test-id", "dev", "0.1.0", Some(hash));
        assert!(install_images_from_staging(&manifest, &staging, &base, false, false).is_ok());

        let installed = base.join("images").join(format!("{image_id}.raw"));
        assert!(installed.exists());
    }

    #[test]
    fn test_spot_hash_cache_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut hashes = HashMap::new();
        hashes.insert("test.raw".to_string(), "abcdef".to_string());
        let cache = SpotHashCache {
            version: 1,
            spot_check_bytes: 4096,
            hashes,
        };
        cache.save(tmp.path()).unwrap();

        let loaded = SpotHashCache::load(tmp.path()).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.spot_check_bytes, 4096);
        assert_eq!(loaded.hashes.get("test.raw").unwrap(), "abcdef");
    }

    #[test]
    fn test_spot_hash_cache_load_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(SpotHashCache::load(tmp.path()).is_none());
    }

    #[test]
    fn test_spot_hash_cache_load_corrupt() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(SPOT_HASHES_FILENAME), "not valid json").unwrap();
        assert!(SpotHashCache::load(tmp.path()).is_none());
    }

    #[test]
    fn test_generate_spot_hashes() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(images_dir.join(format!("{image_id}.raw")), b"image data").unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");
        let cache = generate_spot_hashes(&manifest, tmp.path(), 4096).unwrap();

        assert_eq!(cache.version, 1);
        assert_eq!(cache.spot_check_bytes, 4096);
        assert!(cache.hashes.contains_key(&format!("{image_id}.raw")));
    }

    #[test]
    fn test_verify_spot_hashes_ok() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(images_dir.join(format!("{image_id}.raw")), b"image data").unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");

        // Generate and save cache, then create the active symlink
        let cache = generate_spot_hashes(&manifest, tmp.path(), 4096).unwrap();
        let runtime_dir = tmp.path().join("runtimes").join("test-id");
        fs::create_dir_all(&runtime_dir).unwrap();
        cache.save(&runtime_dir).unwrap();
        unix_fs::symlink("runtimes/test-id", tmp.path().join("active")).unwrap();

        // Verification should pass
        assert!(verify_spot_hashes(&manifest, tmp.path(), 4096, false).is_ok());
    }

    #[test]
    fn test_verify_spot_hashes_mismatch() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(images_dir.join(format!("{image_id}.raw")), b"image data").unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");

        // Generate and save cache
        let cache = generate_spot_hashes(&manifest, tmp.path(), 4096).unwrap();
        let runtime_dir = tmp.path().join("runtimes").join("test-id");
        fs::create_dir_all(&runtime_dir).unwrap();
        cache.save(&runtime_dir).unwrap();
        unix_fs::symlink("runtimes/test-id", tmp.path().join("active")).unwrap();

        // Corrupt the image
        fs::write(
            images_dir.join(format!("{image_id}.raw")),
            b"corrupted data!",
        )
        .unwrap();

        // Verification should fail
        let result = verify_spot_hashes(&manifest, tmp.path(), 4096, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity check failed"));
    }

    #[test]
    fn test_verify_spot_hashes_no_cache() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(images_dir.join(format!("{image_id}.raw")), b"image data").unwrap();

        let manifest = make_manifest("test-id", "dev", "0.1.0");

        // Create active symlink but no spot hash cache
        let runtime_dir = tmp.path().join("runtimes").join("test-id");
        fs::create_dir_all(&runtime_dir).unwrap();
        unix_fs::symlink("runtimes/test-id", tmp.path().join("active")).unwrap();

        // Should fall back to full SHA256 and pass (no sha256 in manifest = skip)
        assert!(verify_spot_hashes(&manifest, tmp.path(), 4096, false).is_ok());

        // Should have generated the spot cache as a side effect
        assert!(SpotHashCache::load(&runtime_dir).is_some());
    }

    #[test]
    fn test_verify_no_cache_falls_back_to_full_sha256() {
        let tmp = TempDir::new().unwrap();
        let images_dir = tmp.path().join("images");
        fs::create_dir_all(&images_dir).unwrap();

        let image_data = b"test image content";
        let image_id = "a1b2c3d4-e5f6-5789-abcd-ef0123456789";
        fs::write(images_dir.join(format!("{image_id}.raw")), image_data).unwrap();

        // Manifest with WRONG sha256 — should fail the full validation
        let manifest =
            make_manifest_with_hash("test-id", "dev", "0.1.0", Some("badhash".to_string()));

        let runtime_dir = tmp.path().join("runtimes").join("test-id");
        fs::create_dir_all(&runtime_dir).unwrap();
        unix_fs::symlink("runtimes/test-id", tmp.path().join("active")).unwrap();

        // No spot cache, falls back to full SHA256, which should fail
        let result = verify_spot_hashes(&manifest, tmp.path(), 4096, false);
        assert!(result.is_err());
    }
}
