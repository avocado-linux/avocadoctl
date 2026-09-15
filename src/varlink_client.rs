use crate::output::OutputManager;
use crate::varlink::{
    org_avocado_Extensions as vl_ext, org_avocado_Hitl as vl_hitl,
    org_avocado_RootAuthority as vl_ra, org_avocado_Runtimes as vl_rt,
};
use std::sync::{Arc, RwLock};
use varlink::Connection;

pub use vl_ext::VarlinkClientInterface as ExtClientInterface;
pub use vl_hitl::VarlinkClientInterface as HitlClientInterface;
pub use vl_ra::VarlinkClientInterface as RaClientInterface;
pub use vl_rt::VarlinkClientInterface as RtClientInterface;

/// Connect to the varlink daemon socket.
/// Prints an error and exits with code 1 if the daemon is not reachable.
pub fn connect_or_exit(address: &str, output: &OutputManager) -> Arc<RwLock<Connection>> {
    match varlink::Connection::with_address(address) {
        Ok(conn) => conn,
        Err(e) => {
            output.error(
                "Daemon Not Running",
                &format!(
                    "Cannot connect to avocadoctl daemon at {address}: {e}\n   \
                     Start it with: systemctl start avocadoctl"
                ),
            );
            std::process::exit(1);
        }
    }
}

/// Print an RPC error and exit with code 1.
///
/// The varlink error types are generated, and their `Display` is
/// `write!(f, "org.avocado.Iface.ErrorName: {:#?}", args)` -- an interface path
/// followed by a pretty-printed Rust struct. Shown verbatim that reads as a
/// crash dump ("RPC Error: org.avocado.Runtimes.RuntimeNotFound: Some(
/// RuntimeNotFound_Args { id: \"x\" })"). [`humanize_rpc_error`] turns it into
/// a sentence; `--verbose` still gets the raw form for debugging.
pub fn exit_with_rpc_error(
    err: impl std::fmt::Display + std::fmt::Debug,
    output: &OutputManager,
) -> ! {
    if output.is_verbose() {
        output.error("Error", &format!("{err:?}"));
    } else {
        output.error("Error", &humanize_rpc_error(&err.to_string()));
    }
    std::process::exit(1);
}

/// Turn a generated varlink error `Display` string into a readable message.
///
/// `org.avocado.Runtimes.RuntimeNotFound: Some(RuntimeNotFound_Args { id: "x" })`
/// becomes `Runtime not found (id: x)`. Anything that does not match the shape
/// is returned unchanged -- a plain error message passes through as-is.
pub fn humanize_rpc_error(raw: &str) -> String {
    // Split "iface.path.ErrorName: <debug struct>" on the first ": ".
    let (path, body) = match raw.split_once(": ") {
        Some((p, b)) if p.starts_with("org.avocado.") => (p, b),
        // Not one of our varlink errors; leave it alone.
        _ => return raw.to_string(),
    };
    let error_name = path.rsplit('.').next().unwrap_or(path);
    let mut msg = split_camel_case(error_name);

    // Pull the `field: value` pairs out of the debug struct, keeping the ones
    // that carry information (skip None, empty, and the redundant *_Args tag).
    let fields = extract_debug_fields(body);
    if !fields.is_empty() {
        msg.push_str(" (");
        msg.push_str(&fields.join(", "));
        msg.push(')');
    }
    msg
}

/// "RuntimeNotFound" -> "Runtime not found"; "AmbiguousRuntimeId" ->
/// "Ambiguous runtime id". First word capitalized, the rest lowercased.
fn split_camel_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        // Start a new word before an uppercase letter that either follows a
        // lowercase/digit (`aB` -> `a|B`) or begins a Word after an acronym
        // (`NFSFailed` -> `NFS|Failed`: the F is upper, follows an upper, and is
        // followed by a lowercase). This keeps runs of capitals (acronyms)
        // together instead of splitting them into single letters.
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if !prev.is_uppercase() || next_lower {
                words.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
        .into_iter()
        .enumerate()
        .map(|(i, w)| {
            // Preserve an all-caps acronym (NFS, TPM) as-is; otherwise the first
            // word is capitalized and the rest lowercased for readability.
            let is_acronym = w.chars().count() > 1 && w.chars().all(|c| c.is_uppercase());
            if is_acronym {
                w
            } else if i == 0 {
                let mut cs = w.chars();
                match cs.next() {
                    Some(f) => f
                        .to_uppercase()
                        .chain(cs.flat_map(char::to_lowercase))
                        .collect(),
                    None => w,
                }
            } else {
                w.to_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract informative `field: value` pairs from a Rust debug struct body.
/// Best-effort and shallow: enough to surface `id`, `reason`, `key`, etc.
fn extract_debug_fields(body: &str) -> Vec<String> {
    // Trim the `Some(Type_Args { ... })` / `Type { ... }` wrapper to the braces.
    let inner = match (body.find('{'), body.rfind('}')) {
        (Some(a), Some(b)) if a < b => &body[a + 1..b],
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for part in split_top_level_commas(inner) {
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        // Check the sentinels on the raw debug token, before unquoting: `None`
        // and `[]` appear unquoted, so a real string value `"None"` / `"[]"`
        // must not be discarded as absent.
        if k.is_empty() || v.is_empty() || v == "None" || v == "[]" {
            continue;
        }
        // Strip at most one surrounding quote pair (the debug quoting of a
        // string), leaving inner and escaped quotes intact.
        let v = v
            .strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or(v);
        if v.is_empty() {
            continue;
        }
        out.push(format!("{k}: {v}"));
    }
    out
}

/// Split on commas that are not inside brackets/braces/parens/quotes, so a
/// nested `candidates: [a, b]` stays one field.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let (mut depth, mut in_str, mut start) = (0i32, false, 0usize);
    let mut escaped = false;
    let b = s.as_bytes();
    for i in 0..b.len() {
        // A backslash-escaped byte inside a string is data, never a delimiter:
        // `reason: "a \", b"` is one field, not two.
        if escaped {
            escaped = false;
            continue;
        }
        match b[i] {
            b'\\' if in_str => escaped = true,
            b'"' => in_str = !in_str,
            b'{' | b'[' | b'(' if !in_str => depth += 1,
            b'}' | b']' | b')' if !in_str => depth -= 1,
            b',' if !in_str && depth == 0 => {
                parts.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].to_string());
    parts
}

// ── Log output helpers ───────────────────────────────────────────────────────

/// Print a single log message from a streaming varlink reply.
/// Re-colorizes `[INFO]` and `[SUCCESS]` prefixes for the caller's terminal.
pub fn print_single_log(message: &str, output: &OutputManager) {
    if message.is_empty() {
        return;
    }
    if let Some(rest) = message.strip_prefix("[INFO] ") {
        output.log_info(rest);
    } else if let Some(rest) = message.strip_prefix("[SUCCESS] ") {
        output.log_success(rest);
    } else {
        println!("{message}");
    }
}

// ── Extension output helpers ─────────────────────────────────────────────────

pub fn print_extensions(extensions: &[vl_ext::Extension], output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string(extensions) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if extensions.is_empty() {
        println!("No extensions found.");
        return;
    }

    let name_width = extensions
        .iter()
        .map(|e| e.name.len() + e.version.as_ref().map(|v| v.len() + 1).unwrap_or(0))
        .max()
        .unwrap_or(9)
        .max(9);

    println!("{:<nw$} {:<12} Path", "Extension", "Type", nw = name_width);
    println!("{}", "=".repeat(name_width + 1 + 12 + 1 + 20));

    for ext in extensions {
        let versioned_name = match &ext.version {
            Some(v) => format!("{}-{}", ext.name, v),
            None => ext.name.clone(),
        };

        let mut types = Vec::new();
        if ext.isSysext {
            types.push("sys");
        }
        if ext.isConfext {
            types.push("conf");
        }
        let type_str = if types.is_empty() {
            "?".to_string()
        } else {
            types.join("+")
        };

        println!(
            "{:<nw$} {:<12} {}",
            versioned_name,
            type_str,
            ext.path,
            nw = name_width
        );
    }

    println!();
    println!("Total: {} extension(s)", extensions.len());
}

pub fn print_extension_status(extensions: &[vl_ext::ExtensionStatus], output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string(extensions) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if extensions.is_empty() {
        println!("No extensions currently merged.");
        return;
    }

    let name_width = extensions
        .iter()
        .map(|e| e.name.len() + e.version.as_ref().map(|v| v.len() + 1).unwrap_or(0))
        .max()
        .unwrap_or(9)
        .max(9);

    println!(
        "{:<nw$} {:<12} {:<8} Origin",
        "Extension",
        "Type",
        "Merged",
        nw = name_width
    );
    println!("{}", "=".repeat(name_width + 1 + 12 + 1 + 8 + 1 + 20));

    for ext in extensions {
        let versioned_name = match &ext.version {
            Some(v) => format!("{}-{}", ext.name, v),
            None => ext.name.clone(),
        };

        let mut types = Vec::new();
        if ext.isSysext {
            types.push("sys");
        }
        if ext.isConfext {
            types.push("conf");
        }
        let type_str = if types.is_empty() {
            "?".to_string()
        } else {
            let base = types.join("+");
            if ext.imageType.as_deref() == Some("kab") {
                format!("kab:{base}")
            } else {
                base
            }
        };

        let merged_str = if ext.isMerged { "yes" } else { "no" };
        let origin = ext.origin.as_deref().unwrap_or("-");

        println!("{versioned_name:<name_width$} {type_str:<12} {merged_str:<8} {origin}");
    }

    println!();
    let merged_count = extensions.iter().filter(|e| e.isMerged).count();
    println!(
        "Total: {} extension(s), {} merged",
        extensions.len(),
        merged_count
    );
}

// ── Runtime output helpers ────────────────────────────────────────────────────

pub fn print_runtimes(runtimes: &[vl_rt::Runtime], output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string(runtimes) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if runtimes.is_empty() {
        println!("No runtimes found.");
        return;
    }

    println!("{:<32} {:<12} Built At", "Runtime", "Active",);
    println!("{}", "=".repeat(32 + 1 + 12 + 1 + 20));

    for rt in runtimes {
        let short_id = &rt.id[..rt.id.len().min(8)];
        let runtime_label = format!("{} {} ({short_id})", rt.runtime.name, rt.runtime.version);
        let active_str = if rt.active { "* active" } else { "" };

        println!("{:<32} {:<12} {}", runtime_label, active_str, rt.builtAt,);
    }

    println!();
    println!("Total: {} runtime(s)", runtimes.len());
}

pub fn print_runtime_detail(rt: &vl_rt::Runtime, output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string(rt) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    let short_id = &rt.id[..rt.id.len().min(8)];
    println!();
    println!(
        "  Runtime: {} {} ({short_id})",
        rt.runtime.name, rt.runtime.version
    );
    println!("  ID:      {}", rt.id);
    println!("  Built:   {}", rt.builtAt);
    println!("  Active:  {}", if rt.active { "yes" } else { "no" });

    if rt.osBuildId.is_some() || rt.initramfsBuildId.is_some() {
        println!();
        println!("  OS Release:");
        if let Some(ref id) = rt.osBuildId {
            println!("    Rootfs Build ID:    {id}");
        }
        if let Some(ref id) = rt.initramfsBuildId {
            println!("    Initramfs Build ID: {id}");
        }
    }

    if !rt.extensions.is_empty() {
        println!();
        println!("  Extensions:");
        for ext in &rt.extensions {
            let img = ext.imageId.as_deref().unwrap_or("-");
            let type_str = ext.imageType.as_deref().unwrap_or("raw");
            let sha = match &ext.sha256 {
                Some(h) if h.len() >= 12 => &h[..12],
                Some(h) => h.as_str(),
                None => "-",
            };
            println!(
                "    {} {} (image: {}, type: {}, sha256: {})",
                ext.name, ext.version, img, type_str, sha
            );
        }
    }
    println!();
}

// ── Metadata output helpers ──────────────────────────────────────────────────

pub fn print_metadata_value(key: &str, value: &str, output: &OutputManager) {
    if output.is_json() {
        println!("{}", serde_json::json!({"key": key, "value": value}));
    } else {
        println!("{value}");
    }
}

pub fn print_metadata_list(entries: &[vl_rt::MetadataEntry], output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string_pretty(entries) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    if entries.is_empty() {
        println!("No metadata set for this runtime.");
        return;
    }

    let key_width = entries
        .iter()
        .map(|e| e.key.len())
        .max()
        .unwrap_or(3)
        .max(3);

    println!("{:<kw$} VALUE", "KEY", kw = key_width);
    for entry in entries {
        println!("{:<kw$} {}", entry.key, entry.value, kw = key_width);
    }
}

// ── Root authority output helper ──────────────────────────────────────────────

pub fn print_root_authority(info: &Option<vl_ra::RootAuthorityInfo>, output: &OutputManager) {
    if output.is_json() {
        match serde_json::to_string(info) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                output.error("Output", &format!("JSON serialization failed: {e}"));
                std::process::exit(1);
            }
        }
        return;
    }

    match info {
        None => {
            output.info(
                "Root Authority",
                "No root authority configured. Build and provision a runtime with avocado build to enable verified updates.",
            );
        }
        Some(ra) => {
            println!();
            println!("  Root authority:");
            println!();
            println!("    Version:  {}", ra.version);
            println!("    Expires:  {}", ra.expires);
            println!();
            println!("    Trusted signing keys:");
            println!();
            println!("      {:<18} {:<12} ROLES", "KEY ID", "TYPE");
            for key in &ra.keys {
                let short_id = &key.keyId[..key.keyId.len().min(16)];
                let roles_str = key.roles.join(", ");
                println!("      {short_id:<18} {:<12} {roles_str}", key.keyType);
            }
            println!();
        }
    }
}

#[cfg(test)]
mod rpc_error_tests {
    use super::*;

    #[test]
    fn acronyms_are_not_split_into_single_letters() {
        assert_eq!(split_camel_case("MountNFSFailed"), "Mount NFS failed");
        assert_eq!(split_camel_case("RuntimeNotFound"), "Runtime not found");
        assert_eq!(split_camel_case("TPMError"), "TPM error");
    }

    #[test]
    fn literal_none_string_value_is_kept_not_dropped() {
        // `id: "None"` is a real string value, not the Option::None sentinel.
        let raw = r#"org.avocado.Runtimes.RuntimeNotFound: Some(RuntimeNotFound_Args { id: "None", candidates: None })"#;
        assert_eq!(humanize_rpc_error(raw), "Runtime not found (id: None)");
    }

    #[test]
    fn escaped_quote_in_a_field_is_not_a_split_point() {
        // The comma lives inside an escaped-quote run within the value, so the
        // field must stay whole rather than split at that comma.
        let parts = split_top_level_commas(r#"reason: "route \"a, b\" failed", id: 7"#);
        assert_eq!(parts.len(), 2, "got {parts:?}");
        assert_eq!(parts[0].trim(), r#"reason: "route \"a, b\" failed""#);
        assert_eq!(parts[1].trim(), "id: 7");
    }

    #[test]
    fn humanizes_a_generated_varlink_error() {
        let raw = r#"org.avocado.Runtimes.RuntimeNotFound: Some(RuntimeNotFound_Args { id: "zzzzz", candidates: None })"#;
        assert_eq!(humanize_rpc_error(raw), "Runtime not found (id: zzzzz)");
    }

    #[test]
    fn multiword_error_and_multiple_fields() {
        let raw = r#"org.avocado.Runtimes.StagingFailed: Some(StagingFailed_Args { reason: "disk full" })"#;
        assert_eq!(
            humanize_rpc_error(raw),
            "Staging failed (reason: disk full)"
        );
        let raw2 = r#"org.avocado.Hitl.MountFailed: Some(MountFailed_Args { extension: "vmm", reason: "no route" })"#;
        assert_eq!(
            humanize_rpc_error(raw2),
            "Mount failed (extension: vmm, reason: no route)"
        );
    }

    #[test]
    fn no_fields_is_just_the_name() {
        let raw = "org.avocado.Runtimes.RemoveActiveRuntime: None";
        assert_eq!(humanize_rpc_error(raw), "Remove active runtime");
    }

    #[test]
    fn a_plain_message_passes_through() {
        assert_eq!(
            humanize_rpc_error("connection refused"),
            "connection refused"
        );
        assert_eq!(
            humanize_rpc_error("Failed to fetch http://x/y: timeout"),
            "Failed to fetch http://x/y: timeout"
        );
    }
}
