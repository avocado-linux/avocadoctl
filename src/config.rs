use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Default configuration file path
pub const DEFAULT_CONFIG_PATH: &str = "/etc/avocado/avocadoctl.conf";

/// Configuration structure for avocadoctl
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Avocado extension configuration
    pub avocado: AvocadoConfig,
}

/// Avocado-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvocadoConfig {
    /// Extension configuration
    pub ext: ExtConfig,
    /// Override for the avocado base directory (default: /var/lib/avocado)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtimes_dir: Option<String>,
    /// Varlink socket address for daemon communication
    /// (default: unix:/run/avocado/avocadoctl.sock)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Update settings (streaming, etc.)
    #[serde(default)]
    pub update: UpdateSettings,
    /// Garbage collection settings
    #[serde(default)]
    pub gc: GcSettings,
}

/// Update configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateSettings {
    /// Stream OS bundle artifacts directly from HTTP to partitions without staging to disk.
    /// Reduces disk I/O and temporary storage but disables resumable downloads for the OS bundle.
    /// Default: false
    #[serde(default)]
    pub stream_os_to_partition: bool,
}

/// Garbage collection configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcSettings {
    /// Maximum number of runtimes to keep (including the active one).
    /// The active runtime and any runtime referenced by a pending OS update are always kept.
    /// Minimum: 1 (always keep the active runtime). Default: 3.
    #[serde(default = "default_runtime_retention")]
    pub runtime_retention: u32,
    /// Whether to automatically run garbage collection after adding a runtime.
    /// When false, GC only runs when explicitly invoked via `avocadoctl runtime gc`.
    /// Default: true.
    #[serde(default = "default_auto_gc")]
    pub auto_gc: bool,
}

impl Default for GcSettings {
    fn default() -> Self {
        Self {
            runtime_retention: default_runtime_retention(),
            auto_gc: default_auto_gc(),
        }
    }
}

fn default_auto_gc() -> bool {
    true
}

fn default_runtime_retention() -> u32 {
    3
}

/// Extension configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtConfig {
    /// Directory where extensions are stored
    pub dir: String,
    /// Mutability mode for system extensions - sysext (/usr, /opt)
    pub sysext_mutable: Option<String>,
    /// Mutability mode for configuration extensions - confext (/etc)
    pub confext_mutable: Option<String>,
    /// Legacy mutable option (deprecated, use sysext_mutable and confext_mutable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable: Option<String>,
    /// Number of bytes to read from head and tail of each extension image for spot-check hashing.
    /// Total I/O per file = 2 * spot_check_bytes. Default: 4096.
    #[serde(default = "default_spot_check_bytes")]
    pub spot_check_bytes: u64,
}

fn default_spot_check_bytes() -> u64 {
    4096
}

impl Default for Config {
    fn default() -> Self {
        Self {
            avocado: AvocadoConfig {
                ext: ExtConfig {
                    dir: "/var/lib/avocado/images".to_string(),
                    sysext_mutable: None,
                    confext_mutable: None,
                    mutable: None,
                    spot_check_bytes: default_spot_check_bytes(),
                },
                runtimes_dir: None,
                socket: None,
                update: UpdateSettings::default(),
                gc: GcSettings::default(),
            },
        }
    }
}

impl Config {
    /// Load configuration from file, falling back to defaults if file doesn't exist
    pub fn load<P: AsRef<Path>>(config_path: P) -> Result<Self, ConfigError> {
        let path = config_path.as_ref();

        if !path.exists() {
            // Return default config if file doesn't exist
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path).map_err(|e| ConfigError::FileRead {
            path: path.to_path_buf(),
            source: e,
        })?;

        let config: Config = toml::from_str(&content).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            source: e,
        })?;

        Ok(config)
    }

    /// Load configuration from the default path or a custom path
    pub fn load_with_override(custom_path: Option<&str>) -> Result<Self, ConfigError> {
        let config_path = custom_path.unwrap_or(DEFAULT_CONFIG_PATH);
        Self::load(config_path)
    }

    /// Get the varlink socket address for daemon communication.
    /// Resolution order: config file → hardcoded default.
    pub fn socket_address(&self) -> &str {
        self.avocado
            .socket
            .as_deref()
            .unwrap_or("unix:/run/avocado/avocadoctl.sock")
    }

    /// Whether to stream OS bundle artifacts directly to partitions (default: false)
    pub fn stream_os_to_partition(&self) -> bool {
        self.avocado.update.stream_os_to_partition
    }

    /// Get the extensions directory, checking environment variable first
    pub fn get_extensions_dir(&self) -> String {
        // Environment variable takes precedence (for testing)
        std::env::var("AVOCADO_EXTENSIONS_PATH").unwrap_or_else(|_| self.avocado.ext.dir.clone())
    }

    /// Get the avocado base directory (parent of extensions/, runtimes/, active).
    /// Checks AVOCADO_BASE_DIR env var first, then config, then default.
    pub fn get_avocado_base_dir(&self) -> String {
        std::env::var("AVOCADO_BASE_DIR").unwrap_or_else(|_| {
            self.avocado
                .runtimes_dir
                .clone()
                .unwrap_or_else(|| crate::manifest::DEFAULT_AVOCADO_DIR.to_string())
        })
    }

    /// Per-OS-version directory holding the enable/disable symlinks for
    /// `version_id`.
    ///
    /// Derived from [`Self::get_avocado_base_dir`] so relocating the base
    /// directory moves enablement state with it. Five call sites used to
    /// inline `/var/lib/avocado/os-releases/{version_id}` with a private
    /// `AVOCADO_TEST_MODE` branch, which meant configuring the base
    /// directory silently failed to move this one — the merge path read
    /// enablement from a directory nothing else agreed on. The test-mode
    /// redirect is preserved here so it stays in one place.
    pub fn get_os_releases_dir(&self, version_id: &str) -> String {
        if std::env::var("AVOCADO_TEST_MODE").is_ok() {
            let temp_base = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
            return format!("{temp_base}/avocado/os-releases/{version_id}");
        }
        format!("{}/os-releases/{version_id}", self.get_avocado_base_dir())
    }

    /// Get the spot check size in bytes for integrity hashing during merge.
    pub fn get_spot_check_bytes(&self) -> u64 {
        self.avocado.ext.spot_check_bytes
    }

    /// Get the runtime retention count, clamped to a minimum of 1.
    pub fn runtime_retention(&self) -> u32 {
        self.avocado.gc.runtime_retention.max(1)
    }

    /// Whether automatic GC after runtime add is enabled.
    pub fn auto_gc(&self) -> bool {
        self.avocado.gc.auto_gc
    }

    /// Get the sysext mutable mode, defaulting to "ephemeral" if not set
    /// Validates that the value is one of the supported systemd options
    pub fn get_sysext_mutable(&self) -> Result<String, ConfigError> {
        // Priority: sysext_mutable > legacy mutable > default
        let value = self
            .avocado
            .ext
            .sysext_mutable
            .as_ref()
            .or(self.avocado.ext.mutable.as_ref())
            .unwrap_or(&"ephemeral".to_string())
            .clone();

        // Validate against supported systemd options
        match value.as_str() {
            "no" | "auto" | "yes" | "import" | "ephemeral" | "ephemeral-import" => Ok(value),
            _ => Err(ConfigError::InvalidMutableValue { value }),
        }
    }

    /// Get the confext mutable mode, defaulting to "ephemeral" if not set
    /// Validates that the value is one of the supported systemd options
    pub fn get_confext_mutable(&self) -> Result<String, ConfigError> {
        // Priority: confext_mutable > legacy mutable > default
        let value = self
            .avocado
            .ext
            .confext_mutable
            .as_ref()
            .or(self.avocado.ext.mutable.as_ref())
            .unwrap_or(&"ephemeral".to_string())
            .clone();

        // Validate against supported systemd options
        match value.as_str() {
            "no" | "auto" | "yes" | "import" | "ephemeral" | "ephemeral-import" => Ok(value),
            _ => Err(ConfigError::InvalidMutableValue { value }),
        }
    }

    /// Legacy method for backward compatibility
    /// Get the extension mutable mode, defaulting to "ephemeral" if not set
    /// Validates that the value is one of the supported systemd options
    #[deprecated(note = "Use get_sysext_mutable() and get_confext_mutable() instead")]
    #[allow(dead_code)]
    pub fn get_extension_mutable(&self) -> Result<String, ConfigError> {
        // For backward compatibility, return sysext_mutable if available, otherwise legacy mutable
        self.get_sysext_mutable()
    }

    /// Save configuration to file (mainly for testing)
    #[cfg(test)]
    pub fn save<P: AsRef<Path>>(&self, config_path: P) -> Result<(), ConfigError> {
        let path = config_path.as_ref();
        let content =
            toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize { source: e })?;

        // Create parent directory if it doesn't exist
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| ConfigError::FileWrite {
                path: path.to_path_buf(),
                source: e,
            })?;
        }

        fs::write(path, content).map_err(|e| ConfigError::FileWrite {
            path: path.to_path_buf(),
            source: e,
        })?;

        Ok(())
    }
}

/// Configuration-related errors
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ConfigError {
    #[error("Failed to read config file '{path}': {source}")]
    FileRead {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to write config file '{path}': {source}")]
    FileWrite {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("Failed to parse config file '{path}': {source}")]
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },

    #[error("Failed to serialize config: {source}")]
    Serialize { source: toml::ser::Error },

    #[error("Invalid mutable value '{value}'. Must be one of: no, auto, yes, import, ephemeral, ephemeral-import")]
    InvalidMutableValue { value: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    // Mutex to serialize tests that modify AVOCADO_EXTENSIONS_PATH environment variable
    static ENV_VAR_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.avocado.ext.dir, "/var/lib/avocado/images");
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = Config::load("/nonexistent/path/config.toml");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.avocado.ext.dir, "/var/lib/avocado/images");
    }

    #[test]
    fn test_load_valid_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test_config.toml");

        let config_content = r#"
[avocado.ext]
dir = "/custom/extensions/path"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.avocado.ext.dir, "/custom/extensions/path");
    }

    #[test]
    fn test_load_invalid_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("invalid_config.toml");

        fs::write(&config_path, "invalid toml content [[[").unwrap();

        let result = Config::load(&config_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ConfigError::Parse { .. }));
    }

    #[test]
    fn test_save_and_load_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("roundtrip_config.toml");

        let mut config = Config::default();
        config.avocado.ext.dir = "/test/extensions".to_string();

        config.save(&config_path).unwrap();

        let loaded_config = Config::load(&config_path).unwrap();
        assert_eq!(loaded_config.avocado.ext.dir, "/test/extensions");
    }

    #[test]
    fn test_get_extensions_dir_with_env_var() {
        // Lock the mutex to prevent env var interference from other tests
        let _guard = ENV_VAR_MUTEX.lock().unwrap();

        // Save original environment variable value for restoration
        let original_value = std::env::var("AVOCADO_EXTENSIONS_PATH").ok();

        let config = Config::default();

        // Test without environment variable
        std::env::remove_var("AVOCADO_EXTENSIONS_PATH");
        assert_eq!(config.get_extensions_dir(), "/var/lib/avocado/images");

        // Test with environment variable
        std::env::set_var("AVOCADO_EXTENSIONS_PATH", "/env/override/path");
        assert_eq!(config.get_extensions_dir(), "/env/override/path");

        // Restore original environment variable
        match original_value {
            Some(val) => std::env::set_var("AVOCADO_EXTENSIONS_PATH", val),
            None => std::env::remove_var("AVOCADO_EXTENSIONS_PATH"),
        }
    }

    #[test]
    fn test_get_sysext_mutable() {
        // Test default value
        let config = Config::default();
        assert_eq!(config.get_sysext_mutable().unwrap(), "ephemeral");

        // Test with valid custom values
        let valid_values = [
            "no",
            "auto",
            "yes",
            "import",
            "ephemeral",
            "ephemeral-import",
        ];
        for value in valid_values {
            let mut config = Config::default();
            config.avocado.ext.sysext_mutable = Some(value.to_string());
            assert_eq!(config.get_sysext_mutable().unwrap(), value);
        }

        // Test with invalid value
        let mut config = Config::default();
        config.avocado.ext.sysext_mutable = Some("invalid".to_string());
        assert!(config.get_sysext_mutable().is_err());
    }

    #[test]
    fn test_get_confext_mutable() {
        // Test default value
        let config = Config::default();
        assert_eq!(config.get_confext_mutable().unwrap(), "ephemeral");

        // Test with valid custom values
        let valid_values = [
            "no",
            "auto",
            "yes",
            "import",
            "ephemeral",
            "ephemeral-import",
        ];
        for value in valid_values {
            let mut config = Config::default();
            config.avocado.ext.confext_mutable = Some(value.to_string());
            assert_eq!(config.get_confext_mutable().unwrap(), value);
        }

        // Test with invalid value
        let mut config = Config::default();
        config.avocado.ext.confext_mutable = Some("invalid".to_string());
        assert!(config.get_confext_mutable().is_err());
    }

    #[test]
    fn test_backward_compatibility_mutable() {
        // Test that legacy mutable option works for both sysext and confext
        let mut config = Config::default();
        config.avocado.ext.mutable = Some("yes".to_string());

        // Both should fall back to legacy mutable value
        assert_eq!(config.get_sysext_mutable().unwrap(), "yes");
        assert_eq!(config.get_confext_mutable().unwrap(), "yes");

        // Test priority: specific options override legacy
        config.avocado.ext.sysext_mutable = Some("auto".to_string());
        config.avocado.ext.confext_mutable = Some("no".to_string());

        assert_eq!(config.get_sysext_mutable().unwrap(), "auto");
        assert_eq!(config.get_confext_mutable().unwrap(), "no");
    }

    #[test]
    fn test_get_extension_mutable() {
        // Test legacy method for backward compatibility
        let config = Config::default();
        #[allow(deprecated)]
        {
            assert_eq!(config.get_extension_mutable().unwrap(), "ephemeral");
        }

        // Test with valid custom values
        let valid_values = [
            "no",
            "auto",
            "yes",
            "import",
            "ephemeral",
            "ephemeral-import",
        ];
        for value in valid_values {
            let mut config = Config::default();
            config.avocado.ext.mutable = Some(value.to_string());
            #[allow(deprecated)]
            {
                assert_eq!(config.get_extension_mutable().unwrap(), value);
            }
        }

        // Test with invalid value
        let mut config = Config::default();
        config.avocado.ext.mutable = Some("invalid".to_string());
        #[allow(deprecated)]
        {
            assert!(config.get_extension_mutable().is_err());
        }
    }

    #[test]
    fn test_load_config_with_separate_mutable_options() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("separate_mutable_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/test/extensions"
sysext_mutable = "yes"
confext_mutable = "auto"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.avocado.ext.dir, "/test/extensions");
        assert_eq!(config.get_sysext_mutable().unwrap(), "yes");
        assert_eq!(config.get_confext_mutable().unwrap(), "auto");
    }

    #[test]
    fn test_load_config_with_mutable_option() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("mutable_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/test/extensions"
mutable = "yes"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.avocado.ext.dir, "/test/extensions");
        #[allow(deprecated)]
        {
            assert_eq!(config.get_extension_mutable().unwrap(), "yes");
        }
        // Legacy mutable should apply to both
        assert_eq!(config.get_sysext_mutable().unwrap(), "yes");
        assert_eq!(config.get_confext_mutable().unwrap(), "yes");
    }

    #[test]
    fn test_save_and_load_config_with_separate_mutable() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir
            .path()
            .join("roundtrip_separate_mutable_config.toml");

        let mut config = Config::default();
        config.avocado.ext.dir = "/test/extensions".to_string();
        config.avocado.ext.sysext_mutable = Some("auto".to_string());
        config.avocado.ext.confext_mutable = Some("yes".to_string());

        config.save(&config_path).unwrap();

        let loaded_config = Config::load(&config_path).unwrap();
        assert_eq!(loaded_config.avocado.ext.dir, "/test/extensions");
        assert_eq!(loaded_config.get_sysext_mutable().unwrap(), "auto");
        assert_eq!(loaded_config.get_confext_mutable().unwrap(), "yes");
    }

    #[test]
    fn test_save_and_load_config_with_mutable() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("roundtrip_mutable_config.toml");

        let mut config = Config::default();
        config.avocado.ext.dir = "/test/extensions".to_string();
        config.avocado.ext.mutable = Some("auto".to_string());

        config.save(&config_path).unwrap();

        let loaded_config = Config::load(&config_path).unwrap();
        assert_eq!(loaded_config.avocado.ext.dir, "/test/extensions");
        #[allow(deprecated)]
        {
            assert_eq!(loaded_config.get_extension_mutable().unwrap(), "auto");
        }
    }

    #[test]
    fn test_mutable_validation_error_message() {
        // Test sysext validation error
        let mut config = Config::default();
        config.avocado.ext.sysext_mutable = Some("invalid_value".to_string());

        let result = config.get_sysext_mutable();
        assert!(result.is_err());

        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Invalid mutable value 'invalid_value'"));
        assert!(error_message
            .contains("Must be one of: no, auto, yes, import, ephemeral, ephemeral-import"));

        // Test confext validation error
        let mut config = Config::default();
        config.avocado.ext.confext_mutable = Some("invalid_value".to_string());

        let result = config.get_confext_mutable();
        assert!(result.is_err());

        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Invalid mutable value 'invalid_value'"));
        assert!(error_message
            .contains("Must be one of: no, auto, yes, import, ephemeral, ephemeral-import"));

        // Test legacy validation error
        let mut config = Config::default();
        config.avocado.ext.mutable = Some("invalid_value".to_string());

        #[allow(deprecated)]
        let result = config.get_extension_mutable();
        assert!(result.is_err());

        let error_message = result.unwrap_err().to_string();
        assert!(error_message.contains("Invalid mutable value 'invalid_value'"));
        assert!(error_message
            .contains("Must be one of: no, auto, yes, import, ephemeral, ephemeral-import"));
    }

    #[test]
    fn test_runtime_retention_default() {
        let config = Config::default();
        assert_eq!(config.runtime_retention(), 3);
    }

    #[test]
    fn test_runtime_retention_clamps_to_min_1() {
        let mut config = Config::default();
        config.avocado.gc.runtime_retention = 0;
        assert_eq!(config.runtime_retention(), 1);
    }

    #[test]
    fn test_runtime_retention_from_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("gc_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/var/lib/avocado/images"

[avocado.gc]
runtime_retention = 5
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.runtime_retention(), 5);
    }

    #[test]
    fn test_gc_defaults_when_omitted() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("no_gc_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/var/lib/avocado/images"
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.runtime_retention(), 3);
        assert!(config.auto_gc());
    }

    #[test]
    fn test_auto_gc_default_true() {
        let config = Config::default();
        assert!(config.auto_gc());
    }

    #[test]
    fn test_auto_gc_disabled_from_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("auto_gc_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/var/lib/avocado/images"

[avocado.gc]
auto_gc = false
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert!(!config.auto_gc());
    }

    #[test]
    fn test_stream_os_to_partition_default_false() {
        let config = Config::default();
        assert!(!config.stream_os_to_partition());
    }

    #[test]
    fn test_stream_os_to_partition_from_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("stream_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/var/lib/avocado/images"

[avocado.update]
stream_os_to_partition = true
"#;

        fs::write(&config_path, config_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert!(config.stream_os_to_partition());
    }

    #[test]
    fn test_load_with_override() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("override_test.toml");

        let config_content = r#"
[avocado.ext]
dir = "/override/test/path"
"#;

        fs::write(&config_path, config_content).unwrap();

        // Test with custom path
        let config = Config::load_with_override(Some(config_path.to_str().unwrap())).unwrap();
        assert_eq!(config.avocado.ext.dir, "/override/test/path");

        // Test with default path (should return default config since default doesn't exist)
        let default_config = Config::load_with_override(None).unwrap();
        assert_eq!(default_config.avocado.ext.dir, "/var/lib/avocado/images");
    }

    /// The point of the change: setting the base directory in the config
    /// file must actually move enablement state. Before, five call sites
    /// inlined `/var/lib/avocado/os-releases/…` and this was silently a
    /// no-op.
    #[test]
    fn os_releases_dir_follows_the_configured_base_dir() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let restore = ScopedEnv::clear(&["AVOCADO_TEST_MODE", "AVOCADO_BASE_DIR"]);

        let mut config = Config::default();
        config.avocado.runtimes_dir = Some("/mnt/state/avocado".to_string());
        assert_eq!(
            config.get_os_releases_dir("2026.1"),
            "/mnt/state/avocado/os-releases/2026.1"
        );

        // Unconfigured still lands on the shipped default.
        assert_eq!(
            Config::default().get_os_releases_dir("2026.1"),
            "/var/lib/avocado/os-releases/2026.1"
        );
        drop(restore);
    }

    #[test]
    fn os_releases_dir_honours_the_base_dir_env_override() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let restore = ScopedEnv::clear(&["AVOCADO_TEST_MODE"]);
        std::env::set_var("AVOCADO_BASE_DIR", "/env/base");

        let mut config = Config::default();
        config.avocado.runtimes_dir = Some("/config/base".to_string());
        // Env wins over config, matching get_avocado_base_dir.
        assert_eq!(config.get_os_releases_dir("v1"), "/env/base/os-releases/v1");

        std::env::remove_var("AVOCADO_BASE_DIR");
        drop(restore);
    }

    /// Test mode redirects to TMPDIR regardless of config, and now does so
    /// from a single place rather than five copies that could drift.
    #[test]
    fn test_mode_redirects_os_releases_to_tmpdir() {
        let _guard = ENV_VAR_MUTEX.lock().unwrap();
        let restore = ScopedEnv::clear(&[]);
        std::env::set_var("AVOCADO_TEST_MODE", "1");
        // A real directory, kept: tests in other modules call TempDir::new()
        // without ENV_VAR_MUTEX and read TMPDIR while this is set. A path that
        // does not exist (the old "/scratch") made them fail on NotFound.
        let scratch = tempfile::tempdir().unwrap().keep();
        std::env::set_var("TMPDIR", &scratch);

        let mut config = Config::default();
        config.avocado.runtimes_dir = Some("/mnt/state/avocado".to_string());
        assert_eq!(
            config.get_os_releases_dir("v1"),
            format!("{}/avocado/os-releases/v1", scratch.display())
        );

        std::env::remove_var("AVOCADO_TEST_MODE");
        drop(restore);
    }

    /// Saves and restores env vars around a test so the shared process
    /// environment doesn't leak between them.
    struct ScopedEnv(Vec<(String, Option<String>)>);

    impl ScopedEnv {
        fn clear(names: &[&str]) -> Self {
            let saved: Vec<_> = ["AVOCADO_TEST_MODE", "AVOCADO_BASE_DIR", "TMPDIR"]
                .iter()
                .map(|n| (n.to_string(), std::env::var(n).ok()))
                .collect();
            for n in names {
                std::env::remove_var(n);
            }
            Self(saved)
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}
