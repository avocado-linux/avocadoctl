//! Service lifecycle for extensions across an update.
//!
//! An extension declares the units it owns with `enable_services`, which
//! avocado-cli writes into its release file as `AVOCADO_ENABLE_SERVICES`. At boot
//! nothing here runs: `avocado-ensure-extensions.service` reloads the manager and
//! retriggers the activation targets, so enabled units start in boot order. After
//! an update, though, a refresh swaps the extensions under running services and
//! nothing would stop, restart or start them. This module does that, and only
//! there: the update path is the one place that knows both the previous and the
//! new runtime, and it runs on a booted system, so waiting on a unit is safe.

use crate::manifest::{ManifestExtension, RuntimeManifest};
use crate::output::OutputManager;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// How long one `systemctl` call may take before it is reported as stuck.
const UNIT_TIMEOUT: Duration = Duration::from_secs(60);

/// An extension that is merged, with the identity used to tell whether it
/// changed and the units it declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedServices {
    identity: String,
    units: Vec<String>,
}

/// What to do with units across one update.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Units of removed extensions, stopped before the unmerge.
    pub stop: Vec<String>,
    /// Units of changed extensions, restarted after the merge.
    pub restart: Vec<String>,
    /// Units of added extensions, started after the merge.
    pub start: Vec<String>,
}

/// The release directories of the merged hierarchy.
pub fn merged_release_dirs() -> Vec<PathBuf> {
    if let Ok(root) = std::env::var("AVOCADO_MERGED_ROOT") {
        let root = Path::new(&root);
        return vec![
            root.join("usr/lib/extension-release.d"),
            root.join("etc/extension-release.d"),
        ];
    }
    vec![
        PathBuf::from("/usr/lib/extension-release.d"),
        PathBuf::from("/etc/extension-release.d"),
    ]
}

/// A rebuild with the same version is still a change, so the content address
/// wins when there is one.
fn identity(ext: &ManifestExtension) -> String {
    match &ext.image_id {
        Some(id) => format!("image:{id}"),
        None => format!("version:{}", ext.version),
    }
}

/// Whether `file` is the release file for exactly this extension and version:
/// `extension-release.<name>-<version>`, optionally with avocadoctl's `NN-`
/// ordering prefix (two digits at least: the 101st extension is `100-`). Release files of older versions that an image still carries
/// are not it.
fn is_release_file_for(file: &str, name: &str, version: Option<&str>) -> bool {
    let Some(rest) = file.strip_prefix("extension-release.") else {
        return false;
    };
    let want = match version {
        Some(v) => format!("{name}-{v}"),
        None => name.to_string(),
    };
    if rest == want {
        return true;
    }
    match rest.split_once('-') {
        Some((nn, tail)) => nn.len() >= 2 && nn.bytes().all(|b| b.is_ascii_digit()) && tail == want,
        None => false,
    }
}

/// Unit names as systemctl reads them: a name with no unit type is a service.
/// Keeps `nginx` and `nginx.service` from counting as two units.
fn normalize_unit(unit: &str) -> String {
    const TYPES: &[&str] = &[
        ".service",
        ".socket",
        ".target",
        ".device",
        ".mount",
        ".automount",
        ".swap",
        ".timer",
        ".path",
        ".slice",
        ".scope",
    ];
    if TYPES.iter().any(|t| unit.ends_with(t)) {
        unit.to_string()
    } else {
        format!("{unit}.service")
    }
}

/// The merged extensions of `manifest` that declare units, read from the
/// release files in `dirs`. An extension with no matching release file is not
/// merged (disabled, or the merge skipped it) and is left out. `hitl` names
/// HITL-mounted extensions, which replace the manifest's extension of the same
/// name with their own version.
pub fn merged_services(
    manifest: &RuntimeManifest,
    dirs: &[PathBuf],
    hitl: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, MergedServices> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for dir in dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                files.push((e.file_name().to_string_lossy().into_owned(), e.path()));
            }
        }
    }

    // What the merge takes: every HITL mount, in place of the manifest's
    // extension of the same name or on its own, and the manifest's others.
    let hitl_id = |v: &Option<String>| format!("hitl:{}", v.as_deref().unwrap_or("-"));
    let mut candidates: Vec<(&str, Option<&str>, String)> = hitl
        .iter()
        .map(|(name, v)| (name.as_str(), v.as_deref(), hitl_id(v)))
        .collect();
    for ext in &manifest.extensions {
        if !hitl.contains_key(&ext.name) {
            candidates.push((&ext.name, Some(&ext.version), identity(ext)));
        }
    }

    let mut out = BTreeMap::new();
    for (name, version, id) in candidates {
        let mut found = false;
        let mut units: Vec<String> = Vec::new();
        for (file, path) in &files {
            if !is_release_file_for(file, name, version) {
                continue;
            }
            found = true;
            if let Ok(content) = std::fs::read_to_string(path) {
                for unit in crate::commands::ext::parse_avocado_enable_services(&content) {
                    let unit = normalize_unit(&unit);
                    if !units.contains(&unit) {
                        units.push(unit);
                    }
                }
            }
        }
        if found {
            out.insert(
                name.to_string(),
                MergedServices {
                    identity: id,
                    units,
                },
            );
        }
    }
    out
}

/// What to do after the merge, comparing what was merged before the update
/// with what is merged now. `stopped` is what already ran before the unmerge.
///
/// The merge has the last word, not the manifest: it can skip an extension the
/// manifest keeps, or fall back to another runtime, so the stop pass before the
/// unmerge was a prediction. Here, any unit no longer declared by anything is
/// stopped (a skipped extension, a unit a changed extension dropped), and any
/// unit stopped earlier that is declared again is started (a fallback that
/// brought its extension back).
pub fn plan(
    before: &BTreeMap<String, MergedServices>,
    after: &BTreeMap<String, MergedServices>,
    stopped: &[String],
) -> Plan {
    let mut plan = Plan::default();
    let declared_after: Vec<&String> = after.values().flat_map(|m| &m.units).collect();
    for (name, old) in before {
        for unit in &old.units {
            if !declared_after.contains(&unit) && !stopped.contains(unit) {
                plan.stop.push(unit.clone());
            }
        }
        if let Some(new) = after.get(name) {
            if new.identity != old.identity {
                plan.restart
                    .extend(new.units.iter().filter(|u| !stopped.contains(u)).cloned());
            }
        }
    }
    for (name, new) in after {
        let added = !before.contains_key(name);
        for unit in &new.units {
            if (added || stopped.contains(unit)) && !plan.start.contains(unit) {
                plan.start.push(unit.clone());
            }
        }
    }
    plan
}

/// Units to stop before the unmerge: those of extensions merged now that the
/// new runtime will not merge, because it drops them or an override disables
/// them. Their unit files are still present, so the stop runs cleanly.
pub fn stop_before_unmerge(
    before: &BTreeMap<String, MergedServices>,
    new: Option<&RuntimeManifest>,
    overrides: &crate::overrides::RuntimeOverrides,
    hitl: &BTreeMap<String, Option<String>>,
) -> Vec<String> {
    // A HITL mount is merged whatever the manifest says.
    let kept = |name: &str| {
        hitl.contains_key(name)
            || new.is_some_and(|m| {
                m.extensions
                    .iter()
                    .any(|e| e.name == name && crate::overrides::effective_enabled(e, overrides))
            })
    };
    before
        .iter()
        .filter(|(name, _)| !kept(name))
        .flat_map(|(_, m)| m.units.iter().cloned())
        .collect()
}

fn systemctl() -> &'static str {
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        "mock-systemctl"
    } else {
        "systemctl"
    }
}

/// avocadoctl's own units. Stopping or restarting them from inside the daemon
/// would kill the refresh doing it, so they are never acted on here.
const OWN_UNITS: &[&str] = &["avocadoctl", "avocadoctl.service", "avocadoctl.socket"];

/// Split `units` into those to act on and avocadoctl's own, which are skipped.
fn without_own_units(units: &[String]) -> (Vec<String>, Vec<String>) {
    units
        .iter()
        .cloned()
        .partition(|u| !OWN_UNITS.contains(&u.as_str()))
}

/// Run `systemctl <verb>` on all of `units` in one call, so systemd queues them
/// as one transaction and orders the jobs by their `After=`/`Before=`, and wait
/// up to [`UNIT_TIMEOUT`] for it. A failure is reported on the operator channel
/// and does not fail the update: the extensions are merged either way.
pub fn run(verb: &str, units: &[String], output: &OutputManager) {
    let (units, own) = without_own_units(units);
    for unit in &own {
        output.log_info(&format!(
            "Service {verb}: skipping {unit}, avocadoctl does not act on its own unit"
        ));
    }
    if units.is_empty() {
        return;
    }
    let list = units.join(", ");
    match run_one(verb, &units) {
        Ok(()) => output.log_info(&format!("Service {verb}: {list}")),
        Err(why) => output.log_error(&format!("Service {verb} ({list}) {why}")),
    }
}

fn run_one(verb: &str, units: &[String]) -> Result<(), String> {
    let mut child = Command::new(systemctl())
        .arg(verb)
        .args(units)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run systemctl: {e}"))?;
    // Drained on its own thread so a full pipe can never stall systemctl.
    let stderr = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut err = String::new();
            let _ = std::io::Read::read_to_string(&mut s, &mut err);
            err
        })
    });
    let deadline = Instant::now() + UNIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // The jobs stay queued in systemd; only the wait ends here.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "did not finish within {}s; its jobs are still queued",
                    UNIT_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("could not wait for systemctl: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let err = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    Err(format!("failed ({status}): {}", err.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ext(name: &str, version: &str, image_id: Option<&str>) -> ManifestExtension {
        serde_json::from_value(serde_json::json!({
            "name": name, "version": version, "image_id": image_id,
        }))
        .unwrap()
    }

    fn manifest(exts: Vec<ManifestExtension>) -> RuntimeManifest {
        let mut m: RuntimeManifest = serde_json::from_str(
            r#"{"manifest_version":1,"id":"r","runtime":{"name":"dev","version":"1"},"built_at":"2026-01-01T00:00:00Z","extensions":[]}"#,
        )
        .unwrap();
        m.extensions = exts;
        m
    }

    fn merged(entries: &[(&str, &str, &[&str])]) -> BTreeMap<String, MergedServices> {
        entries
            .iter()
            .map(|(name, id, units)| {
                (
                    name.to_string(),
                    MergedServices {
                        identity: id.to_string(),
                        units: units.iter().map(|u| u.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn only_the_exact_release_file_counts() {
        assert!(is_release_file_for(
            "extension-release.app-1.2",
            "app",
            Some("1.2")
        ));
        assert!(is_release_file_for(
            "extension-release.07-app-1.2",
            "app",
            Some("1.2")
        ));
        assert!(is_release_file_for(
            "extension-release.100-app-1.2",
            "app",
            Some("1.2")
        ));
        assert!(!is_release_file_for(
            "extension-release.7-app-1.2",
            "app",
            Some("1.2")
        ));
        // An older version the image still carries is not this extension.
        assert!(!is_release_file_for(
            "extension-release.app-1.1",
            "app",
            Some("1.2")
        ));
        assert!(!is_release_file_for(
            "extension-release.07-app-1.1",
            "app",
            Some("1.2")
        ));
        // Nor is another extension whose name ends the same way.
        assert!(!is_release_file_for(
            "extension-release.my-app-1.2",
            "app",
            Some("1.2")
        ));
        assert!(!is_release_file_for(
            "extension-release.app-1.2.1",
            "app",
            Some("1.2")
        ));
    }

    #[test]
    fn services_come_from_the_merged_release_files() {
        let tmp = TempDir::new().unwrap();
        let usr = tmp.path().join("usr/lib/extension-release.d");
        let etc = tmp.path().join("etc/extension-release.d");
        std::fs::create_dir_all(&usr).unwrap();
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(
            usr.join("extension-release.01-app-2"),
            "ID=_any\nAVOCADO_ENABLE_SERVICES=\"app.service\"\n",
        )
        .unwrap();
        // A stale file from an older build names a unit the extension dropped.
        std::fs::write(
            usr.join("extension-release.app-1"),
            "AVOCADO_ENABLE_SERVICES=\"old.service\"\n",
        )
        .unwrap();
        std::fs::write(
            etc.join("extension-release.01-app-2"),
            "AVOCADO_ENABLE_SERVICES=\"app.service app.socket\"\n",
        )
        .unwrap();
        std::fs::write(usr.join("extension-release.02-lib-1"), "ID=_any\n").unwrap();

        let m = manifest(vec![
            ext("app", "2", Some("img-a")),
            ext("lib", "1", None),
            ext("off", "1", None),
        ]);
        let got = merged_services(&m, &[usr, etc], &BTreeMap::new());

        assert_eq!(got["app"].units, vec!["app.service", "app.socket"]);
        assert_eq!(got["app"].identity, "image:img-a");
        assert!(got["lib"].units.is_empty(), "merged, no services declared");
        assert!(!got.contains_key("off"), "no release file: not merged");
    }

    #[test]
    fn the_plan_stops_removed_restarts_changed_and_starts_added() {
        let before = merged(&[
            ("gone", "image:1", &["gone.service"]),
            ("same", "image:2", &["same.service"]),
            ("bumped", "image:3", &["bumped.service"]),
        ]);
        let after = merged(&[
            ("same", "image:2", &["same.service"]),
            ("bumped", "image:4", &["bumped.service", "bumped.timer"]),
            ("new", "image:5", &["new.service"]),
        ]);
        assert_eq!(
            plan(&before, &after, &[]),
            Plan {
                stop: vec!["gone.service".into()],
                restart: vec!["bumped.service".into(), "bumped.timer".into()],
                start: vec!["new.service".into()],
            }
        );
    }

    #[test]
    fn a_unit_a_changed_extension_drops_is_stopped() {
        let before = merged(&[("app", "image:1", &["app.service", "old.service"])]);
        let after = merged(&[("app", "image:2", &["app.service"])]);
        let p = plan(&before, &after, &[]);
        assert_eq!(p.stop, vec!["old.service"]);
        assert_eq!(p.restart, vec!["app.service"]);
    }

    #[test]
    fn an_extension_the_merge_skipped_has_its_units_stopped_after() {
        // Kept by the new manifest, so not stopped before the unmerge, but
        // the merge left it out.
        let before = merged(&[("app", "image:1", &["app.service"])]);
        assert_eq!(plan(&before, &merged(&[]), &[]).stop, vec!["app.service"]);
    }

    #[test]
    fn a_unit_stopped_before_a_fallback_merge_is_started_again() {
        // The new runtime dropped `app`, so it was stopped before the unmerge,
        // but the merge fell back to a runtime that still has it.
        let before = merged(&[("app", "image:1", &["app.service"])]);
        let after = merged(&[("app", "image:1", &["app.service"])]);
        let stopped = vec!["app.service".to_string()];
        let p = plan(&before, &after, &stopped);
        assert_eq!(p.start, vec!["app.service"]);
        assert!(p.stop.is_empty() && p.restart.is_empty());
        // Stopped already and gone for good: not stopped twice.
        assert!(plan(&before, &merged(&[]), &stopped).stop.is_empty());
    }

    #[test]
    fn units_of_dropped_or_disabled_extensions_stop_before_the_unmerge() {
        let before = merged(&[
            ("kept", "image:1", &["kept.service"]),
            ("dropped", "image:2", &["dropped.service"]),
            ("disabled", "image:3", &["disabled.service"]),
        ]);
        let new = manifest(vec![
            ext("kept", "1", Some("1")),
            ext("disabled", "1", Some("3")),
        ]);
        let overrides: crate::overrides::RuntimeOverrides = serde_json::from_value(
            serde_json::json!({"extensions": {"disabled": {"enabled": false}}}),
        )
        .unwrap();
        assert_eq!(
            stop_before_unmerge(&before, Some(&new), &overrides, &BTreeMap::new()),
            vec!["disabled.service", "dropped.service"]
        );
        assert_eq!(
            stop_before_unmerge(&before, Some(&new), &Default::default(), &BTreeMap::new()),
            vec!["dropped.service"]
        );
    }

    #[test]
    fn a_short_unit_name_is_a_service() {
        assert_eq!(normalize_unit("nginx"), "nginx.service");
        assert_eq!(normalize_unit("nginx.service"), "nginx.service");
        assert_eq!(normalize_unit("sshd.socket"), "sshd.socket");
        assert_eq!(normalize_unit("wpa@wlan0"), "wpa@wlan0.service");
    }

    #[test]
    fn a_hitl_extension_is_read_at_its_own_version() {
        let tmp = TempDir::new().unwrap();
        let etc = tmp.path().join("etc/extension-release.d");
        std::fs::create_dir_all(&etc).unwrap();
        // The HITL mount carries version 1; the manifest has moved to 2.
        std::fs::write(
            etc.join("extension-release.app-1"),
            "AVOCADO_ENABLE_SERVICES=\"app\"\n",
        )
        .unwrap();
        let m = manifest(vec![ext("app", "2", Some("img-2"))]);
        let hitl = BTreeMap::from([("app".to_string(), Some("1".to_string()))]);

        let got = merged_services(&m, std::slice::from_ref(&etc), &hitl);
        assert_eq!(got["app"].units, vec!["app.service"]);
        assert_eq!(got["app"].identity, "hitl:1");
        assert!(merged_services(&m, std::slice::from_ref(&etc), &BTreeMap::new()).is_empty());

        // A HITL mount the manifest does not list is merged all the same.
        let none = manifest(vec![]);
        let got = merged_services(&none, &[etc], &hitl);
        assert_eq!(got["app"].units, vec!["app.service"]);
    }

    #[test]
    fn a_hitl_extension_is_never_stopped_before_the_unmerge() {
        let before = merged(&[("app", "hitl:1", &["app.service"])]);
        let dropped_it = manifest(vec![]);
        let hitl = BTreeMap::from([("app".to_string(), Some("1".to_string()))]);
        assert!(
            stop_before_unmerge(&before, Some(&dropped_it), &Default::default(), &hitl).is_empty()
        );
    }

    #[test]
    fn avocadoctl_never_acts_on_its_own_units() {
        let units: Vec<String> = ["app.service", "avocadoctl.service", "avocadoctl.socket"]
            .iter()
            .map(|u| u.to_string())
            .collect();
        let (act, own) = without_own_units(&units);
        assert_eq!(act, vec!["app.service"]);
        assert_eq!(own, vec!["avocadoctl.service", "avocadoctl.socket"]);
    }

    #[test]
    fn a_rebuild_with_the_same_version_is_a_change() {
        let a = manifest(vec![ext("app", "1", Some("img-old"))]);
        let b = manifest(vec![ext("app", "1", Some("img-new"))]);
        assert_ne!(identity(&a.extensions[0]), identity(&b.extensions[0]));
        let c = manifest(vec![ext("app", "1", None)]);
        let d = manifest(vec![ext("app", "2", None)]);
        assert_ne!(identity(&c.extensions[0]), identity(&d.extensions[0]));
    }
}
