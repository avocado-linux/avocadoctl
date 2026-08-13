use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_AVOCADO_DIR: &str = "/var/lib/avocado";
pub const ACTIVE_LINK_NAME: &str = "active";
pub const RUNTIMES_DIR_NAME: &str = "runtimes";
pub const MANIFEST_FILENAME: &str = "manifest.json";
pub const IMAGES_DIR_NAME: &str = "images";
pub const METADATA_FILENAME: &str = "metadata.json";

/// Fixed namespace UUID for generating content-addressable image IDs.
/// Must match the constant used in avocado-cli.
pub const AVOCADO_IMAGE_NAMESPACE: uuid::Uuid = uuid::uuid!("7488fa35-6390-425b-bbbf-b156cfe1eed2");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsBundleRef {
    pub image_id: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs_build_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeManifest {
    pub manifest_version: u32,
    pub id: String,
    pub built_at: String,
    pub runtime: RuntimeInfo,
    pub extensions: Vec<ManifestExtension>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_bundle: Option<OsBundleRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestExtension {
    pub name: String,
    pub version: String,
    /// UUIDv5 content-addressable image identifier
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<String>,
    /// Image type: absent/null = raw squashfs/erofs, "kab" = KAB-wrapped image
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_type: Option<String>,
    /// SHA256 hash of the extension image for integrity verification
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Build-time default activation state. Absent/true = auto-activated
    /// at refresh; false = present-but-inactive (user must opt in via
    /// `avocadoctl ext enable`). Skipped from JSON when true to keep
    /// existing manifests serializing byte-identical.
    #[serde(
        default = "ManifestExtension::default_enabled",
        skip_serializing_if = "ManifestExtension::is_default_enabled"
    )]
    pub enabled: bool,
}

impl ManifestExtension {
    fn default_enabled() -> bool {
        true
    }

    fn is_default_enabled(v: &bool) -> bool {
        *v
    }

    /// Returns true if this extension image is a KAB file.
    pub fn is_kab(&self) -> bool {
        self.image_type.as_deref() == Some("kab")
    }

    /// Resolve the on-disk path for this extension image.
    pub fn resolve_path(&self, base_dir: &Path) -> PathBuf {
        let ext = if self.is_kab() { "kab" } else { "raw" };
        if let Some(ref id) = self.image_id {
            base_dir.join(IMAGES_DIR_NAME).join(format!("{id}.{ext}"))
        } else {
            base_dir
                .join(IMAGES_DIR_NAME)
                .join(format!("{}-{}.{ext}", self.name, self.version))
        }
    }
}

impl RuntimeManifest {
    /// Resolve the on-disk path for the OS bundle image, if present.
    pub fn resolve_os_bundle_path(&self, base_dir: &Path) -> Option<PathBuf> {
        self.os_bundle.as_ref().map(|b| {
            base_dir
                .join(IMAGES_DIR_NAME)
                .join(format!("{}.raw", b.image_id))
        })
    }

    /// Load a manifest from a specific directory containing manifest.json.
    pub fn load_from(dir: &Path) -> Option<Self> {
        let manifest_path = dir.join(MANIFEST_FILENAME);
        let content = fs::read_to_string(&manifest_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Load the active runtime manifest by following the `active` symlink.
    /// Returns None if no active symlink or manifest file exists.
    pub fn load_active(base_dir: &Path) -> Option<Self> {
        let active_path = base_dir.join(ACTIVE_LINK_NAME);
        if !active_path.exists() {
            return None;
        }
        Self::load_from(&active_path)
    }

    /// Resolve the UUID directory name that the `active` symlink points to.
    fn resolve_active_id(base_dir: &Path) -> Option<String> {
        let active_path = base_dir.join(ACTIVE_LINK_NAME);
        let target = fs::read_link(&active_path).ok()?;
        // target is relative like "runtimes/<uuid>" -- extract the last component
        target
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    }

    /// List all available runtime manifests.
    /// Returns each manifest paired with a bool indicating whether it is the active runtime.
    /// Sorted by (name ASC, version ASC, built_at DESC).
    pub fn list_all(base_dir: &Path) -> Vec<(Self, bool)> {
        let active_id = Self::resolve_active_id(base_dir);
        let runtimes_dir = base_dir.join(RUNTIMES_DIR_NAME);

        let mut results: Vec<(Self, bool)> = Vec::new();

        let entries = match fs::read_dir(&runtimes_dir) {
            Ok(entries) => entries,
            Err(_) => return results,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(manifest) = Self::load_from(&path) {
                let dir_name = entry.file_name().to_str().unwrap_or_default().to_string();
                let is_active = active_id.as_deref() == Some(&dir_name);
                results.push((manifest, is_active));
            }
        }

        results.sort_by(|(a, a_active), (b, b_active)| {
            // Active runtime always first, then newest-built first
            b_active
                .cmp(a_active)
                .then_with(|| b.built_at.cmp(&a.built_at))
        });

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs as unix_fs;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, manifest: &RuntimeManifest) {
        fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string_pretty(manifest).unwrap();
        fs::write(dir.join(MANIFEST_FILENAME), json).unwrap();
    }

    fn make_manifest(id: &str, name: &str, version: &str, built_at: &str) -> RuntimeManifest {
        RuntimeManifest {
            manifest_version: 1,
            id: id.to_string(),
            built_at: built_at.to_string(),
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
    fn test_load_from() {
        let tmp = TempDir::new().unwrap();
        let rt_dir = tmp.path().join("runtimes").join("abc-123");
        let manifest = make_manifest("abc-123", "dev", "0.1.0", "2026-02-18T15:00:00Z");
        write_manifest(&rt_dir, &manifest);

        let loaded = RuntimeManifest::load_from(&rt_dir).unwrap();
        assert_eq!(loaded.id, "abc-123");
        assert_eq!(loaded.runtime.name, "dev");
        assert_eq!(loaded.runtime.version, "0.1.0");
        assert_eq!(loaded.extensions.len(), 1);
    }

    #[test]
    fn test_load_from_missing() {
        let tmp = TempDir::new().unwrap();
        assert!(RuntimeManifest::load_from(tmp.path()).is_none());
    }

    #[test]
    fn test_load_active() {
        let tmp = TempDir::new().unwrap();
        let rt_dir = tmp.path().join("runtimes").join("uuid-1");
        let manifest = make_manifest("uuid-1", "dev", "0.1.0", "2026-02-18T15:00:00Z");
        write_manifest(&rt_dir, &manifest);

        unix_fs::symlink("runtimes/uuid-1", tmp.path().join("active")).unwrap();

        let loaded = RuntimeManifest::load_active(tmp.path()).unwrap();
        assert_eq!(loaded.id, "uuid-1");
    }

    #[test]
    fn test_load_active_missing_symlink() {
        let tmp = TempDir::new().unwrap();
        assert!(RuntimeManifest::load_active(tmp.path()).is_none());
    }

    #[test]
    fn test_list_all_sorted() {
        let tmp = TempDir::new().unwrap();
        let runtimes = tmp.path().join("runtimes");

        let m1 = make_manifest("aaa", "dev", "0.1.0", "2026-02-17T10:00:00Z");
        let m2 = make_manifest("bbb", "dev", "0.1.0", "2026-02-18T15:00:00Z");
        let m3 = make_manifest("ccc", "dev", "0.2.0", "2026-02-16T09:00:00Z");

        write_manifest(&runtimes.join("aaa"), &m1);
        write_manifest(&runtimes.join("bbb"), &m2);
        write_manifest(&runtimes.join("ccc"), &m3);

        unix_fs::symlink("runtimes/bbb", tmp.path().join("active")).unwrap();

        let list = RuntimeManifest::list_all(tmp.path());
        assert_eq!(list.len(), 3);

        // Active first, then newest-built first
        assert_eq!(list[0].0.id, "bbb");
        assert!(list[0].1); // active
        assert_eq!(list[1].0.id, "aaa"); // 2026-02-17
        assert!(!list[1].1);
        assert_eq!(list[2].0.id, "ccc"); // 2026-02-16
        assert!(!list[2].1);
    }

    #[test]
    fn test_list_all_no_runtimes_dir() {
        let tmp = TempDir::new().unwrap();
        let list = RuntimeManifest::list_all(tmp.path());
        assert!(list.is_empty());
    }

    #[test]
    fn test_manifest_serialization_roundtrip() {
        let manifest = make_manifest("test-id", "prod", "1.2.3", "2026-01-01T00:00:00Z");
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: RuntimeManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-id");
        assert_eq!(parsed.runtime.name, "prod");
        assert_eq!(parsed.runtime.version, "1.2.3");
        assert_eq!(parsed.built_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn test_manifest_deserialize_image_id() {
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-02-19T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.2.0" },
            "extensions": [
                { "name": "app", "version": "0.1.0", "image_id": "a1b2c3d4-e5f6-5789-abcd-ef0123456789" }
            ]
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.manifest_version, 1);
        let ext = &parsed.extensions[0];
        assert_eq!(
            ext.image_id,
            Some("a1b2c3d4-e5f6-5789-abcd-ef0123456789".to_string())
        );
    }

    #[test]
    fn test_manifest_serialization_contains_image_id() {
        let manifest = make_manifest("test-id", "dev", "0.2.0", "2026-02-19T00:00:00Z");
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            json.contains("image_id"),
            "manifest should contain image_id"
        );
    }

    #[test]
    fn test_resolve_path_image_id() {
        let ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: Some("a1b2c3d4-e5f6-5789-abcd-ef0123456789".to_string()),
            image_type: None,
            sha256: None,
            enabled: true,
        };
        let base = Path::new("/var/lib/avocado");
        let path = ext.resolve_path(base);
        assert_eq!(
            path,
            Path::new("/var/lib/avocado/images/a1b2c3d4-e5f6-5789-abcd-ef0123456789.raw")
        );
    }

    #[test]
    fn test_os_bundle_deserialization() {
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-03-01T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": [],
            "os_bundle": {
                "image_id": "deadbeef-1234-5678-abcd-000000000000",
                "sha256": "abcdef1234567890"
            }
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        let bundle = parsed.os_bundle.unwrap();
        assert_eq!(bundle.image_id, "deadbeef-1234-5678-abcd-000000000000");
        assert_eq!(bundle.sha256, "abcdef1234567890");
    }

    #[test]
    fn test_os_bundle_optional() {
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-03-01T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": []
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        assert!(parsed.os_bundle.is_none());
    }

    #[test]
    fn test_resolve_os_bundle_path() {
        let mut manifest = make_manifest("test", "dev", "0.1.0", "2026-03-01T00:00:00Z");
        assert!(manifest
            .resolve_os_bundle_path(Path::new("/var/lib/avocado"))
            .is_none());

        manifest.os_bundle = Some(OsBundleRef {
            image_id: "deadbeef-1234-5678-abcd-000000000000".to_string(),
            sha256: "abc".to_string(),
            os_build_id: None,
            initramfs_build_id: None,
        });
        assert_eq!(
            manifest
                .resolve_os_bundle_path(Path::new("/var/lib/avocado"))
                .unwrap(),
            Path::new("/var/lib/avocado/images/deadbeef-1234-5678-abcd-000000000000.raw")
        );
    }

    #[test]
    fn test_os_bundle_with_initramfs_build_id() {
        let json = r#"{
            "manifest_version": 2,
            "id": "test-id",
            "built_at": "2026-03-01T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": [],
            "os_bundle": {
                "image_id": "deadbeef-1234-5678-abcd-000000000000",
                "sha256": "abcdef1234567890",
                "os_build_id": "rootfs-abc",
                "initramfs_build_id": "initramfs-def"
            }
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        let bundle = parsed.os_bundle.unwrap();
        assert_eq!(bundle.os_build_id, Some("rootfs-abc".to_string()));
        assert_eq!(bundle.initramfs_build_id, Some("initramfs-def".to_string()));
    }

    #[test]
    fn test_os_bundle_without_initramfs_build_id() {
        // Backward compatibility
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-03-01T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.1.0" },
            "extensions": [],
            "os_bundle": {
                "image_id": "deadbeef-1234-5678-abcd-000000000000",
                "sha256": "abcdef1234567890",
                "os_build_id": "rootfs-abc"
            }
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        let bundle = parsed.os_bundle.unwrap();
        assert_eq!(bundle.os_build_id, Some("rootfs-abc".to_string()));
        assert!(bundle.initramfs_build_id.is_none());
    }

    #[test]
    fn test_resolve_path_fallback_without_image_id() {
        let ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: None,
            image_type: None,
            sha256: None,
            enabled: true,
        };
        let base = Path::new("/var/lib/avocado");
        let path = ext.resolve_path(base);
        assert_eq!(path, Path::new("/var/lib/avocado/images/app-0.1.0.raw"));
    }

    #[test]
    fn test_is_kab() {
        let raw_ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: None,
            image_type: None,
            sha256: None,
            enabled: true,
        };
        assert!(!raw_ext.is_kab());

        let kab_ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: None,
            image_type: Some("kab".to_string()),
            sha256: None,
            enabled: true,
        };
        assert!(kab_ext.is_kab());
    }

    #[test]
    fn test_resolve_path_kab_with_image_id() {
        let ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: Some("a1b2c3d4-e5f6-5789-abcd-ef0123456789".to_string()),
            image_type: Some("kab".to_string()),
            sha256: None,
            enabled: true,
        };
        let base = Path::new("/var/lib/avocado");
        let path = ext.resolve_path(base);
        assert_eq!(
            path,
            Path::new("/var/lib/avocado/images/a1b2c3d4-e5f6-5789-abcd-ef0123456789.kab")
        );
    }

    #[test]
    fn test_resolve_path_kab_without_image_id() {
        let ext = ManifestExtension {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            image_id: None,
            image_type: Some("kab".to_string()),
            sha256: None,
            enabled: true,
        };
        let base = Path::new("/var/lib/avocado");
        let path = ext.resolve_path(base);
        assert_eq!(path, Path::new("/var/lib/avocado/images/app-0.1.0.kab"));
    }

    #[test]
    fn test_manifest_deserialize_image_type() {
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-02-19T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.2.0" },
            "extensions": [
                { "name": "app", "version": "0.1.0", "image_id": "a1b2c3d4-e5f6-5789-abcd-ef0123456789", "image_type": "kab" }
            ]
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        let ext = &parsed.extensions[0];
        assert!(ext.is_kab());
        assert_eq!(ext.image_type, Some("kab".to_string()));
    }

    #[test]
    fn test_manifest_deserialize_missing_image_type() {
        let json = r#"{
            "manifest_version": 1,
            "id": "test-id",
            "built_at": "2026-02-19T00:00:00Z",
            "runtime": { "name": "dev", "version": "0.2.0" },
            "extensions": [
                { "name": "app", "version": "0.1.0", "image_id": "a1b2c3d4-e5f6-5789-abcd-ef0123456789" }
            ]
        }"#;
        let parsed: RuntimeManifest = serde_json::from_str(json).unwrap();
        let ext = &parsed.extensions[0];
        assert!(!ext.is_kab());
        assert_eq!(ext.image_type, None);
    }

    #[test]
    fn test_manifest_serialization_skips_none_image_type() {
        let manifest = make_manifest("test-id", "dev", "0.2.0", "2026-02-19T00:00:00Z");
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            !json.contains("image_type"),
            "manifest should not contain image_type when None"
        );
    }
}
