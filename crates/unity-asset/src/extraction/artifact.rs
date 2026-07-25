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

#[derive(Debug, Error)]
pub(crate) enum OutputArtifactError {
    #[error(transparent)]
    InvalidName(#[from] ArtifactNameError),
    #[error("output names {first:?} and {second:?} cannot coexist on portable filesystems")]
    PortableCollision { first: String, second: String },
    #[error("output path has no parent: {0:?}")]
    MissingParent(PathBuf),
    #[error("output layout does not contain planned path {0:?}")]
    UnplannedPath(String),
    #[error("failed to {operation} {path:?}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to publish {path:?}: {source}")]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("temporary output identity changed before hashing: {0:?}")]
    TemporaryIdentityChanged(PathBuf),
}

#[derive(Debug)]
pub(crate) struct OutputLayout {
    paths: BTreeMap<String, PreparedOutputPath>,
    _execution_lock: File,
}

impl OutputLayout {
    pub(crate) fn prepare<'path>(
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
                operation: "create output root",
                path: root.clone(),
                source,
            })?;
        let lock_path = root.join(EXECUTION_LOCK_NAME);
        let execution_lock =
            acquire_private_lock_in_parent(&lock_path, &root_identity).map_err(|source| {
                OutputArtifactError::Io {
                    operation: "lock output root",
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
                    operation: "create output directory",
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

    pub(crate) fn path(&self, relative: &str) -> Result<&PreparedOutputPath, OutputArtifactError> {
        self.paths
            .get(relative)
            .ok_or_else(|| OutputArtifactError::UnplannedPath(relative.to_owned()))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedOutputPath {
    final_path: PathBuf,
    parent_identity: DirectoryIdentity,
}

impl PreparedOutputPath {
    pub(crate) fn create_staging(&self) -> Result<StagingOutput, OutputArtifactError> {
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
                            operation: "capture temporary output identity",
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
                        operation: "create temporary output",
                        path: temporary_path,
                        source,
                    });
                }
            }
        }
        Err(OutputArtifactError::Io {
            operation: "create unique temporary output",
            path: parent.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "temporary output name attempts were exhausted",
            ),
        })
    }

    pub(crate) fn open_existing(&self) -> Result<Option<File>, OutputArtifactError> {
        match open_readonly_regular_in_parent(&self.final_path, &self.parent_identity) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(OutputArtifactError::Io {
                operation: "open existing output",
                path: self.final_path.clone(),
                source,
            }),
        }
    }

    pub(crate) fn hash_existing(&self) -> Result<Option<(u64, DigestV1)>, OutputArtifactError> {
        let Some(mut file) = self.open_existing()? else {
            return Ok(None);
        };
        let length = file
            .metadata()
            .map_err(|source| OutputArtifactError::Io {
                operation: "inspect existing output",
                path: self.final_path.clone(),
                source,
            })?
            .len();
        let digest =
            DigestV1::hash_reader(&mut file, length).map_err(|source| OutputArtifactError::Io {
                operation: "hash existing output",
                path: self.final_path.clone(),
                source,
            })?;
        Ok(Some((length, digest)))
    }
}

pub(crate) struct StagingOutput {
    file: Option<File>,
    temporary_path: PathBuf,
    final_path: PathBuf,
    identity: FileIdentity,
    parent_identity: DirectoryIdentity,
}

impl StagingOutput {
    pub(crate) fn writer(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("staging writer is only available before finish")
    }

    pub(crate) fn finish(mut self) -> Result<StagedOutput, OutputArtifactError> {
        let mut file = self
            .file
            .take()
            .expect("staging writer is only finished once");
        file.flush().map_err(|source| OutputArtifactError::Io {
            operation: "flush temporary output",
            path: self.temporary_path.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| OutputArtifactError::Io {
            operation: "synchronize temporary output",
            path: self.temporary_path.clone(),
            source,
        })?;
        drop(file);

        let observed = observe_file_identity(&self.temporary_path).map_err(|source| {
            OutputArtifactError::Io {
                operation: "revalidate temporary output",
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
                    operation: "open temporary output for hashing",
                    path: self.temporary_path.clone(),
                    source,
                },
            )?;
        let length = reader
            .metadata()
            .map_err(|source| OutputArtifactError::Io {
                operation: "inspect temporary output",
                path: self.temporary_path.clone(),
                source,
            })?
            .len();
        let digest = DigestV1::hash_reader(&mut reader, length).map_err(|source| {
            OutputArtifactError::Io {
                operation: "hash temporary output",
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
pub(crate) struct StagedOutput {
    temporary_path: PathBuf,
    final_path: PathBuf,
    identity: FileIdentity,
    parent_identity: DirectoryIdentity,
    length: u64,
    digest: DigestV1,
    released: bool,
}

impl StagedOutput {
    pub(crate) const fn length(&self) -> u64 {
        self.length
    }

    pub(crate) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    pub(crate) fn publish(mut self, replace_existing: bool) -> Result<(), OutputArtifactError> {
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
                Err(OutputArtifactError::Publish {
                    path: self.final_path.clone(),
                    source: error.into_error(),
                })
            }
        }
    }

    pub(crate) fn discard(mut self) -> Result<(), OutputArtifactError> {
        remove_owned_file_in_parent(&self.temporary_path, &self.identity, &self.parent_identity)
            .map_err(|source| OutputArtifactError::Io {
                operation: "remove temporary output",
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
                operation: "resolve current directory",
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
            operation: "validate output root",
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
