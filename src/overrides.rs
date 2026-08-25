//! Per-runtime user-applied overrides on top of the manifest's defaults.
//!
//! The manifest is content-addressed (and AMF-signed for kos runtimes), so
//! we can't mutate it in place when a user runs `avocadoctl ext enable
//! <name>`. Instead, those mutations live in a sibling `overrides.json`
//! file next to the manifest. The activation pipeline always consults the
//! manifest *and* the overrides via [`effective_enabled`] — that helper is
//! the single source of truth for "should this extension be active?"
//! across the codebase.

use crate::manifest::ManifestExtension;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const OVERRIDES_FILENAME: &str = "overrides.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeOverrides {
    /// Schema version. Bumped only on non-additive changes; new optional
    /// fields can be added without bumping.
    #[serde(default = "RuntimeOverrides::default_version")]
    pub version: u32,
    /// Per-extension overrides keyed by extension name.
    #[serde(default)]
    pub extensions: HashMap<String, ExtensionOverride>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtensionOverride {
    /// `Some(true)` forces enabled regardless of manifest default;
    /// `Some(false)` forces disabled; `None` means "no override, use
    /// manifest's value". Stored as Option so a future user-level
    /// "I haven't touched this" state is distinguishable from a deliberate
    /// flip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl RuntimeOverrides {
    fn default_version() -> u32 {
        1
    }

    /// Path of the overrides file inside a runtime directory.
    pub fn path(runtime_dir: &Path) -> PathBuf {
        runtime_dir.join(OVERRIDES_FILENAME)
    }

    /// Load overrides from `<runtime_dir>/overrides.json`. Returns an
    /// empty `RuntimeOverrides` (no overrides applied) if the file is
    /// missing or unparseable — never an error, so the activation
    /// pipeline can keep going on a corrupt file.
    pub fn load(runtime_dir: &Path) -> Self {
        let path = Self::path(runtime_dir);
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist the current overrides to
    /// `<runtime_dir>/overrides.json`. Writes to `<file>.tmp` and renames
    /// so a SIGKILL mid-write leaves the previous file intact.
    pub fn save(&self, runtime_dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(runtime_dir)?;
        let path = Self::path(runtime_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Look up the active override for `name`. Returns `None` when the
    /// user hasn't expressed a preference (manifest default applies).
    pub fn enabled_override(&self, name: &str) -> Option<bool> {
        self.extensions.get(name).and_then(|e| e.enabled)
    }

    /// Set or clear the enabled override for `name`. Pass `Some(true)` /
    /// `Some(false)` to force, `None` to clear — clearing removes the
    /// extension entry entirely when it has no other fields, keeping the
    /// JSON tidy.
    pub fn set_enabled(&mut self, name: &str, enabled: Option<bool>) {
        if enabled.is_none() {
            self.extensions.remove(name);
            return;
        }
        self.extensions.entry(name.to_string()).or_default().enabled = enabled;
    }
}

/// The single point of truth for "should avocadoctl activate this
/// extension right now?". Combines the manifest's build-time default
/// with any user override stored alongside the runtime. All activation
/// gates — scan-time, list-time, ext enable/disable status — must go
/// through this function rather than reading the fields directly, so
/// the policy stays consistent if it ever grows beyond a single bool.
pub fn effective_enabled(ext: &ManifestExtension, overrides: &RuntimeOverrides) -> bool {
    overrides.enabled_override(&ext.name).unwrap_or(ext.enabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manifest_ext(name: &str, enabled: bool) -> ManifestExtension {
        ManifestExtension {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            image_id: None,
            image_type: None,
            sha256: None,
            root_hash: None,
            enabled,
        }
    }

    #[test]
    fn missing_file_yields_empty() {
        let tmp = TempDir::new().unwrap();
        let o = RuntimeOverrides::load(tmp.path());
        assert!(o.extensions.is_empty());
    }

    #[test]
    fn corrupt_file_yields_empty() {
        let tmp = TempDir::new().unwrap();
        fs::write(RuntimeOverrides::path(tmp.path()), "{ not json").unwrap();
        let o = RuntimeOverrides::load(tmp.path());
        assert!(o.extensions.is_empty());
    }

    #[test]
    fn roundtrip_set_and_save() {
        let tmp = TempDir::new().unwrap();
        let mut o = RuntimeOverrides::default();
        o.set_enabled("microclaw", Some(true));
        o.set_enabled("ext-x", Some(false));
        o.save(tmp.path()).unwrap();
        let reloaded = RuntimeOverrides::load(tmp.path());
        assert_eq!(reloaded.enabled_override("microclaw"), Some(true));
        assert_eq!(reloaded.enabled_override("ext-x"), Some(false));
        assert_eq!(reloaded.enabled_override("never-set"), None);
    }

    #[test]
    fn clearing_removes_entry() {
        let mut o = RuntimeOverrides::default();
        o.set_enabled("ext", Some(false));
        o.set_enabled("ext", None);
        assert!(o.extensions.is_empty());
    }

    #[test]
    fn effective_enabled_uses_manifest_when_no_override() {
        let o = RuntimeOverrides::default();
        assert!(effective_enabled(&manifest_ext("a", true), &o));
        assert!(!effective_enabled(&manifest_ext("b", false), &o));
    }

    #[test]
    fn effective_enabled_override_wins() {
        let mut o = RuntimeOverrides::default();
        o.set_enabled("a", Some(false));
        assert!(!effective_enabled(&manifest_ext("a", true), &o));
        o.set_enabled("b", Some(true));
        assert!(effective_enabled(&manifest_ext("b", false), &o));
    }
}
