use crate::manifest::{RuntimeManifest, IMAGES_DIR_NAME};
use crate::staging;
use ed25519_compact::PublicKey;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("No update authority configured. Build and provision a runtime with 'avocado build' to enable verified updates.")]
    NoTrustAnchor,

    #[error("Failed to fetch {0}: {1}")]
    FetchFailed(String, String),

    #[error("Signature verification failed for {0}: {1}")]
    SignatureVerification(String, String),

    #[error("Hash mismatch for target '{target}': expected {expected}, got {actual}")]
    HashMismatch {
        target: String,
        expected: String,
        actual: String,
    },

    #[error("Staging failed: {0}")]
    StagingFailed(String),

    #[error("Metadata error: {0}")]
    MetadataError(String),
}

/// Perform a TUF-based runtime update.
/// Returns `Ok(true)` if an OS update was applied and a reboot is required
/// before extensions can be merged. Returns `Ok(false)` otherwise.
pub fn perform_update(
    url: &str,
    base_dir: &Path,
    auth_token: Option<&str>,
    artifacts_url: Option<&str>,
    stream_os_to_partition: bool,
    verbose: bool,
    spot_check_bytes: u64,
) -> Result<bool, UpdateError> {
    let url = url.trim_end_matches('/');

    // 1. Load the local trust anchor
    let root_path = base_dir.join("metadata").join("root.json");
    let root_content = fs::read_to_string(&root_path).map_err(|_| UpdateError::NoTrustAnchor)?;

    let signed_root: tough::schema::Signed<tough::schema::Root> =
        serde_json::from_str(&root_content).map_err(|e| {
            UpdateError::MetadataError(format!("Failed to parse local root.json: {e}"))
        })?;

    let root = &signed_root.signed;
    let trusted_keys = extract_trusted_keys(root)?;

    println!(
        "  Trust anchor: version {}, {} trusted key(s)",
        root.version,
        trusted_keys.len()
    );

    // 2. Fetch and verify remote metadata (TUF order: timestamp -> snapshot -> targets)
    println!("  Fetching update metadata...");

    let timestamp_url = format!("{url}/metadata/timestamp.json");
    let timestamp_raw = fetch_url(&timestamp_url, auth_token)?;
    let timestamp: tough::schema::Signed<tough::schema::Timestamp> =
        parse_metadata("timestamp.json", &timestamp_raw)?;
    verify_signatures(
        "timestamp.json",
        &timestamp_raw,
        &timestamp.signatures,
        &trusted_keys,
        root,
        &tough::schema::RoleType::Timestamp,
    )?;

    println!(
        "  Verified timestamp.json (version {})",
        timestamp.signed.version
    );

    let snapshot_url = format!("{url}/metadata/snapshot.json");
    let snapshot_raw = fetch_url(&snapshot_url, auth_token)?;
    let snapshot: tough::schema::Signed<tough::schema::Snapshot> =
        parse_metadata("snapshot.json", &snapshot_raw)?;
    verify_signatures(
        "snapshot.json",
        &snapshot_raw,
        &snapshot.signatures,
        &trusted_keys,
        root,
        &tough::schema::RoleType::Snapshot,
    )?;

    println!(
        "  Verified snapshot.json (version {})",
        snapshot.signed.version
    );

    let targets_url = format!("{url}/metadata/targets.json");
    let targets_raw = fetch_url(&targets_url, auth_token)?;
    let targets: tough::schema::Signed<tough::schema::Targets> =
        parse_metadata("targets.json", &targets_raw)?;
    verify_signatures(
        "targets.json",
        &targets_raw,
        &targets.signatures,
        &trusted_keys,
        root,
        &tough::schema::RoleType::Targets,
    )?;

    let inline_count = targets.signed.targets.len();
    println!(
        "  Verified targets.json (version {}, {} inline target(s))",
        targets.signed.version, inline_count
    );
    if verbose {
        for name in targets.signed.targets.keys() {
            println!("    inline target: {}", name.raw());
        }
    }

    // 3a. Walk delegations if present — collect delegated targets
    let mut delegated_targets: Vec<(String, tough::schema::Target)> = Vec::new();

    if let Some(delegations) = &targets.signed.delegations {
        println!(
            "  Found {} delegation(s) in targets.json",
            delegations.roles.len()
        );
        for role in &delegations.roles {
            let role_path = format!("delegations/{}.json", role.name);
            let delegation_url = format!("{url}/metadata/{role_path}");
            println!("  Fetching delegation: {}", role.name);
            let delegation_raw = fetch_url(&delegation_url, auth_token)?;

            // Verify hash + length against snapshot meta entry
            verify_delegation_hash(&role_path, &delegation_raw, &snapshot)?;

            // Parse and verify signature against content key from targets.json delegations.keys
            let delegation: tough::schema::Signed<tough::schema::Targets> =
                parse_metadata(&role_path, &delegation_raw)?;
            verify_delegation_signatures(
                &role_path,
                &delegation_raw,
                &delegation.signatures,
                &delegations.keys,
                &role.keyids,
                role.threshold,
            )?;

            println!(
                "  Verified delegation {} ({} target(s))",
                role.name,
                delegation.signed.targets.len()
            );
            if verbose {
                for name in delegation.signed.targets.keys() {
                    println!("    delegated target: {}", name.raw());
                }
            }
            if delegation.signed.targets.is_empty() {
                println!("  WARNING: Delegation '{}' has no targets — extension images will not be downloaded!", role.name);
            }

            for (name, info) in &delegation.signed.targets {
                delegated_targets.push((name.raw().to_string(), info.clone()));
            }
        }
    } else {
        println!("  No delegations found in targets.json");
    }

    // 3b. Enumerate and download targets (inline + delegated)
    let inline_targets: Vec<(String, &tough::schema::Target)> = targets
        .signed
        .targets
        .iter()
        .map(|(k, v)| (k.raw().to_string(), v))
        .collect();

    let all_count = inline_targets.len() + delegated_targets.len();
    println!("  Processing {all_count} target(s)...");

    let staging_dir = base_dir.join(".update-staging");
    fs::create_dir_all(&staging_dir).map_err(|e| {
        UpdateError::StagingFailed(format!("Failed to create staging directory: {e}"))
    })?;

    let images_dir = base_dir.join(IMAGES_DIR_NAME);
    fs::create_dir_all(&images_dir).map_err(|e| {
        UpdateError::StagingFailed(format!("Failed to create images directory: {e}"))
    })?;

    // Build a set of image files already present on disk so we can skip
    // downloading targets that match a content-addressable image_id.
    let existing_images: std::collections::HashSet<String> = fs::read_dir(&images_dir)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Download manifest.json first so we can check os_build_id before
    // downloading the (potentially large) OS bundle image.
    for (name_str, target_info) in &inline_targets {
        if name_str == "manifest.json" {
            download_target(
                url,
                name_str,
                target_info,
                &staging_dir,
                &existing_images,
                auth_token,
                artifacts_url,
                None,
                verbose,
            )?;
        }
    }
    for (name_str, target_info) in &delegated_targets {
        if name_str == "manifest.json" {
            download_target(
                url,
                name_str,
                target_info,
                &staging_dir,
                &existing_images,
                auth_token,
                artifacts_url,
                None,
                verbose,
            )?;
        }
    }

    // Check if OS bundle download can be skipped by comparing os_build_id.
    // When streaming mode is enabled, the OS bundle is never downloaded to staging —
    // it will be streamed directly to partitions after runtime activation.
    let mut existing_images = existing_images;
    let mut os_bundle_skipped = false;
    let manifest_path = staging_dir.join("manifest.json");
    if manifest_path.exists() {
        if let Ok(content) = fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<RuntimeManifest>(&content) {
                if let Some(ref os_bundle) = manifest.os_bundle {
                    let bundle_filename = format!("{}.raw", os_bundle.image_id);

                    if stream_os_to_partition {
                        // In streaming mode, skip downloading the .aos — we'll stream it later
                        existing_images.insert(bundle_filename.clone());
                    }

                    if let Some(ref expected_id) = os_bundle.os_build_id {
                        let matches =
                            crate::os_update::verify_os_release(&crate::os_update::VerifyConfig {
                                verify_type: "os-release".to_string(),
                                field: "AVOCADO_OS_BUILD_ID".to_string(),
                                expected: expected_id.clone(),
                            })
                            .unwrap_or(false);
                        if matches {
                            // OS is already at target version — skip downloading the bundle
                            println!(
                                "    OS already at target version (AVOCADO_OS_BUILD_ID={expected_id}), skipping OS bundle download"
                            );
                            existing_images.insert(bundle_filename);
                            os_bundle_skipped = true;
                        }
                    }
                }
            }
        }
    }

    // When streaming mode is enabled, download extension .raw files directly to images/
    let direct_images = if stream_os_to_partition {
        Some(images_dir.as_path())
    } else {
        None
    };

    // Download remaining targets (skipping manifest.json which is already downloaded)
    for (name_str, target_info) in &inline_targets {
        if name_str == "manifest.json" {
            continue;
        }
        download_target(
            url,
            name_str,
            target_info,
            &staging_dir,
            &existing_images,
            auth_token,
            artifacts_url,
            direct_images,
            verbose,
        )?;
    }
    for (name_str, target_info) in &delegated_targets {
        if name_str == "manifest.json" {
            continue;
        }
        download_target(
            url,
            name_str,
            target_info,
            &staging_dir,
            &existing_images,
            auth_token,
            artifacts_url,
            direct_images,
            verbose,
        )?;
    }

    // 4. Parse the downloaded manifest and stage the update
    println!("  Staging runtime update...");

    let manifest_path = staging_dir.join("manifest.json");
    let manifest_content = fs::read_to_string(&manifest_path).map_err(|e| {
        UpdateError::StagingFailed(format!("No manifest.json in update targets: {e}"))
    })?;

    let new_manifest: RuntimeManifest = serde_json::from_str(&manifest_content)
        .map_err(|e| UpdateError::StagingFailed(format!("Invalid manifest.json: {e}")))?;

    let short_id = &new_manifest.id[..8.min(new_manifest.id.len())];
    println!(
        "  New runtime: {} {} ({short_id})",
        new_manifest.runtime.name, new_manifest.runtime.version,
    );
    println!(
        "  Manifest lists {} extension(s):",
        new_manifest.extensions.len()
    );
    for ext in &new_manifest.extensions {
        let img = ext.image_id.as_deref().unwrap_or("none");
        println!("    {} {} (image: {})", ext.name, ext.version, img);
    }

    staging::install_images_from_staging(
        &new_manifest,
        &staging_dir,
        base_dir,
        os_bundle_skipped,
        verbose,
    )
    .map_err(|e| UpdateError::StagingFailed(e.to_string()))?;

    staging::stage_manifest(&new_manifest, &manifest_content, base_dir, verbose)
        .map_err(|e| UpdateError::StagingFailed(e.to_string()))?;

    // Best-effort spot hash cache generation
    if let Ok(cache) = staging::generate_spot_hashes(&new_manifest, base_dir, spot_check_bytes) {
        let runtime_dir = base_dir.join("runtimes").join(&new_manifest.id);
        let _ = cache.save(&runtime_dir);
    }

    // From here on, any failure must clean up the staged runtime directory
    // so we don't leave untrusted/broken runtimes on disk.
    let result = finish_update(
        &new_manifest,
        base_dir,
        url,
        auth_token,
        artifacts_url,
        stream_os_to_partition,
        verbose,
        os_bundle_skipped,
    );

    if result.is_err() {
        let runtime_dir = base_dir.join("runtimes").join(&new_manifest.id);
        if runtime_dir.exists() {
            eprintln!(
                "  Cleaning up failed runtime: {}",
                &new_manifest.id[..8.min(new_manifest.id.len())]
            );
            let _ = fs::remove_dir_all(&runtime_dir);
        }
    }

    // Clean up staging directory
    let _ = fs::remove_dir_all(&staging_dir);

    let reboot_required = result?;
    println!("  Update staged successfully.");
    Ok(reboot_required)
}

/// Complete the update after the runtime has been staged.
/// Separated so the caller can clean up the runtime directory on failure.
#[allow(clippy::too_many_arguments)]
fn finish_update(
    new_manifest: &RuntimeManifest,
    base_dir: &Path,
    url: &str,
    auth_token: Option<&str>,
    artifacts_url: Option<&str>,
    stream_os_to_partition: bool,
    verbose: bool,
    _os_bundle_skipped: bool,
) -> Result<bool, UpdateError> {
    let mut reboot_required = false;

    // Apply OS update if bundle is present and OS is not already at target version.
    // When an OS update is applied, the runtime stays pending (not active) — it will
    // be promoted to active on the next boot after the OS build ID is verified.
    if let Some(ref os_bundle) = new_manifest.os_bundle {
        let skip = if let Some(ref expected_id) = os_bundle.os_build_id {
            crate::os_update::verify_os_release(&crate::os_update::VerifyConfig {
                verify_type: "os-release".to_string(),
                field: "AVOCADO_OS_BUILD_ID".to_string(),
                expected: expected_id.clone(),
            })
            .unwrap_or(false)
        } else {
            false
        };

        if skip {
            println!(
                "  OS already up to date (AVOCADO_OS_BUILD_ID={})",
                os_bundle.os_build_id.as_deref().unwrap_or("unknown")
            );
        } else if stream_os_to_partition {
            let bundle_filename = format!("{}.raw", os_bundle.image_id);
            let target_url = if let Some(art_url) = artifacts_url {
                let art_url = art_url.trim_end_matches('/');
                format!("{art_url}/{bundle_filename}")
            } else {
                format!("{url}/targets/{bundle_filename}")
            };

            println!("  OS bundle detected. Streaming directly to partitions...");
            let mut body = fetch_url_response(&target_url, auth_token)?;
            let applied =
                crate::os_update::apply_os_update_streaming(body.as_reader(), base_dir, verbose)
                    .map_err(|e| {
                        UpdateError::StagingFailed(format!("Streaming OS update failed: {e}"))
                    })?;
            if applied {
                reboot_required = true;
            }
        } else {
            let aos_path = base_dir
                .join(IMAGES_DIR_NAME)
                .join(format!("{}.raw", os_bundle.image_id));
            println!("  OS bundle detected. Applying OS update...");
            let applied = crate::os_update::apply_os_update(&aos_path, base_dir, verbose)
                .map_err(|e| UpdateError::StagingFailed(format!("OS update failed: {e}")))?;
            if applied {
                reboot_required = true;
            }
        }

        if reboot_required {
            // Write runtime_id into the pending-update marker so the next boot
            // can promote this runtime to active after verifying the OS.
            crate::os_update::set_pending_runtime_id(&new_manifest.id, base_dir).map_err(|e| {
                UpdateError::StagingFailed(format!("Failed to set pending runtime: {e}"))
            })?;
        }
    }

    if reboot_required {
        // OS update applied — don't activate the runtime yet.
        // It will be promoted on next boot after OS verification.
        let short_id = &new_manifest.id[..8.min(new_manifest.id.len())];
        println!(
            "  Staged runtime: {} {} ({short_id}) — pending OS verification on next boot",
            new_manifest.runtime.name, new_manifest.runtime.version,
        );
    } else {
        // No OS update — activate the runtime immediately
        staging::activate_runtime(&new_manifest.id, base_dir)
            .map_err(|e| UpdateError::StagingFailed(e.to_string()))?;

        let short_id = &new_manifest.id[..8.min(new_manifest.id.len())];
        println!(
            "  Activated runtime: {} {} ({short_id})",
            new_manifest.runtime.name, new_manifest.runtime.version,
        );
    }

    Ok(reboot_required)
}

/// Download a single target file, verifying hash and length.
/// Skips content-addressable image files that already exist on disk.
/// Large `.raw` files use resumable streaming downloads; small files use in-memory fetch.
///
/// When `direct_images_dir` is set, `.raw` extension images are downloaded directly
/// to the images directory (skipping the staging copy step).
#[allow(clippy::too_many_arguments)]
fn download_target(
    url: &str,
    name_str: &str,
    target_info: &tough::schema::Target,
    staging_dir: &Path,
    existing_images: &std::collections::HashSet<String>,
    auth_token: Option<&str>,
    artifacts_url: Option<&str>,
    direct_images_dir: Option<&Path>,
    verbose: bool,
) -> Result<(), UpdateError> {
    // Content-addressable skip: if this target is an image that already
    // exists locally, the UUIDv5 name guarantees identical content.
    if name_str != "manifest.json" && existing_images.contains(name_str) {
        println!("    Already on disk, skipping download: {name_str}");
        return Ok(());
    }

    // .raw image files are fetched from the artifacts URL (shared blob storage)
    // rather than the per-device TUF repo, but still verified against TUF hashes.
    let target_url = if name_str.ends_with(".raw") {
        if let Some(art_url) = artifacts_url {
            let art_url = art_url.trim_end_matches('/');
            format!("{art_url}/{name_str}")
        } else {
            format!("{url}/targets/{name_str}")
        }
    } else {
        format!("{url}/targets/{name_str}")
    };

    let expected_hex = hex_encode(target_info.hashes.sha256.as_ref());

    // When streaming mode is on, download .raw extension images directly to images/
    let dest_path = if name_str.ends_with(".raw") {
        if let Some(images_dir) = direct_images_dir {
            images_dir.join(name_str)
        } else {
            staging_dir.join(name_str)
        }
    } else {
        staging_dir.join(name_str)
    };

    // Use resumable streaming for .raw files (can be 100MB+)
    if name_str.ends_with(".raw") {
        download_target_streaming(
            &target_url,
            &dest_path,
            target_info.length,
            &expected_hex,
            auth_token,
            verbose,
        )?;
    } else {
        if verbose {
            println!("    Downloading {name_str}...");
        }
        let data = fetch_url_bytes(&target_url, auth_token)?;

        if data.len() as u64 != target_info.length {
            return Err(UpdateError::HashMismatch {
                target: name_str.to_string(),
                expected: format!("{} bytes", target_info.length),
                actual: format!("{} bytes", data.len()),
            });
        }

        let actual_hash = sha256_hex(&data);
        if actual_hash != expected_hex {
            return Err(UpdateError::HashMismatch {
                target: name_str.to_string(),
                expected: expected_hex,
                actual: actual_hash,
            });
        }

        fs::write(&dest_path, &data)
            .map_err(|e| UpdateError::StagingFailed(format!("Failed to write {name_str}: {e}")))?;
    }

    Ok(())
}

/// Resumable streaming download for large target files.
///
/// Downloads to a `.part` temp file, resuming from the last byte on interruption.
/// On completion, verifies SHA256 + length against TUF metadata and atomically
/// renames to the final path. Handles servers that don't support Range requests
/// by falling back to a full download.
fn download_target_streaming(
    url: &str,
    dest_path: &Path,
    expected_len: u64,
    expected_sha256: &str,
    auth_token: Option<&str>,
    verbose: bool,
) -> Result<(), UpdateError> {
    let name = dest_path.file_name().unwrap_or_default().to_string_lossy();

    // 1. Check if the final file already exists and is valid
    if dest_path.exists() {
        if let Ok(meta) = dest_path.metadata() {
            if meta.len() == expected_len {
                let actual = sha256_file(dest_path)?;
                if actual == expected_sha256 {
                    println!("    Already in staging, verified: {name}");
                    return Ok(());
                }
            }
        }
        // Wrong size or hash — remove and re-download
        let _ = fs::remove_file(dest_path);
    }

    // 2. Check for a partial download from a previous interrupted attempt
    let part_path = dest_path.with_extension("raw.part");
    let mut existing_len: u64 = 0;

    if part_path.exists() {
        if let Ok(meta) = part_path.metadata() {
            let len = meta.len();
            if len > expected_len {
                // Corrupted — larger than expected, start fresh
                let _ = fs::remove_file(&part_path);
            } else if len == expected_len {
                // Looks complete — verify hash
                let actual = sha256_file(&part_path)?;
                if actual == expected_sha256 {
                    fs::rename(&part_path, dest_path).map_err(|e| {
                        UpdateError::StagingFailed(format!("Failed to rename {name}: {e}"))
                    })?;
                    println!("    Completed partial verified: {name}");
                    return Ok(());
                }
                // Hash mismatch — start fresh
                let _ = fs::remove_file(&part_path);
            } else {
                existing_len = len;
            }
        }
    }

    // 3. Download (with Range header if resuming)
    if existing_len > 0 {
        println!(
            "    Resuming {name} from {} / {} bytes",
            existing_len, expected_len
        );
    } else if verbose {
        println!("    Downloading {name} ({expected_len} bytes)...");
    }

    let (mut file, bytes_before) = fetch_streaming(url, &part_path, existing_len, auth_token)?;

    // 4. Stream response body to disk
    let mut buf = [0u8; 64 * 1024];
    let mut total = bytes_before;
    let mut reader = file.1.as_reader();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;
        if n == 0 {
            break;
        }
        file.0
            .write_all(&buf[..n])
            .map_err(|e| UpdateError::StagingFailed(format!("Write failed for {name}: {e}")))?;
        total += n as u64;
    }
    file.0
        .sync_all()
        .map_err(|e| UpdateError::StagingFailed(format!("Sync failed for {name}: {e}")))?;

    // 5. Verify length
    if total != expected_len {
        let _ = fs::remove_file(&part_path);
        return Err(UpdateError::HashMismatch {
            target: name.to_string(),
            expected: format!("{expected_len} bytes"),
            actual: format!("{total} bytes"),
        });
    }

    // 6. Verify SHA256 of the complete file
    let actual_hash = sha256_file(&part_path)?;
    if actual_hash != expected_sha256 {
        let _ = fs::remove_file(&part_path);
        return Err(UpdateError::HashMismatch {
            target: name.to_string(),
            expected: expected_sha256.to_string(),
            actual: actual_hash,
        });
    }

    // 7. Atomic rename
    fs::rename(&part_path, dest_path)
        .map_err(|e| UpdateError::StagingFailed(format!("Failed to rename {name}: {e}")))?;

    println!("    Downloaded and verified: {name}");
    Ok(())
}

/// Issue an HTTP GET (optionally with Range header), returning the open file
/// handle and a body reader. If the server doesn't support Range (returns 200
/// instead of 206), the file is truncated and download starts from the beginning.
/// Returns (file, body_reader) and the effective byte offset we're writing from.
fn fetch_streaming(
    url: &str,
    part_path: &Path,
    existing_len: u64,
    auth_token: Option<&str>,
) -> Result<((File, ureq::Body), u64), UpdateError> {
    let make_request = |range_from: Option<u64>| {
        let mut req = ureq::get(url);
        if let Some(token) = auth_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(from) = range_from {
            req = req.header("Range", format!("bytes={from}-"));
        }
        req.call()
            .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))
    };

    if existing_len > 0 {
        match make_request(Some(existing_len)) {
            Ok(response) => {
                let status = response.status().as_u16();
                if status == 206 {
                    // Server supports Range — append to existing .part file
                    let file = OpenOptions::new()
                        .append(true)
                        .open(part_path)
                        .map_err(|e| {
                            UpdateError::StagingFailed(format!(
                                "Failed to open .part for append: {e}"
                            ))
                        })?;
                    return Ok(((file, response.into_body()), existing_len));
                }
                // 200 or other — server sent the full file; start fresh
                let file = File::create(part_path).map_err(|e| {
                    UpdateError::StagingFailed(format!("Failed to create .part: {e}"))
                })?;
                return Ok(((file, response.into_body()), 0));
            }
            Err(_) => {
                // Request failed (possibly 416) — delete .part and try fresh
                let _ = fs::remove_file(part_path);
            }
        }
    }

    // Full download from scratch
    let response = make_request(None)?;
    let file = File::create(part_path)
        .map_err(|e| UpdateError::StagingFailed(format!("Failed to create .part: {e}")))?;
    Ok(((file, response.into_body()), 0))
}

/// Compute SHA256 hash of a file by streaming, avoiding loading the full file into memory.
fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(|e| {
        UpdateError::StagingFailed(format!(
            "Failed to open {} for hashing: {e}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| {
            UpdateError::StagingFailed(format!(
                "Failed to read {} for hashing: {e}",
                path.display()
            ))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

/// Verify a delegation file's hash and length against the snapshot metadata.
fn verify_delegation_hash(
    role_path: &str,
    raw_json: &str,
    snapshot: &tough::schema::Signed<tough::schema::Snapshot>,
) -> Result<(), UpdateError> {
    // The snapshot meta key uses the full path like "delegations/runtime-<uuid>.json"
    let meta_entry = snapshot.signed.meta.get(role_path).ok_or_else(|| {
        UpdateError::MetadataError(format!(
            "Delegation '{role_path}' not found in snapshot.json meta"
        ))
    })?;

    let actual_len = raw_json.len() as u64;
    if let Some(expected_len) = meta_entry.length {
        if actual_len != expected_len {
            return Err(UpdateError::MetadataError(format!(
                "Length mismatch for '{role_path}': snapshot says {expected_len}, got {actual_len}"
            )));
        }
    }

    let actual_hash = sha256_hex(raw_json.as_bytes());
    let hashes = meta_entry.hashes.as_ref().ok_or_else(|| {
        UpdateError::MetadataError(format!("No hashes in snapshot.json for '{role_path}'"))
    })?;
    let expected_hash = hex_encode(hashes.sha256.as_ref());
    if actual_hash != expected_hash {
        return Err(UpdateError::MetadataError(format!(
            "Hash mismatch for '{role_path}': snapshot says {expected_hash}, got {actual_hash}"
        )));
    }

    Ok(())
}

/// Verify signatures on a delegation file using the keys declared in the
/// parent targets.json `delegations.keys` map.
fn verify_delegation_signatures<K: AsRef<[u8]>>(
    name: &str,
    raw_json: &str,
    signatures: &[tough::schema::Signature],
    delegation_keys: &std::collections::HashMap<K, tough::schema::key::Key>,
    authorized_keyids: &[K],
    threshold: std::num::NonZeroU64,
) -> Result<(), UpdateError> {
    let authorized_hex: Vec<String> = authorized_keyids
        .iter()
        .map(|id| hex_encode(id.as_ref()))
        .collect();

    let threshold = threshold.get() as usize;

    // Build a map of keyid-hex → PublicKey from the delegation keys
    let mut key_map: Vec<(String, PublicKey)> = Vec::new();
    for (key_id, key) in delegation_keys {
        let key_id_hex = hex_encode(key_id.as_ref());
        if let tough::schema::key::Key::Ed25519 { keyval, .. } = key {
            let public_hex = hex_encode(keyval.public.as_ref());
            if let Ok(public_bytes) = hex_decode(&public_hex) {
                if let Ok(pk) = PublicKey::from_slice(&public_bytes) {
                    key_map.push((key_id_hex, pk));
                }
            }
        }
    }

    let canonical = extract_signed_canonical(raw_json)
        .map_err(|e| UpdateError::SignatureVerification(name.to_string(), e))?;

    let mut valid_count = 0;

    for sig in signatures {
        let sig_key_id = hex_encode(sig.keyid.as_ref());

        if !authorized_hex.contains(&sig_key_id) {
            continue;
        }

        if let Some((_, pk)) = key_map.iter().find(|(id, _)| *id == sig_key_id) {
            if let Ok(signature) = ed25519_compact::Signature::from_slice(sig.sig.as_ref()) {
                if pk.verify(canonical.as_bytes(), &signature).is_ok() {
                    valid_count += 1;
                }
            }
        }
    }

    if valid_count < threshold {
        return Err(UpdateError::SignatureVerification(
            name.to_string(),
            format!("Insufficient valid signatures: got {valid_count}, need {threshold}"),
        ));
    }

    Ok(())
}

fn extract_trusted_keys(
    root: &tough::schema::Root,
) -> Result<Vec<(String, PublicKey)>, UpdateError> {
    let mut keys = Vec::new();
    for (key_id, key) in &root.keys {
        let key_id_hex = hex_encode(key_id.as_ref());
        match key {
            tough::schema::key::Key::Ed25519 { keyval, .. } => {
                let public_hex = hex_encode(keyval.public.as_ref());
                let public_bytes = hex_decode(&public_hex).map_err(|e| {
                    UpdateError::MetadataError(format!("Invalid public key hex: {e}"))
                })?;
                let pk = PublicKey::from_slice(&public_bytes).map_err(|_| {
                    UpdateError::MetadataError("Invalid ed25519 public key length".to_string())
                })?;
                keys.push((key_id_hex, pk));
            }
            _ => {
                // Skip non-ed25519 keys for now
            }
        }
    }
    if keys.is_empty() {
        return Err(UpdateError::MetadataError(
            "No ed25519 keys found in root.json".to_string(),
        ));
    }
    Ok(keys)
}

fn verify_signatures(
    name: &str,
    raw_json: &str,
    signatures: &[tough::schema::Signature],
    trusted_keys: &[(String, PublicKey)],
    root: &tough::schema::Root,
    role_type: &tough::schema::RoleType,
) -> Result<(), UpdateError> {
    // Find which key IDs are authorized for this role
    let role_def = root.roles.get(role_type).ok_or_else(|| {
        UpdateError::MetadataError(format!("No role definition for {role_type:?} in root.json"))
    })?;

    let authorized_key_ids: Vec<String> = role_def
        .keyids
        .iter()
        .map(|id| hex_encode(id.as_ref()))
        .collect();

    let threshold = role_def.threshold.get() as usize;

    // Extract the raw "signed" portion from the JSON string for verification.
    // We must use the exact bytes from the original JSON to match the signature,
    // so we extract the substring rather than re-serializing.
    let canonical = extract_signed_canonical(raw_json)
        .map_err(|e| UpdateError::SignatureVerification(name.to_string(), e))?;

    let mut valid_count = 0;

    for sig in signatures {
        let sig_key_id = hex_encode(sig.keyid.as_ref());

        if !authorized_key_ids.contains(&sig_key_id) {
            continue;
        }

        if let Some((_, pk)) = trusted_keys.iter().find(|(id, _)| *id == sig_key_id) {
            if let Ok(signature) = ed25519_compact::Signature::from_slice(sig.sig.as_ref()) {
                if pk.verify(canonical.as_bytes(), &signature).is_ok() {
                    valid_count += 1;
                }
            }
        }
    }

    if valid_count < threshold {
        return Err(UpdateError::SignatureVerification(
            name.to_string(),
            format!("Insufficient valid signatures: got {valid_count}, need {threshold}"),
        ));
    }

    Ok(())
}

/// Extract the canonical JSON string for the "signed" field from a TUF metadata envelope.
/// This re-serializes the parsed "signed" value to compact JSON (serde_json::to_string)
/// which produces deterministic output because serde_json uses BTreeMap for key ordering.
fn extract_signed_canonical(raw_json: &str) -> Result<String, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(raw_json).map_err(|e| format!("Invalid JSON: {e}"))?;

    let signed = parsed
        .get("signed")
        .ok_or_else(|| "Missing 'signed' field".to_string())?;

    serde_json::to_string(signed).map_err(|e| format!("Failed to serialize: {e}"))
}

fn fetch_url(url: &str, auth_token: Option<&str>) -> Result<String, UpdateError> {
    let req = ureq::get(url);
    let response = match auth_token {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
    .call()
    .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;

    let mut body = String::new();
    response
        .into_body()
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;

    Ok(body)
}

fn fetch_url_bytes(url: &str, auth_token: Option<&str>) -> Result<Vec<u8>, UpdateError> {
    let req = ureq::get(url);
    let response = match auth_token {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
    .call()
    .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;

    let mut body = Vec::new();
    response
        .into_body()
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;

    Ok(body)
}

/// Fetch a URL and return the response body for streaming.
fn fetch_url_response(url: &str, auth_token: Option<&str>) -> Result<ureq::Body, UpdateError> {
    let req = ureq::get(url);
    let response = match auth_token {
        Some(token) => req.header("Authorization", format!("Bearer {token}")),
        None => req,
    }
    .call()
    .map_err(|e| UpdateError::FetchFailed(url.to_string(), e.to_string()))?;

    Ok(response.into_body())
}

fn parse_metadata<T: serde::de::DeserializeOwned>(
    name: &str,
    raw: &str,
) -> Result<tough::schema::Signed<T>, UpdateError> {
    serde_json::from_str(raw)
        .map_err(|e| UpdateError::MetadataError(format!("Failed to parse {name}: {e}")))
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("Odd-length hex string".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| format!("Invalid hex at position {i}: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_keypair() -> ed25519_compact::KeyPair {
        let seed_bytes = [42u8; 32];
        ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::from(seed_bytes))
    }

    fn content_keypair() -> ed25519_compact::KeyPair {
        let seed_bytes = [99u8; 32];
        ed25519_compact::KeyPair::from_seed(ed25519_compact::Seed::from(seed_bytes))
    }

    fn make_test_root_json() -> (String, ed25519_compact::KeyPair) {
        let kp = test_keypair();
        let pk_hex = hex_encode(kp.pk.as_ref());
        let key_id = {
            let canonical = format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{pk_hex}"}},"scheme":"ed25519"}}"#
            );
            sha256_hex(canonical.as_bytes())
        };

        let signed: serde_json::Value = serde_json::json!({
            "_type": "root",
            "consistent_snapshot": false,
            "expires": "2027-02-18T00:00:00Z",
            "keys": {
                &key_id: {
                    "keytype": "ed25519",
                    "keyval": { "public": pk_hex },
                    "scheme": "ed25519"
                }
            },
            "roles": {
                "root": { "keyids": [&key_id], "threshold": 1 },
                "snapshot": { "keyids": [&key_id], "threshold": 1 },
                "targets": { "keyids": [&key_id], "threshold": 1 },
                "timestamp": { "keyids": [&key_id], "threshold": 1 }
            },
            "spec_version": "1.0.0",
            "version": 1
        });

        let canonical = serde_json::to_string(&signed).unwrap();
        let sig = kp.sk.sign(&canonical, None);
        let sig_hex = hex_encode(sig.as_ref());

        let root = serde_json::json!({
            "signatures": [{ "keyid": key_id, "sig": sig_hex }],
            "signed": signed
        });

        (serde_json::to_string_pretty(&root).unwrap(), kp)
    }

    /// Build a signed TUF metadata envelope.
    fn sign_json(payload: &serde_json::Value, kp: &ed25519_compact::KeyPair) -> (String, String) {
        let pk_hex = hex_encode(kp.pk.as_ref());
        let key_id = sha256_hex(
            format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{pk_hex}"}},"scheme":"ed25519"}}"#
            )
            .as_bytes(),
        );
        let canonical = serde_json::to_string(payload).unwrap();
        let sig = kp.sk.sign(canonical.as_bytes(), None);
        let sig_hex = hex_encode(sig.as_ref());
        let envelope = serde_json::json!({
            "signatures": [{ "keyid": &key_id, "sig": sig_hex }],
            "signed": payload
        });
        (serde_json::to_string_pretty(&envelope).unwrap(), key_id)
    }

    #[test]
    fn test_extract_trusted_keys() {
        let (root_json, _kp) = make_test_root_json();
        let signed_root: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_str(&root_json).unwrap();
        let keys = extract_trusted_keys(&signed_root.signed).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].1.as_ref().len(), 32);
    }

    #[test]
    fn test_sha256_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.bin");
        fs::write(&path, b"hello world").unwrap();

        let hash = sha256_file(&path).unwrap();
        // Known SHA256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_file_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("empty.bin");
        fs::write(&path, b"").unwrap();

        let hash = sha256_file(&path).unwrap();
        // Known SHA256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_download_target_streaming_skips_valid_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("test.raw");
        let data = b"test content for streaming";
        fs::write(&dest, data).unwrap();

        let hash = sha256_hex(data);
        let result = download_target_streaming(
            "http://localhost:1/nonexistent",
            &dest,
            data.len() as u64,
            &hash,
            None,
            false,
        );
        assert!(
            result.is_ok(),
            "Should skip download for valid existing file"
        );
    }

    #[test]
    fn test_download_target_streaming_rejects_wrong_size() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("bad.raw");
        // File has wrong size — should be removed
        fs::write(&dest, b"short").unwrap();

        let hash = sha256_hex(b"this is the expected content");
        // This will try to download from a nonexistent URL after removing the bad file
        let result = download_target_streaming(
            "http://127.0.0.1:1/nonexistent",
            &dest,
            28,
            &hash,
            None,
            false,
        );
        // Should fail because the URL is unreachable, but the bad file should be gone
        assert!(result.is_err());
        assert!(!dest.exists(), "Bad file should have been removed");
    }

    #[test]
    fn test_hex_roundtrip() {
        let data = vec![0xab, 0xcd, 0xef, 0x01, 0x23];
        let hex = hex_encode(&data);
        assert_eq!(hex, "abcdef0123");
        let decoded = hex_decode(&hex).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_hex_decode_error() {
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zzzz").is_err());
    }

    #[test]
    fn test_no_trust_anchor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = perform_update(
            "http://localhost:9999",
            tmp.path(),
            None,
            None,
            false,
            false,
            4096,
        );
        assert!(matches!(result, Err(UpdateError::NoTrustAnchor)));
    }

    #[test]
    fn test_verify_signatures_with_real_key() {
        let (root_json, kp) = make_test_root_json();

        let signed_root: tough::schema::Signed<tough::schema::Root> =
            serde_json::from_str(&root_json).unwrap();
        let trusted_keys = extract_trusted_keys(&signed_root.signed).unwrap();

        let pk_hex = hex_encode(kp.pk.as_ref());
        let key_id = {
            let canonical = format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{pk_hex}"}},"scheme":"ed25519"}}"#
            );
            sha256_hex(canonical.as_bytes())
        };

        let signed_payload: serde_json::Value = serde_json::json!({
            "_type": "targets",
            "expires": "2027-02-18T00:00:00Z",
            "spec_version": "1.0.0",
            "targets": {},
            "version": 1
        });

        // Build the envelope first, then extract the canonical form the same way
        // verify_signatures does -- this avoids any key-ordering drift between
        // serde_json's Map implementation and re-serialization.
        let unsigned_envelope = serde_json::json!({
            "signatures": [],
            "signed": signed_payload
        });
        let unsigned_raw = serde_json::to_string(&unsigned_envelope).unwrap();
        let canonical = extract_signed_canonical(&unsigned_raw).unwrap();

        let sig = kp.sk.sign(canonical.as_bytes(), None);
        let sig_hex = hex_encode(sig.as_ref());

        let full_json = serde_json::json!({
            "signatures": [{ "keyid": key_id, "sig": sig_hex }],
            "signed": signed_payload
        });

        let raw = serde_json::to_string(&full_json).unwrap();
        let parsed: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&raw).unwrap();

        let result = verify_signatures(
            "targets.json",
            &raw,
            &parsed.signatures,
            &trusted_keys,
            &signed_root.signed,
            &tough::schema::RoleType::Targets,
        );

        assert!(
            result.is_ok(),
            "Signature verification should succeed: {result:?}"
        );
    }

    // ---- Delegation tests ----

    fn make_delegated_targets_json(
        runtime_uuid: &str,
        content_kp: &ed25519_compact::KeyPair,
        targets: &[(&str, &str, u64)], // (name, sha256_hex, size)
    ) -> String {
        let mut targets_map = serde_json::Map::new();
        for (name, hash, size) in targets {
            targets_map.insert(
                name.to_string(),
                serde_json::json!({
                    "hashes": { "sha256": hash },
                    "length": size
                }),
            );
        }
        let payload = serde_json::json!({
            "_type": "targets",
            "expires": "2030-01-01T00:00:00Z",
            "spec_version": "1.0.0",
            "targets": targets_map,
            "version": 1,
            "_delegation_name": format!("runtime-{runtime_uuid}")
        });
        let (json, _) = sign_json(&payload, content_kp);
        json
    }

    fn make_targets_with_delegation(
        runtime_uuid: &str,
        content_kp: &ed25519_compact::KeyPair,
        signer_kp: &ed25519_compact::KeyPair,
    ) -> String {
        let content_pk_hex = hex_encode(content_kp.pk.as_ref());
        let content_key_id = sha256_hex(
            format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{content_pk_hex}"}},"scheme":"ed25519"}}"#
            )
            .as_bytes(),
        );

        let payload = serde_json::json!({
            "_type": "targets",
            "expires": "2030-01-01T00:00:00Z",
            "spec_version": "1.0.0",
            "targets": {},
            "delegations": {
                "keys": {
                    &content_key_id: {
                        "keytype": "ed25519",
                        "keyval": { "public": content_pk_hex },
                        "scheme": "ed25519"
                    }
                },
                "roles": [
                    {
                        "name": format!("runtime-{runtime_uuid}"),
                        "keyids": [&content_key_id],
                        "threshold": 1,
                        "paths": ["manifest.json", "*.raw"],
                        "terminating": true
                    }
                ]
            },
            "version": 1
        });
        let (json, _) = sign_json(&payload, signer_kp);
        json
    }

    fn make_snapshot_with_delegation(
        targets_json: &str,
        delegation_json: &str,
        runtime_uuid: &str,
        signer_kp: &ed25519_compact::KeyPair,
    ) -> String {
        let targets_hash = sha256_hex(targets_json.as_bytes());
        let targets_len = targets_json.len() as u64;
        let del_hash = sha256_hex(delegation_json.as_bytes());
        let del_len = delegation_json.len() as u64;
        let del_path = format!("delegations/runtime-{runtime_uuid}.json");

        let payload = serde_json::json!({
            "_type": "snapshot",
            "expires": "2030-01-01T00:00:00Z",
            "spec_version": "1.0.0",
            "meta": {
                "targets.json": {
                    "hashes": { "sha256": targets_hash },
                    "length": targets_len,
                    "version": 1
                },
                del_path: {
                    "hashes": { "sha256": del_hash },
                    "length": del_len,
                    "version": 1
                }
            },
            "version": 1
        });
        let (json, _) = sign_json(&payload, signer_kp);
        json
    }

    #[test]
    fn test_verify_delegation_hash_ok() {
        let kp = test_keypair();
        let ckp = content_keypair();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let del_json = make_delegated_targets_json(uuid, &ckp, &[]);
        let targets_json = make_targets_with_delegation(uuid, &ckp, &kp);
        let snapshot_json = make_snapshot_with_delegation(&targets_json, &del_json, uuid, &kp);
        let snapshot: tough::schema::Signed<tough::schema::Snapshot> =
            serde_json::from_str(&snapshot_json).unwrap();

        let role_path = format!("delegations/runtime-{uuid}.json");
        assert!(verify_delegation_hash(&role_path, &del_json, &snapshot).is_ok());
    }

    #[test]
    fn test_verify_delegation_hash_mismatch() {
        let kp = test_keypair();
        let ckp = content_keypair();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let del_json = make_delegated_targets_json(uuid, &ckp, &[]);
        let targets_json = make_targets_with_delegation(uuid, &ckp, &kp);
        let snapshot_json = make_snapshot_with_delegation(&targets_json, &del_json, uuid, &kp);
        let snapshot: tough::schema::Signed<tough::schema::Snapshot> =
            serde_json::from_str(&snapshot_json).unwrap();

        let role_path = format!("delegations/runtime-{uuid}.json");
        let tampered = del_json.replace("runtime", "TAMPERED");
        assert!(verify_delegation_hash(&role_path, &tampered, &snapshot).is_err());
    }

    #[test]
    fn test_verify_delegation_signatures_ok() {
        let ckp = content_keypair();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let del_json =
            make_delegated_targets_json(uuid, &ckp, &[("manifest.json", &"aa".repeat(32), 10)]);

        let del: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&del_json).unwrap();

        // Build keys + keyids matching the content keypair
        let content_pk_hex = hex_encode(ckp.pk.as_ref());
        let content_key_id_hex = sha256_hex(
            format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{content_pk_hex}"}},"scheme":"ed25519"}}"#
            )
            .as_bytes(),
        );

        // Parse from a full targets.json with delegation block to get proper tough types
        let kp = test_keypair();
        let targets_json = make_targets_with_delegation(uuid, &ckp, &kp);
        let targets: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&targets_json).unwrap();
        let delegations = targets.signed.delegations.unwrap();
        let role = &delegations.roles[0];

        let role_path = format!("delegations/runtime-{uuid}.json");
        let result = verify_delegation_signatures(
            &role_path,
            &del_json,
            &del.signatures,
            &delegations.keys,
            &role.keyids,
            role.threshold,
        );
        assert!(
            result.is_ok(),
            "Delegation signature verification should succeed: {result:?}"
        );
        let _ = content_key_id_hex;
    }

    #[test]
    fn test_verify_delegation_signatures_wrong_key() {
        let ckp = content_keypair();
        let wrong_kp = test_keypair(); // different key
        let uuid = "550e8400-e29b-41d4-a716-446655440000";

        // Sign with the content key, but declare a different key in delegation
        let del_json = make_delegated_targets_json(uuid, &wrong_kp, &[]);
        let del: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&del_json).unwrap();

        // targets.json delegates to ckp, but the file is signed by wrong_kp
        let targets_json = make_targets_with_delegation(uuid, &ckp, &ckp);
        let targets: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&targets_json).unwrap();
        let delegations = targets.signed.delegations.unwrap();
        let role = &delegations.roles[0];

        let role_path = format!("delegations/runtime-{uuid}.json");
        let result = verify_delegation_signatures(
            &role_path,
            &del_json,
            &del.signatures,
            &delegations.keys,
            &role.keyids,
            role.threshold,
        );
        assert!(result.is_err(), "Should fail with wrong signing key");
    }

    #[test]
    fn test_flat_targets_no_delegation() {
        // Without a delegations block, delegated_targets should be empty
        // and processing continues using inline targets only.
        let kp = test_keypair();
        let pk_hex = hex_encode(kp.pk.as_ref());
        let key_id = sha256_hex(
            format!(
                r#"{{"keytype":"ed25519","keyval":{{"public":"{pk_hex}"}},"scheme":"ed25519"}}"#
            )
            .as_bytes(),
        );

        // Build a flat targets.json without delegations
        let payload = serde_json::json!({
            "_type": "targets",
            "expires": "2030-01-01T00:00:00Z",
            "spec_version": "1.0.0",
            "targets": {
                "manifest.json": {
                    "hashes": { "sha256": "aa".repeat(32) },
                    "length": 10
                }
            },
            "version": 1
        });
        let canonical = serde_json::to_string(&payload).unwrap();
        let sig = kp.sk.sign(canonical.as_bytes(), None);
        let sig_hex = hex_encode(sig.as_ref());
        let targets_json = serde_json::to_string(&serde_json::json!({
            "signatures": [{ "keyid": key_id, "sig": sig_hex }],
            "signed": payload
        }))
        .unwrap();

        let targets: tough::schema::Signed<tough::schema::Targets> =
            serde_json::from_str(&targets_json).unwrap();

        // No delegations block → no delegation walking
        assert!(targets.signed.delegations.is_none());
        assert_eq!(targets.signed.targets.len(), 1);
    }
}
