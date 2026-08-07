use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

// ---------------------------------------------------------------------------
// Error type (moved from ext.rs)
// ---------------------------------------------------------------------------

/// Errors related to system command execution during image operations.
#[derive(Debug, thiserror::Error)]
pub enum SystemdError {
    #[error("Failed to run command '{command}': {source}")]
    CommandFailed {
        command: String,
        source: std::io::Error,
    },

    #[error("Command '{command}' exited with error code {exit_code:?}: {stderr}")]
    CommandExitedWithError {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("Configuration error: {message}")]
    ConfigurationError { message: String },
}

// ---------------------------------------------------------------------------
// Image type tag (replaces is_directory + is_kab booleans on Extension)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageTypeTag {
    Directory,
    Raw,
    Kab,
}

// ---------------------------------------------------------------------------
// ImageAdaptor trait
// ---------------------------------------------------------------------------

pub trait ImageAdaptor {
    /// Mount the image and return the mount point path.
    /// If already mounted with correct backing, return existing mount point.
    fn mount(
        &self,
        mount_name: &str,
        image_path: &Path,
        verbose: bool,
    ) -> Result<PathBuf, SystemdError>;

    /// Check whether the image is currently mounted.
    fn is_mounted(&self, mount_name: &str) -> bool;

    /// Unmount a single extension.
    fn unmount(&self, mount_name: &str, verbose: bool) -> Result<(), SystemdError>;

    /// Unmount all extensions managed by this adaptor type.
    fn unmount_all(&self) -> Result<(), SystemdError>;

    /// Check whether the backing image has changed and requires remounting.
    fn needs_remount(&self, mount_name: &str, image_path: &Path) -> bool;

    /// The tag identifying this adaptor type.
    fn type_tag(&self) -> ImageTypeTag;
}

// ---------------------------------------------------------------------------
// Dispatch enum
// ---------------------------------------------------------------------------

pub enum ImageType {
    Raw(RawAdaptor),
    Kab(KabAdaptor),
}

impl ImageType {
    /// Select the appropriate adaptor based on the manifest `image_type` field.
    pub fn from_manifest(image_type: &Option<String>) -> Self {
        match image_type.as_deref() {
            Some("kab") => ImageType::Kab(KabAdaptor),
            _ => ImageType::Raw(RawAdaptor),
        }
    }
}

impl ImageAdaptor for ImageType {
    fn mount(
        &self,
        mount_name: &str,
        image_path: &Path,
        verbose: bool,
    ) -> Result<PathBuf, SystemdError> {
        match self {
            ImageType::Raw(a) => a.mount(mount_name, image_path, verbose),
            ImageType::Kab(a) => a.mount(mount_name, image_path, verbose),
        }
    }

    fn is_mounted(&self, mount_name: &str) -> bool {
        match self {
            ImageType::Raw(a) => a.is_mounted(mount_name),
            ImageType::Kab(a) => a.is_mounted(mount_name),
        }
    }

    fn unmount(&self, mount_name: &str, verbose: bool) -> Result<(), SystemdError> {
        match self {
            ImageType::Raw(a) => a.unmount(mount_name, verbose),
            ImageType::Kab(a) => a.unmount(mount_name, verbose),
        }
    }

    fn unmount_all(&self) -> Result<(), SystemdError> {
        match self {
            ImageType::Raw(a) => a.unmount_all(),
            ImageType::Kab(a) => a.unmount_all(),
        }
    }

    fn needs_remount(&self, mount_name: &str, image_path: &Path) -> bool {
        match self {
            ImageType::Raw(a) => a.needs_remount(mount_name, image_path),
            ImageType::Kab(a) => a.needs_remount(mount_name, image_path),
        }
    }

    fn type_tag(&self) -> ImageTypeTag {
        match self {
            ImageType::Raw(a) => a.type_tag(),
            ImageType::Kab(a) => a.type_tag(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Compute the mount point path for an extension, respecting AVOCADO_TEST_MODE.
pub fn extension_mount_point(mount_name: &str) -> String {
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/extensions/{mount_name}")
    } else {
        format!("/run/avocado/extensions/{mount_name}")
    }
}

/// Resolve the systemd-dissect command name (real or mock in test mode).
fn dissect_command() -> &'static str {
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        "mock-systemd-dissect"
    } else {
        "systemd-dissect"
    }
}

fn is_test_mode() -> bool {
    std::env::var("AVOCADO_TEST_MODE").is_ok()
}

/// Mount an image (file or block device) using systemd-dissect.
/// Shared final mount step used by both RawAdaptor and KabAdaptor.
///
/// When `use_loop_ref` is true, systemd-dissect manages the loop device (raw path).
/// When false, the caller has already set up a loop device (KAB path).
fn mount_with_dissect(
    mount_name: &str,
    image_source: &Path,
    mount_point: &str,
    use_loop_ref: bool,
    verbose: bool,
) -> Result<(), SystemdError> {
    // Create mount point parent directory
    if let Some(parent) = Path::new(mount_point).parent() {
        fs::create_dir_all(parent).map_err(|e| SystemdError::CommandFailed {
            command: "create_dir_all".to_string(),
            source: e,
        })?;
    }

    if verbose {
        println!("Mounting {mount_name} via systemd-dissect...");
    }

    let cmd = dissect_command();

    let mut args: Vec<String> = Vec::new();
    if use_loop_ref {
        args.push(format!("--loop-ref={mount_name}"));
    }
    args.extend_from_slice(&[
        "--mkdir".to_string(),
        "-r".to_string(),
        "-M".to_string(),
        image_source.to_str().unwrap_or("").to_string(),
        mount_point.to_string(),
    ]);

    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = ProcessCommand::new(cmd)
        .args(&arg_refs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: cmd.to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: cmd.to_string(),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    if verbose {
        println!("Mounted {mount_name} to {mount_point}");
    }
    Ok(())
}

/// Unmount using systemd-dissect -U.
fn unmount_with_dissect(mount_point: &str, verbose: bool) -> Result<(), SystemdError> {
    let cmd = dissect_command();

    let output = ProcessCommand::new(cmd)
        .args(["-U", mount_point])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: cmd.to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: cmd.to_string(),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    if verbose {
        println!("Unmounted {mount_point}");
    }
    Ok(())
}

/// Check if a loop device's backing file differs from the expected path.
/// `loop_dev` can be a symlink (e.g. `/dev/disk/by-loop-ref/name`) or a direct
/// device path (`/dev/loopN`).
fn check_backing_file_changed(loop_dev: &Path, expected_path: &Path) -> bool {
    if is_test_mode() {
        return false;
    }

    // Resolve to the actual /dev/loopN device
    let resolved = match fs::read_link(loop_dev) {
        Ok(target) => target,
        Err(_) => loop_dev.to_path_buf(),
    };

    let dev_name = match resolved.file_name().and_then(|f| f.to_str()) {
        Some(name) => name.to_string(),
        None => return false,
    };

    let backing_path = format!("/sys/block/{dev_name}/loop/backing_file");
    let backing_file = match fs::read_to_string(&backing_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return false,
    };

    let expected = expected_path
        .canonicalize()
        .unwrap_or_else(|_| expected_path.to_path_buf());
    let current = PathBuf::from(&backing_file)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&backing_file));

    if expected != current {
        return true;
    }

    // Same path is not the same image. Replacing an extension in place - a
    // plain `>` redirect, scp, or anything that truncates and rewrites - keeps
    // both the path and the inode, so the comparison above sees no change while
    // the loop still exposes the image it was attached to. That is how an
    // extension update comes out reporting success and serving the previous
    // contents.
    //
    // The loop's size is fixed when it is attached, so comparing it against the
    // file on disk catches a replacement of a different size without needing to
    // hash anything. A same-size replacement still slips through; detecting
    // that needs a fingerprint recorded at mount time, which is worth doing
    // separately rather than folding into this predicate.
    //
    // Unreadable sysfs or metadata reports unchanged, matching how every other
    // failure in this function behaves - the size check only ever adds
    // detections, it never removes one.
    let sysfs_size = format!("/sys/block/{dev_name}/size");
    let loop_sectors = match fs::read_to_string(&sysfs_size) {
        Ok(s) => match s.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => return false,
        },
        Err(_) => return false,
    };

    let file_bytes = match fs::metadata(expected_path) {
        Ok(m) => m.len(),
        Err(_) => return false,
    };

    !loop_size_matches(loop_sectors, file_bytes)
}

/// Whether a loop device of `loop_sectors` is backed by a file of `file_bytes`.
///
/// `/sys/block/<dev>/size` counts 512-byte sectors regardless of the device's
/// logical block size. Split out from `check_backing_file_changed` so the
/// arithmetic is testable without a loop device: everything around it is gated
/// behind `is_test_mode()` and cannot run under `cargo test`.
fn loop_size_matches(loop_sectors: u64, file_bytes: u64) -> bool {
    loop_sectors.saturating_mul(512) == file_bytes
}

/// What mounting an extension should do, given what is already attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountPlan {
    /// Nothing is attached. Mount the file; systemd-dissect attaches a loop.
    Fresh,
    /// A loop is attached and still exposes this exact image. Mount that device
    /// rather than the file, so no second loop is attached for the same content.
    Reuse,
    /// A loop is attached but exposes different content. Tear it down, then
    /// mount the file.
    Replace,
}

/// Decide how to mount, from the two facts the caller can observe.
///
/// This exists as a pure function because the rest of the mount path cannot be
/// exercised under `cargo test` - `is_test_mode()` short-circuits the very
/// `systemd-dissect` and `losetup` calls the behaviour depends on. That gap is
/// how the previous version of this fix reached hardware with a wrong premise:
/// 276 green tests said nothing about whether it worked.
///
/// `ReuseLoop` is the case that matters. `systemd-dissect -U` leaves the loop
/// attached by design, and on an unmerge/merge cycle the kernel does not
/// release it even after an explicit `losetup -d` - measured on an i.MX93
/// board, where no open fd, no mount in any namespace, no overlay lowerdir and
/// no block holder referenced the loop, and a cache drop did not free it.
/// Mounting the file again therefore attaches a SECOND loop for identical
/// content and strands the first: two per cycle, on a device that merges
/// extensions on every update.
pub(crate) fn plan_mount(loop_ref_present: bool, backing_changed: bool) -> MountPlan {
    match (loop_ref_present, backing_changed) {
        (false, _) => MountPlan::Fresh,
        (true, false) => MountPlan::Reuse,
        (true, true) => MountPlan::Replace,
    }
}

/// Detach the persistent loop behind `mount_name`, if one is still attached.
///
/// `systemd-dissect -U` unmounts the mount point but deliberately leaves a
/// `--loop-ref` loop attached - that is what makes it persistent. Every caller
/// of `unmount` wants the extension gone, either to tear it down outright or to
/// replace it with a remount, so a loop that outlives the unmount is never what
/// was asked for: it keeps the old image open, and `/dev/disk/by-loop-ref/<name>`
/// keeps pointing at it, so the next mount is skipped as already-mounted.
///
/// Absent loop-ref is success, not failure - it means nothing is attached.
fn detach_loop_ref(mount_name: &str, verbose: bool) -> Result<(), SystemdError> {
    if is_test_mode() {
        return Ok(());
    }

    let loop_ref = PathBuf::from(format!("/dev/disk/by-loop-ref/{mount_name}"));

    // canonicalize resolves the ../../loopN symlink and tells us in one step
    // whether anything is still there.
    let dev = match fs::canonicalize(&loop_ref) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };

    let output = ProcessCommand::new("losetup")
        .args(["-d", dev.to_str().unwrap_or("")])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| SystemdError::CommandFailed {
            command: "losetup -d".to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SystemdError::CommandExitedWithError {
            command: "losetup -d".to_string(),
            exit_code: output.status.code(),
            stderr: stderr.to_string(),
        });
    }

    if verbose {
        println!("Detached {} for {mount_name}", dev.display());
    }

    Ok(())
}

/// Check if a mount point is currently active by scanning /proc/mounts.
fn is_mount_active(mount_point: &str) -> bool {
    if is_test_mode() {
        return Path::new(mount_point).exists();
    }

    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        mounts.lines().any(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            parts.len() >= 2 && parts[1] == mount_point
        })
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// Scope / initrd utility functions (moved from ext.rs)
// ---------------------------------------------------------------------------

/// Detect if we are running in the initrd by checking for /etc/initrd-release
pub(crate) fn is_running_in_initrd() -> bool {
    Path::new("/etc/initrd-release").exists()
}

/// Parse scope values from release file content (e.g., SYSEXT_SCOPE or CONFEXT_SCOPE)
pub(crate) fn parse_scope_from_release_content(content: &str, scope_key: &str) -> Vec<String> {
    let mut scopes = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with(&format!("{scope_key}=")) {
            let value = line
                .split_once('=')
                .map(|x| x.1)
                .unwrap_or("")
                .trim_matches('"')
                .trim();

            for scope in value.split_whitespace() {
                if !scope.is_empty() {
                    scopes.push(scope.to_string());
                }
            }
            break;
        }
    }

    scopes
}

/// Check if a sysext is enabled for the current environment (initrd vs system)
pub(crate) fn is_sysext_enabled_for_current_environment(
    extension_path: &Path,
    extension_name: &str,
) -> bool {
    let in_initrd = is_running_in_initrd();
    let required_scope = if in_initrd { "initrd" } else { "system" };

    let sysext_release_path = extension_path
        .join("usr/lib/extension-release.d")
        .join(format!("extension-release.{extension_name}"));

    if sysext_release_path.exists() {
        if let Ok(content) = fs::read_to_string(&sysext_release_path) {
            let scopes = parse_scope_from_release_content(&content, "SYSEXT_SCOPE");
            if scopes.is_empty() {
                return true;
            }
            return scopes.contains(&required_scope.to_string());
        }
    }

    true
}

/// Check if a confext is enabled for the current environment (initrd vs system)
pub(crate) fn is_confext_enabled_for_current_environment(
    extension_path: &Path,
    extension_name: &str,
) -> bool {
    let in_initrd = is_running_in_initrd();
    let required_scope = if in_initrd { "initrd" } else { "system" };

    let confext_release_path = extension_path
        .join("etc/extension-release.d")
        .join(format!("extension-release.{extension_name}"));

    if confext_release_path.exists() {
        if let Ok(content) = fs::read_to_string(&confext_release_path) {
            let scopes = parse_scope_from_release_content(&content, "CONFEXT_SCOPE");
            if scopes.is_empty() {
                return true;
            }
            return scopes.contains(&required_scope.to_string());
        }
    }

    true
}

/// Check if a release file's scope allows it to run in the current environment.
pub(crate) fn is_scope_enabled_for_current_environment(content: &str, scope_key: &str) -> bool {
    let in_initrd = is_running_in_initrd();
    let required_scope = if in_initrd { "initrd" } else { "system" };
    let scopes = parse_scope_from_release_content(content, scope_key);
    if scopes.is_empty() {
        return true;
    }
    scopes.contains(&required_scope.to_string())
}

// ---------------------------------------------------------------------------
// Shared extension analysis (deduplicates ext.rs analysis functions)
// ---------------------------------------------------------------------------

/// After mounting an extension image at `mount_path`, detect whether it contains
/// sysext and/or confext release files, and check scope for the current environment.
///
/// Returns `(sysext_enabled, confext_enabled)`.
///
/// Also detects a version from versioned release file names (e.g.
/// `extension-release.app-1.0.0`). If found it is returned as the third tuple
/// element so callers can update the version field of the Extension.
pub fn analyze_mounted_extension(
    name: &str,
    version: &Option<String>,
    mount_path: &Path,
) -> (bool, bool, Option<String>) {
    let mut is_sysext = false;
    let mut is_confext = false;
    let mut detected_version: Option<String> = version.clone();

    // --- sysext release file detection ---
    let sysext_release_path = mount_path
        .join("usr/lib/extension-release.d")
        .join(format!("extension-release.{name}"));

    if sysext_release_path.exists() {
        is_sysext = true;
    } else {
        let sysext_dir = mount_path.join("usr/lib/extension-release.d");
        if sysext_dir.exists() {
            if let Ok(entries) = fs::read_dir(&sysext_dir) {
                let prefix = format!("extension-release.{name}-");
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(&prefix) {
                        is_sysext = true;
                        if detected_version.is_none() {
                            let ver = filename_str.strip_prefix(&prefix).unwrap_or("");
                            if !ver.is_empty() {
                                detected_version = Some(ver.to_string());
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // --- confext release file detection ---
    let confext_release_path = mount_path
        .join("etc/extension-release.d")
        .join(format!("extension-release.{name}"));

    if confext_release_path.exists() {
        is_confext = true;
    } else {
        let confext_dir = mount_path.join("etc/extension-release.d");
        if confext_dir.exists() {
            if let Ok(entries) = fs::read_dir(&confext_dir) {
                let prefix = format!("extension-release.{name}-");
                for entry in entries.flatten() {
                    let filename = entry.file_name();
                    let filename_str = filename.to_string_lossy();
                    if filename_str.starts_with(&prefix) {
                        is_confext = true;
                        if detected_version.is_none() {
                            let ver = filename_str.strip_prefix(&prefix).unwrap_or("");
                            if !ver.is_empty() {
                                detected_version = Some(ver.to_string());
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    // Default to both if no release files found
    if !is_sysext && !is_confext {
        is_sysext = true;
        is_confext = true;
    }

    // Scope checking
    let scope_check_name = if let Some(ref ver) = detected_version {
        format!("{name}-{ver}")
    } else {
        name.to_string()
    };

    let sysext_enabled = if is_sysext {
        is_sysext_enabled_for_current_environment(mount_path, &scope_check_name)
    } else {
        false
    };

    let confext_enabled = if is_confext {
        is_confext_enabled_for_current_environment(mount_path, &scope_check_name)
    } else {
        false
    };

    (sysext_enabled, confext_enabled, detected_version)
}

// ---------------------------------------------------------------------------
// RawAdaptor — mounts .raw files via systemd-dissect with persistent loop
// ---------------------------------------------------------------------------

pub struct RawAdaptor;

impl ImageAdaptor for RawAdaptor {
    fn mount(
        &self,
        mount_name: &str,
        raw_path: &Path,
        verbose: bool,
    ) -> Result<PathBuf, SystemdError> {
        let mount_point = extension_mount_point(mount_name);

        if verbose {
            println!("Mounting raw file {mount_name} with persistent loop...");
        }

        if is_test_mode() {
            // In test mode, call mock-systemd-dissect but skip actual mounting
            mount_with_dissect(mount_name, raw_path, &mount_point, true, verbose)?;
            return Ok(PathBuf::from(mount_point));
        }

        let loop_ref = PathBuf::from(format!("/dev/disk/by-loop-ref/{mount_name}"));
        let present = loop_ref.exists();
        let plan = plan_mount(
            present,
            present && check_backing_file_changed(&loop_ref, raw_path),
        );

        match plan {
            // Mount the loop we already have. Mounting the file instead would
            // attach a second loop for the same image and leak the first, since
            // the unmount left it attached and the kernel will not release it.
            MountPlan::Reuse => {
                if verbose {
                    println!("Reusing attached loop for {mount_name}");
                }
                mount_with_dissect(mount_name, &loop_ref, &mount_point, false, verbose)?;
            }
            // The image behind the loop is not the one we were asked to mount,
            // so the loop is worthless: drop it and attach a fresh one. A loop
            // may survive this - the detach is best-effort - but only an actual
            // image change pays that cost, not every merge.
            MountPlan::Replace => {
                self.unmount(mount_name, verbose)?;
                mount_with_dissect(mount_name, raw_path, &mount_point, true, verbose)?;
            }
            MountPlan::Fresh => {
                mount_with_dissect(mount_name, raw_path, &mount_point, true, verbose)?;
            }
        }

        Ok(PathBuf::from(mount_point))
    }

    fn is_mounted(&self, mount_name: &str) -> bool {
        // The loop-ref symlink alone is not enough. systemd-dissect -U unmounts
        // the mount point but leaves a --loop-ref loop attached, which is what
        // "persistent" means, so the symlink outlives the mount. Reporting
        // mounted on the strength of the symlink makes the caller skip the
        // remount and present an extension whose contents are not there:
        // observed as docker.service vanishing after an unmerge/merge cycle
        // while `avocadoctl ext status` still said merged.
        //
        // KabAdaptor already pairs its loop check with is_mount_active for the
        // same reason.
        let loop_ref_path = format!("/dev/disk/by-loop-ref/{mount_name}");
        Path::new(&loop_ref_path).exists() && is_mount_active(&extension_mount_point(mount_name))
    }

    fn unmount(&self, mount_name: &str, verbose: bool) -> Result<(), SystemdError> {
        let mount_point = extension_mount_point(mount_name);
        unmount_with_dissect(&mount_point, verbose)?;

        // -U leaves the persistent loop attached; drop it too, or the next
        // mount sees a live loop-ref and skips itself.
        detach_loop_ref(mount_name, verbose)?;

        if verbose {
            println!("Unmounted loop for {mount_name}");
        }
        Ok(())
    }

    fn unmount_all(&self) -> Result<(), SystemdError> {
        let loop_ref_dir = "/dev/disk/by-loop-ref";
        if !Path::new(loop_ref_dir).exists() {
            return Ok(());
        }

        let entries = fs::read_dir(loop_ref_dir).map_err(|e| SystemdError::CommandFailed {
            command: "read_dir".to_string(),
            source: e,
        })?;

        for entry in entries.flatten() {
            if let Some(loop_name) = entry.file_name().to_str() {
                println!("Unmounting raw loop: {loop_name}");
                self.unmount(loop_name, false)?;
            }
        }

        Ok(())
    }

    fn needs_remount(&self, mount_name: &str, expected_path: &Path) -> bool {
        if is_test_mode() {
            return false;
        }
        let loop_ref = format!("/dev/disk/by-loop-ref/{mount_name}");
        check_backing_file_changed(Path::new(&loop_ref), expected_path)
    }

    fn type_tag(&self) -> ImageTypeTag {
        ImageTypeTag::Raw
    }
}

// ---------------------------------------------------------------------------
// KabAdaptor — two-phase mount: losetup offset unwrap → systemd-dissect
// ---------------------------------------------------------------------------

/// KAB footer structure (12 bytes, big-endian):
///   u16 symbol_table_len
///   u16 directory_count
///   u32 directory_len
///   u32 marker (0x11223344)
const KAB_SIGNATURE_LEN: u64 = 256;
const KAB_FOOTER_LEN: u64 = 12;
const KAB_DIRECTORY_MARKER: u32 = 0x11223344;

/// Information about an embedded file within a KAB.
struct KabEntry {
    offset: u64,
    len: u64,
}

pub struct KabAdaptor;

impl KabAdaptor {
    /// State directory for tracking outer offset loop devices.
    fn kab_loops_dir() -> String {
        if is_test_mode() {
            let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
            format!("{temp_base}/avocado/kab-loops")
        } else {
            "/run/avocado/kab-loops".to_string()
        }
    }

    /// Parse the KAB footer and directory table to find the "layer.img" entry.
    fn find_image_entry(path: &Path) -> Result<KabEntry, SystemdError> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = fs::File::open(path).map_err(|e| SystemdError::CommandFailed {
            command: format!("open KAB file {}", path.display()),
            source: e,
        })?;

        let file_len = file
            .metadata()
            .map_err(|e| SystemdError::CommandFailed {
                command: "stat KAB file".to_string(),
                source: e,
            })?
            .len();

        if file_len < KAB_SIGNATURE_LEN + KAB_FOOTER_LEN {
            return Err(SystemdError::ConfigurationError {
                message: format!(
                    "KAB file too small: {} bytes ({})",
                    file_len,
                    path.display()
                ),
            });
        }

        // Read footer (12 bytes before the 256-byte signature at EOF)
        let footer_offset = file_len - KAB_SIGNATURE_LEN - KAB_FOOTER_LEN;
        file.seek(SeekFrom::Start(footer_offset))
            .map_err(|e| SystemdError::CommandFailed {
                command: "seek to KAB footer".to_string(),
                source: e,
            })?;

        let mut footer_buf = [0u8; 12];
        file.read_exact(&mut footer_buf)
            .map_err(|e| SystemdError::CommandFailed {
                command: "read KAB footer".to_string(),
                source: e,
            })?;

        let symbol_table_len = u16::from_be_bytes([footer_buf[0], footer_buf[1]]) as u64;
        let directory_count = u16::from_be_bytes([footer_buf[2], footer_buf[3]]) as usize;
        let directory_len =
            u32::from_be_bytes([footer_buf[4], footer_buf[5], footer_buf[6], footer_buf[7]]) as u64;
        let marker =
            u32::from_be_bytes([footer_buf[8], footer_buf[9], footer_buf[10], footer_buf[11]]);

        if marker != KAB_DIRECTORY_MARKER {
            return Err(SystemdError::ConfigurationError {
                message: format!(
                    "Invalid KAB directory marker: 0x{:08X} (expected 0x{:08X}) in {}",
                    marker,
                    KAB_DIRECTORY_MARKER,
                    path.display()
                ),
            });
        }

        // Read directory table
        let dir_offset = footer_offset - symbol_table_len - directory_len;
        file.seek(SeekFrom::Start(dir_offset))
            .map_err(|e| SystemdError::CommandFailed {
                command: "seek to KAB directory".to_string(),
                source: e,
            })?;

        let mut dir_buf = vec![0u8; directory_len as usize];
        file.read_exact(&mut dir_buf)
            .map_err(|e| SystemdError::CommandFailed {
                command: "read KAB directory".to_string(),
                source: e,
            })?;

        // Parse directory entries to find "layer.img"
        // Each entry: u16 name_len, name bytes, u32 offset, u32 len, u8 flags,
        //             u16 user_idx, u16 group_idx, u16 perms
        let mut pos = 0usize;
        for _ in 0..directory_count {
            if pos + 2 > dir_buf.len() {
                break;
            }
            let name_len = u16::from_be_bytes([dir_buf[pos], dir_buf[pos + 1]]) as usize;
            pos += 2;

            if pos + name_len + 11 > dir_buf.len() {
                break;
            }
            let name = &dir_buf[pos..pos + name_len];
            pos += name_len;

            let entry_offset = u32::from_be_bytes([
                dir_buf[pos],
                dir_buf[pos + 1],
                dir_buf[pos + 2],
                dir_buf[pos + 3],
            ]) as u64;
            let entry_len = u32::from_be_bytes([
                dir_buf[pos + 4],
                dir_buf[pos + 5],
                dir_buf[pos + 6],
                dir_buf[pos + 7],
            ]) as u64;
            // skip flags (1), user_idx (2), group_idx (2), perms (2) = 7 bytes
            pos += 8 + 7;

            // Match "/layer.img" or "layer.img"
            let name_str = std::str::from_utf8(name).unwrap_or("");
            let basename = name_str.trim_start_matches('/');
            if basename == "layer.img" {
                return Ok(KabEntry {
                    offset: entry_offset,
                    len: entry_len,
                });
            }
        }

        Err(SystemdError::ConfigurationError {
            message: format!(
                "No layer.img entry found in KAB directory table ({})",
                path.display()
            ),
        })
    }

    /// Create an offset-based loop device exposing the inner image.
    /// Returns the loop device path (e.g. `/dev/loop0`).
    fn setup_offset_loop(kab_path: &Path, entry: &KabEntry) -> Result<PathBuf, SystemdError> {
        let output = ProcessCommand::new("losetup")
            .args([
                "--find",
                "--show",
                "--read-only",
                &format!("--offset={}", entry.offset),
                &format!("--sizelimit={}", entry.len),
                kab_path.to_str().unwrap_or(""),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SystemdError::CommandFailed {
                command: "losetup".to_string(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SystemdError::CommandExitedWithError {
                command: "losetup".to_string(),
                exit_code: output.status.code(),
                stderr: stderr.to_string(),
            });
        }

        let dev = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(PathBuf::from(dev))
    }

    /// Save outer loop device path for later cleanup.
    fn save_loop_state(mount_name: &str, loop_dev: &Path) -> Result<(), SystemdError> {
        let dir = Self::kab_loops_dir();
        fs::create_dir_all(&dir).map_err(|e| SystemdError::CommandFailed {
            command: "create_dir_all kab-loops".to_string(),
            source: e,
        })?;

        let state_path = format!("{dir}/{mount_name}");
        fs::write(&state_path, loop_dev.to_str().unwrap_or("")).map_err(|e| {
            SystemdError::CommandFailed {
                command: "write kab loop state".to_string(),
                source: e,
            }
        })?;
        Ok(())
    }

    /// Read saved outer loop device path.
    fn read_loop_state(mount_name: &str) -> Option<PathBuf> {
        let state_path = format!("{}/{mount_name}", Self::kab_loops_dir());
        fs::read_to_string(&state_path)
            .ok()
            .map(|s| PathBuf::from(s.trim()))
    }

    /// Detach the outer offset loop device.
    fn detach_offset_loop(loop_dev: &Path) -> Result<(), SystemdError> {
        let output = ProcessCommand::new("losetup")
            .args(["-d", loop_dev.to_str().unwrap_or("")])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| SystemdError::CommandFailed {
                command: "losetup -d".to_string(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SystemdError::CommandExitedWithError {
                command: "losetup -d".to_string(),
                exit_code: output.status.code(),
                stderr: stderr.to_string(),
            });
        }

        Ok(())
    }

    /// Remove state file for a mount.
    fn remove_loop_state(mount_name: &str) {
        let state_path = format!("{}/{mount_name}", Self::kab_loops_dir());
        let _ = fs::remove_file(state_path);
    }
}

impl ImageAdaptor for KabAdaptor {
    fn mount(
        &self,
        mount_name: &str,
        kab_path: &Path,
        verbose: bool,
    ) -> Result<PathBuf, SystemdError> {
        let mount_point = extension_mount_point(mount_name);

        // Phase 1: KAB unwrap — parse footer and expose inner image via offset loop
        let entry = Self::find_image_entry(kab_path)?;

        if verbose {
            println!(
                "KAB {mount_name}: layer.img at offset={}, len={}",
                entry.offset, entry.len
            );
        }

        if is_test_mode() {
            // In test mode, skip actual losetup and dissect
            fs::create_dir_all(&mount_point).map_err(|e| SystemdError::CommandFailed {
                command: "create_dir_all".to_string(),
                source: e,
            })?;
            if verbose {
                println!("Test mode: skipping mount for KAB {mount_name}");
            }
            return Ok(PathBuf::from(mount_point));
        }

        let loop_dev = Self::setup_offset_loop(kab_path, &entry)?;
        Self::save_loop_state(mount_name, &loop_dev)?;

        if verbose {
            println!("KAB {mount_name}: offset loop at {}", loop_dev.display());
        }

        // Phase 2: Mount via systemd-dissect (shared path)
        // No --loop-ref since we manage the outer loop ourselves
        if let Err(e) = mount_with_dissect(mount_name, &loop_dev, &mount_point, false, verbose) {
            // Cleanup the offset loop on mount failure
            let _ = Self::detach_offset_loop(&loop_dev);
            Self::remove_loop_state(mount_name);
            return Err(e);
        }

        Ok(PathBuf::from(mount_point))
    }

    fn is_mounted(&self, mount_name: &str) -> bool {
        Self::read_loop_state(mount_name).is_some()
            && is_mount_active(&extension_mount_point(mount_name))
    }

    fn unmount(&self, mount_name: &str, verbose: bool) -> Result<(), SystemdError> {
        let mount_point = extension_mount_point(mount_name);

        if is_test_mode() {
            if verbose {
                println!("Test mode: skipping unmount for KAB {mount_name}");
            }
            Self::remove_loop_state(mount_name);
            return Ok(());
        }

        // Phase 1: systemd-dissect unmount
        unmount_with_dissect(&mount_point, verbose)?;

        // Phase 2: Detach outer offset loop
        if let Some(loop_dev) = Self::read_loop_state(mount_name) {
            Self::detach_offset_loop(&loop_dev)?;
        }
        Self::remove_loop_state(mount_name);

        if verbose {
            println!("Unmounted KAB for {mount_name}");
        }
        Ok(())
    }

    fn unmount_all(&self) -> Result<(), SystemdError> {
        let loops_dir = Self::kab_loops_dir();
        if !Path::new(&loops_dir).exists() {
            return Ok(());
        }

        let entries = fs::read_dir(&loops_dir).map_err(|e| SystemdError::CommandFailed {
            command: "read_dir kab-loops".to_string(),
            source: e,
        })?;

        for entry in entries.flatten() {
            if let Some(mount_name) = entry.file_name().to_str() {
                println!("Unmounting KAB: {mount_name}");
                // Best-effort: log errors but continue
                if let Err(e) = self.unmount(mount_name, false) {
                    eprintln!("Warning: failed to unmount KAB {mount_name}: {e}");
                }
            }
        }

        Ok(())
    }

    fn needs_remount(&self, mount_name: &str, kab_path: &Path) -> bool {
        if is_test_mode() {
            return false;
        }
        if let Some(loop_dev) = Self::read_loop_state(mount_name) {
            check_backing_file_changed(&loop_dev, kab_path)
        } else {
            false
        }
    }

    fn type_tag(&self) -> ImageTypeTag {
        ImageTypeTag::Kab
    }
}

// ---------------------------------------------------------------------------
// Convenience: unmount all persistent mounts across all adaptor types
// ---------------------------------------------------------------------------

pub fn unmount_all_persistent_mounts() -> Result<(), SystemdError> {
    println!("Unmounting all persistent mounts...");
    RawAdaptor.unmount_all()?;
    KabAdaptor.unmount_all()?;
    println!("All persistent mounts unmounted.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_env::ENV_VAR_MUTEX;

    #[test]
    fn loop_size_matches_an_exactly_sized_image() {
        // 121126912 bytes is the docker extension that exposed this: an
        // in-place replacement kept the path, so only the size distinguished
        // the new image from the loop's.
        assert!(loop_size_matches(236576, 121126912));
    }

    #[test]
    fn loop_size_detects_an_image_replaced_with_a_different_size() {
        // The replacement was 121131008 bytes against a loop still sized for
        // the previous 121126912. Before this check the two were
        // indistinguishable and the stale image stayed merged.
        assert!(!loop_size_matches(236576, 121131008));
    }

    #[test]
    fn loop_size_treats_a_smaller_replacement_as_changed() {
        assert!(!loop_size_matches(236576, 4096));
    }

    /// An unmerge/merge cycle on an unchanged image must not attach a loop.
    ///
    /// This is the whole point of the change. `unmerge --unmount` leaves the
    /// loop attached - measured, and not fixable from here - so the merge that
    /// follows sees `loop_ref_present` with unchanged backing. Planning a fresh
    /// loop there is what stranded two loops per cycle on hardware.
    #[test]
    fn an_unchanged_image_behind_an_attached_loop_reuses_it() {
        assert_eq!(plan_mount(true, false), MountPlan::Reuse);
    }

    /// A genuinely replaced image cannot reuse the loop: it exposes the old
    /// content, which is the staleness this whole area exists to prevent.
    #[test]
    fn a_replaced_image_behind_an_attached_loop_replaces_it() {
        assert_eq!(plan_mount(true, true), MountPlan::Replace);
    }

    /// First mount after a boot, and the only case that legitimately attaches.
    #[test]
    fn nothing_attached_mounts_the_file() {
        assert_eq!(plan_mount(false, false), MountPlan::Fresh);
    }

    /// With no loop attached there is nothing to compare against, so a stale
    /// "changed" reading must not divert into a teardown of something absent.
    #[test]
    fn nothing_attached_mounts_the_file_even_if_change_is_reported() {
        assert_eq!(plan_mount(false, true), MountPlan::Fresh);
    }

    #[test]
    fn loop_size_does_not_panic_on_an_absurd_sector_count() {
        // saturating_mul rather than *: a garbage or truncated sysfs read must
        // report "changed" instead of overflowing in release builds.
        assert!(!loop_size_matches(u64::MAX, 121126912));
    }

    #[test]
    fn test_image_type_from_manifest() {
        // None or unknown defaults to Raw
        let raw = ImageType::from_manifest(&None);
        assert_eq!(raw.type_tag(), ImageTypeTag::Raw);

        let raw2 = ImageType::from_manifest(&Some("unknown".to_string()));
        assert_eq!(raw2.type_tag(), ImageTypeTag::Raw);

        // "kab" selects Kab
        let kab = ImageType::from_manifest(&Some("kab".to_string()));
        assert_eq!(kab.type_tag(), ImageTypeTag::Kab);
    }

    #[test]
    fn test_extension_mount_point_test_mode() {
        // Serialize against every other test that mutates AVOCADO_TEST_MODE.
        // Without this, a parallel hitl test can observe AVOCADO_TEST_MODE
        // unset mid-flight when this test's remove_var() races against it.
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let original = std::env::var("AVOCADO_TEST_MODE").ok();

        std::env::set_var("AVOCADO_TEST_MODE", "1");
        let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let mp = extension_mount_point("test-ext");
        assert_eq!(mp, format!("{tmpdir}/avocado/extensions/test-ext"));

        match original {
            Some(val) => std::env::set_var("AVOCADO_TEST_MODE", val),
            None => std::env::remove_var("AVOCADO_TEST_MODE"),
        }
    }

    #[test]
    fn test_parse_scope_from_release_content() {
        let content = r#"ID=_any
SYSEXT_SCOPE=initrd system
"#;
        let scopes = parse_scope_from_release_content(content, "SYSEXT_SCOPE");
        assert_eq!(scopes, vec!["initrd", "system"]);

        let empty = parse_scope_from_release_content(content, "CONFEXT_SCOPE");
        assert!(empty.is_empty());
    }

    #[test]
    fn test_image_type_tag_equality() {
        assert_eq!(ImageTypeTag::Directory, ImageTypeTag::Directory);
        assert_ne!(ImageTypeTag::Raw, ImageTypeTag::Kab);
    }
}
