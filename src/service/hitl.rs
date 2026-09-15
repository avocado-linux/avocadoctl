//! Hardware-in-the-loop: serve an extension's sysroot from the developer's
//! machine over NFS and merge it in place of the installed image.
//!
//! This is the one implementation. The varlink daemon calls it with a quiet
//! output; the `AVOCADO_TEST_MODE` direct path in `commands::hitl` calls it
//! with a real one. It used to exist twice, and the two copies carried the
//! same three bugs, so the fixes had to land twice and would have drifted.
//!
//! The invariants, each of which was violated by the first cut and found on
//! hardware:
//!
//! - **Nothing is masked until the mount is proven live.** A `systemd-mount
//!   --no-block` returns 0 when the job is *queued*; the mount then failed
//!   asynchronously, the code created the sysext symlink anyway, the masking
//!   logic removed the installed extension in favour of an empty directory,
//!   and the services that depended on it stopped. `hitl mount` reported
//!   success throughout.
//! - **Unmount unwinds what merge put inside the mount.** The merge bind-mounts
//!   `extension-release.d` inside the NFS mount (once per hierarchy). A plain
//!   `umount` of the NFS mount then fails EBUSY forever.
//! - **The board is never left unmerged.** `systemd-sysext` is all-or-nothing,
//!   so unmounting needs a full unmerge first. If anything after that fails,
//!   the merge must still run -- the first cut bailed out instead, and took
//!   sshd (an extension) with it. Recovery needed the serial console.
//! - **No `daemon-reload` while unmerged.** Reloading with a confext's socket
//!   unit files absent makes systemd drop the listener; re-merging the files
//!   does not bring it back. `sshd.socket` reported active with nothing on :22.
//! - **Soft mounts.** With `hard`, a server that goes away (laptop lid, Ctrl-C)
//!   blocks every lookup that falls through the merged `/usr` overlay to the
//!   NFS layer -- which is every process on the device, not just the HITL'd
//!   extension. The board froze until the server returned.

use crate::commands::ext;
use crate::commands::hitl;
use crate::config::Config;
use crate::output::OutputManager;
use crate::service::error::AvocadoError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

/// Default NFS port the HITL server listens on. Must match the CLI's
/// `HITL_DEFAULT_PORT`.
pub const DEFAULT_PORT: &str = "12049";

/// Client mount options.
///
/// `soft,softreval`: a dead server returns EIO after `timeo*retrans` instead
/// of hanging the merged overlay; `softreval` keeps serving cached attributes
/// meanwhile so `ls` still works. `timeo` is deciseconds.
///
/// Caching is ON, negative lookups included. The previous options
/// (`lookupcache=none`, `ac*=0..1`) bought coherence by making every path
/// component a server round trip: 177 files took 9 s to stat. Coherence now
/// comes from an explicit sync -- the host asks the device to refresh after a
/// build, and `ext refresh` remounts every HITL mount, which drops the client
/// cache deterministically. Until that sync, the device may not see a file the
/// host added or changed. That is the trade, and it is deliberate.
///
/// `lookupcache=all` rather than `pos` because of what happens when the server
/// goes away. The HITL directory is a lower layer of the merged /usr overlay,
/// so every process on the device does negative lookups into it -- sshd
/// starting up does dozens. With `pos` each one is an RPC that blocks for the
/// soft timeout, and the board is unreachable for minutes even though nothing
/// is hung. Measured: `soft` + `lookupcache=pos` still produced
/// "Connection timed out during banner exchange" on every ssh attempt.
///
/// `timeo` is deciseconds: 3 s, two retries, so an unreachable server costs
/// ~9 s before EIO instead of ~35.
pub const MOUNT_OPTIONS: &str = "vers=4.1,soft,softreval,timeo=30,retrans=2,\
    lookupcache=all,acregmin=3,acregmax=30,acdirmin=3,acdirmax=30,noatime";

/// How the watchdog decides the server is gone: this many consecutive failed
/// TCP connects to the NFS port, this far apart. ~9 s from lid-close to
/// fallback. A TCP connect rather than an NFS operation on purpose -- probing
/// through the mount would itself block on a dead server.
pub const WATCHDOG_INTERVAL_SECS: u64 = 3;
pub const WATCHDOG_FAILURES: u32 = 3;

/// Transient unit name for the watchdog of one extension.
pub fn watchdog_unit(extension: &str) -> String {
    format!("avocado-hitl-watchdog-{extension}")
}

/// Where that unit writes; tmpfs, gone on reboot like the mount it watches.
pub fn watchdog_log(extension: &str) -> String {
    format!("/run/avocado/hitl-watchdog-{extension}.log")
}

/// Whether `name` is a safe HITL extension name: a single path component. It
/// becomes a directory under the HITL root and the NFS export path, and this
/// service runs in the privileged daemon, so a value like `../../../etc` would
/// mount the export over an arbitrary local path (the same value also reaches
/// the watchdog unit and log paths). Reject separators, dot-components,
/// empties, and control characters.
fn is_valid_extension_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(|c| c.is_control())
}

/// Where HITL mounts live. `/run` so a reboot forgets them: the device then
/// boots the installed extension, which is the safe default.
fn base_dir() -> String {
    if std::env::var("AVOCADO_TEST_MODE").is_ok() {
        let temp_base = std::env::var("AVOCADO_TEST_TMPDIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/avocado/hitl")
    } else {
        "/run/avocado/hitl".to_string()
    }
}

fn test_mode() -> bool {
    std::env::var("AVOCADO_TEST_MODE").is_ok()
}

/// Mount `extensions` from `server_ip:server_port` and merge them in place of
/// their installed versions.
pub fn mount(
    server_ip: &str,
    server_port: Option<&str>,
    extensions: &[String],
    output: &OutputManager,
) -> Result<(), AvocadoError> {
    for extension in extensions {
        if !is_valid_extension_name(extension) {
            return Err(AvocadoError::MountFailed {
                extension: extension.clone(),
                reason: "invalid extension name: must be a single path component".to_string(),
            });
        }
    }
    let port = server_port.unwrap_or(DEFAULT_PORT);
    let base = base_dir();
    output.info(
        "HITL Mount",
        &format!("Mounting extensions from {server_ip}:{port}"),
    );

    for extension in extensions {
        let dir = format!("{base}/{extension}");
        output.step("HITL Mount", &format!("Setting up extension: {extension}"));

        fs::create_dir_all(&dir)?;

        if let Err(e) = mount_one(server_ip, port, extension, &dir) {
            // Nothing has been masked yet, so the installed extension is
            // untouched. Leave no empty directory behind: the extension scan
            // treats any directory under the HITL root as an extension.
            let _ = fs::remove_dir(&dir);
            return Err(e);
        }

        let services = ext::scan_extension_for_enable_services(Path::new(&dir), extension);
        if !services.is_empty() {
            output.info(
                "HITL Mount",
                &format!(
                    "Found {} enabled service(s) in extension {}: {}",
                    services.len(),
                    extension,
                    services.join(", ")
                ),
            );
            let _ = hitl::create_service_dropins(extension, &dir, &services, output);
        }
        // The watchdog is the dead-server safety net; a mount with no watchdog
        // is the exact hazard this feature prevents. If it will not start, roll
        // back THIS extension (drop-ins + mount) and fail, rather than return a
        // mount that cannot recover.
        if let Err(e) = start_watchdog(server_ip, port, extension, output) {
            let services = ext::scan_extension_for_enable_services(Path::new(&dir), extension);
            if !services.is_empty() {
                let _ = hitl::cleanup_service_dropins(extension, &services, output);
            }
            let _ = unmount_one(extension, &dir, false, output);
            return Err(e);
        }
        output.progress(&format!("Successfully mounted extension: {extension}"));
    }

    // Everything is merged at this point, so a reload is safe.
    let _ = hitl::systemd_daemon_reload(output);

    // `refresh`, not `merge`: it is the one that runs the lifecycle
    // (enable_services, on_merge) and remounts HITL mounts to drop caches.
    output.info(
        "HITL Mount",
        "Refreshing extensions to apply mounted changes",
    );
    let config = Config::default();
    output.info(
        "Extension Refresh",
        &format!(
            "Starting extension refresh process in {}",
            ext::environment_label()
        ),
    );
    forward(output, crate::service::ext::refresh_extensions(&config))?;
    output.success("Extension Refresh", "Extensions refreshed successfully");
    Ok(())
}

/// Surface a service call's progress lines through the caller's output. The
/// service layer collects them so the daemon can stay quiet; the CLI path
/// wants to see them.
fn forward(
    output: &OutputManager,
    result: Result<Vec<String>, AvocadoError>,
) -> Result<(), AvocadoError> {
    let lines = result?;
    for line in lines {
        output.progress(&line);
    }
    Ok(())
}

/// Mount one export and prove it is live. On any failure the mount point is
/// left unmounted (so the caller can remove the directory) and the error
/// carries the reason the mount unit gave, not just "Job failed".
fn mount_one(server_ip: &str, port: &str, extension: &str, dir: &str) -> Result<(), AvocadoError> {
    let source = format!("{server_ip}:/{extension}");
    let options = format!("port={port},{MOUNT_OPTIONS}");
    let cmd = if test_mode() {
        "mock-systemd-mount"
    } else {
        "systemd-mount"
    };

    // A transient mount unit, so systemd orders it before the network goes
    // down at shutdown. --collect removes the unit once unmounted. NOT
    // --no-block: the exit status has to mean the mount happened.
    let out = ProcessCommand::new(cmd)
        .args(["--collect", "-t", "nfs4", "-o", &options, &source, dir])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason: format!("failed to run {cmd}: {e}"),
        })?;

    if !out.status.success() {
        let mut reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // systemd-mount says "Job failed. See journalctl" and nothing else, and
        // --collect has already removed the unit that knew why. mount.nfs4
        // itself says why ("reason given by server: No such file or
        // directory"), so ask it -- and undo it in the unlikely case it works.
        if !test_mode() {
            if let Ok(probe) = ProcessCommand::new("mount")
                .args(["-t", "nfs4", "-o", &options, &source, dir])
                .output()
            {
                if probe.status.success() {
                    let _ = ProcessCommand::new("umount").arg(dir).output();
                    reason = format!(
                        "{reason} (a direct mount of {source} succeeds; systemd-mount did not)"
                    );
                } else {
                    let why = String::from_utf8_lossy(&probe.stderr).trim().to_string();
                    if !why.is_empty() {
                        reason = why;
                    }
                }
            }
        }
        return Err(AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason,
        });
    }

    if test_mode() {
        return Ok(());
    }

    // The job succeeding is necessary, not sufficient. Check the mount is
    // actually there and actually has content -- an export the server failed
    // to load can still produce a mount that lists as empty.
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    if !is_mounted(&mounts, dir) {
        return Err(AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason: format!("{dir} is not a mount point after systemd-mount reported success"),
        });
    }
    let empty = fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    if empty {
        let _ = ProcessCommand::new("umount").arg(dir).output();
        return Err(AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason: format!(
                "{source} mounted but is empty -- the server is up but exports nothing at /{extension}"
            ),
        });
    }
    Ok(())
}

/// Unmount `extensions` and go back to their installed versions.
///
/// Errors are collected, not returned early: whatever happened, the merge at
/// the end runs. Leaving the board unmerged because one umount failed is how
/// it lost sshd.
pub fn unmount(extensions: &[String], output: &OutputManager) -> Result<(), AvocadoError> {
    unmount_with(extensions, false, output)
}

/// [`unmount`] for a server that is gone: force-unmounts rather than waiting on
/// it. This is what the watchdog runs.
pub fn unmount_lost(extensions: &[String], output: &OutputManager) -> Result<(), AvocadoError> {
    unmount_with(extensions, true, output)
}

fn unmount_with(
    extensions: &[String],
    force: bool,
    output: &OutputManager,
) -> Result<(), AvocadoError> {
    for extension in extensions {
        if !is_valid_extension_name(extension) {
            return Err(AvocadoError::UnmountFailed {
                extension: extension.clone(),
                reason: "invalid extension name: must be a single path component".to_string(),
            });
        }
    }
    let base = base_dir();
    if force {
        eprintln!(
            "{} fallback: begin (force unmount of {})",
            stamp(),
            extensions.join(", ")
        );
    }
    // The forced path runs INSIDE the watchdog unit. Stopping "the watchdog"
    // there is `systemctl stop` on our own unit: SIGTERM before the first
    // umount, and the fallback never happened. Found on hardware.
    if !force {
        for extension in extensions {
            stop_watchdog(extension);
        }
    }

    // Which services got drop-ins. Normally read from the release file on the
    // mount; when the server is gone that read is itself a blocking RPC, so
    // the forced path asks the drop-in files we wrote instead.
    let mut extension_services: Vec<(String, Vec<String>)> = Vec::new();
    for extension in extensions {
        let services = if force {
            services_from_dropins(extension)
        } else {
            let dir = format!("{base}/{extension}");
            ext::scan_extension_for_enable_services(Path::new(&dir), extension)
        };
        if !services.is_empty() {
            extension_services.push((extension.clone(), services));
        }
    }

    // Ordering is circular against a dead server, and the force path breaks
    // the circle. sysext is all-or-nothing, so the overlay -- which holds the
    // NFS mount as a lower dir -- must be unmerged before the NFS can come
    // down cleanly. But the unmerge's release-dir bind cleanup does
    // `umount /run/avocado/hitl/<ext>/usr/lib/extension-release.d`, and
    // resolving that path walks INTO the NFS mount and blocks when the server
    // is gone. So when forced, sever the NFS first with `umount -f -l`: force
    // aborts the in-flight and future RPCs (EIO, not hang), lazy detaches it
    // from the tree even though the overlay holds it busy. The unmerge then
    // sees an empty local directory. Without this the fallback hung inside the
    // unmerge until the server came back -- 90 s in testing, unbounded in life.
    if force {
        for extension in extensions {
            let dir = format!("{base}/{extension}");
            eprintln!("{} fallback: severing dead mount {dir}", stamp());
            sever_dead_mount(&dir, output);
        }
        // Un-poison /usr before anything execs. The dead HITL NFS is a lower
        // layer of the sysext/confext overlays at /usr, /opt and /etc, so with
        // them mounted every uncached lookup through /usr -- including the exec
        // of the systemd-sysext that the unmerge below would run -- blocks in
        // the dead server. (Bisected on hardware: the fallback stalled the
        // instant after the sever, entering `unmerge`, for as long as the
        // server stayed down.) Detaching the overlays by syscall reverts those
        // trees to the base rootfs; the unmerge/merge that follows then runs
        // from a healthy /usr and rebuilds the overlays without the HITL layer.
        eprintln!("{} fallback: detaching sysext overlays", stamp());
        detach_sysext_overlays(output);
    }

    let mut first_error: Option<AvocadoError> = None;

    output.step("HITL Unmount", "Unmerging extensions");
    if force {
        eprintln!("{} fallback: unmerge", stamp());
    }
    // Do NOT bail on an unmerge error: this function's invariant is that it
    // always ends with the installed extensions merged back. A `?` here would
    // skip the unmount loop AND the final merge, leaving the board unmerged --
    // the exact failure the forced path exists to prevent. Record it, keep going.
    if let Err(e) = forward(output, crate::service::ext::unmerge_extensions(false)) {
        output.error(
            "HITL Unmount",
            &format!("unmerge failed, continuing to restore: {e}"),
        );
        first_error.get_or_insert(e);
    }
    if force {
        eprintln!("{} fallback: unmerged; unmounting", stamp());
    }

    for extension in extensions {
        let dir = format!("{base}/{extension}");
        output.step(
            "HITL Unmount",
            &format!("Unmounting extension: {extension}"),
        );
        match unmount_one(extension, &dir, force, output) {
            Ok(()) => output.progress(&format!("Successfully unmounted extension: {extension}")),
            Err(e) => {
                output.error("HITL Unmount", &format!("{e}"));
                first_error.get_or_insert(e);
            }
        }
    }

    for (extension, services) in &extension_services {
        let _ = hitl::cleanup_service_dropins(extension, services, output);
    }

    // Merge FIRST, reload SECOND. A reload while unmerged, with a confext's
    // socket unit files absent, drops the listener for good.
    output.step("HITL Unmount", "Restoring installed extensions");
    if force {
        eprintln!(
            "{} fallback: unmounted; merging installed extensions",
            stamp()
        );
    }
    let config = Config::default();
    output.info(
        "Extension Merge",
        &format!(
            "Starting extension merge process in {}",
            ext::environment_label()
        ),
    );
    let merge_result = forward(output, crate::service::ext::merge_extensions(&config));
    if merge_result.is_ok() {
        output.success("Extension Merge", "Extensions merged successfully");
        // Reload only after a SUCCESSFUL merge. A reload while still unmerged,
        // with a confext's socket unit files absent, drops the listener for good.
        let _ = hitl::systemd_daemon_reload(output);
    }

    match first_error {
        Some(e) => Err(e),
        None => merge_result,
    }
}

/// Unwind everything mounted under `dir`, then `dir` itself, then remove it.
/// Falls back to a lazy unmount rather than fail: an EBUSY here must not
/// stop the caller from re-merging.
///
/// `force` is for a server that is known to be gone: `umount -f` aborts the
/// in-flight NFS requests, where a plain umount would join them in waiting.
fn unmount_one(
    extension: &str,
    dir: &str,
    force: bool,
    output: &OutputManager,
) -> Result<(), AvocadoError> {
    if !Path::new(dir).exists() {
        return Ok(());
    }
    let cmd = if test_mode() { "mock-umount" } else { "umount" };
    let force_args: &[&str] = if force { &["-f", "-l"] } else { &[] };

    if !test_mode() {
        let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
        for nested in nested_mounts(&mounts, dir) {
            output.progress(&format!("Unmounting nested {nested}"));
            let _ = ProcessCommand::new(cmd).arg(&nested).output();
        }
        if !is_mounted(&mounts, dir) {
            let _ = fs::remove_dir(dir);
            return Ok(());
        }
    }

    let out = ProcessCommand::new(cmd)
        .args(force_args)
        .arg(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AvocadoError::UnmountFailed {
            extension: extension.to_string(),
            reason: format!("failed to run {cmd}: {e}"),
        })?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Something still holds it. Detach it from the tree anyway so the
        // extension scan stops seeing it; the kernel frees it when the last
        // user goes. Report it, because a lingering holder is worth knowing.
        let lazy = ProcessCommand::new(cmd).args(["-l", dir]).output();
        let lazy_ok = lazy.map(|o| o.status.success()).unwrap_or(false);
        if !lazy_ok {
            return Err(AvocadoError::UnmountFailed {
                extension: extension.to_string(),
                reason: stderr,
            });
        }
        output.progress(&format!(
            "{dir}: {stderr}; detached lazily, it will be released when its last user exits"
        ));
    }
    let _ = fs::remove_dir(dir);
    Ok(())
}

/// Run the server watchdog for one extension. Blocks; meant to be the body of
/// a transient systemd unit started by [`start_watchdog`]. Returns when the
/// server is judged gone and the fallback has run, or when the mount is no
/// longer there (a normal `hitl unmount` stopped us first).
///
/// The fallback is [`unmount_lost`]: force-unmount, re-merge the installed
/// extension. Without it a server that goes away (laptop lid, Ctrl-C) leaves
/// the device serving EIO from a dead mount and, because the HITL directory is
/// a lower layer of the merged /usr, blocks unrelated processes -- sshd could
/// not complete a handshake. Soft mount options bounded that but did not
/// prevent it: one stuck RPC queues everything behind it.
pub fn watchdog(server_ip: &str, port: &str, extension: &str, output: &OutputManager) {
    use std::net::SocketAddr;
    use std::time::Duration;

    if !is_valid_extension_name(extension) {
        output.progress(&format!(
            "HITL watchdog: refusing invalid extension name '{extension}'"
        ));
        return;
    }

    let dir = format!("{}/{extension}", base_dir());
    let addr: Option<SocketAddr> = format!("{server_ip}:{port}").parse().ok();
    let mut failures = 0u32;
    loop {
        std::thread::sleep(Duration::from_secs(WATCHDOG_INTERVAL_SECS));
        let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
        if !is_mounted(&mounts, &dir) {
            eprintln!("{} {extension}: no longer mounted, exiting", stamp());
            return;
        }
        let up = addr
            .map(|a| nfs_null_probe(a, Duration::from_secs(2)))
            .unwrap_or(false);
        if up {
            failures = 0;
            continue;
        }
        failures += 1;
        eprintln!(
            "{} {extension}: {server_ip}:{port} did not answer ({failures}/{WATCHDOG_FAILURES})",
            stamp()
        );
        if failures >= WATCHDOG_FAILURES {
            eprintln!(
                "{} {extension}: HITL server {server_ip}:{port} is gone; falling back to the installed extension",
                stamp()
            );
            // Verbose on purpose: this runs as a unit, its stdout is the
            // journal, and the step trace is the only way to see where a
            // fallback against a dead server stalls.
            let _ = output;
            let loud = OutputManager::new(true, false);
            match unmount_lost(&[extension.to_string()], &loud) {
                Ok(()) => eprintln!(
                    "{} {extension}: fallback complete, installed extension restored",
                    stamp()
                ),
                Err(e) => eprintln!("{} {extension}: fallback FAILED: {e}", stamp()),
            }
            return;
        }
    }
}

/// Ask the NFS server to answer: an ONC RPC NULL call to program 100003
/// (NFS) version 4 over a fresh TCP connection, with a reply deadline.
///
/// Not a TCP connect. A paused or wedged server still completes TCP
/// handshakes -- the kernel accepts into the backlog on its behalf -- so a
/// connect probe reported a frozen ganesha as healthy for as long as it was
/// frozen. NULL has to be answered by the process itself. And not an NFS
/// operation through the mount: that is what blocks when the server is gone.
///
/// A reply of any shape counts as alive; a server may answer NULL for v4
/// with PROG_MISMATCH and that is still an answer.
pub fn nfs_null_probe(addr: std::net::SocketAddr, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let Ok(mut s) = TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    const XID: u32 = 0x6176_6f63; // "avoc"
    if s.write_all(&rpc_null_call(XID)).is_err() {
        return false;
    }
    // Record mark (4) + xid (4) + msg_type REPLY (4) is the least a reply has.
    let mut buf = [0u8; 12];
    match s.read_exact(&mut buf) {
        Ok(()) => {
            let xid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let msg_type = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
            xid == XID && msg_type == 1
        }
        Err(_) => false,
    }
}

/// RFC 5531 CALL for NFS (100003) v4 procedure 0 (NULL), AUTH_NONE, with the
/// record mark TCP transport needs. 44 bytes.
fn rpc_null_call(xid: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(40);
    // xid, CALL, rpcvers 2, prog, vers, proc NULL, cred AUTH_NONE(flavor,len), verf AUTH_NONE(flavor,len)
    for word in [xid, 0, 2, 100003, 4, 0, 0, 0, 0, 0] {
        body.extend_from_slice(&word.to_be_bytes());
    }
    let mut msg = Vec::with_capacity(body.len() + 4);
    msg.extend_from_slice(&((body.len() as u32) | 0x8000_0000).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// The services `hitl mount` wrote drop-ins for, read back from the drop-in
/// files themselves. For the forced path: the mount is dead, so the release
/// file that lists them cannot be read.
fn services_from_dropins(extension: &str) -> Vec<String> {
    let systemd_dir = if test_mode() {
        let temp_base = std::env::var("AVOCADO_TEST_TMPDIR")
            .or_else(|_| std::env::var("TMPDIR"))
            .unwrap_or_else(|_| "/tmp".to_string());
        format!("{temp_base}/run/systemd/system")
    } else {
        "/run/systemd/system".to_string()
    };
    let marker = format!("10-hitl-{extension}.conf");
    let mut v = Vec::new();
    if let Ok(entries) = fs::read_dir(&systemd_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(unit) = name.strip_suffix(".d") {
                if e.path().join(&marker).exists() {
                    v.push(unit.to_string());
                }
            }
        }
    }
    v.sort();
    v
}

/// Start the watchdog as a transient unit so it outlives this call and the
/// daemon's socket-activated lifetime. `--collect` removes it when it exits.
fn start_watchdog(
    server_ip: &str,
    port: &str,
    extension: &str,
    output: &OutputManager,
) -> Result<(), AvocadoError> {
    if test_mode() {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "avocadoctl".to_string());
    let unit = watchdog_unit(extension);
    // A previous one may still be around from a mount that was not unmounted
    // through us (reboot loses /run, but not a daemon restart).
    let _ = ProcessCommand::new("systemctl")
        .args(["stop", &unit])
        .output();
    // Log to a tmpfs file, not the journal: on the boards this was built on,
    // journald kept nothing for transient units, and the fallback's step
    // trace is the only record of a server loss.
    let log = format!("append:{}", watchdog_log(extension));
    let r = ProcessCommand::new("systemd-run")
        .args([
            "--unit",
            &unit,
            "--collect",
            "--quiet",
            "--description",
            &format!("HITL server watchdog for {extension}"),
            "--property",
            &format!("StandardOutput={log}"),
            "--property",
            &format!("StandardError={log}"),
            &exe,
            "hitl",
            "watchdog",
            "--server-ip",
            server_ip,
            "--server-port",
            port,
            "--extension",
            extension,
        ])
        .output();
    match r {
        Ok(o) if o.status.success() => {
            output.progress(&format!("Started {unit}"));
            Ok(())
        }
        Ok(o) => Err(AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason: format!(
                "could not start watchdog {unit}: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        }),
        Err(e) => Err(AvocadoError::MountFailed {
            extension: extension.to_string(),
            reason: format!("could not start watchdog {unit}: {e}"),
        }),
    }
}

fn stop_watchdog(extension: &str) {
    if test_mode() {
        return;
    }
    let _ = ProcessCommand::new("systemctl")
        .args(["stop", &watchdog_unit(extension)])
        .output();
}

/// Seconds since boot, for the watchdog log; no clock dependency.
fn stamp() -> String {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|u| u.split_whitespace().next().map(|t| format!("[{t:>9}]")))
        .unwrap_or_default()
}

/// Force + lazy unmount everything at and under `dir`, deepest first, for a
/// server that is gone. `-f` aborts the NFS RPCs so the calls return instead
/// of joining the hang; `-l` detaches even though the sysext overlay holds the
/// mount busy. Does not remove the directory -- the post-unmerge pass does,
/// once the overlay that pins it is down.
fn sever_dead_mount(dir: &str, output: &OutputManager) {
    if test_mode() {
        return;
    }
    let dir = dir.trim_end_matches('/');
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    if !is_mounted(&mounts, dir) {
        return;
    }
    // umount2(MNT_DETACH) -- the raw syscall, NOT `umount -f -l`. Everything
    // about the exec version was wrong against a dead server:
    //
    //   - execing `umount` is a lookup of /usr/bin/umount, and /usr is the
    //     merged overlay whose lower layers include this dead NFS; a cache
    //     miss on that lookup blocks in the server the sever is trying to
    //     escape.
    //   - `-f` (force) sends the server an abort RPC. There is no server.
    //
    // MNT_DETACH (lazy) does neither. It unlinks the vfsmount from the
    // namespace -- a pure VFS operation, no server round trip, no exec -- and
    // takes the whole subtree (the NFS mount and the extension-release.d binds
    // inside it) with it. /proc/mounts then shows nothing under `dir`, so the
    // unmerge that follows finds no binds to clean up and its
    // `systemd-sysext unmerge` restores /usr to the base rootfs, healthy
    // again, before anything has to look a path up through the dead layer.
    match detach(dir) {
        Ok(()) => output.progress(&format!("sever {dir}: detached (MNT_DETACH)")),
        Err(e) => output.progress(&format!("sever {dir}: detach failed ({e}); continuing")),
    }
}

/// MNT_DETACH the sysext/confext overlays so /usr, /opt and /etc revert to the
/// base rootfs. See the call site: this is what lets the rebuild exec its
/// tools after a HITL server has died under the overlay.
///
/// Lazy on purpose. These trees are in use by every process; a lazy detach
/// unlinks the overlay from the namespace (new lookups fall through to the
/// base rootfs, healthy) while existing open files keep the old mount alive
/// until they close. systemd-sysext's own state under /run is left as is --
/// the merge that follows reconciles it.
fn detach_sysext_overlays(output: &OutputManager) {
    if test_mode() {
        return;
    }
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    // Only overlays whose lowerdir names the sysext staging -- never a bare
    // /usr that is not overlaid. Deepest path first so a `.systemd-sysext`
    // marker mount comes off before the tree it sits in.
    let mut targets: Vec<String> = mounts
        .lines()
        .filter_map(|l| {
            // /proc/mounts fields: device mountpoint fstype options ...
            let mut f = l.split_whitespace();
            let _dev = f.next();
            let target = f.next().map(unescape_mount_path);
            let fstype = f.next();
            let opts = f.next().unwrap_or("");
            match (target, fstype) {
                (Some(t), Some("overlay"))
                    if opts.contains("/run/systemd/sysext/")
                        && (t.starts_with("/usr")
                            || t.starts_with("/opt")
                            || t.starts_with("/etc")) =>
                {
                    Some(t)
                }
                _ => None,
            }
        })
        .collect();
    targets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    for t in targets {
        match detach(&t) {
            Ok(()) => output.progress(&format!("detached overlay {t}")),
            Err(e) => output.progress(&format!("detach {t}: {e} (continuing)")),
        }
    }
}

/// umount2(path, MNT_DETACH) -- lazy detach with no exec and no server round
/// trip. The one primitive the dead-server fallback can rely on.
fn detach(path: &str) -> Result<(), std::io::Error> {
    let c = std::ffi::CString::new(path)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: c is a valid NUL-terminated path; MNT_DETACH returns no buffers.
    if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whether `dir` appears as a mount target in `/proc/mounts` text.
fn is_mounted(proc_mounts: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    proc_mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|t| unescape_mount_path(t) == dir)
}

/// Mount targets strictly below `root`, deepest first, so each can be
/// unmounted before its parent.
fn nested_mounts(proc_mounts: &str, root: &str) -> Vec<String> {
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let mut v: Vec<String> = proc_mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(unescape_mount_path)
        .filter(|t| t.starts_with(&prefix))
        .collect();
    v.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    v.dedup();
    v
}

/// `/proc/mounts` escapes space, tab, newline and backslash as `\ooo`.
pub(crate) fn unescape_mount_path(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Where the extension's HITL directory would be; for callers that report on
/// state without changing it.
#[allow(dead_code)]
pub fn mount_dir(extension: &str) -> PathBuf {
    PathBuf::from(base_dir()).join(extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_name_must_be_a_single_component() {
        // Ordinary names are accepted.
        assert!(is_valid_extension_name("vmm"));
        assert!(is_valid_extension_name("my-ext_1.2.3"));
        // A privileged caller cannot escape the HITL root or the export path.
        assert!(!is_valid_extension_name("../../../etc"));
        assert!(!is_valid_extension_name("a/b"));
        assert!(!is_valid_extension_name(".."));
        assert!(!is_valid_extension_name("."));
        assert!(!is_valid_extension_name(""));
        assert!(!is_valid_extension_name("a\\b"));
        assert!(!is_valid_extension_name("bad\nname"));
    }

    const MOUNTS: &str = "\
10.10.0.10:/vmm /run/avocado/hitl/vmm nfs4 rw,relatime 0 0
tmpfs /run/avocado/hitl/vmm/usr/lib/extension-release.d tmpfs rw 0 0
tmpfs /run/avocado/hitl/vmm/etc/extension-release.d tmpfs rw 0 0
/dev/loop3 /run/avocado/extensions/vmm-0.1.6 erofs ro 0 0
tmpfs /run/avocado/hitl/with\\040space tmpfs rw 0 0
";

    /// The bug: umount of the NFS mount failed EBUSY because the merge had
    /// bind-mounted release dirs inside it. They must come off first, deepest
    /// first, and the sibling extension mount must not be touched.
    #[test]
    fn nested_mounts_are_found_deepest_first_and_siblings_are_not() {
        let nested = nested_mounts(MOUNTS, "/run/avocado/hitl/vmm");
        assert_eq!(
            nested,
            vec![
                "/run/avocado/hitl/vmm/usr/lib/extension-release.d".to_string(),
                "/run/avocado/hitl/vmm/etc/extension-release.d".to_string(),
            ]
        );
        assert!(nested_mounts(MOUNTS, "/run/avocado/hitl/other").is_empty());
        // trailing slash is not a different directory
        assert_eq!(nested_mounts(MOUNTS, "/run/avocado/hitl/vmm/").len(), 2);
    }

    #[test]
    fn is_mounted_matches_the_target_exactly() {
        assert!(is_mounted(MOUNTS, "/run/avocado/hitl/vmm"));
        assert!(is_mounted(MOUNTS, "/run/avocado/hitl/vmm/"));
        // a prefix is not a mount
        assert!(!is_mounted(MOUNTS, "/run/avocado/hitl"));
        // an empty directory created for a mount that never happened
        assert!(!is_mounted(MOUNTS, "/run/avocado/hitl/ghost"));
    }

    #[test]
    fn proc_mounts_octal_escapes_are_decoded() {
        assert_eq!(unescape_mount_path("/a\\040b"), "/a b");
        assert_eq!(unescape_mount_path("/plain"), "/plain");
        assert!(is_mounted(MOUNTS, "/run/avocado/hitl/with space"));
    }

    #[test]
    fn rpc_null_call_is_well_formed() {
        let m = rpc_null_call(0xdead_beef);
        assert_eq!(m.len(), 44);
        assert_eq!(&m[0..4], &[0x80, 0, 0, 40]); // last fragment, 40-byte body
        assert_eq!(&m[4..8], &0xdead_beefu32.to_be_bytes());
        assert_eq!(&m[8..12], &[0, 0, 0, 0]); // CALL
        assert_eq!(&m[12..16], &[0, 0, 0, 2]); // RPC v2
        assert_eq!(&m[16..20], &100003u32.to_be_bytes()); // NFS
        assert_eq!(&m[20..24], &[0, 0, 0, 4]); // v4
        assert_eq!(&m[24..28], &[0, 0, 0, 0]); // NULL
    }

    /// Watchdog v1's blind spot: a listener that accepts and never answers.
    /// TCP connect called it alive for as long as it stayed frozen.
    #[test]
    fn a_silent_listener_is_not_alive() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = std::time::Instant::now();
        assert!(!nfs_null_probe(addr, std::time::Duration::from_millis(300)));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "the probe must respect its deadline"
        );
        drop(l);
    }

    #[test]
    fn a_closed_port_is_not_alive() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        assert!(!nfs_null_probe(addr, std::time::Duration::from_millis(300)));
    }

    /// Any RPC REPLY carrying our xid means the server process answered.
    #[test]
    fn a_replying_server_is_alive() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut req = [0u8; 44];
            c.read_exact(&mut req).unwrap();
            let mut reply = Vec::new();
            reply.extend_from_slice(&(24u32 | 0x8000_0000).to_be_bytes());
            reply.extend_from_slice(&req[4..8]); // xid
            reply.extend_from_slice(&1u32.to_be_bytes()); // REPLY
            reply.extend_from_slice(&[0u8; 16]);
            c.write_all(&reply).unwrap();
        });
        assert!(nfs_null_probe(addr, std::time::Duration::from_secs(2)));
        h.join().unwrap();
    }

    /// The forced path learns the services from the drop-ins it wrote, not
    /// from the (dead) mount.
    #[test]
    fn forced_unmount_reads_services_from_dropins() {
        // Serialize with every other test that toggles process-global env vars;
        // this test sets AVOCADO_TEST_MODE/TMPDIR, which others read.
        let _guard = crate::commands::test_env::ENV_VAR_MUTEX.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("AVOCADO_TEST_MODE", "1");
        std::env::set_var("AVOCADO_TEST_TMPDIR", tmp.path());
        let sysd = tmp.path().join("run/systemd/system");
        for unit in ["nginx.service", "prometheus.service"] {
            let d = sysd.join(format!("{unit}.d"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("10-hitl-my-ext.conf"), "[Unit]\n").unwrap();
        }
        // another extension's drop-in must not be picked up
        let other = sysd.join("sshd.service.d");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("10-hitl-other.conf"), "[Unit]\n").unwrap();

        assert_eq!(
            services_from_dropins("my-ext"),
            vec![
                "nginx.service".to_string(),
                "prometheus.service".to_string()
            ]
        );
        std::env::remove_var("AVOCADO_TEST_TMPDIR");
        std::env::remove_var("AVOCADO_TEST_MODE");
    }

    /// The options are a contract with the failure modes above; a change to
    /// `hard` or `lookupcache=none` should be deliberate.
    #[test]
    fn mount_options_are_soft_and_cached() {
        assert!(MOUNT_OPTIONS.contains("soft"));
        assert!(!MOUNT_OPTIONS.split(',').any(|o| o == "hard"));
        // negative lookups cached: a dead server must not turn every miss
        // through the merged /usr into a blocking RPC
        assert!(MOUNT_OPTIONS.contains("lookupcache=all"));
    }
}
