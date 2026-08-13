#![allow(non_snake_case)]

use crate::config::Config;
use crate::manifest::RuntimeManifest;
use crate::service;
use crate::service::error::AvocadoError;
use crate::varlink::{
    org_avocado_Extensions as vl_ext, org_avocado_Hitl as vl_hitl,
    org_avocado_RootAuthority as vl_ra, org_avocado_Runtimes as vl_rt,
};
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use varlink::CallTrait;

// ── Streaming helper ───────────────────────────────────────────────

/// Drain a streaming channel, sending each message as a varlink reply with
/// `continues: true`. After the channel closes, join the worker thread and
/// send a final reply (success or error).
///
/// The `reply_fn` sends one intermediate message. The `done_fn` sends the
/// final success reply. The `error_fn` sends an error reply.
fn drain_stream<C, R, D, E>(
    call: &mut C,
    rx: mpsc::Receiver<String>,
    handle: thread::JoinHandle<Result<(), AvocadoError>>,
    reply_fn: R,
    done_fn: D,
    error_fn: E,
) -> varlink::Result<()>
where
    C: CallTrait + ?Sized,
    R: Fn(&mut C, String) -> varlink::Result<()>,
    D: Fn(&mut C) -> varlink::Result<()>,
    E: Fn(&mut C, AvocadoError) -> varlink::Result<()>,
{
    call.set_continues(true);
    for message in rx {
        reply_fn(call, message)?;
    }
    // Channel closed — worker thread is done
    let result = handle.join().unwrap_or_else(|_| {
        Err(AvocadoError::MergeFailed {
            reason: "internal panic".into(),
        })
    });
    call.set_continues(false);
    match result {
        Ok(()) => done_fn(call),
        Err(e) => {
            eprintln!("  Error: {e}");
            error_fn(call, e)
        }
    }
}

// ── Extensions handler ──────────────────────────────────────────────

pub struct ExtensionsHandler {
    config: Config,
}

macro_rules! map_ext_error {
    ($call:expr, $err:expr) => {
        match $err {
            AvocadoError::ExtensionNotFound { name } => $call.reply_extension_not_found(name),
            AvocadoError::MergeFailed { reason } => $call.reply_merge_failed(reason),
            AvocadoError::UnmergeFailed { reason } => $call.reply_unmerge_failed(reason),
            AvocadoError::ConfigurationError { message } => {
                $call.reply_configuration_error(message)
            }
            e => $call.reply_command_failed("avocadoctl".to_string(), e.to_string()),
        }
    };
}

impl vl_ext::VarlinkInterface for ExtensionsHandler {
    fn list(&self, call: &mut dyn vl_ext::Call_List) -> varlink::Result<()> {
        match service::ext::list_extensions(&self.config) {
            Ok(extensions) => {
                let vl: Vec<vl_ext::Extension> = extensions
                    .into_iter()
                    .map(|e| vl_ext::Extension {
                        r#name: e.name,
                        r#version: e.version,
                        r#path: e.path,
                        r#isSysext: e.is_sysext,
                        r#isConfext: e.is_confext,
                        r#isDirectory: e.is_directory,
                    })
                    .collect();
                call.reply(vl)
            }
            Err(e) => map_ext_error!(call, e),
        }
    }

    fn merge(&self, call: &mut dyn vl_ext::Call_Merge) -> varlink::Result<()> {
        if call.wants_more() {
            let (rx, handle) = service::ext::merge_extensions_streaming(&self.config);
            drain_stream(
                call,
                rx,
                handle,
                |c, msg| c.reply(msg, false),
                |c| c.reply(String::new(), true),
                |c, e| map_ext_error!(c, e),
            )
        } else {
            match service::ext::merge_extensions(&self.config) {
                Ok(log) => call.reply(log.join("\n"), true),
                Err(e) => map_ext_error!(call, e),
            }
        }
    }

    fn unmerge(
        &self,
        call: &mut dyn vl_ext::Call_Unmerge,
        r#unmount: Option<bool>,
    ) -> varlink::Result<()> {
        if call.wants_more() {
            let (rx, handle) = service::ext::unmerge_extensions_streaming(unmount.unwrap_or(false));
            drain_stream(
                call,
                rx,
                handle,
                |c, msg| c.reply(msg, false),
                |c| c.reply(String::new(), true),
                |c, e| map_ext_error!(c, e),
            )
        } else {
            match service::ext::unmerge_extensions(unmount.unwrap_or(false)) {
                Ok(log) => call.reply(log.join("\n"), true),
                Err(e) => map_ext_error!(call, e),
            }
        }
    }

    fn refresh(&self, call: &mut dyn vl_ext::Call_Refresh) -> varlink::Result<()> {
        if call.wants_more() {
            let (rx, handle) = service::ext::refresh_extensions_streaming(&self.config);
            drain_stream(
                call,
                rx,
                handle,
                |c, msg| c.reply(msg, false),
                |c| c.reply(String::new(), true),
                |c, e| map_ext_error!(c, e),
            )
        } else {
            match service::ext::refresh_extensions(&self.config) {
                Ok(log) => call.reply(log.join("\n"), true),
                Err(e) => map_ext_error!(call, e),
            }
        }
    }

    fn enable(
        &self,
        call: &mut dyn vl_ext::Call_Enable,
        r#extensions: Vec<String>,
        r#osRelease: Option<String>,
    ) -> varlink::Result<()> {
        let ext_refs: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
        match service::ext::enable_extensions(osRelease.as_deref(), &ext_refs, &self.config) {
            Ok(result) => call.reply(result.enabled as i64, result.failed as i64),
            Err(e) => map_ext_error!(call, e),
        }
    }

    fn disable(
        &self,
        call: &mut dyn vl_ext::Call_Disable,
        r#extensions: Option<Vec<String>>,
        r#all: Option<bool>,
        r#osRelease: Option<String>,
    ) -> varlink::Result<()> {
        let ext_refs: Option<Vec<&str>> = extensions
            .as_ref()
            .map(|v| v.iter().map(|s| s.as_str()).collect());
        match service::ext::disable_extensions(
            osRelease.as_deref(),
            ext_refs.as_deref(),
            all.unwrap_or(false),
            &self.config,
        ) {
            Ok(result) => call.reply(result.disabled as i64, result.failed as i64),
            Err(e) => map_ext_error!(call, e),
        }
    }

    fn status(&self, call: &mut dyn vl_ext::Call_Status) -> varlink::Result<()> {
        match service::ext::status_extensions(&self.config) {
            Ok(extensions) => call.reply(extensions),
            Err(e) => map_ext_error!(call, e),
        }
    }

    fn set_enabled(
        &self,
        call: &mut dyn vl_ext::Call_SetEnabled,
        r#extensions: Vec<String>,
        r#enabled: bool,
    ) -> varlink::Result<()> {
        let ext_refs: Vec<&str> = extensions.iter().map(|s| s.as_str()).collect();
        match service::ext::set_extensions_enabled(&ext_refs, enabled, &self.config) {
            Ok(result) => call.reply(result.updated as i64, result.missing as i64),
            Err(e) => map_ext_error!(call, e),
        }
    }
}

// ── Runtimes handler ────────────────────────────────────────────────

pub struct RuntimesHandler {
    config: Config,
}

macro_rules! map_rt_error {
    ($call:expr, $err:expr) => {{
        eprintln!("  Error: {}", $err);
        match $err {
            AvocadoError::RuntimeNotFound { id } => $call.reply_runtime_not_found(id),
            AvocadoError::AmbiguousRuntimeId { id, candidates } => {
                $call.reply_ambiguous_runtime_id(id, candidates)
            }
            AvocadoError::RemoveActiveRuntime => $call.reply_remove_active_runtime(),
            AvocadoError::StagingFailed { reason } => $call.reply_staging_failed(reason),
            AvocadoError::UpdateFailed { reason } => $call.reply_update_failed(reason),
            AvocadoError::MetadataKeyNotFound { id, key } => {
                $call.reply_metadata_key_not_found(id, key)
            }
            e => $call.reply_staging_failed(e.to_string()),
        }
    }};
}

fn runtime_entry_to_varlink(entry: crate::service::types::RuntimeEntry) -> vl_rt::Runtime {
    vl_rt::Runtime {
        r#id: entry.id,
        r#manifestVersion: entry.manifest_version as i64,
        r#builtAt: entry.built_at,
        r#runtime: vl_rt::RuntimeInfo {
            r#name: entry.name,
            r#version: entry.version,
        },
        r#extensions: entry
            .extensions
            .into_iter()
            .map(|e| vl_rt::ManifestExtension {
                r#name: e.name,
                r#version: e.version,
                r#imageId: e.image_id,
                r#imageType: e.image_type,
                r#sha256: e.sha256,
            })
            .collect(),
        r#active: entry.active,
        r#osBuildId: entry.os_build_id,
        r#initramfsBuildId: entry.initramfs_build_id,
    }
}

/// Load the active runtime and convert it to a varlink Runtime type.
fn load_active_runtime_varlink(config: &Config) -> Option<vl_rt::Runtime> {
    let base_dir = config.get_avocado_base_dir();
    let base_path = Path::new(&base_dir);
    let manifest = RuntimeManifest::load_active(base_path)?;
    let entry = service::runtime::manifest_to_entry(&manifest, true);
    Some(runtime_entry_to_varlink(entry))
}

impl vl_rt::VarlinkInterface for RuntimesHandler {
    fn list(&self, call: &mut dyn vl_rt::Call_List) -> varlink::Result<()> {
        match service::runtime::list_runtimes(&self.config) {
            Ok(runtimes) => {
                let vl: Vec<vl_rt::Runtime> =
                    runtimes.into_iter().map(runtime_entry_to_varlink).collect();
                call.reply(vl)
            }
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn add_from_url(
        &self,
        call: &mut dyn vl_rt::Call_AddFromUrl,
        r#url: String,
        r#authToken: Option<String>,
        r#artifactsUrl: Option<String>,
    ) -> varlink::Result<()> {
        if call.wants_more() {
            let config = self.config.clone();
            match service::runtime::add_from_url_streaming(
                &url,
                authToken.as_deref(),
                artifactsUrl.as_deref(),
                &self.config,
            ) {
                Ok((rx, handle)) => drain_stream(
                    call,
                    rx,
                    handle,
                    |c, msg| c.reply(msg, false, None),
                    |c| {
                        let rt = load_active_runtime_varlink(&config);
                        c.reply(String::new(), true, rt)
                    },
                    |c, e| map_rt_error!(c, e),
                ),
                Err(e) => map_rt_error!(call, e),
            }
        } else {
            match service::runtime::add_from_url(
                &url,
                authToken.as_deref(),
                artifactsUrl.as_deref(),
                &self.config,
            ) {
                Ok(log) => {
                    let rt = load_active_runtime_varlink(&self.config);
                    call.reply(log.join("\n"), true, rt)
                }
                Err(e) => map_rt_error!(call, e),
            }
        }
    }

    fn add_from_manifest(
        &self,
        call: &mut dyn vl_rt::Call_AddFromManifest,
        r#manifestPath: String,
    ) -> varlink::Result<()> {
        if call.wants_more() {
            let config = self.config.clone();
            match service::runtime::add_from_manifest_streaming(&manifestPath, &self.config) {
                Ok((rx, handle)) => drain_stream(
                    call,
                    rx,
                    handle,
                    |c, msg| c.reply(msg, false, None),
                    |c| {
                        let rt = load_active_runtime_varlink(&config);
                        c.reply(String::new(), true, rt)
                    },
                    |c, e| map_rt_error!(c, e),
                ),
                Err(e) => map_rt_error!(call, e),
            }
        } else {
            match service::runtime::add_from_manifest(&manifestPath, &self.config) {
                Ok(log) => {
                    let rt = load_active_runtime_varlink(&self.config);
                    call.reply(log.join("\n"), true, rt)
                }
                Err(e) => map_rt_error!(call, e),
            }
        }
    }

    fn remove(&self, call: &mut dyn vl_rt::Call_Remove, r#id: String) -> varlink::Result<()> {
        match service::runtime::remove_runtime(&id, &self.config) {
            Ok(()) => call.reply(),
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn activate(&self, call: &mut dyn vl_rt::Call_Activate, r#id: String) -> varlink::Result<()> {
        if call.wants_more() {
            let config = self.config.clone();
            match service::runtime::activate_runtime_streaming(&id, &self.config) {
                Ok(Some((rx, handle))) => drain_stream(
                    call,
                    rx,
                    handle,
                    |c, msg| c.reply(msg, false, None),
                    |c| {
                        let rt = load_active_runtime_varlink(&config);
                        c.reply(String::new(), true, rt)
                    },
                    |c, e| map_rt_error!(c, e),
                ),
                Ok(None) => {
                    // Already active, return current runtime info
                    let rt = load_active_runtime_varlink(&self.config);
                    call.reply(String::new(), true, rt)
                }
                Err(e) => map_rt_error!(call, e),
            }
        } else {
            match service::runtime::activate_runtime(&id, &self.config) {
                Ok(log) => {
                    let rt = load_active_runtime_varlink(&self.config);
                    call.reply(log.join("\n"), true, rt)
                }
                Err(e) => map_rt_error!(call, e),
            }
        }
    }

    fn inspect(
        &self,
        call: &mut dyn vl_rt::Call_Inspect,
        r#id: Option<String>,
    ) -> varlink::Result<()> {
        match service::runtime::inspect_runtime(id.as_deref(), &self.config) {
            Ok(entry) => call.reply(runtime_entry_to_varlink(entry)),
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn metadata_set(
        &self,
        call: &mut dyn vl_rt::Call_MetadataSet,
        r#id: String,
        r#key: String,
        r#value: String,
    ) -> varlink::Result<()> {
        match service::runtime::metadata_set(&id, &key, &value, &self.config) {
            Ok(()) => call.reply(),
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn metadata_get(
        &self,
        call: &mut dyn vl_rt::Call_MetadataGet,
        r#id: String,
        r#key: String,
    ) -> varlink::Result<()> {
        match service::runtime::metadata_get(&id, &key, &self.config) {
            Ok(value) => call.reply(value),
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn metadata_list(
        &self,
        call: &mut dyn vl_rt::Call_MetadataList,
        r#id: String,
    ) -> varlink::Result<()> {
        match service::runtime::metadata_list(&id, &self.config) {
            Ok(entries) => {
                let vl_entries: Vec<vl_rt::MetadataEntry> = entries
                    .into_iter()
                    .map(|(k, v)| vl_rt::MetadataEntry {
                        r#key: k,
                        r#value: v,
                    })
                    .collect();
                call.reply(vl_entries)
            }
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn metadata_delete(
        &self,
        call: &mut dyn vl_rt::Call_MetadataDelete,
        r#id: String,
        r#key: String,
    ) -> varlink::Result<()> {
        match service::runtime::metadata_delete(&id, &key, &self.config) {
            Ok(()) => call.reply(),
            Err(e) => map_rt_error!(call, e),
        }
    }

    fn garbage_collect(&self, call: &mut dyn vl_rt::Call_GarbageCollect) -> varlink::Result<()> {
        match service::runtime::garbage_collect(&self.config) {
            Ok(result) => call.reply(vl_rt::GcResult {
                r#removedRuntimes: result.removed_runtimes,
                r#removedImages: result.removed_images,
            }),
            Err(e) => map_rt_error!(call, e),
        }
    }
}

// ── HITL handler ────────────────────────────────────────────────────

pub struct HitlHandler;

macro_rules! map_hitl_error {
    ($call:expr, $err:expr) => {
        match $err {
            AvocadoError::MountFailed { extension, reason } => {
                $call.reply_mount_failed(extension, reason)
            }
            AvocadoError::UnmountFailed { extension, reason } => {
                $call.reply_unmount_failed(extension, reason)
            }
            e => $call.reply_mount_failed("unknown".to_string(), e.to_string()),
        }
    };
}

impl vl_hitl::VarlinkInterface for HitlHandler {
    fn mount(
        &self,
        call: &mut dyn vl_hitl::Call_Mount,
        r#serverIp: String,
        r#serverPort: Option<String>,
        r#extensions: Vec<String>,
    ) -> varlink::Result<()> {
        match service::hitl::mount(&serverIp, serverPort.as_deref(), &extensions) {
            Ok(()) => call.reply(),
            Err(e) => map_hitl_error!(call, e),
        }
    }

    fn unmount(
        &self,
        call: &mut dyn vl_hitl::Call_Unmount,
        r#extensions: Vec<String>,
    ) -> varlink::Result<()> {
        match service::hitl::unmount(&extensions) {
            Ok(()) => call.reply(),
            Err(e) => map_hitl_error!(call, e),
        }
    }
}

// ── Root Authority handler ──────────────────────────────────────────

pub struct RootAuthorityHandler {
    config: Config,
}

impl vl_ra::VarlinkInterface for RootAuthorityHandler {
    fn show(&self, call: &mut dyn vl_ra::Call_Show) -> varlink::Result<()> {
        match service::root_authority::show(&self.config) {
            Ok(Some(info)) => {
                let vl_info = vl_ra::RootAuthorityInfo {
                    r#version: info.version as i64,
                    r#expires: info.expires,
                    r#keys: info
                        .keys
                        .into_iter()
                        .map(|k| vl_ra::TrustedKey {
                            r#keyId: k.key_id,
                            r#keyType: k.key_type,
                            r#roles: k.roles,
                        })
                        .collect(),
                };
                call.reply(Some(vl_info))
            }
            Ok(None) => call.reply(None),
            Err(AvocadoError::NoRootAuthority) => call.reply_no_root_authority(),
            Err(AvocadoError::ParseFailed { reason }) => call.reply_parse_failed(reason),
            Err(e) => call.reply_parse_failed(e.to_string()),
        }
    }
}

// ── Server entry point ──────────────────────────────────────────────

pub fn run_server(address: &str, config: Config) -> varlink::Result<()> {
    let ext_handler = ExtensionsHandler {
        config: config.clone(),
    };
    let rt_handler = RuntimesHandler {
        config: config.clone(),
    };
    let hitl_handler = HitlHandler;
    let ra_handler = RootAuthorityHandler { config };

    let service = varlink::VarlinkService::new(
        "org.avocado",
        "avocadoctl",
        env!("CARGO_PKG_VERSION"),
        "https://avocado-linux.org",
        vec![
            Box::new(vl_ext::new(Box::new(ext_handler))),
            Box::new(vl_rt::new(Box::new(rt_handler))),
            Box::new(vl_hitl::new(Box::new(hitl_handler))),
            Box::new(vl_ra::new(Box::new(ra_handler))),
        ],
    );

    let listen_config = varlink::ListenConfig {
        idle_timeout: 0,
        ..Default::default()
    };

    varlink::listen(service, address, &listen_config)
}
