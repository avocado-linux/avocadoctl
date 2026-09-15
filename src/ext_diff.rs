//! Extension-set diffing for OTA narration.
//!
//! Computes the human-facing "what changed" between the previously-active runtime
//! manifest and the newly-activated one, keyed by extension name. This module is
//! pure (no IO): callers own *when* the manifests are captured. In particular the
//! old manifest must be read **before** the `active` symlink is repointed by
//! [`crate::staging::activate_runtime`], because afterwards `load_active` returns
//! the new one.

use crate::manifest::RuntimeManifest;
use std::collections::BTreeMap;

/// A single extension-level change between two runtime manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtChange {
    Added {
        name: String,
        version: String,
    },
    Updated {
        name: String,
        from: String,
        to: String,
    },
    Removed {
        name: String,
        version: String,
    },
    Unchanged {
        name: String,
        version: String,
    },
}

impl ExtChange {
    /// The extension name this change refers to.
    pub fn name(&self) -> &str {
        match self {
            ExtChange::Added { name, .. }
            | ExtChange::Updated { name, .. }
            | ExtChange::Removed { name, .. }
            | ExtChange::Unchanged { name, .. } => name,
        }
    }

    fn is_unchanged(&self) -> bool {
        matches!(self, ExtChange::Unchanged { .. })
    }
}

/// Diff two runtime manifests' extension sets (old → new).
///
/// `old` is `None` when there was no previously-active runtime (first provision).
/// Returns changes sorted by extension name for stable, readable output. Each
/// extension name appears at most once (a name is either present in the new set or
/// removed from the old set — never both), so the sort yields a clean alphabetical
/// list.
pub fn diff_extensions(old: Option<&RuntimeManifest>, new: &RuntimeManifest) -> Vec<ExtChange> {
    let old_map: BTreeMap<&str, &str> = old
        .map(|m| {
            m.extensions
                .iter()
                .map(|e| (e.name.as_str(), e.version.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let new_map: BTreeMap<&str, &str> = new
        .extensions
        .iter()
        .map(|e| (e.name.as_str(), e.version.as_str()))
        .collect();

    let mut changes = Vec::new();
    for (name, &new_ver) in &new_map {
        match old_map.get(name) {
            None => changes.push(ExtChange::Added {
                name: name.to_string(),
                version: new_ver.to_string(),
            }),
            Some(&old_ver) if old_ver != new_ver => changes.push(ExtChange::Updated {
                name: name.to_string(),
                from: old_ver.to_string(),
                to: new_ver.to_string(),
            }),
            Some(&old_ver) => changes.push(ExtChange::Unchanged {
                name: name.to_string(),
                version: old_ver.to_string(),
            }),
        }
    }
    for (name, &old_ver) in &old_map {
        if !new_map.contains_key(name) {
            changes.push(ExtChange::Removed {
                name: name.to_string(),
                version: old_ver.to_string(),
            });
        }
    }

    changes.sort_by(|a, b| a.name().cmp(b.name()));
    changes
}

/// Render the diff as human-facing lines for the OTA log.
///
/// Returns `None` when there is nothing worth showing — i.e. every entry is
/// `Unchanged`. This is the common case for a plain boot-time re-merge (no runtime
/// change), where callers should stay quiet rather than print a wall of
/// "= unchanged" on every boot.
///
/// When `first_install` is set (there was no previous active runtime), the compact
/// "Installed N extensions" phrasing is used instead of a long list of `+ added`.
pub fn render_diff(changes: &[ExtChange], first_install: bool) -> Option<Vec<String>> {
    if changes.is_empty() || changes.iter().all(ExtChange::is_unchanged) {
        return None;
    }

    if first_install {
        let count = changes.len();
        let noun = if count == 1 {
            "extension"
        } else {
            "extensions"
        };
        return Some(vec![format!("Installed {count} {noun}")]);
    }

    // Column-align names so versions line up.
    let name_width = changes.iter().map(|c| c.name().len()).max().unwrap_or(0);

    let mut lines = vec!["Extensions:".to_string()];
    for change in changes {
        lines.push(match change {
            ExtChange::Updated { name, from, to } => {
                format!("   ~ updated    {name:name_width$}  {from} → {to}")
            }
            ExtChange::Added { name, version } => {
                format!("   + added      {name:name_width$}  {version}")
            }
            ExtChange::Removed { name, version } => {
                format!("   - removed    {name:name_width$}  {version}")
            }
            ExtChange::Unchanged { name, version } => {
                format!("   = unchanged  {name:name_width$}  {version}")
            }
        });
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestExtension, RuntimeInfo, RuntimeManifest};

    /// Build a runtime manifest with the given `(name, version)` extensions.
    fn mk(exts: &[(&str, &str)]) -> RuntimeManifest {
        RuntimeManifest {
            manifest_version: 1,
            id: "test-runtime".to_string(),
            built_at: "2026-01-01T00:00:00Z".to_string(),
            runtime: RuntimeInfo {
                name: "test".to_string(),
                version: "1.0.0".to_string(),
            },
            extensions: exts
                .iter()
                .map(|(name, version)| ManifestExtension {
                    name: name.to_string(),
                    version: version.to_string(),
                    image_id: None,
                    image_type: None,
                    sha256: None,
                    root_hash: None,
                    enabled: true,
                })
                .collect(),
            os_bundle: None,
        }
    }

    #[test]
    fn added_updated_removed_unchanged() {
        let old = mk(&[
            ("microclaw", "0.1.56"),
            ("avocado-conn", "1.2.0"),
            ("legacy", "0.9.0"),
        ]);
        let new = mk(&[
            ("microclaw", "0.1.57"),   // updated
            ("avocado-conn", "1.2.0"), // unchanged
            ("avocado-rat", "0.3.0"),  // added
                                       // legacy removed
        ]);
        let changes = diff_extensions(Some(&old), &new);
        assert_eq!(
            changes,
            vec![
                ExtChange::Unchanged {
                    name: "avocado-conn".into(),
                    version: "1.2.0".into()
                },
                ExtChange::Added {
                    name: "avocado-rat".into(),
                    version: "0.3.0".into()
                },
                ExtChange::Removed {
                    name: "legacy".into(),
                    version: "0.9.0".into()
                },
                ExtChange::Updated {
                    name: "microclaw".into(),
                    from: "0.1.56".into(),
                    to: "0.1.57".into()
                },
            ]
        );
    }

    #[test]
    fn first_install_is_all_added() {
        let new = mk(&[("a", "1.0"), ("b", "2.0")]);
        let changes = diff_extensions(None, &new);
        assert!(changes.iter().all(|c| matches!(c, ExtChange::Added { .. })));
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn identical_sets_all_unchanged() {
        let m = mk(&[("a", "1.0"), ("b", "2.0")]);
        let changes = diff_extensions(Some(&m), &m);
        assert!(changes
            .iter()
            .all(|c| matches!(c, ExtChange::Unchanged { .. })));
    }

    #[test]
    fn version_only_change_is_update() {
        let old = mk(&[("a", "1.0")]);
        let new = mk(&[("a", "1.1")]);
        assert_eq!(
            diff_extensions(Some(&old), &new),
            vec![ExtChange::Updated {
                name: "a".into(),
                from: "1.0".into(),
                to: "1.1".into()
            }]
        );
    }

    #[test]
    fn disjoint_sets() {
        let old = mk(&[("a", "1.0")]);
        let new = mk(&[("b", "2.0")]);
        // Sorted by name: "a" (removed) before "b" (added).
        assert_eq!(
            diff_extensions(Some(&old), &new),
            vec![
                ExtChange::Removed {
                    name: "a".into(),
                    version: "1.0".into()
                },
                ExtChange::Added {
                    name: "b".into(),
                    version: "2.0".into()
                },
            ]
        );
    }

    #[test]
    fn ordering_is_stable_by_name() {
        let old = mk(&[("zeta", "1.0"), ("alpha", "1.0")]);
        let new = mk(&[("mid", "1.0"), ("alpha", "2.0")]);
        let changes = diff_extensions(Some(&old), &new);
        let names: Vec<&str> = changes.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn render_none_when_all_unchanged() {
        let m = mk(&[("a", "1.0")]);
        let changes = diff_extensions(Some(&m), &m);
        assert_eq!(render_diff(&changes, false), None);
    }

    #[test]
    fn render_none_when_empty() {
        assert_eq!(render_diff(&[], false), None);
        assert_eq!(render_diff(&[], true), None);
    }

    #[test]
    fn render_first_install_compact() {
        let new = mk(&[("a", "1.0"), ("b", "2.0")]);
        let changes = diff_extensions(None, &new);
        assert_eq!(
            render_diff(&changes, true),
            Some(vec!["Installed 2 extensions".to_string()])
        );
    }

    #[test]
    fn render_first_install_singular() {
        let new = mk(&[("only", "1.0")]);
        let changes = diff_extensions(None, &new);
        assert_eq!(
            render_diff(&changes, true),
            Some(vec!["Installed 1 extension".to_string()])
        );
    }

    #[test]
    fn render_diff_block_formats_all_variants() {
        let old = mk(&[("microclaw", "0.1.56"), ("legacy", "0.9.0")]);
        let new = mk(&[("microclaw", "0.1.57"), ("avocado-rat", "0.3.0")]);
        let changes = diff_extensions(Some(&old), &new);
        let lines = render_diff(&changes, false).expect("has changes");
        assert_eq!(lines[0], "Extensions:");
        // Names are column-aligned to the widest ("avocado-rat" = 11 chars).
        assert!(lines
            .iter()
            .any(|l| l.contains("+ added      avocado-rat  0.3.0")));
        assert!(lines
            .iter()
            .any(|l| l.contains("- removed    legacy       0.9.0")));
        assert!(lines
            .iter()
            .any(|l| l.contains("~ updated    microclaw    0.1.56 → 0.1.57")));
    }
}
