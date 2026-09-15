# Changelog

Notable changes to `avocadoctl`, newest first.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [semantic versioning](https://semver.org/spec/v2.0.0.html), where the
public interface is the CLI and the varlink API, not the Rust API.

Started at 0.12.0. For 0.11.1 and earlier, see the annotated tags and `git log`.

## [Unreleased]

## [0.12.0] - 2026-09-15

### Added

- **OS-update `file:` artifact targets.** `apply`/streaming updates can write an
  artifact into a file inside a partition's filesystem (`file:<partlabel>:<path>`,
  e.g. a UKI to `file:efi:EFI/Linux/…`), not only to a whole partition or device
  offset. The write is atomic (temp + fsync + rename over the target) and read
  back by streaming SHA-256, and it streams in bounded chunks so a large boot
  artifact never has to fit in memory.
- **HITL dead-server survival.** A per-extension watchdog probes the NFS server
  (RPC NULL) and, when it is gone, force-unmounts, detaches the sysext/confext
  overlays by syscall, and re-merges the installed extensions so the device
  recovers on its own instead of freezing on the dead mount. When several
  extensions come from one server, one coordinated fallback recovers them all in
  a single pass, serialized by a lock so two watchdogs cannot race the
  unmerge/merge.
- **OTA / merge narration with an extension diff.** Merge and update output is
  now structured English with a `~ updated / + added / = unchanged` diff, merge
  counts, and a one-line `✓`/`✗` result; nudge/fleet updates narrate on-device
  through the daemon journal. Per-command detail moves behind `--verbose`.

### Changed

- **`ext list` lists by name.** Extensions are listed from the active runtime
  manifest by name and version (deduped) rather than by content-addressed image
  UUID; loose `<name>-<version>.raw` dev images are still shown so `list` agrees
  with `enable`. `list` stays a working diagnostic on a device whose manifest
  cannot be read (it warns and falls back to a directory listing).
- **Readable RPC errors.** Non-verbose CLI output humanizes varlink errors
  (`org.avocado.Runtimes.RuntimeNotFound: …` → `Error: Runtime not found (id: …)`);
  `--verbose` still shows the raw debug form.
- After `ext enable`/`disable`, a hint to run `avocadoctl ext refresh` is shown
  in normal output (not only `--verbose`).

### Fixed

- The OS-update pending marker is written before the activating artifact, so on
  a sorting bootloader (highest-versioned UKI wins, where the file write *is* the
  activation) a failure can no longer leave a new OS live with no marker to
  verify or roll back.
- An absent `file:` partlabel is a hard error rather than a silent no-op, so a
  typo (`file:esp` on a board labeled `ESP`) fails loudly instead of rebooting
  onto the old OS.
- `commit` slot actions from the manifest reach the pending marker, so the
  post-boot finalize (e.g. `avocado-bls bless`) runs.
- Device `maj:min` is decoded with the kernel encoding, so a high-minor device
  can no longer alias another and be written to by mistake.
- HITL refresh decodes `/proc/mounts` octal escapes before unmounting, so an
  extension whose name contains a space is actually unmounted.

[Unreleased]: https://github.com/avocado-linux/avocadoctl/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/avocado-linux/avocadoctl/compare/v0.11.1...v0.12.0
