use crate::config::Config;
use crate::manifest::{RuntimeManifest, IMAGES_DIR_NAME};
use crate::output::OutputManager;
use crate::update::UpdateOutcome;
use crate::{staging, update};
use clap::{Arg, ArgGroup, ArgMatches, Command};
use std::path::Path;

pub fn create_command() -> Command {
    Command::new("runtime")
        .about("Manage runtimes")
        .subcommand(Command::new("list").about("List available runtimes"))
        .subcommand(
            Command::new("add")
                .about("Add a runtime from a TUF repository or local manifest")
                .arg(
                    Arg::new("url")
                        .long("url")
                        .help("URL of a TUF update repository"),
                )
                .arg(
                    Arg::new("manifest")
                        .long("manifest")
                        .help("Path to a local manifest.json file"),
                )
                .group(
                    ArgGroup::new("source")
                        .args(["url", "manifest"])
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("remove").about("Remove a staged runtime").arg(
                Arg::new("id")
                    .required(true)
                    .help("Runtime build ID (full or prefix)"),
            ),
        )
        .subcommand(
            Command::new("activate")
                .about("Activate a staged runtime")
                .arg(
                    Arg::new("id")
                        .required(true)
                        .help("Runtime build ID (full or prefix)"),
                ),
        )
        .subcommand(
            Command::new("inspect")
                .about("Inspect a runtime's details and extensions")
                .arg(
                    Arg::new("id").required(false).help(
                        "Runtime build ID (full or prefix). Omit to inspect the active runtime",
                    ),
                ),
        )
        .subcommand(Command::new("gc").about("Remove old runtimes and unreferenced images"))
        .subcommand(
            Command::new("metadata")
                .about("Manage runtime metadata key-value pairs")
                .subcommand(
                    Command::new("set")
                        .about("Set a metadata key-value pair")
                        .arg(
                            Arg::new("id")
                                .required(true)
                                .help("Runtime build ID (full or prefix)"),
                        )
                        .arg(Arg::new("key").required(true).help("Metadata key"))
                        .arg(Arg::new("value").required(true).help("Metadata value")),
                )
                .subcommand(
                    Command::new("get")
                        .about("Get a metadata value by key")
                        .arg(
                            Arg::new("id")
                                .required(true)
                                .help("Runtime build ID (full or prefix)"),
                        )
                        .arg(Arg::new("key").required(true).help("Metadata key")),
                )
                .subcommand(
                    Command::new("list")
                        .about("List all metadata for a runtime")
                        .arg(
                            Arg::new("id")
                                .required(true)
                                .help("Runtime build ID (full or prefix)"),
                        ),
                )
                .subcommand(
                    Command::new("delete")
                        .about("Delete a metadata key")
                        .arg(
                            Arg::new("id")
                                .required(true)
                                .help("Runtime build ID (full or prefix)"),
                        )
                        .arg(Arg::new("key").required(true).help("Metadata key")),
                ),
        )
}

pub fn handle_command(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    match matches.subcommand() {
        Some(("list", _)) => {
            list_runtimes(config, output);
        }
        Some(("add", add_matches)) => {
            handle_add(add_matches, config, output);
        }
        Some(("remove", remove_matches)) => {
            handle_remove(remove_matches, config, output);
        }
        Some(("activate", activate_matches)) => {
            handle_activate(activate_matches, config, output);
        }
        Some(("inspect", inspect_matches)) => {
            handle_inspect(inspect_matches, config, output);
        }
        Some(("gc", _)) => {
            handle_gc(config, output);
        }
        Some(("metadata", meta_matches)) => {
            handle_metadata(meta_matches, config, output);
        }
        _ => {
            println!("Use 'runtime list' to see available runtimes.");
            println!("Run 'avocadoctl runtime --help' for more information.");
        }
    }
}

fn handle_add(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    if let Some(url) = matches.get_one::<String>("url") {
        println!();
        println!("  Adding runtime from {url}");
        println!();

        let auth_token = std::env::var("AVOCADO_TUF_AUTH_TOKEN").ok();
        match update::perform_update(
            url,
            base_path,
            auth_token.as_deref(),
            None,
            config.stream_os_to_partition(),
            output.is_verbose(),
            config.get_spot_check_bytes(),
        ) {
            Ok(UpdateOutcome::RebootRequired) => {
                println!();
                output.step(
                    "Runtime Add",
                    "OS update applied. Rebooting to activate new OS...",
                );
                let _ = std::process::Command::new("reboot").status();
            }
            // Nothing changed, so don't refresh extensions.
            Ok(UpdateOutcome::AlreadyCurrent) => {
                println!();
                output.success("Runtime Add", "Runtime already at target version.");
            }
            Ok(UpdateOutcome::Activated) => {
                crate::commands::ext::refresh_extensions(config, output);
                println!();
                output.success("Runtime Add", "Runtime added successfully.");
            }
            Err(e) => {
                println!();
                output.error("Runtime Add", &format!("{e}"));
                std::process::exit(1);
            }
        }
    } else if let Some(manifest_path) = matches.get_one::<String>("manifest") {
        println!();
        println!("  Adding runtime from manifest: {manifest_path}");
        println!();

        let manifest_content = match std::fs::read_to_string(manifest_path) {
            Ok(c) => c,
            Err(e) => {
                output.error("Runtime Add", &format!("Failed to read manifest: {e}"));
                std::process::exit(1);
            }
        };

        let manifest: RuntimeManifest = match serde_json::from_str(&manifest_content) {
            Ok(m) => m,
            Err(e) => {
                output.error("Runtime Add", &format!("Invalid manifest.json: {e}"));
                std::process::exit(1);
            }
        };

        if let Err(e) = staging::validate_manifest_images(&manifest, base_path) {
            output.error("Runtime Add", &format!("{e}"));
            std::process::exit(1);
        }

        if let Err(e) =
            staging::stage_manifest(&manifest, &manifest_content, base_path, output.is_verbose())
        {
            output.error("Runtime Add", &format!("{e}"));
            std::process::exit(1);
        }

        // Best-effort spot hash cache generation
        if let Ok(cache) =
            staging::generate_spot_hashes(&manifest, base_path, config.get_spot_check_bytes())
        {
            let runtime_dir = base_path.join("runtimes").join(&manifest.id);
            let _ = cache.save(&runtime_dir);
        }

        if let Err(e) = staging::activate_runtime(&manifest.id, base_path) {
            output.error("Runtime Add", &format!("{e}"));
            std::process::exit(1);
        }

        let short_id = &manifest.id[..8.min(manifest.id.len())];
        println!(
            "  Activated runtime: {} {} ({short_id})",
            manifest.runtime.name, manifest.runtime.version,
        );

        crate::commands::ext::refresh_extensions(config, output);
        println!();
        output.success("Runtime Add", "Runtime added successfully.");
    }
}

fn handle_remove(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id_prefix = matches.get_one::<String>("id").expect("id is required");
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let runtimes = RuntimeManifest::list_all(base_path);
    let (matched, _is_active) = match resolve_runtime_id(id_prefix, &runtimes, output) {
        Some(m) => m,
        None => return,
    };

    if let Err(e) = staging::remove_runtime(&matched.id, base_path) {
        output.error("Runtime Remove", &format!("{e}"));
        std::process::exit(1);
    }

    let short_id = &matched.id[..8.min(matched.id.len())];
    println!();
    output.success(
        "Runtime Remove",
        &format!(
            "Removed runtime: {} {} ({short_id})",
            matched.runtime.name, matched.runtime.version,
        ),
    );
}

fn handle_activate(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id_prefix = matches.get_one::<String>("id").expect("id is required");
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let runtimes = RuntimeManifest::list_all(base_path);
    let (matched, is_active) = match resolve_runtime_id(id_prefix, &runtimes, output) {
        Some(m) => m,
        None => return,
    };

    let short_id = &matched.id[..8.min(matched.id.len())];

    if is_active {
        output.info(
            "Runtime Activate",
            &format!(
                "Runtime {} {} ({short_id}) is already active.",
                matched.runtime.name, matched.runtime.version,
            ),
        );
        return;
    }

    // Check if the target runtime requires a different OS
    if let Some(ref os_bundle) = matched.os_bundle {
        if let Some(ref expected_id) = os_bundle.os_build_id {
            let already_matches =
                crate::os_update::verify_os_release(&crate::os_update::VerifyConfig {
                    verify_type: "os-release".to_string(),
                    field: "AVOCADO_OS_BUILD_ID".to_string(),
                    expected: expected_id.clone(),
                })
                .unwrap_or(false);

            if !already_matches {
                // OS change required — apply update, mark pending, reboot
                let aos_path = base_path
                    .join(IMAGES_DIR_NAME)
                    .join(format!("{}.raw", os_bundle.image_id));

                if !aos_path.exists() {
                    output.error(
                        "Runtime Activate",
                        &format!("OS bundle image not found: {}", aos_path.display()),
                    );
                    std::process::exit(1);
                }

                output.step(
                    "Runtime Activate",
                    &format!(
                        "OS change required (target AVOCADO_OS_BUILD_ID={})",
                        expected_id
                    ),
                );

                if let Err(e) = crate::os_update::apply_os_update(&aos_path, base_path, false) {
                    output.error("Runtime Activate", &format!("OS update failed: {e}"));
                    std::process::exit(1);
                }

                if let Err(e) = crate::os_update::set_pending_runtime_id(&matched.id, base_path) {
                    output.error(
                        "Runtime Activate",
                        &format!("Failed to set pending runtime: {e}"),
                    );
                    std::process::exit(1);
                }

                output.step(
                    "Runtime Activate",
                    "OS update applied. Rebooting to activate new OS...",
                );
                let _ = std::process::Command::new("reboot").status();
                return;
            }
        }
    }

    // Pre-flight: verify target runtime's images before tearing down current extensions
    let runtime_dir = base_path.join("runtimes").join(&matched.id);
    if let Err(e) = staging::verify_runtime_integrity(
        matched,
        base_path,
        &runtime_dir,
        config.get_spot_check_bytes(),
        output.is_verbose(),
    ) {
        output.error("Runtime Activate", &format!("{e}"));
        std::process::exit(1);
    }

    // No OS change needed — activate immediately and refresh
    if let Err(e) = staging::activate_runtime(&matched.id, base_path) {
        output.error("Runtime Activate", &format!("{e}"));
        std::process::exit(1);
    }

    println!(
        "  Activated runtime: {} {} ({short_id})",
        matched.runtime.name, matched.runtime.version,
    );

    crate::commands::ext::refresh_extensions(config, output);
    println!();
    output.success(
        "Runtime Activate",
        &format!(
            "Switched to runtime: {} {} ({short_id})",
            matched.runtime.name, matched.runtime.version,
        ),
    );
}

fn handle_inspect(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let runtimes = RuntimeManifest::list_all(base_path);

    let (matched, is_active) = if let Some(id_prefix) = matches.get_one::<String>("id") {
        match resolve_runtime_id(id_prefix, &runtimes, output) {
            Some(m) => m,
            None => return,
        }
    } else {
        match runtimes.iter().find(|(_, active)| *active) {
            Some((m, _)) => (m, true),
            None => {
                output.error("Runtime Inspect", "No active runtime found.");
                std::process::exit(1);
            }
        }
    };

    if output.is_json() {
        match serde_json::to_string_pretty(matched) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Runtime Inspect", &format!("Failed to serialize: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    let short_id = if matched.id.len() >= 8 {
        &matched.id[..8]
    } else {
        &matched.id
    };

    let active_marker = if is_active { " (active)" } else { "" };

    println!();
    println!(
        "  Runtime: {} {} ({short_id}){active_marker}",
        matched.runtime.name, matched.runtime.version
    );
    println!("  Build ID: {}", matched.id);
    println!("  Built:    {}", matched.built_at);
    println!("  Manifest: v{}", matched.manifest_version);
    println!();

    if matched.extensions.is_empty() {
        println!("  No extensions.");
    } else {
        let name_width = matched
            .extensions
            .iter()
            .map(|e| e.name.len())
            .max()
            .unwrap_or(4)
            .max(4); // at least as wide as "NAME"

        println!(
            "  {:<nw$} {:<12} {:<10} SHA256",
            "NAME",
            "VERSION",
            "IMAGE ID",
            nw = name_width
        );

        for ext in &matched.extensions {
            let short_image_id = match &ext.image_id {
                Some(id) if id.len() >= 8 => &id[..8],
                Some(id) => id.as_str(),
                None => "-",
            };
            let short_sha = match &ext.sha256 {
                Some(h) if h.len() >= 12 => &h[..12],
                Some(h) => h.as_str(),
                None => "-",
            };
            println!(
                "  {:<nw$} {:<12} {:<10} {short_sha}",
                ext.name,
                ext.version,
                short_image_id,
                nw = name_width
            );
        }
    }

    println!();

    if let Some(ref os_bundle) = matched.os_bundle {
        println!("  OS Bundle:");
        println!("    Image ID:          {}", os_bundle.image_id);
        println!("    SHA256:            {}", os_bundle.sha256);
        if let Some(ref id) = os_bundle.os_build_id {
            println!("    OS Build ID:       {id}");
        }
        if let Some(ref id) = os_bundle.initramfs_build_id {
            println!("    Initramfs Build ID: {id}");
        }
        println!();
    }

    if output.is_verbose() {
        println!("  Full image IDs:");
        for ext in &matched.extensions {
            let id_display = ext.image_id.as_deref().unwrap_or("-");
            let sha_display = ext.sha256.as_deref().unwrap_or("-");
            println!(
                "    {} {}: {} sha256:{}",
                ext.name, ext.version, id_display, sha_display
            );
        }
        println!();
    }
}

/// Resolve a runtime ID prefix to a unique runtime from the list.
/// Returns the matched runtime manifest and its active status, or None on error.
fn resolve_runtime_id<'a>(
    id_prefix: &str,
    runtimes: &'a [(RuntimeManifest, bool)],
    output: &OutputManager,
) -> Option<(&'a RuntimeManifest, bool)> {
    let matches: Vec<&(RuntimeManifest, bool)> = runtimes
        .iter()
        .filter(|(m, _)| m.id.starts_with(id_prefix))
        .collect();

    match matches.len() {
        0 => {
            output.error(
                "Runtime",
                &format!("No runtime found with ID starting with '{id_prefix}'."),
            );
            std::process::exit(1);
        }
        1 => Some((&matches[0].0, matches[0].1)),
        _ => {
            let ids: Vec<String> = matches
                .iter()
                .map(|(m, active)| {
                    let marker = if *active { " (active)" } else { "" };
                    let sid = &m.id[..8.min(m.id.len())];
                    format!(
                        "  {} {} ({sid}){}",
                        m.runtime.name, m.runtime.version, marker
                    )
                })
                .collect();
            output.error(
                "Runtime",
                &format!(
                    "Ambiguous runtime ID '{id_prefix}', matches:\n{}",
                    ids.join("\n")
                ),
            );
            std::process::exit(1);
        }
    }
}

fn handle_gc(config: &Config, output: &OutputManager) {
    match crate::service::runtime::garbage_collect(config) {
        Ok(result) => {
            if output.is_json() {
                let json = serde_json::json!({
                    "removed_runtimes": result.removed_runtimes,
                    "removed_images": result.removed_images,
                });
                println!("{}", serde_json::to_string_pretty(&json).unwrap());
                return;
            }
            if result.removed_runtimes.is_empty() && result.removed_images.is_empty() {
                output.info("Runtime GC", "Nothing to clean up.");
            } else {
                for id in &result.removed_runtimes {
                    let short_id = &id[..8.min(id.len())];
                    println!("  Removed runtime: {short_id}");
                }
                for img in &result.removed_images {
                    println!("  Removed image: {img}");
                }
                println!();
                output.success(
                    "Runtime GC",
                    &format!(
                        "Removed {} runtime(s), {} image(s)",
                        result.removed_runtimes.len(),
                        result.removed_images.len(),
                    ),
                );
            }
        }
        Err(e) => {
            output.error("Runtime GC", &format!("{e}"));
            std::process::exit(1);
        }
    }
}

fn handle_metadata(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    match matches.subcommand() {
        Some(("set", set_matches)) => handle_metadata_set(set_matches, config, output),
        Some(("get", get_matches)) => handle_metadata_get(get_matches, config, output),
        Some(("list", list_matches)) => handle_metadata_list(list_matches, config, output),
        Some(("delete", del_matches)) => handle_metadata_delete(del_matches, config, output),
        _ => {
            println!("Use 'avocadoctl runtime metadata --help' for available commands.");
        }
    }
}

fn handle_metadata_set(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id = matches.get_one::<String>("id").expect("id is required");
    let key = matches.get_one::<String>("key").expect("key is required");
    let value = matches
        .get_one::<String>("value")
        .expect("value is required");

    match crate::service::runtime::metadata_set(id, key, value, config) {
        Ok(()) => {
            if output.is_json() {
                println!("{{\"status\":\"ok\"}}");
            } else {
                output.success("Metadata Set", &format!("Set '{key}' on runtime {id}"));
            }
        }
        Err(e) => {
            output.error("Metadata Set", &format!("{e}"));
            std::process::exit(1);
        }
    }
}

fn handle_metadata_get(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id = matches.get_one::<String>("id").expect("id is required");
    let key = matches.get_one::<String>("key").expect("key is required");

    match crate::service::runtime::metadata_get(id, key, config) {
        Ok(value) => {
            if output.is_json() {
                println!("{}", serde_json::json!({"key": key, "value": value}));
            } else {
                println!("{value}");
            }
        }
        Err(e) => {
            output.error("Metadata Get", &format!("{e}"));
            std::process::exit(1);
        }
    }
}

fn handle_metadata_list(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id = matches.get_one::<String>("id").expect("id is required");

    match crate::service::runtime::metadata_list(id, config) {
        Ok(entries) => {
            if output.is_json() {
                let json_entries: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&json_entries).unwrap());
            } else if entries.is_empty() {
                output.info("Metadata List", "No metadata set for this runtime.");
            } else {
                let key_width = entries
                    .iter()
                    .map(|(k, _)| k.len())
                    .max()
                    .unwrap_or(3)
                    .max(3);

                println!();
                println!("  {:<kw$} VALUE", "KEY", kw = key_width);
                for (k, v) in &entries {
                    println!("  {:<kw$} {v}", k, kw = key_width);
                }
                println!();
            }
        }
        Err(e) => {
            output.error("Metadata List", &format!("{e}"));
            std::process::exit(1);
        }
    }
}

fn handle_metadata_delete(matches: &ArgMatches, config: &Config, output: &OutputManager) {
    let id = matches.get_one::<String>("id").expect("id is required");
    let key = matches.get_one::<String>("key").expect("key is required");

    match crate::service::runtime::metadata_delete(id, key, config) {
        Ok(()) => {
            if output.is_json() {
                println!("{{\"status\":\"ok\"}}");
            } else {
                output.success(
                    "Metadata Delete",
                    &format!("Deleted '{key}' from runtime {id}"),
                );
            }
        }
        Err(e) => {
            output.error("Metadata Delete", &format!("{e}"));
            std::process::exit(1);
        }
    }
}

fn list_runtimes(config: &Config, output: &OutputManager) {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);

    let runtimes = RuntimeManifest::list_all(base_path);

    if output.is_json() {
        let json_runtimes: Vec<serde_json::Value> = runtimes
            .iter()
            .map(|(m, is_active)| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.runtime.name,
                    "version": m.runtime.version,
                    "built_at": m.built_at,
                    "active": is_active,
                    "manifest_version": m.manifest_version,
                    "extensions": m.extensions.len(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_runtimes).unwrap());
        return;
    }

    if runtimes.is_empty() {
        output.info(
            "Runtime List",
            "No runtimes found. Build and provision a runtime first.",
        );
        return;
    }

    println!();
    println!("  {:<32} {:<12} BUILT AT", "RUNTIME", "ACTIVE");

    for (manifest, is_active) in &runtimes {
        let short_id = &manifest.id[..8.min(manifest.id.len())];
        let runtime_label = format!(
            "{} {} ({short_id})",
            manifest.runtime.name, manifest.runtime.version
        );

        let built_at_display = manifest.built_at.replace('T', " ").replace('Z', "");
        let status = if *is_active { "* active" } else { "" };

        println!(
            "  {:<32} {:<12} {}",
            runtime_label, status, built_at_display
        );
    }

    println!();

    if output.is_verbose() {
        println!("  Full build IDs:");
        for (manifest, is_active) in &runtimes {
            let marker = if *is_active { " (active)" } else { "" };
            println!("    {} {}{marker}", manifest.id, manifest.runtime.name,);
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestExtension, RuntimeInfo};

    fn make_runtime(id: &str, name: &str, version: &str, built_at: &str) -> RuntimeManifest {
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
                image_id: Some("img-id".to_string()),
                image_type: None,
                sha256: None,
                enabled: true,
            }],
            os_bundle: None,
        }
    }

    #[test]
    fn test_resolve_runtime_id_exact_match() {
        let runtimes = vec![
            (
                make_runtime("abcd1234-5678", "dev", "0.1.0", "2026-02-19T00:00:00Z"),
                true,
            ),
            (
                make_runtime("efgh5678-1234", "prod", "1.0.0", "2026-02-18T00:00:00Z"),
                false,
            ),
        ];
        let output = OutputManager::new(false, false);
        let result = resolve_runtime_id("abcd1234-5678", &runtimes, &output);
        assert!(result.is_some());
        let (m, active) = result.unwrap();
        assert_eq!(m.id, "abcd1234-5678");
        assert!(active);
    }

    #[test]
    fn test_resolve_runtime_id_prefix_match() {
        let runtimes = vec![
            (
                make_runtime("abcd1234-5678", "dev", "0.1.0", "2026-02-19T00:00:00Z"),
                false,
            ),
            (
                make_runtime("efgh5678-1234", "prod", "1.0.0", "2026-02-18T00:00:00Z"),
                true,
            ),
        ];
        let output = OutputManager::new(false, false);
        let result = resolve_runtime_id("abcd", &runtimes, &output);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.id, "abcd1234-5678");
    }
}
