use thiserror::Error;

/// Unified error type for all avocadoctl service operations.
/// Maps 1:1 to Varlink error declarations.
#[derive(Error, Debug)]
pub enum AvocadoError {
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

    #[error("Extension not found: {name}")]
    ExtensionNotFound { name: String },

    #[error("Runtime not found: {id}")]
    RuntimeNotFound { id: String },

    #[error("Ambiguous runtime ID '{id}': matches {candidates:?}")]
    AmbiguousRuntimeId { id: String, candidates: Vec<String> },

    #[error("Cannot remove the active runtime. Activate a different runtime first.")]
    RemoveActiveRuntime,

    #[error("Staging error: {reason}")]
    StagingFailed { reason: String },

    #[error("Update error: {reason}")]
    UpdateFailed { reason: String },

    #[error("Merge failed: {reason}")]
    MergeFailed { reason: String },

    #[error("Unmerge failed: {reason}")]
    UnmergeFailed { reason: String },

    #[error("Mount failed for '{extension}': {reason}")]
    MountFailed { extension: String, reason: String },

    #[error("Unmount failed for '{extension}': {reason}")]
    UnmountFailed { extension: String, reason: String },

    #[error("{failures} of {attempted} unmount operations failed: {details}")]
    UnmountBatchFailed {
        attempted: usize,
        failures: usize,
        details: String,
    },

    #[error("No root authority configured")]
    NoRootAuthority,

    #[error("Metadata key '{key}' not found for runtime '{id}'")]
    MetadataKeyNotFound { id: String, key: String },

    #[error("Parse failed: {reason}")]
    ParseFailed { reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convert from commands::ext::SystemdError
impl From<crate::commands::ext::SystemdError> for AvocadoError {
    fn from(e: crate::commands::ext::SystemdError) -> Self {
        match e {
            crate::commands::ext::SystemdError::CommandFailed { command, source } => {
                AvocadoError::CommandFailed { command, source }
            }
            crate::commands::ext::SystemdError::CommandExitedWithError {
                command,
                exit_code,
                stderr,
            } => AvocadoError::CommandExitedWithError {
                command,
                exit_code,
                stderr,
            },
            crate::commands::ext::SystemdError::ConfigurationError { message } => {
                AvocadoError::ConfigurationError { message }
            }
            crate::commands::ext::SystemdError::UnmountBatchFailed {
                attempted,
                failures,
                details,
            } => AvocadoError::UnmountBatchFailed {
                attempted,
                failures,
                details,
            },
        }
    }
}

/// Convert from staging::StagingError
impl From<crate::staging::StagingError> for AvocadoError {
    fn from(e: crate::staging::StagingError) -> Self {
        match e {
            crate::staging::StagingError::StagingFailed(reason) => {
                AvocadoError::StagingFailed { reason }
            }
            crate::staging::StagingError::RemoveActiveRuntime => AvocadoError::RemoveActiveRuntime,
            crate::staging::StagingError::RuntimeNotFound(id) => {
                AvocadoError::RuntimeNotFound { id }
            }
            crate::staging::StagingError::MissingImages(details) => {
                AvocadoError::StagingFailed { reason: details }
            }
            crate::staging::StagingError::HashMismatch(details) => {
                AvocadoError::StagingFailed { reason: details }
            }
            crate::staging::StagingError::UpdateInProgress => AvocadoError::StagingFailed {
                reason: crate::staging::StagingError::UpdateInProgress.to_string(),
            },
        }
    }
}

/// Convert from update::UpdateError
impl From<crate::update::UpdateError> for AvocadoError {
    fn from(e: crate::update::UpdateError) -> Self {
        match e {
            crate::update::UpdateError::NoTrustAnchor => AvocadoError::NoRootAuthority,
            other => AvocadoError::UpdateFailed {
                reason: other.to_string(),
            },
        }
    }
}

/// Convert from commands::hitl::HitlError
impl From<crate::commands::hitl::HitlError> for AvocadoError {
    fn from(e: crate::commands::hitl::HitlError) -> Self {
        match e {
            crate::commands::hitl::HitlError::Mount {
                extension,
                mount_point: _,
                error,
            } => AvocadoError::MountFailed {
                extension,
                reason: error,
            },
            crate::commands::hitl::HitlError::Unmount { mount_point, error } => {
                AvocadoError::UnmountFailed {
                    extension: mount_point,
                    reason: error,
                }
            }
            other => AvocadoError::CommandFailed {
                command: "hitl".to_string(),
                source: std::io::Error::other(other.to_string()),
            },
        }
    }
}

/// Convert from config::ConfigError
impl From<crate::config::ConfigError> for AvocadoError {
    fn from(e: crate::config::ConfigError) -> Self {
        AvocadoError::ConfigurationError {
            message: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch unmount failure must keep its `attempted`/`failures` counts in
    /// their own fields rather than stuffed into `UnmountFailed`'s `extension`
    /// field, which every other producer of that variant (see
    /// `commands::hitl::HitlError::Unmount` below) fills with a real mount
    /// point name. Reusing the field for a count broke that contract for
    /// anything reading `AvocadoError` structurally rather than as a rendered
    /// string.
    #[test]
    fn unmount_batch_failure_keeps_the_count_out_of_the_extension_field() {
        let sys_err = crate::commands::ext::SystemdError::UnmountBatchFailed {
            attempted: 3,
            failures: 2,
            details: "a: busy; b: busy".to_string(),
        };

        let avocado_err = AvocadoError::from(sys_err);

        assert!(
            matches!(
                avocado_err,
                AvocadoError::UnmountBatchFailed {
                    attempted: 3,
                    failures: 2,
                    ..
                }
            ),
            "a batch failure must not be reshaped into UnmountFailed's per-extension fields"
        );
        assert_eq!(
            avocado_err.to_string(),
            "2 of 3 unmount operations failed: a: busy; b: busy"
        );
    }
}
