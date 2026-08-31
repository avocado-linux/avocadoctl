use std::collections::HashSet;
use std::fs;
use std::path::Path;

use crate::manifest::{RuntimeManifest, IMAGES_DIR_NAME};
use crate::staging::{self, StagingError};

/// Result of a garbage collection run.
#[derive(Debug, Clone, Default)]
pub struct GcResult {
    /// IDs of runtimes that were removed.
    pub removed_runtimes: Vec<String>,
    /// Filenames of images that were removed from the shared pool.
    pub removed_images: Vec<String>,
}

/// Run garbage collection: remove old runtimes and unreferenced images.
///
/// Keeps at most `retention` runtimes. The active runtime and any runtime
/// referenced by `pending-update.json` are always kept regardless of the limit.
pub fn collect_garbage(base_dir: &Path, retention: u32) -> Result<GcResult, StagingError> {
    // Same lock as `update::perform_update`. Between an update installing its
    // images and writing the runtime manifest, those images are referenced by
    // nothing on disk, and `cleanup_orphaned_images` would delete them from
    // under the pass. The agent calls GC right after an update returns, and a
    // duplicate "update available" nudge can have a second pass mid-flight by
    // then. Skip rather than wait: a stuck download must not pin this thread.
    let _lock = crate::update::acquire_update_lock(base_dir).map_err(|e| match e {
        crate::update::UpdateError::UpdateInProgress => StagingError::UpdateInProgress,
        other => StagingError::StagingFailed(other.to_string()),
    })?;
    let retention = retention.max(1) as usize;
    let mut result = GcResult::default();

    // 1. Load all runtimes, sorted: active first, then by built_at DESC.
    //    Checked: an active manifest that is present but unreadable would
    //    otherwise contribute no references, and cleanup_orphaned_images would
    //    delete every image only the running runtime uses.
    let runtimes = RuntimeManifest::list_all_checked(base_dir)
        .map_err(|e| StagingError::StagingFailed(format!("refusing to collect garbage: {e}")))?;
    if runtimes.len() <= retention {
        // Still clean up orphaned images even when no runtimes need removal
        result.removed_images = cleanup_orphaned_images(base_dir, &runtimes);
        return Ok(result);
    }

    // 2. Determine protected runtime IDs (always kept regardless of retention)
    let mut protected: HashSet<String> = HashSet::new();

    for (m, is_active) in &runtimes {
        if *is_active {
            protected.insert(m.id.clone());
        }
    }

    // Protect any runtime referenced by pending-update.json
    let pending_path = base_dir.join("pending-update.json");
    if let Some(pending) = crate::os_update::read_pending_update_from(&pending_path) {
        if let Some(ref rt_id) = pending.runtime_id {
            protected.insert(rt_id.clone());
        }
    }

    // 3. Build the keep set: iterate sorted list, add until retention reached
    //    list_all is sorted active-first then by built_at DESC, so this
    //    naturally keeps the active runtime and the newest inactive ones.
    let mut keep: HashSet<String> = HashSet::new();
    for (m, _) in &runtimes {
        if keep.len() >= retention {
            break;
        }
        keep.insert(m.id.clone());
    }
    // Ensure all protected runtimes are always in the keep set
    keep.extend(protected);

    // 4. Remove runtimes not in the keep set
    for (m, _) in &runtimes {
        if !keep.contains(&m.id) {
            match staging::remove_runtime(&m.id, base_dir) {
                Ok(()) => result.removed_runtimes.push(m.id.clone()),
                Err(StagingError::RemoveActiveRuntime) => {
                    // Shouldn't happen since we protect active, but skip gracefully
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // 5. Clean up orphaned images
    let surviving = RuntimeManifest::list_all_checked(base_dir)
        .map_err(|e| StagingError::StagingFailed(format!("refusing to collect garbage: {e}")))?;
    result.removed_images = cleanup_orphaned_images(base_dir, &surviving);

    Ok(result)
}

/// Collect all image filenames referenced by the given runtimes, then scan
/// the images directory and delete any files not in that set.
/// Returns the list of deleted filenames.
fn cleanup_orphaned_images(base_dir: &Path, runtimes: &[(RuntimeManifest, bool)]) -> Vec<String> {
    let mut referenced: HashSet<String> = HashSet::new();

    for (m, _) in runtimes {
        for ext in &m.extensions {
            // resolve_path gives us the full path; we only need the filename
            let path = ext.resolve_path(base_dir);
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                referenced.insert(filename.to_string());
            }
            // The dm-verity hash tree is a sibling file, not something
            // resolve_path returns. Without this it reads as an orphan and gets
            // collected, leaving an image that declares a root hash whose tree
            // is gone -- which now refuses to mount rather than mounting
            // unverified, so losing it takes the extension down.
            if let Some(vpath) = ext.resolve_verity_path(base_dir) {
                if let Some(filename) = vpath.file_name().and_then(|n| n.to_str()) {
                    referenced.insert(filename.to_string());
                }
            }
        }
        if let Some(ref os_bundle) = m.os_bundle {
            referenced.insert(format!("{}.raw", os_bundle.image_id));
        }
    }

    let images_dir = base_dir.join(IMAGES_DIR_NAME);
    let mut removed = Vec::new();

    if let Ok(entries) = fs::read_dir(&images_dir) {
        for entry in entries.flatten() {
            let filename = entry.file_name().to_string_lossy().to_string();
            if !referenced.contains(&filename) && fs::remove_file(entry.path()).is_ok() {
                removed.push(filename);
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unreadable_active_manifest_stops_gc_instead_of_orphaning_its_images() {
        // The data-loss path from issue #23: list_all silently drops a manifest
        // that does not parse, so the running runtime contributes no references
        // and every image only it uses is collected. auto_gc is on by default.
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        let runtimes = base.join("runtimes");
        let live = runtimes.join("live-id");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(base.join("images")).unwrap();
        fs::write(base.join("images").join("img-1.raw"), b"payload").unwrap();
        fs::write(live.join("manifest.json"), b"{ this is not json").unwrap();
        std::os::unix::fs::symlink("runtimes/live-id", base.join("active")).unwrap();

        let err = collect_garbage(base, 3).unwrap_err();
        assert!(
            err.to_string().contains("refusing to collect garbage"),
            "{err}"
        );
        assert!(
            base.join("images").join("img-1.raw").exists(),
            "an image was deleted while the active runtime's references were unknown"
        );
    }

    #[test]
    fn a_dangling_active_link_stops_gc() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path();
        fs::create_dir_all(base.join("runtimes")).unwrap();
        fs::create_dir_all(base.join("images")).unwrap();
        fs::write(base.join("images").join("img-1.raw"), b"payload").unwrap();
        // The link is there; its target is not.
        std::os::unix::fs::symlink("runtimes/gone-id", base.join("active")).unwrap();

        let err = collect_garbage(base, 3).unwrap_err();
        assert!(
            err.to_string().contains("refusing to collect garbage"),
            "{err}"
        );
        assert!(base.join("images").join("img-1.raw").exists());
    }

    use super::*;
    use crate::manifest::{ManifestExtension, OsBundleRef, RuntimeInfo, RuntimeManifest};
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn make_manifest(id: &str, built_at: &str, image_id: &str) -> RuntimeManifest {
        RuntimeManifest {
            manifest_version: 1,
            id: id.to_string(),
            built_at: built_at.to_string(),
            runtime: RuntimeInfo {
                name: "dev".to_string(),
                version: "0.1.0".to_string(),
            },
            extensions: vec![ManifestExtension {
                name: "app".to_string(),
                version: "0.1.0".to_string(),
                image_id: Some(image_id.to_string()),
                image_type: None,
                sha256: None,
                root_hash: None,
                enabled: true,
            }],
            os_bundle: None,
        }
    }

    fn write_manifest(base_dir: &Path, manifest: &RuntimeManifest) {
        let dir = base_dir.join("runtimes").join(&manifest.id);
        fs::create_dir_all(&dir).unwrap();
        let json = serde_json::to_string_pretty(manifest).unwrap();
        fs::write(dir.join("manifest.json"), json).unwrap();
    }

    fn write_image(base_dir: &Path, filename: &str) {
        let images_dir = base_dir.join("images");
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(images_dir.join(filename), b"image data").unwrap();
    }

    fn set_active(base_dir: &Path, id: &str) {
        let link = base_dir.join("active");
        let _ = fs::remove_file(&link);
        unix_fs::symlink(format!("runtimes/{id}"), &link).unwrap();
    }

    fn write_pending_update(base_dir: &Path, runtime_id: &str) {
        let pending = serde_json::json!({
            "os_build_id": "test-os",
            "verify": null,
            "rollback": null,
            "previous_slot": "a",
            "runtime_id": runtime_id
        });
        fs::write(
            base_dir.join("pending-update.json"),
            serde_json::to_string_pretty(&pending).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn gc_keeps_the_verity_sidecar_of_a_referenced_image() {
        // The hash tree is a sibling file that resolve_path never returns, so
        // it looks like an orphan. Collecting it would leave an image that
        // declares a root hash with no tree -- which now refuses to mount.
        let tmp = TempDir::new().unwrap();
        let mut m = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        m.extensions[0].root_hash = Some("beef".to_string());
        write_manifest(tmp.path(), &m);
        write_image(tmp.path(), "img-1.raw");
        write_image(tmp.path(), "img-1.verity");
        write_image(tmp.path(), "orphan.verity");

        let removed = cleanup_orphaned_images(tmp.path(), &[(m, true)]);

        assert!(
            !removed.contains(&"img-1.verity".to_string()),
            "the referenced image's hash tree must survive GC, removed: {removed:?}"
        );
        assert!(
            tmp.path()
                .join(IMAGES_DIR_NAME)
                .join("img-1.verity")
                .exists(),
            "hash tree should still be on disk"
        );
        assert!(
            removed.contains(&"orphan.verity".to_string()),
            "an unreferenced hash tree is still garbage, removed: {removed:?}"
        );
    }

    #[test]
    fn test_gc_skipped_while_update_holds_the_lock() {
        let tmp = TempDir::new().unwrap();
        let held = crate::update::acquire_update_lock(tmp.path()).unwrap();
        assert!(matches!(
            collect_garbage(tmp.path(), 3),
            Err(StagingError::UpdateInProgress)
        ));
        drop(held);
        collect_garbage(tmp.path(), 3).unwrap();
    }

    #[test]
    fn test_gc_noop_when_under_retention() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-2");

        let result = collect_garbage(tmp.path(), 3).unwrap();
        assert!(result.removed_runtimes.is_empty());
        // Both runtimes should still exist
        assert!(tmp.path().join("runtimes/rt-1").exists());
        assert!(tmp.path().join("runtimes/rt-2").exists());
    }

    #[test]
    fn test_gc_removes_oldest_runtimes() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        let m3 = make_manifest("rt-3", "2026-01-03T00:00:00Z", "img-3");
        let m4 = make_manifest("rt-4", "2026-01-04T00:00:00Z", "img-4");
        let m5 = make_manifest("rt-5", "2026-01-05T00:00:00Z", "img-5");

        for m in [&m1, &m2, &m3, &m4, &m5] {
            write_manifest(tmp.path(), m);
        }
        set_active(tmp.path(), "rt-5");

        let result = collect_garbage(tmp.path(), 3).unwrap();
        assert_eq!(result.removed_runtimes.len(), 2);
        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));
        assert!(result.removed_runtimes.contains(&"rt-2".to_string()));

        // Kept: rt-5 (active), rt-4, rt-3
        assert!(tmp.path().join("runtimes/rt-5").exists());
        assert!(tmp.path().join("runtimes/rt-4").exists());
        assert!(tmp.path().join("runtimes/rt-3").exists());
        assert!(!tmp.path().join("runtimes/rt-1").exists());
        assert!(!tmp.path().join("runtimes/rt-2").exists());
    }

    #[test]
    fn test_gc_protects_active_runtime() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-1"); // Active is the oldest

        let result = collect_garbage(tmp.path(), 1).unwrap();
        // rt-1 is active so it's protected; rt-2 is newest so it fills retention=1
        // But active must be kept, so both may be kept depending on algorithm
        // retention=1 means keep 1 runtime. Active-first sort means rt-1 fills the slot.
        // rt-2 is not protected and exceeds retention.
        assert_eq!(result.removed_runtimes.len(), 1);
        assert!(result.removed_runtimes.contains(&"rt-2".to_string()));
        assert!(tmp.path().join("runtimes/rt-1").exists());
    }

    #[test]
    fn test_gc_protects_pending_update_runtime() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        let m3 = make_manifest("rt-3", "2026-01-03T00:00:00Z", "img-3");
        let m4 = make_manifest("rt-4", "2026-01-04T00:00:00Z", "img-4");

        for m in [&m1, &m2, &m3, &m4] {
            write_manifest(tmp.path(), m);
        }
        set_active(tmp.path(), "rt-4");
        write_pending_update(tmp.path(), "rt-1"); // Oldest is pending

        // retention=2: keep rt-4 (active, newest), rt-3 (2nd newest)
        // But rt-1 is protected by pending update, so it's also kept
        let result = collect_garbage(tmp.path(), 2).unwrap();
        assert_eq!(result.removed_runtimes.len(), 1);
        assert!(result.removed_runtimes.contains(&"rt-2".to_string()));

        assert!(tmp.path().join("runtimes/rt-4").exists());
        assert!(tmp.path().join("runtimes/rt-3").exists());
        assert!(tmp.path().join("runtimes/rt-1").exists()); // Protected
        assert!(!tmp.path().join("runtimes/rt-2").exists());
    }

    #[test]
    fn test_gc_cleans_orphaned_images() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        let m3 = make_manifest("rt-3", "2026-01-03T00:00:00Z", "img-shared");

        for m in [&m1, &m2, &m3] {
            write_manifest(tmp.path(), m);
        }
        set_active(tmp.path(), "rt-3");

        // Write images: img-1 only used by rt-1, img-2 only by rt-2, img-shared by rt-3
        write_image(tmp.path(), "img-1.raw");
        write_image(tmp.path(), "img-2.raw");
        write_image(tmp.path(), "img-shared.raw");
        write_image(tmp.path(), "orphan.raw"); // Not referenced by any runtime

        // retention=2: keep rt-3 (active), rt-2
        let result = collect_garbage(tmp.path(), 2).unwrap();

        assert_eq!(result.removed_runtimes.len(), 1);
        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));

        // img-1.raw and orphan.raw should be removed (rt-1 removed, orphan unreferenced)
        assert!(result.removed_images.contains(&"img-1.raw".to_string()));
        assert!(result.removed_images.contains(&"orphan.raw".to_string()));
        // img-2.raw and img-shared.raw should remain
        assert!(tmp.path().join("images/img-2.raw").exists());
        assert!(tmp.path().join("images/img-shared.raw").exists());
        assert!(!tmp.path().join("images/img-1.raw").exists());
        assert!(!tmp.path().join("images/orphan.raw").exists());
    }

    #[test]
    fn test_gc_keeps_shared_images() {
        let tmp = TempDir::new().unwrap();
        // Both runtimes reference the same image
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "shared-img");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "shared-img");
        let m3 = make_manifest("rt-3", "2026-01-03T00:00:00Z", "img-3");

        for m in [&m1, &m2, &m3] {
            write_manifest(tmp.path(), m);
        }
        set_active(tmp.path(), "rt-3");
        write_image(tmp.path(), "shared-img.raw");
        write_image(tmp.path(), "img-3.raw");

        // retention=2: keep rt-3, rt-2; remove rt-1
        let result = collect_garbage(tmp.path(), 2).unwrap();

        assert_eq!(result.removed_runtimes.len(), 1);
        // shared-img.raw is still referenced by rt-2, so it should NOT be deleted
        assert!(tmp.path().join("images/shared-img.raw").exists());
        assert!(result.removed_images.is_empty());
    }

    #[test]
    fn test_gc_retention_zero_clamps_to_one() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-2");

        let result = collect_garbage(tmp.path(), 0).unwrap();
        // retention clamped to 1, keep only rt-2 (active + newest)
        assert_eq!(result.removed_runtimes.len(), 1);
        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));
        assert!(tmp.path().join("runtimes/rt-2").exists());
    }

    #[test]
    fn test_gc_handles_no_active_symlink() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        let m3 = make_manifest("rt-3", "2026-01-03T00:00:00Z", "img-3");

        for m in [&m1, &m2, &m3] {
            write_manifest(tmp.path(), m);
        }
        // No active symlink

        let result = collect_garbage(tmp.path(), 2).unwrap();
        // Sorted by built_at DESC: rt-3, rt-2, rt-1. Keep rt-3, rt-2.
        assert_eq!(result.removed_runtimes.len(), 1);
        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));
    }

    #[test]
    fn test_gc_handles_os_bundle_images() {
        let tmp = TempDir::new().unwrap();
        let mut m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        m1.os_bundle = Some(OsBundleRef {
            image_id: "os-img-1".to_string(),
            sha256: "abc".to_string(),
            os_build_id: None,
            initramfs_build_id: None,
        });
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");

        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-2");

        write_image(tmp.path(), "img-1.raw");
        write_image(tmp.path(), "img-2.raw");
        write_image(tmp.path(), "os-img-1.raw");

        // retention=1: keep only rt-2 (active); remove rt-1
        let result = collect_garbage(tmp.path(), 1).unwrap();

        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));
        // os-img-1.raw and img-1.raw should be removed
        assert!(result.removed_images.contains(&"os-img-1.raw".to_string()));
        assert!(result.removed_images.contains(&"img-1.raw".to_string()));
        // img-2.raw should remain
        assert!(tmp.path().join("images/img-2.raw").exists());
    }

    #[test]
    fn test_gc_cleans_metadata_with_runtime() {
        let tmp = TempDir::new().unwrap();
        let m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");
        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-2");

        // Write metadata for rt-1
        let rt1_dir = tmp.path().join("runtimes/rt-1");
        let mut meta = crate::metadata::RuntimeMetadata::new();
        meta.entries
            .insert("env".to_string(), "staging".to_string());
        meta.save(&rt1_dir).unwrap();
        assert!(rt1_dir.join("metadata.json").exists());

        let result = collect_garbage(tmp.path(), 1).unwrap();
        assert!(result.removed_runtimes.contains(&"rt-1".to_string()));
        // metadata.json should be gone along with the runtime directory
        assert!(!rt1_dir.exists());
        assert!(!rt1_dir.join("metadata.json").exists());
    }

    #[test]
    fn test_gc_handles_kab_images() {
        let tmp = TempDir::new().unwrap();
        let mut m1 = make_manifest("rt-1", "2026-01-01T00:00:00Z", "img-1");
        m1.extensions[0].image_type = Some("kab".to_string());
        let m2 = make_manifest("rt-2", "2026-01-02T00:00:00Z", "img-2");

        write_manifest(tmp.path(), &m1);
        write_manifest(tmp.path(), &m2);
        set_active(tmp.path(), "rt-2");

        write_image(tmp.path(), "img-1.kab");
        write_image(tmp.path(), "img-2.raw");

        let result = collect_garbage(tmp.path(), 1).unwrap();
        assert!(result.removed_images.contains(&"img-1.kab".to_string()));
        assert!(tmp.path().join("images/img-2.raw").exists());
    }
}
