//! Filesystem-only external module installation.
//!
//! `PendingInspection`, `RedeemedInspection`, and `ValidatedStage` are owned
//! by the inspection flow.  The integration lane must expose a
//! `ValidatedStage` as the private wrapper and child module directory used by
//! [`install_staged_module`]; this module intentionally neither redeems nor
//! re-inspects a pending stage.

use rustix::{
    fs::{CWD, RenameFlags},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

use super::manifest::{validate_manifest_at, validate_module_id};
use crate::error::ExternalError;

const OWNER_FILE: &str = "owner.json";
const STAGE_CHILD: &str = "payload";
const WRAPPER_PREFIX: &str = ".lmod-install-";

/// Provenance bound to an installation wrapper.  Strict decoding prevents an
/// abandoned-wrapper cleanup from accepting a file with an ambiguous schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageMarker {
    pub format: u32,
    pub created_by: String,
    pub created_at: u64,
}

#[derive(Debug, Error)]
pub(crate) enum InstallError {
    #[error("module id is invalid")]
    InvalidModuleId,
    #[error("staging wrapper is unsafe or incomplete")]
    UnsafeStage,
    #[error("staging marker is malformed")]
    InvalidMarker,
    #[error("installation root is unsafe")]
    UnsafeInstallRoot,
    #[error("module target already exists")]
    TargetExists,
    #[error("staging and install root are on different filesystems")]
    CrossDevice,
    #[error("filesystem operation failed")]
    Filesystem(#[source] io::Error),
    #[error("installed module manifest did not validate")]
    PostInstallValidationFailed {
        #[source]
        validation: ExternalError,
    },
    #[error("installed module manifest did not validate and target rollback failed")]
    TargetCleanup(#[source] TargetCleanupError),
}

/// A rollback failure for a target that was already made visible. This is
/// distinct from [`StageCleanupError`], which concerns abandoned private staging.
#[derive(Debug, Error)]
#[error("target rollback failed ({kind:?})")]
pub(crate) struct TargetCleanupError {
    #[source]
    pub validation: ExternalError,
    pub kind: io::ErrorKind,
}

/// A successfully installed module and the descriptor validated after its
/// no-replace rename. The runtime must use this descriptor directly.
#[derive(Debug)]
pub(crate) struct InstalledModule {
    pub(crate) descriptor: super::manifest::ExternalModuleDescriptor,
}

/// The only marker schema accepted during abandoned-wrapper cleanup.
pub(crate) const STAGE_FORMAT: u32 = 1;
pub(crate) const STAGE_CREATED_BY: &str = "lavis";

/// Create a strict ownership marker in a private staging wrapper.
pub(crate) fn write_stage_marker(
    wrapper: &Path,
    created_at: SystemTime,
) -> Result<(), InstallError> {
    if !is_plain_directory(wrapper)? {
        return Err(InstallError::UnsafeStage);
    }
    let marker = StageMarker {
        format: STAGE_FORMAT,
        created_by: STAGE_CREATED_BY.into(),
        created_at: created_at
            .duration_since(UNIX_EPOCH)
            .map_err(|_| InstallError::InvalidMarker)?
            .as_secs(),
    };
    let bytes = serde_json::to_vec(&marker).map_err(|_| InstallError::InvalidMarker)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(wrapper.join(OWNER_FILE))
        .map_err(InstallError::Filesystem)?;
    file.write_all(&bytes).map_err(InstallError::Filesystem)
}

/// Remove wrappers left after an interrupted install, but only when their
/// strict owner marker proves they are installer-owned.  Symlinks are never
/// traversed or removed through this routine.
pub(crate) fn cleanup_abandoned_wrappers(
    staging_root: &Path,
) -> Result<Vec<StageCleanupError>, InstallError> {
    if !is_plain_directory(staging_root)? {
        return Err(InstallError::UnsafeStage);
    }
    let mut failures = Vec::new();
    for entry in fs::read_dir(staging_root).map_err(InstallError::Filesystem)? {
        let entry = entry.map_err(InstallError::Filesystem)?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(WRAPPER_PREFIX) {
            continue;
        }
        let wrapper = entry.path();
        if !is_plain_directory(&wrapper)? || read_stage_marker(&wrapper).is_err() {
            continue;
        }
        if let Err(error) = remove_marked_wrapper_no_follow(&wrapper) {
            failures.push(StageCleanupError {
                wrapper,
                kind: error.kind(),
            });
        }
    }
    Ok(failures)
}

/// Cleanup failure after a wrapper was identified as installer-owned. It is
/// distinct from `TargetCleanupError`, which follows a visible install failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StageCleanupError {
    pub wrapper: std::path::PathBuf,
    pub kind: io::ErrorKind,
}

/// Atomically install the `payload` child of an inspection-owned wrapper.
///
/// The caller must obtain `wrapper` and its owner from `ValidatedStage`, after
/// consuming a `RedeemedInspection`.  `PendingInspection` must never reach
/// this function.  The final validation deliberately runs after the rename;
/// if it fails, the newly created target is removed.  A rollback failure is a
/// distinct error because it leaves a visible target behind.
pub(crate) fn install_staged_module(
    wrapper: &Path,
    install_root: &Path,
    module_id: &str,
) -> Result<InstalledModule, InstallError> {
    validate_module_id(module_id).map_err(|_| InstallError::InvalidModuleId)?;
    if !is_plain_directory(wrapper)? {
        return Err(InstallError::UnsafeStage);
    }
    if !is_plain_directory(install_root)? {
        return Err(InstallError::UnsafeInstallRoot);
    }
    read_stage_marker(wrapper)?;
    let source = wrapper.join(STAGE_CHILD);
    if !is_plain_directory(&source)? {
        return Err(InstallError::UnsafeStage);
    }
    let target = install_root.join(module_id);
    match rustix::fs::renameat_with(CWD, &source, CWD, &target, RenameFlags::NOREPLACE) {
        Ok(()) => {}
        Err(error) => return Err(map_rename_error(error)),
    }

    let descriptor = match validate_manifest_at(&target.join("module.json"), Some(module_id)) {
        Ok(descriptor) => descriptor,
        Err(validation) => {
            return match remove_tree_no_follow(&target) {
                Ok(()) => Err(InstallError::PostInstallValidationFailed { validation }),
                Err(error) => Err(InstallError::TargetCleanup(TargetCleanupError {
                    validation,
                    kind: error.kind(),
                })),
            };
        }
    };

    // After the child has moved, the wrapper contains only owner.json.  Its
    // cleanup is best effort: a successful atomic install remains successful.
    if let Err(error) = remove_empty_wrapper(wrapper) {
        tracing::warn!(
            event = "external_module_install_wrapper_cleanup_failed",
            error = %error,
            "Installed external module successfully but could not remove its empty staging wrapper"
        );
    }
    Ok(InstalledModule { descriptor })
}

/// Removes only a redeemed wrapper. Other live approvals are never touched.
pub(crate) fn cleanup_redeemed_stage(wrapper: &Path) -> Result<(), StageCleanupError> {
    if !matches!(is_plain_directory(wrapper), Ok(true)) || read_stage_marker(wrapper).is_err() {
        return Err(StageCleanupError {
            wrapper: wrapper.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
        });
    }
    remove_marked_wrapper_no_follow(wrapper).map_err(|error| StageCleanupError {
        wrapper: wrapper.to_path_buf(),
        kind: error.kind(),
    })
}

fn map_rename_error(error: Errno) -> InstallError {
    match error {
        Errno::EXIST => InstallError::TargetExists,
        Errno::XDEV => InstallError::CrossDevice,
        error => InstallError::Filesystem(io::Error::from_raw_os_error(error.raw_os_error())),
    }
}

fn read_stage_marker(wrapper: &Path) -> Result<StageMarker, InstallError> {
    let owner_path = wrapper.join(OWNER_FILE);
    let metadata = fs::symlink_metadata(&owner_path).map_err(|_| InstallError::InvalidMarker)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4096 {
        return Err(InstallError::InvalidMarker);
    }
    let bytes = fs::read(owner_path).map_err(|_| InstallError::InvalidMarker)?;
    let marker =
        serde_json::from_slice::<StageMarker>(&bytes).map_err(|_| InstallError::InvalidMarker)?;
    if marker.format != STAGE_FORMAT || marker.created_by != STAGE_CREATED_BY {
        return Err(InstallError::InvalidMarker);
    }
    Ok(marker)
}

fn is_plain_directory(path: &Path) -> Result<bool, InstallError> {
    let metadata = fs::symlink_metadata(path).map_err(InstallError::Filesystem)?;
    Ok(metadata.file_type().is_dir() && !metadata.file_type().is_symlink())
}

fn remove_empty_wrapper(wrapper: &Path) -> io::Result<()> {
    let owner = wrapper.join(OWNER_FILE);
    let metadata = fs::symlink_metadata(&owner)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "owner symlink"));
    }
    let marker = fs::read(&owner)?;
    fs::remove_file(owner)?;
    match fs::remove_dir(wrapper) {
        Ok(()) => Ok(()),
        Err(remove_error) => {
            // Restore the exact marker before reporting failure. Startup cleanup
            // can then identify this wrapper even when a concurrent or stray
            // file prevented removing the directory.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(wrapper.join(OWNER_FILE))
            {
                Ok(mut file) => match file.write_all(&marker) {
                    Ok(()) => Err(remove_error),
                    Err(restore_error) => Err(restore_error),
                },
                Err(restore_error) => Err(restore_error),
            }
        }
    }
}

/// Remove a known installer wrapper while preserving `owner.json` until every
/// payload child has been removed. A failure before the final two operations
/// leaves the marker intact for startup cleanup.
fn remove_marked_wrapper_no_follow(wrapper: &Path) -> io::Result<()> {
    for entry in fs::read_dir(wrapper)? {
        let entry = entry?;
        if entry.file_name() == OWNER_FILE {
            continue;
        }
        remove_tree_no_follow(&entry.path())?;
    }
    remove_empty_wrapper(wrapper)
}

fn remove_tree_no_follow(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symlink in removal path",
        ));
    }
    if metadata.is_file() {
        return fs::remove_file(path);
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported removal type",
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let child_metadata = fs::symlink_metadata(&child)?;
        if child_metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink in removal tree",
            ));
        }
        remove_tree_no_follow(&child)?;
    }
    fs::remove_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn root(label: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("lavis-installer-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn wrapper(root: &Path, invalid_manifest: bool) -> std::path::PathBuf {
        let wrapper = root.join(".lmod-install-test");
        fs::create_dir(&wrapper).unwrap();
        write_stage_marker(&wrapper, SystemTime::UNIX_EPOCH).unwrap();
        let payload = wrapper.join(STAGE_CHILD);
        fs::create_dir(&payload).unwrap();
        let manifest: &[u8] = if invalid_manifest {
            b"{}"
        } else {
            b"{\"schema_version\":2,\"id\":\"test\",\"name\":\"Test\",\"version\":\"1\",\"author\":\"A\",\"entrypoint\":\"run\",\"commands\":[{\"name\":\"go\",\"summary_ru\":\"x\",\"description_ru\":\"x\",\"usage\":\"<value>\"}]}"
        };
        fs::write(payload.join("module.json"), manifest).unwrap();
        fs::write(payload.join("run"), b"#!/bin/sh").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(payload.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        wrapper
    }

    #[test]
    fn owner_schema_rejects_unknown_fields() {
        let parsed = serde_json::from_slice::<StageMarker>(
            br#"{"format":1,"created_by":"lavis","created_at":1,"extra":true}"#,
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn no_replace_rename_errors_keep_their_semantics() {
        assert!(matches!(
            map_rename_error(Errno::EXIST),
            InstallError::TargetExists
        ));
        assert!(matches!(
            map_rename_error(Errno::XDEV),
            InstallError::CrossDevice
        ));
    }

    #[test]
    fn failed_empty_wrapper_removal_restores_its_marker() {
        let wrapper = std::env::temp_dir().join(format!(
            "lavis-installer-wrapper-cleanup-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&wrapper);
        fs::create_dir(&wrapper).unwrap();
        write_stage_marker(&wrapper, SystemTime::UNIX_EPOCH).unwrap();
        fs::write(wrapper.join("prevents-removal"), b"x").unwrap();

        assert!(remove_empty_wrapper(&wrapper).is_err());
        assert!(wrapper.join(OWNER_FILE).is_file());
        assert!(read_stage_marker(&wrapper).is_ok());

        fs::remove_dir_all(&wrapper).unwrap();
    }

    #[test]
    fn target_collision_preserves_marked_payload() {
        let root = root("collision");
        let wrapper = wrapper(&root, false);
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        fs::create_dir(install_root.join("test")).unwrap();
        assert!(matches!(
            install_staged_module(&wrapper, &install_root, "test"),
            Err(InstallError::TargetExists)
        ));
        assert!(wrapper.join(OWNER_FILE).is_file());
        assert!(wrapper.join(STAGE_CHILD).is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validation_failure_rolls_back_target_and_retains_marker() {
        let root = root("rollback");
        let wrapper = wrapper(&root, true);
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        assert!(matches!(
            install_staged_module(&wrapper, &install_root, "test"),
            Err(InstallError::PostInstallValidationFailed { .. })
        ));
        assert!(!install_root.join("test").exists());
        assert!(read_stage_marker(&wrapper).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rollback_failure_retains_validation_error() {
        let root = root("rollback-failure");
        let wrapper = wrapper(&root, true);
        symlink(
            "/does-not-matter",
            wrapper.join(STAGE_CHILD).join("blocks-cleanup"),
        )
        .unwrap();
        let install_root = root.join("installed");
        fs::create_dir(&install_root).unwrap();
        match install_staged_module(&wrapper, &install_root, "test") {
            Err(InstallError::TargetCleanup(error)) => {
                assert!(matches!(error.validation, ExternalError::MalformedManifest));
            }
            other => panic!("unexpected result: {other:?}"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn marker_symlinks_and_unknown_fields_are_rejected() {
        let root = root("marker");
        let wrapper = root.join(".lmod-install-marker");
        fs::create_dir(&wrapper).unwrap();
        fs::write(
            wrapper.join(OWNER_FILE),
            br#"{"format":1,"created_by":"lavis","created_at":1,"extra":true}"#,
        )
        .unwrap();
        assert!(matches!(
            read_stage_marker(&wrapper),
            Err(InstallError::InvalidMarker)
        ));
        fs::remove_file(wrapper.join(OWNER_FILE)).unwrap();
        symlink("/tmp", wrapper.join(OWNER_FILE)).unwrap();
        assert!(matches!(
            read_stage_marker(&wrapper),
            Err(InstallError::InvalidMarker)
        ));
        let _ = fs::remove_dir_all(root);
    }
}
