use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use unity_asset_core::DigestV1;
use unity_asset_write::artifact::{ArtifactNameError, LogicalArtifactName};

use crate::workspace::commit::platform::{
    DirectoryIdentity, FileIdentity, acquire_private_lock_in_parent,
    atomic_replace_verified_tracked, create_private_file_in_parent, ensure_directory_no_follow,
    observe_directory_identity, observe_file_identity, open_readonly_regular_in_parent,
    opened_file_identity, remove_owned_file_in_parent,
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
    ReservedPath,
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
            Self::ReservedPath => "reserved_path",
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
            Self::ReservedPath => "validate reserved output names",
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
    #[error("output path is reserved for extraction protocol state: {0:?}")]
    ReservedPath(String),
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
            Self::ReservedPath(_) => ExtractionOutputErrorKind::ReservedPath,
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

#[derive(Debug, Clone, Copy)]
pub(super) struct EvidenceReadBudget {
    remaining: u64,
}

#[derive(Debug)]
enum EvidenceReadError {
    Limit { required: u64, remaining: u64 },
    Io(io::Error),
}

impl EvidenceReadBudget {
    pub(super) const fn new(limit: u64) -> Self {
        Self { remaining: limit }
    }

    fn hash_reader(
        &mut self,
        reader: impl Read,
        length: u64,
    ) -> Result<DigestV1, EvidenceReadError> {
        if length > self.remaining {
            return Err(EvidenceReadError::Limit {
                required: length,
                remaining: self.remaining,
            });
        }
        self.remaining -= length;
        let mut bounded = reader.take(length);
        DigestV1::hash_reader(&mut bounded, length).map_err(EvidenceReadError::Io)
    }
}

impl OutputLayout {
    pub(super) fn has_existing(root: &Path, relative: &str) -> Result<bool, OutputArtifactError> {
        Self::open_existing_at(root, relative).map(|file| file.is_some())
    }

    pub(super) fn open_existing_at(
        root: &Path,
        relative: &str,
    ) -> Result<Option<File>, OutputArtifactError> {
        let name = LogicalArtifactName::new(relative)?;
        let root = resolve_output_root(root)?;
        let root_identity = match observe_directory_identity(&root) {
            Ok(identity) => identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::ValidateRoot,
                    path: root,
                    source,
                });
            }
        };
        let path = root.join(logical_path(name.as_str()));
        if path.parent() != Some(root.as_path()) {
            return Err(OutputArtifactError::UnplannedPath(relative.to_owned()));
        }
        match open_readonly_regular_in_parent(&path, &root_identity) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::OpenExisting,
                path,
                source,
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn prepare<'path>(
        root: &Path,
        relative_paths: impl IntoIterator<Item = &'path str>,
    ) -> Result<Self, OutputArtifactError> {
        Self::prepare_with_internal_paths(root, relative_paths, std::iter::empty::<&str>(), &[])
    }

    pub(super) fn prepare_with_internal_paths<'path, 'internal>(
        root: &Path,
        relative_paths: impl IntoIterator<Item = &'path str>,
        internal_paths: impl IntoIterator<Item = &'internal str>,
        reserved_root_prefixes: &[&str],
    ) -> Result<Self, OutputArtifactError> {
        let lock_name = LogicalArtifactName::new(EXECUTION_LOCK_NAME)?;
        let mut validated = Vec::new();
        for relative in relative_paths {
            let name = LogicalArtifactName::new(relative)?;
            let root_component = name.portability_key().split('/').next().unwrap_or_default();
            if reserved_root_prefixes
                .iter()
                .any(|prefix| root_component.starts_with(prefix))
            {
                return Err(OutputArtifactError::ReservedPath(name.as_str().to_owned()));
            }
            validated.push(name);
        }
        for relative in internal_paths {
            validated.push(LogicalArtifactName::new(relative)?);
        }
        if let Some(name) = validated
            .iter()
            .find(|name| paths_conflict(name.portability_key(), lock_name.portability_key()))
        {
            return Err(OutputArtifactError::PortableCollision {
                first: EXECUTION_LOCK_NAME.to_owned(),
                second: name.as_str().to_owned(),
            });
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

        let root = resolve_output_root(root)?;
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
        budget: &mut EvidenceReadBudget,
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
        let digest = match budget.hash_reader(&mut file, length) {
            Ok(digest) => digest,
            Err(EvidenceReadError::Limit {
                required,
                remaining,
            }) => {
                return Err(OutputArtifactError::ExistingHashLimitExceeded {
                    path: self.final_path.clone(),
                    length: required,
                    limit: remaining,
                });
            }
            Err(EvidenceReadError::Io(source)) => {
                return Err(OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::HashExisting,
                    path: self.final_path.clone(),
                    source,
                });
            }
        };
        let observed_length = file
            .metadata()
            .map_err(|source| OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::HashExisting,
                path: self.final_path.clone(),
                source,
            })?
            .len();
        if observed_length != length {
            return Err(OutputArtifactError::Io {
                kind: ExtractionOutputErrorKind::HashExisting,
                path: self.final_path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "evidence file length changed while hashing",
                ),
            });
        }
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

#[derive(Debug)]
pub(super) enum StagedPublishError {
    NotPublished(OutputArtifactError),
    Uncertain,
}

impl StagedOutput {
    pub(super) const fn length(&self) -> u64 {
        self.length
    }

    pub(super) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    pub(super) fn publish(mut self, replace_existing: bool) -> Result<(), StagedPublishError> {
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
                let uncertain = error.moved_or_unknown_state();
                if uncertain {
                    self.released = true;
                    return Err(StagedPublishError::Uncertain);
                }
                let error = OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::Publish,
                    path: self.final_path.clone(),
                    source: error.into_error(),
                };
                Err(StagedPublishError::NotPublished(error))
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

fn resolve_output_root(path: &Path) -> Result<PathBuf, OutputArtifactError> {
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

    // Resolve aliases only outside the output root. The root leaf and every
    // descendant remain subject to the platform's component-wise no-follow
    // opens, while system aliases such as macOS `/var -> /private/var` do not
    // make an otherwise valid output namespace unusable.
    let Some(mut ancestor) = path.parent().map(Path::to_path_buf) else {
        return Ok(path);
    };
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(canonical) => {
                let suffix = path
                    .strip_prefix(&ancestor)
                    .expect("a parent-derived output ancestor must prefix the root");
                return Ok(canonical.join(suffix));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !ancestor.pop() {
                    return Err(OutputArtifactError::Io {
                        kind: ExtractionOutputErrorKind::ValidateRoot,
                        path,
                        source: error,
                    });
                }
            }
            Err(source) => {
                return Err(OutputArtifactError::Io {
                    kind: ExtractionOutputErrorKind::ValidateRoot,
                    path,
                    source,
                });
            }
        }
    }
}

fn logical_path(value: &str) -> PathBuf {
    value.split('/').collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::{EvidenceReadBudget, EvidenceReadError, OutputArtifactError, OutputLayout};

    #[test]
    fn evidence_read_budget_is_consumed_before_hashing_and_not_refunded_on_failure() {
        let mut budget = EvidenceReadBudget::new(4);
        let error = budget
            .hash_reader(Cursor::new(b"abc"), 4)
            .expect_err("a short reader must fail after reserving its declared length");
        assert!(matches!(
            error,
            EvidenceReadError::Io(error) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ));
        assert_eq!(budget.remaining, 0);

        let mut one_short = EvidenceReadBudget::new(3);
        let error = one_short
            .hash_reader(Cursor::new(b"four"), 4)
            .expect_err("an oversized read must be rejected before consuming bytes");
        assert!(matches!(
            error,
            EvidenceReadError::Limit {
                required: 4,
                remaining: 3,
            }
        ));
        assert_eq!(one_short.remaining, 3);
    }

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
    fn layout_reserves_internal_namespaces_before_creating_directories() {
        let directory = tempfile::tempdir().expect("temporary output root");
        let error = OutputLayout::prepare_with_internal_paths(
            directory.path(),
            ["Protocol-State/attacker.json"],
            ["protocol-state/00000000.json"],
            &["protocol-state"],
        )
        .expect_err("user output must not occupy protocol state");

        assert!(matches!(error, OutputArtifactError::ReservedPath(_)));
        assert!(!directory.path().join("Protocol-State").exists());
        assert!(!directory.path().join("protocol-state").exists());
    }

    #[test]
    fn layout_prepares_declared_internal_paths() {
        let directory = tempfile::tempdir().expect("temporary output root");
        let layout = OutputLayout::prepare_with_internal_paths(
            directory.path(),
            ["objects/item.bin"],
            ["protocol-state/00000000.json"],
            &["protocol-state"],
        )
        .expect("declared internal paths bypass only the user namespace check");

        assert!(layout.path("objects/item.bin").is_ok());
        assert!(layout.path("protocol-state/00000000.json").is_ok());
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
    fn layout_accepts_an_output_root_below_an_external_path_alias() {
        use std::os::unix::fs::symlink;

        let namespace = tempfile::tempdir().expect("output namespace");
        let physical_parent = namespace.path().join("physical");
        let physical_root = physical_parent.join("output");
        std::fs::create_dir_all(&physical_root).expect("physical output root");
        let alias = namespace.path().join("alias");
        symlink(&physical_parent, &alias).expect("external path alias");
        let requested_root = alias.join("output");

        let layout = OutputLayout::prepare(&requested_root, ["objects/item.bin"])
            .expect("an alias outside the output root may name its physical parent");
        let mut staging = layout
            .path("objects/item.bin")
            .expect("prepared output")
            .create_staging()
            .expect("staging file");
        staging.writer().write_all(b"published").unwrap();
        staging.finish().unwrap().publish(false).unwrap();

        assert_eq!(
            std::fs::read(physical_root.join("objects/item.bin")).unwrap(),
            b"published"
        );
        assert!(OutputLayout::has_existing(&requested_root, "objects/item.bin").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn layout_rejects_a_symlinked_output_root() {
        use std::os::unix::fs::symlink;

        let namespace = tempfile::tempdir().expect("output namespace");
        let external = tempfile::tempdir().expect("external directory");
        let root = namespace.path().join("output");
        symlink(external.path(), &root).expect("symlinked output root");

        let error = OutputLayout::prepare(&root, ["objects/item.bin"])
            .expect_err("the output root itself must remain no-follow");

        assert_eq!(error.kind(), super::ExtractionOutputErrorKind::PrepareRoot);
        assert!(!external.path().join(super::EXECUTION_LOCK_NAME).exists());
        assert!(!external.path().join("objects/item.bin").exists());
    }

    #[test]
    fn layout_creates_multiple_missing_output_root_components() {
        let namespace = tempfile::tempdir().expect("output namespace");
        let root = namespace.path().join("missing/parents/output");

        let layout = OutputLayout::prepare(&root, ["objects/item.bin"])
            .expect("the output root retains recursive creation semantics");

        assert!(root.is_dir());
        assert!(layout.path("objects/item.bin").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn layout_rejects_a_symlinked_output_ancestor() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("output root");
        let external = tempfile::tempdir().expect("external directory");
        symlink(external.path(), root.path().join("escape")).expect("directory symlink");

        let error = OutputLayout::prepare(root.path(), ["escape/artifact.bin"])
            .expect_err("symlinked output ancestor must be rejected");

        assert_eq!(
            error.kind(),
            super::ExtractionOutputErrorKind::PrepareDirectory
        );
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
