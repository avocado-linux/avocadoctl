# Changelog

Notable changes to `avocadoctl`, newest first.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow [semantic versioning](https://semver.org/spec/v2.0.0.html), where the
public interface is the CLI and the varlink API, not the Rust API.

Started at 0.12.0. For 0.11.1 and earlier, see the annotated tags and `git log`.

## [Unreleased]

## [0.14.0] - 2026-10-22

### Fixed

- **OS updates write to the disk the system booted from.** A board with Avocado
  on two media (e.g. an SD card and an NVMe) carries every slot label twice,
  and udev's `/dev/disk/by-partlabel` link could point an update at the other
  disk. Slot partitions are now resolved on the disk holding `/`, followed
  through dm-verity or dm-crypt to the backing partition. A label carried
  twice on the booted disk, or only by another disk, is refused, and the
  partition backing `/` is never written (#41).
- Every slot target, including `file:` targets and the sdboot-ab boot
  partition, is resolved before the pending marker or any byte is written, so
  a refused target no longer leaves a half-written slot behind (#41).
- The integration tests no longer run the host's `systemctl daemon-reload`
  (#42).

## [0.13.0] - 2026-10-06

### Changed

- **A rollback reboots straight into the previous slot.** As soon as a pending
  update fails verification and the marker is cleared, the device reboots,
  before the failed slot's extensions are merged, so a bad image never reaches
  userspace. Previously the failed image kept running until something else
  restarted the device (#36).

### Fixed

- **efibootmgr A/B updates stick.** `commit` moves the verified slot to the head
  of `BootOrder` instead of only writing `BootNext`, which the firmware honours
  for a single boot; a committed update no longer reverts to the old slot on the
  next reboot. A slot entry missing from NVRAM (lost to a firmware
  setup-defaults reset) is recreated rather than failing the update (#36).
- `commit` activates a slot entry whose `LOAD_OPTION_ACTIVE` bit is clear, which
  the firmware's `BootOrder` walk would otherwise skip (#36).
- A failed reboot after a rollback fails the extension merge instead of
  reporting success (#36).
- efibootmgr slot entries are matched by partition on the disk holding the ESP,
  not by label alone, so a stale or same-labelled entry (a second disk, a
  reflashed USB stick) can no longer be committed to or rolled back onto (#37).

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

[Unreleased]: https://github.com/avocado-linux/avocadoctl/compare/v0.14.0...HEAD
[0.14.0]: https://github.com/avocado-linux/avocadoctl/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/avocado-linux/avocadoctl/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/avocado-linux/avocadoctl/compare/v0.11.1...v0.12.0
