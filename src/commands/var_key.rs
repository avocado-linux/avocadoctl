//! `avocadoctl var-key`: manage the recovery keyslot of the encrypted /var.
//!
//! The /var LUKS2 container is opened in the initramfs (cryptsetup-var) from a
//! hardware-bound keyslot (CAAM, TPM2) or the Argon2id fallback; none of those
//! passphrases are reachable from the running system, so keyslot changes here
//! are authorised with the *volume key* that cryptsetup-var linked into the
//! root user keyring at open time (`--link-vk-to-keyring`). Nothing this
//! command adds is stored on the device: the operator (avocado-cli) derives the
//! recovery passphrase for this unit and pipes it in; the header only records
//! that a recovery slot exists, as an `avocado-recovery` token.
use crate::output::OutputManager;
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};

/// Kernel key the initramfs links the volume key under (cryptsetup-var.sh).
pub const VOLUME_KEY_KEYRING: &str = "%user:cryptsetup:var";
pub const RECOVERY_TOKEN_TYPE: &str = "avocado-recovery";
const MAP_NAME: &str = "var";

pub fn create_command() -> Command {
    Command::new("var-key")
        .about("Manage the recovery keyslot of the encrypted /var")
        .subcommand_required(true)
        .subcommand(
            Command::new("enroll")
                .about("Add a recovery keyslot from a passphrase on stdin (or --key-file); replaces an existing one")
                .arg(
                    Arg::new("key-file")
                        .long("key-file")
                        .value_name("FILE")
                        .help("Read the passphrase from FILE instead of stdin"),
                )
                .arg(
                    Arg::new("kind")
                        .long("kind")
                        .value_name("KIND")
                        .default_value("hmac-sha256-uid")
                        .help("How the passphrase was derived, recorded in the token for the operator's benefit"),
                ),
        )
        .subcommand(Command::new("list").about("Show the /var keyslots and what unlocks them"))
        .subcommand(
            Command::new("remove")
                .about("Remove the recovery keyslot")
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("Required: confirms the recovery slot is to be removed"),
                ),
        )
}

pub fn handle_command(matches: &ArgMatches, output: &OutputManager) {
    let result = match matches.subcommand() {
        Some(("enroll", m)) => {
            let passphrase = match m.get_one::<String>("key-file") {
                Some(f) => fs::read(f).map_err(|e| format!("cannot read {f}: {e}")),
                None => {
                    let mut buf = Vec::new();
                    std::io::stdin()
                        .read_to_end(&mut buf)
                        .map(|_| buf)
                        .map_err(|e| format!("cannot read passphrase from stdin: {e}"))
                }
            };
            let kind = m.get_one::<String>("kind").cloned().unwrap_or_default();
            passphrase.and_then(|p| enroll(&SystemRunner, &p, &kind))
        }
        Some(("list", _)) => list(&SystemRunner).map(|(dev, h)| {
            if output.is_json() {
                println!("{}", list_json(&dev, &h));
            } else {
                println!("device: {dev}");
                for r in describe(&h) {
                    println!("{r}");
                }
            }
            "listed".to_string()
        }),
        Some(("remove", m)) => {
            if !m.get_flag("yes") {
                Err("refusing without --yes: removing the recovery slot leaves /var recoverable only through the hardware keyslot".to_string())
            } else {
                remove(&SystemRunner)
            }
        }
        // clap has subcommand_required(true), so this is unreachable in
        // practice; refusing loudly keeps a future subcommand from silently
        // reporting success.
        other => Err(format!(
            "unknown var-key subcommand {:?}",
            other.map(|(name, _)| name).unwrap_or("<none>")
        )),
    };
    match result {
        Ok(msg) => output.success("var-key", &msg),
        Err(e) => {
            output.error("var-key", &e);
            std::process::exit(1);
        }
    }
}

/// Everything that touches the system goes through here so the logic is
/// testable against recorded outputs.
pub trait Runner {
    /// Run `cryptsetup <args>`, feeding `stdin` if given; Ok(stdout) on exit 0.
    fn cryptsetup(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String>;
    /// Write `content` to a private temp file and return its path.
    fn secret_file(&self, content: &[u8]) -> Result<PathBuf, String>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn cryptsetup(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<String, String> {
        let mut cmd = Proc::new("cryptsetup");
        cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot run cryptsetup: {e}"))?;
        if let Some(data) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            if let Err(e) = pipe.write_all(data) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot feed cryptsetup: {e}"));
            }
        }
        let out = child
            .wait_with_output()
            .map_err(|e| format!("cryptsetup failed: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "cryptsetup {} failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    fn secret_file(&self, content: &[u8]) -> Result<PathBuf, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = Path::new("/run/avocado");
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        // Random name and O_EXCL: a secret never lands on a path someone else
        // could have guessed and pre-created as a symlink.
        let path = dir.join(format!("var-key.{}", uuid::Uuid::new_v4()));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        f.write_all(content)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }
}

/// The partition under /dev/mapper/var, from `cryptsetup status`.
pub fn backing_device(r: &dyn Runner) -> Result<String, String> {
    let status = r.cryptsetup(&["status", MAP_NAME], None).map_err(|_| {
        "/var is not an open LUKS mapping (is this image built with var.encrypt?)".to_string()
    })?;
    status
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("device:")
                .map(|d| d.trim().to_string())
        })
        .filter(|d| !d.is_empty())
        .ok_or_else(|| "cryptsetup status did not report a backing device".to_string())
}

#[derive(Debug, Default, PartialEq)]
pub struct Header {
    pub keyslots: Vec<u32>,
    /// (token id, type, keyslots the token references)
    pub tokens: Vec<(u32, String, Vec<u32>)>,
}

/// Parse the Keyslots/Tokens sections of `cryptsetup luksDump`.
pub fn parse_dump(dump: &str) -> Header {
    let mut h = Header::default();
    let mut section = "";
    for line in dump.lines() {
        if !line.starts_with([' ', '\t']) {
            section = line.trim_end_matches(':');
            continue;
        }
        let t = line.trim();
        match section {
            "Keyslots" => {
                if let Some((id, rest)) = t.split_once(':') {
                    if rest.trim().starts_with("luks") {
                        if let Ok(n) = id.trim().parse() {
                            h.keyslots.push(n);
                        }
                    }
                }
            }
            "Tokens" => {
                if line.starts_with("  ") && !line.starts_with('\t') {
                    if let Some((id, ty)) = t.split_once(':') {
                        if let Ok(n) = id.trim().parse() {
                            h.tokens.push((n, ty.trim().to_string(), Vec::new()));
                        }
                    }
                } else if let Some(ks) = t.strip_prefix("Keyslot:") {
                    if let (Some(last), Ok(n)) = (h.tokens.last_mut(), ks.trim().parse()) {
                        last.2.push(n);
                    }
                }
            }
            _ => {}
        }
    }
    h
}

fn header(r: &dyn Runner, dev: &str) -> Result<Header, String> {
    Ok(parse_dump(&r.cryptsetup(&["luksDump", dev], None)?))
}

fn recovery_token(h: &Header) -> Option<&(u32, String, Vec<u32>)> {
    h.tokens.iter().find(|(_, ty, _)| ty == RECOVERY_TOKEN_TYPE)
}

fn free_slot(h: &Header) -> Option<u32> {
    (0..32).find(|n| !h.keyslots.contains(n))
}

pub fn enroll(r: &dyn Runner, passphrase: &[u8], kind: &str) -> Result<String, String> {
    if passphrase.is_empty() {
        return Err("empty passphrase".to_string());
    }
    let dev = backing_device(r)?;
    let h = header(r, &dev)?;
    let slot = free_slot(&h).ok_or_else(|| {
        "all 32 LUKS keyslots on /var are in use; remove one before enrolling".to_string()
    })?;
    let keyfile = r.secret_file(passphrase)?;
    let slot_s = slot.to_string();
    let added = r.cryptsetup(
        &[
            "luksAddKey",
            "--volume-key-keyring",
            VOLUME_KEY_KEYRING,
            "--new-keyfile",
            keyfile.to_str().unwrap_or_default(),
            "--new-key-slot",
            &slot_s,
            "--pbkdf",
            "pbkdf2",
            "--pbkdf-force-iterations",
            "1000",
            "--batch-mode",
            &dev,
        ],
        None,
    );
    let _ = fs::remove_file(&keyfile);
    added.map_err(|e| {
        format!(
            "{e} (was /var opened by cryptsetup-var with the volume key linked to the keyring?)"
        )
    })?;

    let token = serde_json::json!({
        "type": RECOVERY_TOKEN_TYPE,
        "keyslots": [slot.to_string()],
        "kind": kind,
    })
    .to_string();
    let token_file = r.secret_file(token.as_bytes())?;
    let imported = r.cryptsetup(
        &[
            "token",
            "import",
            "--json-file",
            token_file.to_str().unwrap_or_default(),
            &dev,
        ],
        None,
    );
    let _ = fs::remove_file(&token_file);
    if let Err(e) = imported {
        let _ = r.cryptsetup(&["luksKillSlot", "--batch-mode", &dev, &slot_s], None);
        return Err(format!("{e}; keyslot {slot} removed again"));
    }

    // Retire the previous recovery slot only once the new token is on disk: a
    // failed import above must leave the existing recovery path usable, not
    // strip the device of every recovery slot it had.
    if let Some((tid, _, slots)) = recovery_token(&h).cloned() {
        r.cryptsetup(
            &["token", "remove", "--token-id", &tid.to_string(), &dev],
            None,
        )?;
        for s in slots {
            r.cryptsetup(
                &["luksKillSlot", "--batch-mode", &dev, &s.to_string()],
                None,
            )?;
        }
    }
    Ok(format!(
        "recovery keyslot {slot} enrolled on {dev} ({kind})"
    ))
}

pub fn remove(r: &dyn Runner) -> Result<String, String> {
    let dev = backing_device(r)?;
    let h = header(r, &dev)?;
    let (tid, _, slots) = recovery_token(&h)
        .cloned()
        .ok_or_else(|| "no recovery keyslot on /var".to_string())?;
    r.cryptsetup(
        &["token", "remove", "--token-id", &tid.to_string(), &dev],
        None,
    )?;
    for s in &slots {
        r.cryptsetup(
            &["luksKillSlot", "--batch-mode", &dev, &s.to_string()],
            None,
        )?;
    }
    Ok(format!("recovery keyslot {:?} removed from {dev}", slots))
}

/// Token types that reference `slot`.
fn unlocks(h: &Header, slot: u32) -> Vec<&str> {
    h.tokens
        .iter()
        .filter(|(_, _, ks)| ks.contains(&slot))
        .map(|(_, ty, _)| ty.as_str())
        .collect()
}

/// One line per keyslot: what unlocks it, from the tokens that reference it.
pub fn describe(h: &Header) -> Vec<String> {
    h.keyslots
        .iter()
        .map(|s| {
            let via = unlocks(h, *s);
            let what = if via.is_empty() {
                "passphrase (Argon2id recovery / derived key)".to_string()
            } else {
                via.join(", ")
            };
            format!("slot {s}: {what}")
        })
        .collect()
}

pub fn list(r: &dyn Runner) -> Result<(String, Header), String> {
    let dev = backing_device(r)?;
    let h = header(r, &dev)?;
    Ok((dev, h))
}

/// Machine-readable form of `list`.
pub fn list_json(dev: &str, h: &Header) -> serde_json::Value {
    serde_json::json!({
        "device": dev,
        "keyslots": h
            .keyslots
            .iter()
            .map(|s| serde_json::json!({ "slot": s, "unlocked_by": unlocks(h, *s) }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const DUMP: &str = "LUKS header information\nVersion:       \t2\n\nKeyslots:\n  0: luks2\n\tKey:        512 bits\n  1: luks2\n\tKey:        512 bits\nTokens:\n  0: avocado-hwkey\n\tKeyslot:    1\nDigests:\n  0: pbkdf2\n";
    const DUMP_WITH_RECOVERY: &str = "Keyslots:\n  0: luks2\n  1: luks2\n  2: luks2\nTokens:\n  0: avocado-hwkey\n\tKeyslot:    1\n  1: avocado-recovery\n\tKeyslot:    2\nDigests:\n  0: pbkdf2\n";

    struct Fake {
        dump: &'static str,
        calls: RefCell<Vec<String>>,
        fail_import: bool,
    }
    impl Fake {
        fn new(dump: &'static str) -> Self {
            Fake {
                dump,
                calls: RefCell::new(vec![]),
                fail_import: false,
            }
        }
    }
    impl Runner for Fake {
        fn cryptsetup(&self, args: &[&str], _stdin: Option<&[u8]>) -> Result<String, String> {
            self.calls.borrow_mut().push(args.join(" "));
            match args[0] {
                "status" => Ok("/dev/mapper/var is active and is in use.\n  type:    LUKS2\n  device:  /dev/mmcblk2p9\n".into()),
                "luksDump" => Ok(self.dump.into()),
                "token" if args[1] == "import" && self.fail_import => Err("token import failed".into()),
                _ => Ok(String::new()),
            }
        }
        fn secret_file(&self, content: &[u8]) -> Result<PathBuf, String> {
            let p = std::env::temp_dir().join(format!(
                "avocadoctl-varkey-test-{}-{}",
                std::process::id(),
                self.calls.borrow().len()
            ));
            fs::write(&p, content).unwrap();
            Ok(p)
        }
    }

    #[test]
    fn parses_keyslots_and_tokens() {
        let h = parse_dump(DUMP_WITH_RECOVERY);
        assert_eq!(h.keyslots, vec![0, 1, 2]);
        assert_eq!(
            h.tokens,
            vec![
                (0, "avocado-hwkey".into(), vec![1]),
                (1, "avocado-recovery".into(), vec![2])
            ]
        );
        assert_eq!(free_slot(&h), Some(3));
        assert_eq!(describe(&h)[2], "slot 2: avocado-recovery");
        let full = Header {
            keyslots: (0..32).collect(),
            tokens: vec![],
        };
        assert_eq!(free_slot(&full), None);
    }

    #[test]
    fn enroll_uses_the_linked_volume_key_and_records_a_token() {
        let f = Fake::new(DUMP);
        let msg = enroll(&f, b"secret", "hmac-sha256-uid").unwrap();
        assert!(msg.contains("keyslot 2"), "{msg}");
        let calls = f.calls.borrow();
        let add = calls.iter().find(|c| c.starts_with("luksAddKey")).unwrap();
        assert!(
            add.contains("--volume-key-keyring %user:cryptsetup:var"),
            "{add}"
        );
        assert!(add.contains("--new-key-slot 2 --pbkdf pbkdf2"), "{add}");
        assert!(add.ends_with("/dev/mmcblk2p9"), "{add}");
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("token import --json-file")),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|c| c.starts_with("luksKillSlot")),
            "nothing to replace"
        );
    }

    #[test]
    fn enroll_replaces_a_previous_recovery_slot_after_adding_the_new_one() {
        let f = Fake::new(DUMP_WITH_RECOVERY);
        enroll(&f, b"secret", "hmac-sha256-uid").unwrap();
        let calls = f.calls.borrow();
        let add = calls
            .iter()
            .position(|c| c.starts_with("luksAddKey --volume-key-keyring"))
            .unwrap();
        let rm_token = calls
            .iter()
            .position(|c| c == "token remove --token-id 1 /dev/mmcblk2p9")
            .unwrap();
        let kill = calls
            .iter()
            .position(|c| c == "luksKillSlot --batch-mode /dev/mmcblk2p9 2")
            .unwrap();
        let import = calls
            .iter()
            .position(|c| c.starts_with("token import --json-file"))
            .unwrap();
        // The new token must be on disk before the old recovery path is retired.
        assert!(
            add < import && import < rm_token && rm_token < kill,
            "{calls:?}"
        );
        assert!(
            calls.iter().any(|c| c.contains("--new-key-slot 3 ")),
            "{calls:?}"
        );
    }

    #[test]
    fn failed_token_import_leaves_the_previous_recovery_path_intact() {
        // Retiring the old token before importing the new one left a device with
        // no recovery slot at all when the import failed.
        let mut f = Fake::new(DUMP_WITH_RECOVERY);
        f.fail_import = true;
        enroll(&f, b"secret", "hmac-sha256-uid").unwrap_err();
        let calls = f.calls.borrow();
        assert!(
            !calls.iter().any(|c| c.starts_with("token remove")),
            "the previous recovery token was removed anyway: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c == "luksKillSlot --batch-mode /dev/mmcblk2p9 2"),
            "the previous recovery keyslot was killed anyway: {calls:?}"
        );
        // Only the slot this run added is rolled back.
        assert!(
            calls
                .iter()
                .any(|c| c == "luksKillSlot --batch-mode /dev/mmcblk2p9 3"),
            "{calls:?}"
        );
    }

    #[test]
    fn failed_token_import_removes_the_new_slot() {
        let mut f = Fake::new(DUMP);
        f.fail_import = true;
        let err = enroll(&f, b"secret", "k").unwrap_err();
        assert!(err.contains("keyslot 2 removed again"), "{err}");
        assert!(f
            .calls
            .borrow()
            .iter()
            .any(|c| c == "luksKillSlot --batch-mode /dev/mmcblk2p9 2"));
    }

    #[test]
    fn remove_needs_a_recovery_token() {
        assert!(remove(&Fake::new(DUMP))
            .unwrap_err()
            .contains("no recovery keyslot"));
        let f = Fake::new(DUMP_WITH_RECOVERY);
        remove(&f).unwrap();
        assert!(f
            .calls
            .borrow()
            .iter()
            .any(|c| c == "luksKillSlot --batch-mode /dev/mmcblk2p9 2"));
    }

    #[test]
    fn empty_passphrase_is_refused_before_touching_the_device() {
        let f = Fake::new(DUMP);
        assert!(enroll(&f, b"", "k").is_err());
        assert!(f.calls.borrow().is_empty());
    }
}
