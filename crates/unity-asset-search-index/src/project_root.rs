use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::AsFd as _;
#[cfg(windows)]
use std::os::windows::io::AsHandle as _;

use anyhow::{Context, Result, anyhow};
use unity_asset_search_local::ProjectIdentityV1;
use unity_asset_search_protocol::ProjectId;

use crate::anchored_fs::{
    AnchoredFsError, OpenPolicy, ReadDirectory, StableDirectoryObjectIdentity,
};
use crate::path_semantics::ProjectPathSpace;

/// Runtime authority for one project root.
///
/// The project identity, lexical coordinate space, and every scanner read are derived from the
/// same retained directory handle. Path-based opens are used only to prove that the current
/// namespace still names this authority; they can never replace it.
#[derive(Clone)]
pub(crate) struct ProjectRootAuthority {
    inner: Arc<ProjectRootAuthorityInner>,
}

struct ProjectRootAuthorityInner {
    canonical_root: PathBuf,
    identity: ProjectIdentityV1,
    path_space: ProjectPathSpace,
    directory: ReadDirectory,
    object_identity: StableDirectoryObjectIdentity,
}

impl ProjectRootAuthority {
    pub(crate) fn open(root: PathBuf) -> Result<Self> {
        Self::open_with_checkpoint(root, || {})
    }

    fn open_with_checkpoint(
        root: PathBuf,
        after_authority_captured: impl FnOnce(),
    ) -> Result<Self> {
        if root.as_os_str().is_empty() {
            return Err(anyhow!("project root must not be empty"));
        }
        let absolute_root = std::path::absolute(&root)
            .with_context(|| format!("resolve project root: {}", root.display()))?;
        let directory = ReadDirectory::open(&absolute_root, OpenPolicy::ProjectSource)
            .with_context(|| {
                format!(
                    "open identity-bound project root: {}",
                    absolute_root.display()
                )
            })?;
        let object_identity = directory
            .object_identity()
            .context("capture project root object identity")?;
        let identity = identity_from_directory(&absolute_root, &directory)
            .context("derive project identity from the retained root authority")?;

        after_authority_captured();

        let canonical_root = std::fs::canonicalize(&absolute_root)
            .with_context(|| format!("canonicalize project root: {}", absolute_root.display()))?;
        ensure_path_names_authority(&canonical_root, object_identity)
            .context("bind canonical project root to the retained authority")?;
        ensure_path_names_authority(&absolute_root, object_identity)
            .context("revalidate requested project root after canonicalization")?;
        directory
            .ensure_object(object_identity)
            .context("revalidate retained project root authority")?;

        let verified_root_alias = (absolute_root != canonical_root).then_some(absolute_root);
        let path_space = ProjectPathSpace::new_with_verified_root_alias(
            canonical_root.clone(),
            verified_root_alias,
            identity.project_id(),
        )
        .context("create the project lexical path space")?;
        let authority = Self {
            inner: Arc::new(ProjectRootAuthorityInner {
                canonical_root,
                identity,
                path_space,
                directory,
                object_identity,
            }),
        };
        authority.revalidate()?;
        Ok(authority)
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.inner.canonical_root
    }

    #[must_use]
    pub(crate) fn identity(&self) -> ProjectIdentityV1 {
        self.inner.identity
    }

    #[must_use]
    pub(crate) fn project_id(&self) -> ProjectId {
        self.inner.identity.project_id()
    }

    #[must_use]
    pub(crate) fn path_space(&self) -> &ProjectPathSpace {
        &self.inner.path_space
    }

    #[must_use]
    pub(crate) fn directory(&self) -> &ReadDirectory {
        &self.inner.directory
    }

    pub(crate) fn revalidate(&self) -> Result<()> {
        self.validate_binding()
            .context("rebind the project root path to its retained authority")
    }

    pub(crate) fn validate_binding(&self) -> Result<(), AnchoredFsError> {
        self.inner
            .directory
            .ensure_object(self.inner.object_identity)?;
        ensure_path_names_authority(self.root(), self.inner.object_identity)
    }

    pub(crate) fn reopen_bound(&self) -> Result<ReadDirectory, AnchoredFsError> {
        self.inner
            .directory
            .ensure_object(self.inner.object_identity)?;
        let rebound = ReadDirectory::open(self.root(), OpenPolicy::ProjectSource)?;
        rebound.ensure_object(self.inner.object_identity)?;
        Ok(rebound)
    }

    /// Proves that a project-relative leaf remains below the retained project namespace.
    ///
    /// The leaf itself may no longer exist because workspace backing is immutable and can outlive
    /// its physical origin. Both binding checks are required: descendant validation must not turn
    /// the retained handle into authority for a replacement now named by the project-root path.
    pub(crate) fn validate_parent_lookup(&self, relative: &Path) -> Result<(), AnchoredFsError> {
        self.validate_binding()?;
        self.inner.directory.validate_parent_lookup(relative)?;
        self.validate_binding()
    }
}

impl std::fmt::Debug for ProjectRootAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectRootAuthority")
            .field("canonical_root", &self.root())
            .field("project_id", &self.project_id())
            .finish_non_exhaustive()
    }
}

fn ensure_path_names_authority(
    path: &Path,
    expected: StableDirectoryObjectIdentity,
) -> Result<(), AnchoredFsError> {
    let rebound = ReadDirectory::open(path, OpenPolicy::ProjectSource)?;
    rebound.ensure_object(expected)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn identity_from_directory(root: &Path, directory: &ReadDirectory) -> Result<ProjectIdentityV1> {
    ProjectIdentityV1::for_open_directory(root, directory.as_fd()).map_err(Into::into)
}

#[cfg(windows)]
fn identity_from_directory(root: &Path, directory: &ReadDirectory) -> Result<ProjectIdentityV1> {
    ProjectIdentityV1::for_open_directory(root, directory.as_handle()).map_err(Into::into)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn identity_from_directory(root: &Path, _directory: &ReadDirectory) -> Result<ProjectIdentityV1> {
    ProjectIdentityV1::for_existing_root(root).map_err(Into::into)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use std::fs;

    use super::*;

    #[cfg(windows)]
    #[test]
    fn construction_rejects_a_case_sensitive_project_root_when_supported() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir(&project).unwrap();
        if let Err(error) =
            crate::anchored_fs::try_enable_case_sensitive_directory_for_test(&project)
        {
            if crate::anchored_fs::case_sensitivity_test_is_unsupported(&error) {
                eprintln!(
                    "skipping per-directory case-sensitivity project-root test for {}: {error}",
                    project.display()
                );
                return;
            }
            panic!(
                "unexpected failure enabling per-directory case sensitivity for {}: {error}",
                project.display()
            );
        }

        let error = ProjectRootAuthority::open(project).unwrap_err();

        assert!(matches!(
            error.downcast_ref::<AnchoredFsError>(),
            Some(AnchoredFsError::UnsupportedCaseSensitiveDirectory)
        ));
    }

    #[test]
    fn construction_rejects_a_path_rebound_after_authority_capture() {
        let temporary = tempfile::tempdir().unwrap();
        let active = temporary.path().join("project");
        let replacement = temporary.path().join("replacement");
        let captured = temporary.path().join("captured");
        fs::create_dir(&active).unwrap();
        fs::create_dir(&replacement).unwrap();

        let error = ProjectRootAuthority::open_with_checkpoint(active.clone(), || {
            fs::rename(&active, &captured).unwrap();
            fs::rename(&replacement, &active).unwrap();
        })
        .unwrap_err();

        assert!(error.to_string().contains("retained authority"));
        fs::rename(&active, &replacement).unwrap();
        fs::rename(&captured, &active).unwrap();
    }

    #[test]
    fn retained_directory_never_reads_from_a_rebound_project_path() {
        let temporary = tempfile::tempdir().unwrap();
        let active = temporary.path().join("project");
        let replacement = temporary.path().join("replacement");
        let captured = temporary.path().join("captured");
        fs::create_dir(&active).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(active.join("OnlyA.asset"), b"A").unwrap();
        fs::write(replacement.join("OnlyB.asset"), b"B").unwrap();
        let authority = ProjectRootAuthority::open(active.clone()).unwrap();

        fs::rename(&active, &captured).unwrap();
        fs::rename(&replacement, &active).unwrap();

        assert!(authority.directory().open_regular("OnlyA.asset").is_ok());
        assert!(authority.directory().open_regular("OnlyB.asset").is_err());
        assert!(authority.reopen_bound().is_err());

        fs::rename(&active, &replacement).unwrap();
        fs::rename(&captured, &active).unwrap();
        authority.revalidate().unwrap();
    }
}
