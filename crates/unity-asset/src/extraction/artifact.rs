use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use unity_asset_core::DigestV1;
use unity_asset_write::artifact::{ArtifactNameError, LogicalArtifactName};

use crate::workspace::commit::platform::{
    DirectoryIdentity, FileIdentity, acquire_private_lock_in_parent,
    atomic_replace_verified_tracked, create_private_file_in_parent, ensure_directory_no_follow,
    observe_file_identity, open_readonly_regular_in_parent, opened_file_identity,
    remove_owned_file_in_parent,
};

use super::model::paths_conflict;

const TEMPORARY_CREATE_ATTEMPTS: usize = 32;
const EXECUTION_LOCK_NAME: &str = ".unity-asset-extraction.lock";

/// Stable machine-readable stage for failures in extraction-owned output storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtractionOutputErrorKind {
    InvalidName,
    PortableCollision,
    MissingParent,
    UnplannedPath,
    ResolveCurrentDirectory,
    ValidateRoot,
    PrepareRoot,
    LockRoot,
    PrepareDirectory,
    CreateTemporary,
    InspectTemporary,
    OpenExisting,
    InspectExisting,
    HashExisting,
    FinalizeTemporary,
    Publish,
    DiscardTemporary,
    TemporaryIdentityChanged,
    ExistingHashLimitExceeded,
}

impl ExtractionOutputErrorKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidName => "invalid_name",
            Self::PortableCollision => "portable_collision",
            Self::MissingParent => "missing_parent",
            Self::UnplannedPath => "unplanned_path",
            Self::ResolveCurrentDirectory => "resolve_current_directory",
            Self::ValidateRoot => "validate_root",
            Self::PrepareRoot => "prepare_root",
            Self::LockRoot => "lock_root",
            Self::PrepareDirectory => "prepare_directory",
            Self::CreateTemporary => "create_temporary",
            Self::InspectTemporary => "inspect_temporary",
            Self::OpenExisting => "open_existing",
            Self::InspectExisting => "inspect_existing",
            Self::HashExisting => "hash_existing",
            Self::FinalizeTemporary => "finalize_temporary",
            Self::Publish => "publish",
            Self::DiscardTemporary => "discard_temporary",
            Self::TemporaryIdentityChanged => "temporary_identity_changed",
            Self::ExistingHashLimitExceeded => "existing_hash_limit_exceeded",
        }
    }
}

impl std::fmt::Display for ExtractionOutputErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ResolveCurrentDirectory => "resolve current directory",
            Self::ValidateRoot => "validate output root",
            Self::PrepareRoot => "create output root",
            Self::LockRoot => "lock output root",
            Self::PrepareDirectory => "create output directory",
            Self::CreateTemporary => "create temporary output",
            Self::InspectTemporary => "inspect temporary output",
            Self::OpenExisting => "open existing output",
            Self::InspectExisting => "inspect existing output",
            Self::HashExisting => "hash existing output",
            Self::FinalizeTemporary => "finalize temporary output",
            Self::Publish => "publish",
            Self::DiscardTemporary => "remove temporary output",
            Self::InvalidName => "validate output name",
            Self::PortableCollision => "validate portable output names",
            Self::MissingParent => "resolve output parent",
            Self::UnplannedPath => "resolve planned output path",
            Self::TemporaryIdentityChanged => "verify temporary output identity",
            Self::ExistingHashLimitExceeded => "enforce existing-output hash limit",
        })
    }
}

#[derive(Debug, Error)]
pub(super) enum OutputArtifactError {
    #[error(transparent)]
    InvalidName(#[from] ArtifactNameError),
    #[error("output names {first:?} and {second:?} cannot coexist on portable filesystems")]
    PortableCollision { first: String, second: String },
    #[error("output path has no parent: {0:?}")]
    MissingParent(PathBuf),
    #[error("output layout does not contain planned path {0:?}")]
    UnplannedPath(String),
    #[error("failed to {kind} {path:?}: {source}")]
    Io {
        kind: ExtractionOutputErrorKind,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("temporary output identity changed before hashing: {0:?}")]
    TemporaryIdentityChanged(PathBuf),
    #[error(
        "existing output {path:?} requires hashing {length} bytes, exceeding the limit {limit}"
    )]
    ExistingHashLimitExceeded {
        path: PathBuf,
        length: u64,
        limit: u64,
    },
}

impl OutputArtifactError {
    pub(super) const fn kind(&self) -> ExtractionOutputErrorKind {
        match self {
            Self::InvalidName(_) => ExtractionOutputErrorKind::InvalidName,
            Self::PortableCollision { .. } => ExtractionOutputErrorKind::PortableCollision,
            Self::MissingParent(_) => ExtractionOutputErrorKind::MissingParent,
            Self::UnplannedPath(_) => ExtractionOutputErrorKind::UnplannedPath,
            Self::Io { kind, .. } => *kind,
            Self::TemporaryIdentityChanged(_) => {
                ExtractionOutputErrorKind::TemporaryIdentityChanged
            }
            Self::ExistingHashLimitExceeded { .. } => {
                ExtractionOutputErrorKind::ExistingHashLimitExceeded
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct OutputLayout {
    paths: BTreeMap<String, PreparedOutputPath>,
    _execution_lock: File,
}

impl OutputLayout {
    pub(super) fn prepare<'path>(
        root: &Path,
        relative_paths: impl IntoIterator<Item = &'path str>,
    ) -> Result<Self, OutputArtifactError> {
        let lock_name = LogicalArtifactName::new(EXECUTION_LOCK_NAME)?;
        let mut validated = Vec::new();
        for relative in relative_paths {
            let name = LogicalArtifactName::new(relative)?;
            if paths_conflict(name.portability_key(), lock_name.portability_key()) {
                return Err(OutputArtifactError::PortableCollision {
                    first: EXECUTION_LOCK_NAME.to_owned(),
                    second: name.as_str().to_owned(),
                });
            }
            validated.push(name);
        }
        validated.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        if let Some(pair) = validated
            .windows(2)
            .find(|pair| paths_conflict(pair[0].as_str(), pair[1].as_str()))
        {
            return Err(OutputArtifactError::PortableCollision {
                first: pair[0].as_str().to_owned(),
                second: pair[1].as_str().to_owned(),
            });
        }
        let mut portable = validated
            .iter()
            .map(|name| (name.portability_key(), name.as_str()))
            .collect::<Vec<_>>();
        portable.sort_unstable();
        if let Some(pair) = portable
            .windows(2)
            .find(|pair| paths_conflict(pair[0].0, pair[1].0))
        {
            return Err(OutputArtifactError::PortableCollision {
                first: pair[0].1.to_owned(),
                second: pair[1].1.to_owned(),
            });
        }

        let root = absolute_path(root)?;
        let root_identity =
            ensure_directory_no_follow(&root).map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::PrepareRoot,
                path: root.clone(),
                source,
            })?;
        let lock_path = root.join(EXECUTION_LOCK_NAME);
        let execution_lock =
            acquire_private_lock_in_parent(&lock_path, &root_identity).map_err(|source| {
                OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::LockRoot,
                    path: lock_path,
                    source,
                }
            })?;

        let mut paths = BTreeMap::new();
        for name in validated {
            let relative = name.as_str().to_owned();
            let final_path = root.join(logical_path(name.as_str()));
            let parent = final_path
                .parent()
                .ok_or_else(|| OutputArtifactError::MissingParent(final_path.clone()))?;
            let parent_identity =
                ensure_directory_no_follow(parent).map_err(|source| OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::PrepareDirectory,
                    path: parent.to_path_buf(),
                    source,
                })?;
            paths.insert(
                relative.clone(),
                PreparedOutputPath {
                    final_path,
                    parent_identity,
                },
            );
        }

        Ok(Self {
            paths,
            _execution_lock: execution_lock,
        })
    }

    pub(super) fn path(&self, relative: &str) -> Result<&PreparedOutputPath, OutputArtifactError> {
        self.paths
            .get(relative)
            .ok_or_else(|| OutputArtifactError::UnplannedPath(relative.to_owned()))
    }
}

#[derive(Debug, Clone)]
pub(super) struct PreparedOutputPath {
    final_path: PathBuf,
    parent_identity: DirectoryIdentity,
}

impl PreparedOutputPath {
    pub(super) fn create_staging(&self) -> Result<StagingOutput, OutputArtifactError> {
        let parent = self
            .final_path
            .parent()
            .ok_or_else(|| OutputArtifactError::MissingParent(self.final_path.clone()))?;
        for _ in 0..TEMPORARY_CREATE_ATTEMPTS {
            let token = rand::random::<u128>();
            let temporary_path = parent.join(format!(".unity-asset-{token:032x}.tmp"));
            match create_private_file_in_parent(&temporary_path, &self.parent_identity) {
                Ok(file) => {
                    let identity =
                        opened_file_identity(&file).map_err(|source| OutputArtifactError::Io {
                            kind: ExtractionOutputErrorKind::InspectTemporary,
                            path: temporary_path.clone(),
                            source,
                        })?;
                    return Ok(StagingOutput {
                        file: Some(file),
                        temporary_path,
                        final_path: self.final_path.clone(),
                        identity,
                        parent_identity: self.parent_identity.clone(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(OutputArtifactError::Io {
                        kind: ExtractionOutputErrorKind::CreateTemporary,
                        path: temporary_path,
                        source,
                    });
                }
            }
        }
        Err(OutputArtifactError::Io {
            kind: ExtractionOutputErrorKind::CreateTemporary,
            path: parent.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary output name attempts were exhausted",
            ),
        })
    }

    pub(super) fn open_existing(&self) -> Result<Option<File>, OutputArtifactError> {
        match open_readonly_regular_in_parent(&self.final_path, &self.parent_identity) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::OpenExisting,
                path: self.final_path.clone(),
                source,
            }),
        }
    }

    pub(super) fn exists(&self) -> Result<bool, OutputArtifactError> {
        self.open_existing().map(|file| file.is_some())
    }

    pub(super) fn hash_existing_bounded(
        &self,
        limit: u64,
    ) -> Result<Option<(u64, DigestV1)>, OutputArtifactError> {
        let Some(mut file) = self.open_existing()? else {
            return Ok(None);
        };
        let length = file
            .metadata()
            .map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::InspectExisting,
                path: self.final_path.clone(),
                source,
            })?
            .len();
        if length > limit {
            return Err(OutputArtifactError::ExistingHashLimitExceeded {
                path: self.final_path.clone(),
                length,
                limit,
            });
        }
        let digest =
            DigestV1::hash_reader(&mut file, length).map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::HashExisting,
                path: self.final_path.clone(),
                source,
            })?;
        Ok(Some((length, digest)))
    }
}

pub(super) struct StagingOutput {
    file: Option<File>,
    temporary_path: PathBuf,
    final_path: PathBuf,
    identity: FileIdentity,
    parent_identity: DirectoryIdentity,
}

impl StagingOutput {
    pub(super) fn writer(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("staging writer is only available before finish")
    }

    pub(super) fn finish(mut self) -> Result<StagedOutput, OutputArtifactError> {
        let mut file = self
            .file
            .take()
            .expect("staging writer is only finished once");
        file.flush().map_err(|source| OutputArtifactError::Io {
            kind: ExtractionOutputErrorKind::FinalizeTemporary,
            path: self.temporary_path.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| OutputArtifactError::Io {
            kind: ExtractionOutputErrorKind::FinalizeTemporary,
            path: self.temporary_path.clone(),
            source,
        })?;
        drop(file);

        let observed = observe_file_identity(&self.temporary_path).map_err(|source| {
            OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::FinalizeTemporary,
                path: self.temporary_path.clone(),
                source,
            }
        })?;
        if !observed.same_object(&self.identity) {
            return Err(OutputArtifactError::TemporaryIdentityChanged(
                self.temporary_path.clone(),
            ));
        }
        let mut reader =
            open_readonly_regular_in_parent(&self.temporary_path, &self.parent_identity).map_err(
                |source| OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::FinalizeTemporary,
                    path: self.temporary_path.clone(),
                    source,
                },
            )?;
        let length = reader
            .metadata()
            .map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::FinalizeTemporary,
                path: self.temporary_path.clone(),
                source,
            })?
            .len();
        let digest = DigestV1::hash_reader(&mut reader, length).map_err(|source| {
            OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::FinalizeTemporary,
                path: self.temporary_path.clone(),
                source,
            }
        })?;

        let staged = StagedOutput {
            temporary_path: self.temporary_path.clone(),
            final_path: self.final_path.clone(),
            identity: observed,
            parent_identity: self.parent_identity.clone(),
            length,
            digest,
            released: false,
        };
        self.disarm();
        Ok(staged)
    }

    fn disarm(&mut self) {
        self.temporary_path.clear();
    }
}

impl Drop for StagingOutput {
    fn drop(&mut self) {
        self.file.take();
        if !self.temporary_path.as_os_str().is_empty() {
            let _ = remove_owned_file_in_parent(
                &self.temporary_path,
                &self.identity,
                &self.parent_identity,
            );
        }
    }
}

#[derive(Debug)]
pub(super) struct StagedOutput {
    temporary_path: PathBuf,
    final_path: PathBuf,
    identity: FileIdentity,
    parent_identity: DirectoryIdentity,
    length: u64,
    digest: DigestV1,
    released: bool,
}

impl StagedOutput {
    pub(super) const fn length(&self) -> u64 {
        self.length
    }

    pub(super) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    pub(super) fn publish(mut self, replace_existing: bool) -> Result<(), OutputArtifactError> {
        let result = atomic_replace_verified_tracked(
            &self.temporary_path,
            &self.final_path,
            replace_existing,
            &self.identity,
            self.digest,
            &self.parent_identity,
            &self.parent_identity,
        );
        match result {
            Ok(()) => {
                self.released = true;
                Ok(())
            }
            Err(error) => {
                if error.moved_or_unknown_state() {
                    self.released = true;
                }
                Err(OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::Publish,
                    path: self.final_path.clone(),
                    source: error.into_error(),
                })
            }
        }
    }

    pub(super) fn discard(mut self) -> Result<(), OutputArtifactError> {
        remove_owned_file_in_parent(&self.temporary_path, &self.identity, &self.parent_identity)
            .map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::DiscardTemporary,
                path: self.temporary_path.clone(),
                source,
            })?;
        self.released = true;
        Ok(())
    }
}

impl Drop for StagedOutput {
    fn drop(&mut self) {
        if !self.released {
            let _ = remove_owned_file_in_parent(
                &self.temporary_path,
                &self.identity,
                &self.parent_identity,
            );
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, OutputArtifactError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::ResolveCurrentDirectory,
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(OutputArtifactError::Io {
            kind: ExtractionOutputErrorKind::ValidateRoot,
            path,
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "output root contains a parent-relative component",
            ),
        });
    }
    Ok(path)
}

fn logical_path(value: &str) -> PathBuf {
    value.split('/').collect()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::{OutputArtifactError, OutputLayout};

    #[test]
    fn layout_rejects_ancestor_and_descendant_paths_before_creating_them() {
        let directory = tempfile::tempdir().expect("temporary output root");
        let error = OutputLayout::prepare(directory.path(), ["objects", "objects/item.bin"])
            .expect_err("a file cannot also be a directory ancestor");

        assert_eq!(
            error.kind(),
            super::ExtractionOutputErrorKind::PortableCollision
        );
        assert!(matches!(
            error,
            OutputArtifactError::PortableCollision { .. }
        ));
        assert!(!directory.path().join("objects").exists());
    }

    #[test]
    fn layout_rejects_case_colliding_paths_before_creating_them() {
        let directory = tempfile::tempdir().expect("temporary output root");
        let error = OutputLayout::prepare(directory.path(), ["Assets/Icon.png", "assets/icon.png"])
            .expect_err("case-insensitive filesystems cannot host both paths");

        assert!(matches!(
            error,
            OutputArtifactError::PortableCollision { .. }
        ));
        assert!(!directory.path().join("Assets").exists());
        assert!(!directory.path().join("assets").exists());
    }

    #[test]
    fn layout_accepts_an_existing_writable_root() {
        let root = tempfile::tempdir().unwrap();
        let layout = OutputLayout::prepare(root.path(), ["documents/item.yaml"]).unwrap();

        assert!(layout.path("documents/item.yaml").is_ok());
    }

    #[test]
    fn layout_stages_and_publishes_into_an_existing_writable_root() {
        let root = tempfile::tempdir().unwrap();
        let layout = OutputLayout::prepare(root.path(), ["documents/item.yaml"]).unwrap();
        let mut staging = layout
            .path("documents/item.yaml")
            .unwrap()
            .create_staging()
            .unwrap();
        staging.writer().write_all(b"published").unwrap();
        let staged = staging.finish().unwrap();

        staged.publish(false).unwrap();

        assert_eq!(
            std::fs::read(root.path().join("documents/item.yaml")).unwrap(),
            b"published"
        );
    }

    #[test]
    fn layout_holds_an_exclusive_root_lock() {
        let directory = tempfile::tempdir().expect("temporary output root");
        let first = OutputLayout::prepare(directory.path(), ["objects/item.bin"])
            .expect("first executor acquires the root lock");
        let error = OutputLayout::prepare(directory.path(), ["objects/item.bin"])
            .expect_err("second executor must not race publication");

        assert_eq!(error.kind(), super::ExtractionOutputErrorKind::LockRoot);
        assert!(matches!(error, OutputArtifactError::Io { .. }));
        drop(first);
    }

    #[cfg(unix)]
    #[test]
    fn layout_rejects_a_symlinked_output_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("output root");
        let external = tempfile::tempdir().expect("external directory");
        symlink(external.path(), root.path().join("escape")).expect("directory symlink");

        OutputLayout::prepare(root.path(), ["escape/artifact.bin"])
            .expect_err("symlinked output ancestor must be rejected");

        assert!(!external.path().join("artifact.bin").exists());
    }

    #[cfg(windows)]
    #[test]
    fn layout_rejects_a_reparse_output_ancestor() {
        use std::os::windows::fs::symlink_dir;

        let root = tempfile::tempdir().expect("output root");
        let external = tempfile::tempdir().expect("external directory");
        if let Err(error) = symlink_dir(external.path(), root.path().join("escape")) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("directory reparse point: {error}");
        }

        OutputLayout::prepare(root.path(), ["escape/artifact.bin"])
            .expect_err("reparse output ancestor must be rejected");

        assert!(!external.path().join("artifact.bin").exists());
    }
}
