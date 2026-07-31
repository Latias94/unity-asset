use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use unity_asset_search_protocol::ProjectId;

use crate::project::ProjectIdentityV1;
use crate::publication::{self, PublicationSlots};
use crate::security_context::CurrentSecurityContextSnapshot;
use crate::{SecurityContextError, SecurityContextIdV1};

const PRODUCT_DIRECTORY: &str = "unity-asset-search-v5";
const ENDPOINT_NAMESPACE_DOMAIN: &[u8] = b"unity-asset:endpoint-namespace:v1\0";
const ENDPOINT_NAMESPACE_KEY_BYTES: usize = 16;
const ENDPOINT_BINDING_FILE: &str = "binding.v1";
const ENDPOINT_BINDING_LOCK_FILE: &str = ".binding-v1.lock";
const ENDPOINT_BINDING_STAGING_FILE: &str = ".binding-v1.staging";
const ENDPOINT_BINDING_VERSION: u16 = 1;
const MAX_ENDPOINT_BINDING_BYTES: u64 = 512;
const PRIVATE_INDEX_ROOT_PREFIX: &str = "index-v1-";
const ENDPOINT_BINDING_PUBLICATION: PublicationSlots =
    PublicationSlots::new(ENDPOINT_BINDING_FILE, ENDPOINT_BINDING_STAGING_FILE, None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateRootKind {
    Runtime,
    Cache,
}

impl fmt::Display for PrivateRootKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Runtime => "runtime",
            Self::Cache => "cache",
        })
    }
}

pub struct PrivateRootV1 {
    kind: PrivateRootKind,
    path: PathBuf,
    security_context_id: SecurityContextIdV1,
    authority: Arc<platform::PrivateDirectory>,
}

impl PrivateRootV1 {
    #[must_use]
    pub const fn kind(&self) -> PrivateRootKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        self.revalidate_for_context(&current)
    }

    pub fn endpoint_namespace(
        &self,
        project_id: ProjectId,
    ) -> Result<EndpointNamespaceV1, PrivateRootsError> {
        if self.kind != PrivateRootKind::Runtime {
            return Err(PrivateRootsError::WrongRootKind {
                expected: PrivateRootKind::Runtime,
                actual: self.kind,
            });
        }
        require_nonzero_project_id(project_id)?;
        let current = CurrentSecurityContextSnapshot::current()?;
        self.revalidate_for_context(&current)?;
        let component = endpoint_namespace_component(project_id, current.id());
        let path = self.path.join(&component);
        let authority = self
            .authority
            .create_private_child(OsStr::new(&component), &current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: self.kind,
                operation: "create project runtime namespace",
                path: path.clone(),
                source,
            })?;
        let namespace = EndpointNamespaceV1 {
            path,
            component,
            project_id,
            security_context_id: self.security_context_id,
            authority: Arc::new(authority),
        };
        namespace.bind(&current)?;
        Ok(namespace)
    }

    pub fn index_root(
        &self,
        project_identity: ProjectIdentityV1,
    ) -> Result<PrivateIndexRootV1, PrivateRootsError> {
        if self.kind != PrivateRootKind::Cache {
            return Err(PrivateRootsError::WrongRootKind {
                expected: PrivateRootKind::Cache,
                actual: self.kind,
            });
        }
        let project_id = project_identity.project_id();
        require_nonzero_project_id(project_id)?;
        let current = CurrentSecurityContextSnapshot::current()?;
        self.revalidate_for_context(&current)?;
        let component = private_index_root_component(project_id);
        let path = self.path.join(&component);
        let authority = self
            .authority
            .create_private_child(OsStr::new(&component), &current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: self.kind,
                operation: "create project private index root",
                path: path.clone(),
                source,
            })?;
        let root = PrivateIndexRootV1 {
            path,
            project_id,
            security_context_id: self.security_context_id,
            parent_path: self.path.clone(),
            parent_authority: Arc::clone(&self.authority),
            authority: Arc::new(authority),
        };
        root.revalidate_for_context(&current)?;
        Ok(root)
    }

    fn revalidate_for_context(
        &self,
        current: &CurrentSecurityContextSnapshot,
    ) -> Result<(), PrivateRootsError> {
        if current.id() != self.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        self.authority
            .revalidate(&self.path, current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: self.kind,
                operation: "revalidate",
                path: self.path.clone(),
                source,
            })
    }
}

#[derive(Clone)]
pub struct EndpointNamespaceV1 {
    path: PathBuf,
    component: String,
    project_id: ProjectId,
    security_context_id: SecurityContextIdV1,
    authority: Arc<platform::PrivateDirectory>,
}

impl EndpointNamespaceV1 {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    #[must_use]
    pub fn component(&self) -> &str {
        &self.component
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        if current.id() != self.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        self.authority
            .revalidate(&self.path, &current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: PrivateRootKind::Runtime,
                operation: "revalidate project runtime namespace",
                path: self.path.clone(),
                source,
            })
    }

    fn bind(&self, current: &CurrentSecurityContextSnapshot) -> Result<(), PrivateRootsError> {
        let expected = EndpointNamespaceBindingV1 {
            binding_version: ENDPOINT_BINDING_VERSION,
            project_id: self.project_id,
            security_context_id: self.security_context_id,
        };
        let encoded = serde_json::to_vec(&expected).map_err(PrivateRootsError::BindingJson)?;
        let binding_path = self.path.join(ENDPOINT_BINDING_FILE);
        let binding_lock = self.open_or_create_binding_lock(current)?;
        binding_lock.lock_exclusive().map_err(|source| {
            binding_filesystem_error(
                "acquire endpoint namespace binding authority",
                binding_path.clone(),
                source,
            )
        })?;

        publication::recover_abandoned(self, ENDPOINT_BINDING_PUBLICATION).map_err(|source| {
            binding_filesystem_error(
                "recover abandoned endpoint namespace binding publication",
                binding_path.clone(),
                source,
            )
        })?;

        let persisted = match self.read_binding(current, &binding_path)? {
            Some(persisted) => persisted,
            None => {
                let prepared = publication::prepare(self, ENDPOINT_BINDING_PUBLICATION, &encoded)
                    .map_err(|source| {
                    binding_filesystem_error(
                        "create endpoint namespace binding staging file",
                        binding_path.clone(),
                        source,
                    )
                })?;
                let commit = prepared.commit(self).map_err(|source| {
                    binding_filesystem_error(
                        "commit endpoint namespace binding",
                        binding_path.clone(),
                        source,
                    )
                })?;
                if commit.durability_unconfirmed() {
                    return Err(binding_filesystem_error(
                        "sync endpoint namespace binding publication",
                        binding_path.clone(),
                        io::Error::other("binding publication durability was not confirmed"),
                    ));
                }
                self.read_binding(current, &binding_path)?.ok_or_else(|| {
                    binding_filesystem_error(
                        "read committed endpoint namespace binding",
                        binding_path.clone(),
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "binding disappeared after its commit point",
                        ),
                    )
                })?
            }
        };
        self.validate_binding(&expected, &persisted, binding_path)
    }

    fn open_or_create_binding_lock(
        &self,
        current: &CurrentSecurityContextSnapshot,
    ) -> Result<File, PrivateRootsError> {
        let path = self.path.join(ENDPOINT_BINDING_LOCK_FILE);
        match self.authority.create_private_file(
            &self.path,
            OsStr::new(ENDPOINT_BINDING_LOCK_FILE),
            current,
        ) {
            Ok(file) => Ok(file),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => self
                .authority
                .open_private_file(
                    &self.path,
                    OsStr::new(ENDPOINT_BINDING_LOCK_FILE),
                    current,
                    true,
                )
                .map_err(|source| {
                    binding_filesystem_error(
                        "open endpoint namespace binding authority",
                        path,
                        source,
                    )
                }),
            Err(source) => Err(binding_filesystem_error(
                "create endpoint namespace binding authority",
                path,
                source,
            )),
        }
    }

    fn read_binding(
        &self,
        current: &CurrentSecurityContextSnapshot,
        binding_path: &Path,
    ) -> Result<Option<Vec<u8>>, PrivateRootsError> {
        let mut file = match self.authority.open_private_file(
            &self.path,
            OsStr::new(ENDPOINT_BINDING_FILE),
            current,
            false,
        ) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(binding_filesystem_error(
                    "open endpoint namespace binding",
                    binding_path.to_path_buf(),
                    source,
                ));
            }
        };
        let mut persisted = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_ENDPOINT_BINDING_BYTES + 1)
            .read_to_end(&mut persisted)
            .map_err(|source| {
                binding_filesystem_error(
                    "read endpoint namespace binding",
                    binding_path.to_path_buf(),
                    source,
                )
            })?;
        if u64::try_from(persisted.len()).unwrap_or(u64::MAX) > MAX_ENDPOINT_BINDING_BYTES {
            return Err(PrivateRootsError::UnsafeBinding {
                path: binding_path.to_path_buf(),
                reason: "binding exceeds its byte limit",
            });
        }
        Ok(Some(persisted))
    }

    fn validate_binding(
        &self,
        expected: &EndpointNamespaceBindingV1,
        persisted: &[u8],
        binding_path: PathBuf,
    ) -> Result<(), PrivateRootsError> {
        let actual: EndpointNamespaceBindingV1 =
            serde_json::from_slice(persisted).map_err(|source| {
                PrivateRootsError::InvalidBinding {
                    path: binding_path.clone(),
                    source,
                }
            })?;
        if actual != *expected || serde_json::to_vec(&actual).ok().as_deref() != Some(persisted) {
            return Err(PrivateRootsError::NamespaceKeyCollision {
                component: self.component.clone(),
            });
        }
        Ok(())
    }

    pub(crate) fn create_file(&self, name: &OsStr) -> io::Result<File> {
        let current = CurrentSecurityContextSnapshot::current().map_err(io::Error::other)?;
        if current.id() != self.security_context_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effective security context changed",
            ));
        }
        self.authority
            .create_private_file(&self.path, name, &current)
    }

    pub(crate) fn open_file(&self, name: &OsStr, writable: bool) -> io::Result<File> {
        let current = CurrentSecurityContextSnapshot::current().map_err(io::Error::other)?;
        if current.id() != self.security_context_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effective security context changed",
            ));
        }
        self.authority
            .open_private_file(&self.path, name, &current, writable)
    }

    pub(crate) fn replace_file(&self, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        self.rename_file(source, destination, true)
    }

    pub(crate) fn rename_file(
        &self,
        source: &OsStr,
        destination: &OsStr,
        replace: bool,
    ) -> io::Result<()> {
        let current = CurrentSecurityContextSnapshot::current().map_err(io::Error::other)?;
        if current.id() != self.security_context_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effective security context changed",
            ));
        }
        self.authority
            .rename_private_file(&self.path, source, destination, replace, &current)
    }

    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        let current = CurrentSecurityContextSnapshot::current().map_err(io::Error::other)?;
        if current.id() != self.security_context_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "effective security context changed",
            ));
        }
        self.authority
            .remove_private_file(&self.path, name, &current)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.revalidate().map_err(io::Error::other)?;
        self.authority.sync()
    }
}

fn binding_filesystem_error(
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
) -> PrivateRootsError {
    PrivateRootsError::Filesystem {
        kind: PrivateRootKind::Runtime,
        operation,
        path,
        source,
    }
}

impl fmt::Debug for EndpointNamespaceV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndpointNamespaceV1")
            .field("path", &self.path)
            .field("component", &self.component)
            .field("project_id", &self.project_id)
            .field("security_context_id", &self.security_context_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EndpointNamespaceBindingV1 {
    binding_version: u16,
    project_id: ProjectId,
    security_context_id: SecurityContextIdV1,
}

fn endpoint_namespace_component(
    project_id: ProjectId,
    security_context_id: SecurityContextIdV1,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(ENDPOINT_NAMESPACE_DOMAIN);
    hasher.update(project_id.as_bytes());
    hasher.update(security_context_id.as_bytes());
    let digest = hasher.finalize();
    format!("p-{}", hex::encode(&digest[..ENDPOINT_NAMESPACE_KEY_BYTES]))
}

fn private_index_root_component(project_id: ProjectId) -> String {
    format!(
        "{PRIVATE_INDEX_ROOT_PREFIX}{}",
        hex::encode(project_id.as_bytes())
    )
}

fn require_nonzero_project_id(project_id: ProjectId) -> Result<(), PrivateRootsError> {
    if project_id.as_bytes().iter().all(|byte| *byte == 0) {
        Err(PrivateRootsError::ZeroProjectId)
    } else {
        Ok(())
    }
}

fn current_private_security_context() -> Result<CurrentSecurityContextSnapshot, PrivateRootsError> {
    CurrentSecurityContextSnapshot::current().map_err(|error| match error {
        SecurityContextError::UnsupportedPlatform => PrivateRootsError::UnsupportedPlatform,
        error => PrivateRootsError::SecurityContext(error),
    })
}

fn absolute_private_path(path: &Path) -> Result<PathBuf, PrivateRootsError> {
    if path.as_os_str().is_empty() {
        return Err(PrivateRootsError::InvalidIndexRootOverride {
            path: path.to_path_buf(),
        });
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: PrivateRootKind::Cache,
            operation: "resolve explicit private index root",
            path: path.to_path_buf(),
            source,
        })
}

impl fmt::Debug for PrivateRootV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRootV1")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("security_context_id", &self.security_context_id)
            .finish_non_exhaustive()
    }
}

/// A verified, per-user root for one project's persisted search index.
///
/// The default constructor derives this root from a stable project identity under the platform
/// private cache root. An explicit override is accepted only after the platform proves that the
/// final directory is an ordinary private directory without following links or reparse points.
/// On Windows, persistent storage is private from other user SIDs; processes running as the same
/// persistent user SID are inside the storage trust boundary even when their logon contexts differ.
#[derive(Clone)]
pub struct PrivateIndexRootV1 {
    path: PathBuf,
    project_id: ProjectId,
    security_context_id: SecurityContextIdV1,
    parent_path: PathBuf,
    parent_authority: Arc<platform::PrivateDirectory>,
    authority: Arc<platform::PrivateDirectory>,
}

impl PrivateIndexRootV1 {
    /// Creates or reopens the default private cache root for a stable project identity.
    pub fn for_project(project_identity: ProjectIdentityV1) -> Result<Self, PrivateRootsError> {
        PrivateRootsV1::discover_for_current_context()?.index_root(project_identity)
    }

    fn open_override_base(path: &Path) -> Result<PrivateIndexOverrideBase, PrivateRootsError> {
        let path = absolute_private_path(path)?;
        let security_context = current_private_security_context()?;
        let security_context_id = security_context.id();
        let authority =
            platform::open_or_create_private_path(&path, &security_context).map_err(|source| {
                PrivateRootsError::Filesystem {
                    kind: PrivateRootKind::Cache,
                    operation: "open or create explicit private index root",
                    path: path.clone(),
                    source,
                }
            })?;
        let path = authority.canonical_path(&path);
        let base = PrivateIndexOverrideBase {
            path,
            security_context_id,
            authority: Arc::new(authority),
        };
        base.revalidate_for_context(&security_context)?;
        Ok(base)
    }

    /// Creates or reopens a project-specific root under an explicit private base directory.
    ///
    /// The base and derived child are created with the platform private-directory policy when
    /// absent. Existing directories must already satisfy that policy; permissive directories,
    /// links, and reparse points are rejected. Deriving a child from the project identity prevents
    /// one override from exposing another project's generations.
    pub fn open_or_create_for_project_override(
        project_identity: ProjectIdentityV1,
        path: impl AsRef<Path>,
    ) -> Result<Self, PrivateRootsError> {
        let project_id = project_identity.project_id();
        require_nonzero_project_id(project_id)?;
        let base = Self::open_override_base(path.as_ref())?;
        let current = CurrentSecurityContextSnapshot::current()?;
        if current.id() != base.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        let component = private_index_root_component(project_id);
        let requested_path = base.path.join(&component);
        let authority = base
            .authority
            .create_private_child(OsStr::new(&component), &current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: PrivateRootKind::Cache,
                operation: "create project child under explicit private index root",
                path: requested_path.clone(),
                source,
            })?;
        let path = authority.canonical_path(&requested_path);
        let root = Self {
            path,
            project_id,
            security_context_id: base.security_context_id,
            parent_path: base.path,
            parent_authority: base.authority,
            authority: Arc::new(authority),
        };
        root.revalidate_for_context(&current)?;
        Ok(root)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the private namespace that contains all project-specific index roots.
    ///
    /// This path describes storage topology only. It may be an ancestor of a project and therefore
    /// must not be used directly as a project source exclusion.
    #[must_use]
    pub fn namespace_path(&self) -> &Path {
        &self.parent_path
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        self.revalidate_for_context(&current)
    }

    fn revalidate_for_context(
        &self,
        current: &CurrentSecurityContextSnapshot,
    ) -> Result<(), PrivateRootsError> {
        if current.id() != self.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        self.parent_authority
            .revalidate(&self.parent_path, current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: PrivateRootKind::Cache,
                operation: "revalidate private index parent root",
                path: self.parent_path.clone(),
                source,
            })?;
        self.authority
            .revalidate(&self.path, current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: PrivateRootKind::Cache,
                operation: "revalidate project private index root",
                path: self.path.clone(),
                source,
            })
    }
}

impl PartialEq for PrivateIndexRootV1 {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.project_id == other.project_id
            && self.security_context_id == other.security_context_id
            && self.parent_path == other.parent_path
    }
}

impl Eq for PrivateIndexRootV1 {}

impl fmt::Debug for PrivateIndexRootV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateIndexRootV1")
            .field("path", &self.path)
            .field("parent_path", &self.parent_path)
            .field("project_id", &self.project_id)
            .field("security_context_id", &self.security_context_id)
            .finish_non_exhaustive()
    }
}

struct PrivateIndexOverrideBase {
    path: PathBuf,
    security_context_id: SecurityContextIdV1,
    authority: Arc<platform::PrivateDirectory>,
}

impl PrivateIndexOverrideBase {
    fn revalidate_for_context(
        &self,
        current: &CurrentSecurityContextSnapshot,
    ) -> Result<(), PrivateRootsError> {
        if current.id() != self.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        self.authority
            .revalidate(&self.path, current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: PrivateRootKind::Cache,
                operation: "revalidate explicit private index base",
                path: self.path.clone(),
                source,
            })
    }
}

pub struct PrivateRootsV1 {
    security_context_id: SecurityContextIdV1,
    runtime: PrivateRootV1,
    cache: PrivateRootV1,
}

impl PrivateRootsV1 {
    pub fn discover_for_current_context() -> Result<Self, PrivateRootsError> {
        let security_context = current_private_security_context()?;
        let security_context_id = security_context.id();
        let discovered = platform::discover(&security_context)?;
        Ok(Self {
            security_context_id,
            runtime: PrivateRootV1 {
                kind: PrivateRootKind::Runtime,
                path: discovered.runtime_path,
                security_context_id,
                authority: Arc::new(discovered.runtime),
            },
            cache: PrivateRootV1 {
                kind: PrivateRootKind::Cache,
                path: discovered.cache_path,
                security_context_id,
                authority: Arc::new(discovered.cache),
            },
        })
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    #[must_use]
    pub const fn runtime(&self) -> &PrivateRootV1 {
        &self.runtime
    }

    #[must_use]
    pub const fn cache(&self) -> &PrivateRootV1 {
        &self.cache
    }

    pub fn index_root(
        &self,
        project_identity: ProjectIdentityV1,
    ) -> Result<PrivateIndexRootV1, PrivateRootsError> {
        self.cache.index_root(project_identity)
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        self.runtime.revalidate_for_context(&current)?;
        self.cache.revalidate_for_context(&current)
    }
}

impl fmt::Debug for PrivateRootsV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRootsV1")
            .field("security_context_id", &self.security_context_id)
            .field("runtime", &self.runtime)
            .field("cache", &self.cache)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum PrivateRootsError {
    #[error(transparent)]
    SecurityContext(#[from] SecurityContextError),
    #[error("private local roots are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("environment variable {variable} {reason}")]
    InvalidEnvironment {
        variable: &'static str,
        reason: &'static str,
    },
    #[error("private index root override is invalid: {path}")]
    InvalidIndexRootOverride { path: PathBuf },
    #[error("the effective user has no usable home directory")]
    MissingHomeDirectory,
    #[error("project ID must not be zero")]
    ZeroProjectId,
    #[error("the effective security context changed while private roots were in use")]
    SecurityContextChanged,
    #[error("private {actual} root cannot provide a {expected} namespace")]
    WrongRootKind {
        expected: PrivateRootKind,
        actual: PrivateRootKind,
    },
    #[error("endpoint namespace key {component} is already bound to another identity")]
    NamespaceKeyCollision { component: String },
    #[error("endpoint namespace binding at {path} is unsafe: {reason}")]
    UnsafeBinding { path: PathBuf, reason: &'static str },
    #[error("endpoint namespace binding at {path} is invalid: {source}")]
    InvalidBinding {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not encode endpoint namespace binding: {0}")]
    BindingJson(serde_json::Error),
    #[error("could not {operation} the private {kind} root at {path}: {source}")]
    Filesystem {
        kind: PrivateRootKind,
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct DiscoveredRoots {
    runtime_path: PathBuf,
    runtime: platform::PrivateDirectory,
    cache_path: PathBuf,
    cache: platform::PrivateDirectory,
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod binding_tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn binding_authority_recovers_abandoned_staging_without_rewriting_current_binding() {
        let roots = PrivateRootsV1::discover_for_current_context().unwrap();
        let mut project_bytes = rand::random::<[u8; 32]>();
        project_bytes[0] |= 1;
        let namespace = roots
            .runtime()
            .endpoint_namespace(ProjectId::from_bytes(project_bytes))
            .unwrap();
        let cleanup_path = namespace.path().to_path_buf();

        let mut staging = namespace
            .create_file(OsStr::new(ENDPOINT_BINDING_STAGING_FILE))
            .unwrap();
        staging.write_all(b"abandoned staging bytes").unwrap();
        staging.sync_all().unwrap();
        drop(staging);

        let current = CurrentSecurityContextSnapshot::current().unwrap();
        namespace.bind(&current).unwrap();

        assert!(!cleanup_path.join(ENDPOINT_BINDING_STAGING_FILE).exists());
        let binding = std::fs::read(cleanup_path.join(ENDPOINT_BINDING_FILE)).unwrap();
        let parsed: EndpointNamespaceBindingV1 = serde_json::from_slice(&binding).unwrap();
        assert_eq!(parsed.project_id, namespace.project_id());
        assert_eq!(parsed.security_context_id, namespace.security_context_id());

        drop(namespace);
        drop(roots);
        for name in [ENDPOINT_BINDING_FILE, ENDPOINT_BINDING_LOCK_FILE] {
            std::fs::remove_file(cleanup_path.join(name)).unwrap();
        }
        std::fs::remove_dir(cleanup_path).unwrap();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_error(
    kind: PrivateRootKind,
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> PrivateRootsError {
    PrivateRootsError::Filesystem {
        kind,
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    #[cfg(target_os = "linux")]
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    #[cfg(target_os = "macos")]
    use std::os::unix::ffi::OsStringExt as _;
    use std::path::{Component, Path, PathBuf};

    use rustix::fs::{
        AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, fchmod, fstat, fsync, mkdirat, openat,
        renameat, renameat_with, statat, unlinkat,
    };
    use rustix::io::Errno;

    use super::{
        DiscoveredRoots, PRODUCT_DIRECTORY, PrivateRootKind, PrivateRootsError, filesystem_error,
    };
    use crate::security_context::CurrentSecurityContextSnapshot;

    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    fn private_file_mode() -> Mode {
        Mode::RUSR | Mode::WUSR
    }
    const PRIVATE_MODE: u32 = 0o700;

    #[derive(Clone, Copy)]
    enum AncestorPolicy {
        Standard,
        Strict,
    }

    pub(super) struct PrivateDirectory {
        descriptor: OwnedFd,
        identity: DirectoryIdentity,
        ancestor_policy: AncestorPolicy,
    }

    impl PrivateDirectory {
        pub(super) fn canonical_path(&self, requested: &Path) -> PathBuf {
            requested.to_path_buf()
        }

        pub(super) fn revalidate(
            &self,
            path: &Path,
            security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            let expected_uid = security_context.effective_uid();
            if validate_private_directory(&self.descriptor, expected_uid)? != self.identity {
                return Err(io::Error::other(
                    "private directory identity changed during revalidation",
                ));
            }
            let reopened = match self.ancestor_policy {
                AncestorPolicy::Standard => open_path(path)?,
                AncestorPolicy::Strict => open_strict_path(path, expected_uid)?,
            };
            if validate_private_directory(&reopened, expected_uid)? != self.identity {
                return Err(io::Error::other(
                    "private directory identity changed during revalidation",
                ));
            }
            Ok(())
        }

        pub(super) fn create_private_child(
            &self,
            name: &OsStr,
            security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<Self> {
            create_or_open_private_child_with_policy(
                &self.descriptor,
                name,
                security_context.effective_uid(),
                self.ancestor_policy,
            )
        }

        pub(super) fn create_private_file(
            &self,
            _directory_path: &Path,
            name: &OsStr,
            security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<File> {
            validate_leaf(name)?;
            let descriptor = openat(
                &self.descriptor,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                private_file_mode(),
            )
            .map_err(io::Error::from)?;
            fchmod(&descriptor, private_file_mode()).map_err(io::Error::from)?;
            validate_private_file(&descriptor, security_context.effective_uid())?;
            Ok(File::from(descriptor))
        }

        pub(super) fn open_private_file(
            &self,
            _directory_path: &Path,
            name: &OsStr,
            security_context: &CurrentSecurityContextSnapshot,
            writable: bool,
        ) -> io::Result<File> {
            validate_leaf(name)?;
            let access = if writable {
                OFlags::RDWR
            } else {
                OFlags::RDONLY
            };
            let descriptor = openat(
                &self.descriptor,
                name,
                access | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            validate_private_file(&descriptor, security_context.effective_uid())?;
            Ok(File::from(descriptor))
        }

        pub(super) fn rename_private_file(
            &self,
            _directory_path: &Path,
            source: &OsStr,
            destination: &OsStr,
            replace: bool,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            validate_leaf(source)?;
            validate_leaf(destination)?;
            if replace {
                renameat(&self.descriptor, source, &self.descriptor, destination)
            } else {
                renameat_with(
                    &self.descriptor,
                    source,
                    &self.descriptor,
                    destination,
                    RenameFlags::NOREPLACE,
                )
            }
            .map_err(io::Error::from)
        }

        pub(super) fn remove_private_file(
            &self,
            _directory_path: &Path,
            name: &OsStr,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            validate_leaf(name)?;
            unlinkat(&self.descriptor, name, AtFlags::empty()).map_err(io::Error::from)
        }

        pub(super) fn sync(&self) -> io::Result<()> {
            fsync(&self.descriptor).map_err(io::Error::from)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct DirectoryIdentity {
        device: u64,
        inode: u64,
    }

    pub(super) fn open_or_create_private_path(
        path: &Path,
        security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<PrivateDirectory> {
        let parent_path = path.parent().ok_or_else(invalid_path)?;
        let name = path.file_name().ok_or_else(invalid_path)?;
        let expected_uid = security_context.effective_uid();
        let parent = open_strict_path(parent_path, expected_uid)?;
        let directory = create_or_open_private_child_with_policy(
            &parent,
            name,
            expected_uid,
            AncestorPolicy::Strict,
        )?;
        directory.revalidate(path, security_context)?;
        Ok(directory)
    }

    pub(super) fn discover(
        security_context: &CurrentSecurityContextSnapshot,
    ) -> Result<DiscoveredRoots, PrivateRootsError> {
        let expected_uid = security_context.effective_uid();
        let (runtime_path, runtime) = discover_runtime(expected_uid)?;
        let (cache_path, cache) = discover_cache(expected_uid)?;
        Ok(DiscoveredRoots {
            runtime_path,
            runtime,
            cache_path,
            cache,
        })
    }

    #[cfg(target_os = "linux")]
    fn discover_runtime(
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        discover_linux_runtime(env::var_os("XDG_RUNTIME_DIR"), expected_uid)
    }

    #[cfg(target_os = "linux")]
    fn discover_linux_runtime(
        xdg_runtime_dir: Option<OsString>,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Runtime;
        if let Some(value) = xdg_runtime_dir {
            let base_path = environment_path("XDG_RUNTIME_DIR", value)?;
            let base = open_path(&base_path).map_err(|source| {
                filesystem_error(kind, "open XDG runtime base", &base_path, source)
            })?;
            validate_private_directory(&base, expected_uid).map_err(|source| {
                filesystem_error(kind, "validate XDG runtime base", &base_path, source)
            })?;
            return create_product_root(kind, &base_path, &base, expected_uid);
        }

        let temporary_path = PathBuf::from("/tmp");
        let temporary = open_path(&temporary_path).map_err(|source| {
            filesystem_error(kind, "open /tmp fallback", &temporary_path, source)
        })?;
        validate_sticky_temporary_base(&temporary).map_err(|source| {
            filesystem_error(kind, "validate /tmp fallback", &temporary_path, source)
        })?;
        let name = OsString::from(format!("{PRODUCT_DIRECTORY}-{expected_uid}"));
        let path = temporary_path.join(&name);
        let root = create_or_open_private_child(&temporary, &name, expected_uid)
            .map_err(|source| filesystem_error(kind, "create /tmp runtime root", &path, source))?;
        Ok((path, root))
    }

    #[cfg(target_os = "macos")]
    fn discover_runtime(
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Runtime;
        let base_path = darwin_user_temporary_directory().map_err(|source| {
            filesystem_error(
                kind,
                "resolve user temporary base",
                Path::new("<darwin-user-temp>"),
                source,
            )
        })?;
        let base = open_path(&base_path).map_err(|source| {
            filesystem_error(kind, "open user temporary base", &base_path, source)
        })?;
        validate_private_directory(&base, expected_uid).map_err(|source| {
            filesystem_error(kind, "validate user temporary base", &base_path, source)
        })?;
        create_product_root(kind, &base_path, &base, expected_uid)
    }

    fn discover_cache(expected_uid: u32) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        #[cfg(target_os = "linux")]
        return discover_linux_cache(
            env::var_os("XDG_CACHE_HOME"),
            env::var_os("HOME"),
            expected_uid,
        );

        #[cfg(target_os = "macos")]
        {
            let kind = PrivateRootKind::Cache;
            let base_path = effective_home_directory(expected_uid)?
                .join("Library")
                .join("Caches");
            let base = open_or_create_owner_controlled_base(&base_path, expected_uid)
                .map_err(|source| filesystem_error(kind, "open cache base", &base_path, source))?;
            create_product_root(kind, &base_path, &base, expected_uid)
        }
    }

    #[cfg(target_os = "linux")]
    fn discover_linux_cache(
        xdg_cache_home: Option<OsString>,
        home: Option<OsString>,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Cache;
        let base_path = match xdg_cache_home {
            Some(value) => environment_path("XDG_CACHE_HOME", value)?,
            None => {
                let home = home.ok_or(PrivateRootsError::MissingHomeDirectory)?;
                environment_path("HOME", home)?.join(".cache")
            }
        };

        let base = open_or_create_owner_controlled_base(&base_path, expected_uid)
            .map_err(|source| filesystem_error(kind, "open cache base", &base_path, source))?;
        create_product_root(kind, &base_path, &base, expected_uid)
    }

    fn create_product_root(
        kind: PrivateRootKind,
        base_path: &Path,
        base: &OwnedFd,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let path = base_path.join(PRODUCT_DIRECTORY);
        let root = create_or_open_private_child(base, OsStr::new(PRODUCT_DIRECTORY), expected_uid)
            .map_err(|source| filesystem_error(kind, "create product root", &path, source))?;
        Ok((path, root))
    }

    fn environment_path(
        variable: &'static str,
        value: OsString,
    ) -> Result<PathBuf, PrivateRootsError> {
        if value.is_empty() {
            return Err(PrivateRootsError::InvalidEnvironment {
                variable,
                reason: "must not be empty",
            });
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(PrivateRootsError::InvalidEnvironment {
                variable,
                reason: "must be an absolute path",
            });
        }
        Ok(path)
    }

    fn open_or_create_owner_controlled_base(path: &Path, expected_uid: u32) -> io::Result<OwnedFd> {
        match open_path(path) {
            Ok(directory) => {
                validate_owner_controlled_directory(&directory, expected_uid)?;
                Ok(directory)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent_path = path.parent().ok_or_else(invalid_path)?;
                let name = path.file_name().ok_or_else(invalid_path)?;
                let parent = open_path(parent_path)?;
                validate_owner_controlled_directory(&parent, expected_uid)?;
                let created = create_or_open_private_child(&parent, name, expected_uid)?;
                Ok(created.descriptor)
            }
            Err(error) => Err(error),
        }
    }

    fn create_or_open_private_child(
        parent: &OwnedFd,
        name: &OsStr,
        expected_uid: u32,
    ) -> io::Result<PrivateDirectory> {
        create_or_open_private_child_with_policy(
            parent,
            name,
            expected_uid,
            AncestorPolicy::Standard,
        )
    }

    fn create_or_open_private_child_with_policy(
        parent: &OwnedFd,
        name: &OsStr,
        expected_uid: u32,
        ancestor_policy: AncestorPolicy,
    ) -> io::Result<PrivateDirectory> {
        validate_leaf(name)?;
        let created = match mkdirat(parent, name, Mode::RWXU) {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(error) => return Err(error.into()),
        };
        let descriptor = open_named_directory(parent, name)?;
        if created {
            fchmod(&descriptor, Mode::RWXU).map_err(io::Error::from)?;
        }
        let identity = validate_private_directory(&descriptor, expected_uid)?;
        Ok(PrivateDirectory {
            descriptor,
            identity,
            ancestor_policy,
        })
    }

    fn validate_private_file(descriptor: &OwnedFd, expected_uid: u32) -> io::Result<()> {
        let metadata = fstat(descriptor).map_err(io::Error::from)?;
        if !FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private namespace entry is not a regular file",
            ));
        }
        if metadata.st_uid != expected_uid || metadata.st_mode & 0o777 != 0o600 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private namespace file must be owner-only",
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::require_empty(descriptor)?;
        if metadata.st_dev == 0 || metadata.st_ino == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private namespace file has no stable identity",
            ));
        }
        Ok(())
    }

    fn open_path(path: &Path) -> io::Result<OwnedFd> {
        if !path.is_absolute() {
            return Err(invalid_path());
        }
        let mut descriptor = openat(CWD, Path::new("/"), directory_flags(), Mode::empty())
            .map_err(io::Error::from)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => descriptor = open_named_directory(&descriptor, name)?,
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(invalid_path());
                }
            }
        }
        Ok(descriptor)
    }

    fn open_strict_path(path: &Path, expected_uid: u32) -> io::Result<OwnedFd> {
        if !path.is_absolute() {
            return Err(invalid_path());
        }
        let mut descriptor = openat(CWD, Path::new("/"), directory_flags(), Mode::empty())
            .map_err(io::Error::from)?;
        validate_trusted_ancestor(&descriptor, expected_uid)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    descriptor = open_named_directory(&descriptor, name)?;
                    validate_trusted_ancestor(&descriptor, expected_uid)?;
                }
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(invalid_path());
                }
            }
        }
        Ok(descriptor)
    }

    fn open_named_directory(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        validate_leaf(name)?;
        let descriptor =
            openat(parent, name, directory_flags(), Mode::empty()).map_err(io::Error::from)?;
        let opened = fstat(&descriptor).map_err(io::Error::from)?;
        let opened_identity = directory_identity(&opened)?;
        let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        if !FileType::from_raw_mode(named.st_mode).is_dir()
            || directory_identity(&named)? != opened_identity
        {
            return Err(io::Error::other(
                "directory identity changed during anchored open",
            ));
        }
        Ok(descriptor)
    }

    fn validate_private_directory(
        directory: &OwnedFd,
        expected_uid: u32,
    ) -> io::Result<DirectoryIdentity> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        let identity = directory_identity(&metadata)?;
        if metadata.st_uid != expected_uid || metadata.st_mode as u32 & 0o777 != PRIVATE_MODE {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private directory must be owned by the effective user with mode 0700",
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::require_empty(directory)?;
        Ok(identity)
    }

    fn validate_owner_controlled_directory(
        directory: &OwnedFd,
        expected_uid: u32,
    ) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        directory_identity(&metadata)?;
        if metadata.st_uid != expected_uid || metadata.st_mode as u32 & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache base must be owned by the effective user and not group/other writable",
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::reject_allow_entries(directory)?;
        Ok(())
    }

    fn validate_trusted_ancestor(directory: &OwnedFd, expected_uid: u32) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        directory_identity(&metadata)?;
        let owner_is_trusted = metadata.st_uid == expected_uid || metadata.st_uid == 0;
        let mode = metadata.st_mode as u32;
        let mutation_is_private = mode & 0o022 == 0;
        let sticky_namespace = owner_is_trusted && mode & 0o1000 != 0;
        if !owner_is_trusted || (!mutation_is_private && !sticky_namespace) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private override ancestor is writable by an untrusted principal",
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::reject_allow_entries(directory)?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    mod macos_acl {
        use std::ffi::c_void;
        use std::io;
        use std::os::fd::{AsRawFd as _, OwnedFd};
        use std::ptr::NonNull;

        const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
        const ACL_FIRST_ENTRY: libc::c_int = 0;
        const ACL_NEXT_ENTRY: libc::c_int = -1;
        const ACL_EXTENDED_ALLOW: libc::c_int = 1;
        const ACL_EXTENDED_DENY: libc::c_int = 2;

        type Acl = *mut c_void;
        type AclEntry = *mut c_void;

        unsafe extern "C" {
            fn acl_get_fd_np(fd: libc::c_int, acl_type: libc::c_int) -> Acl;
            fn acl_get_entry(acl: Acl, entry_id: libc::c_int, entry: *mut AclEntry) -> libc::c_int;
            fn acl_get_tag_type(entry: AclEntry, tag_type: *mut libc::c_int) -> libc::c_int;
            fn acl_free(object: *mut c_void) -> libc::c_int;
            fn acl_valid(acl: Acl) -> libc::c_int;
        }

        struct OwnedAcl(NonNull<c_void>);

        impl OwnedAcl {
            fn read(descriptor: &OwnedFd) -> io::Result<Option<Self>> {
                clear_errno();
                // SAFETY: the descriptor is live and the ACL type is the Darwin extended type.
                let acl = unsafe { acl_get_fd_np(descriptor.as_raw_fd(), ACL_TYPE_EXTENDED) };
                let Some(acl) = NonNull::new(acl) else {
                    let errno = current_errno();
                    return match errno {
                        libc::ENOENT => Ok(None),
                        0 => Err(io::Error::other(
                            "macOS returned no extended ACL without reporting its absence",
                        )),
                        _ => Err(io::Error::from_raw_os_error(errno)),
                    };
                };
                // SAFETY: `acl_get_fd_np` returned owned ACL working storage.
                if unsafe { acl_valid(acl.as_ptr()) } != 0 {
                    return Err(last_acl_error("macOS returned an invalid extended ACL"));
                }
                Ok(Some(Self(acl)))
            }

            fn next_entry(&self, entry_id: libc::c_int) -> io::Result<Option<AclEntry>> {
                let mut entry = std::ptr::null_mut();
                clear_errno();
                // SAFETY: the ACL allocation is valid and `entry` is a writable output pointer.
                let status = unsafe { acl_get_entry(self.0.as_ptr(), entry_id, &raw mut entry) };
                if status == 0 {
                    if entry.is_null() {
                        return Err(io::Error::other("macOS returned a null extended ACL entry"));
                    }
                    return Ok(Some(entry));
                }
                let errno = current_errno();
                if status == -1 && errno == libc::EINVAL {
                    return Ok(None);
                }
                Err(last_acl_error("macOS could not enumerate an extended ACL"))
            }

            fn tag_type(&self, entry: AclEntry) -> io::Result<libc::c_int> {
                let mut tag_type = 0;
                clear_errno();
                // SAFETY: `entry` was returned for this retained ACL allocation.
                if unsafe { acl_get_tag_type(entry, &raw mut tag_type) } != 0 {
                    return Err(last_acl_error(
                        "macOS could not inspect an extended ACL entry",
                    ));
                }
                Ok(tag_type)
            }
        }

        impl Drop for OwnedAcl {
            fn drop(&mut self) {
                // SAFETY: this allocation came from `acl_get_fd_np` and is freed exactly once.
                let _ = unsafe { acl_free(self.0.as_ptr()) };
            }
        }

        pub(super) fn require_empty(descriptor: &OwnedFd) -> io::Result<()> {
            let Some(acl) = OwnedAcl::read(descriptor)? else {
                return Ok(());
            };
            if acl.next_entry(ACL_FIRST_ENTRY)?.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "private macOS object must not have an extended ACL",
                ));
            }
            Ok(())
        }

        pub(super) fn reject_allow_entries(descriptor: &OwnedFd) -> io::Result<()> {
            let Some(acl) = OwnedAcl::read(descriptor)? else {
                return Ok(());
            };
            let mut entry_id = ACL_FIRST_ENTRY;
            while let Some(entry) = acl.next_entry(entry_id)? {
                match acl.tag_type(entry)? {
                    ACL_EXTENDED_DENY => {}
                    ACL_EXTENDED_ALLOW => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "macOS ancestor extended ACL contains an allow entry",
                        ));
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "macOS ancestor extended ACL contains an unsupported entry type",
                        ));
                    }
                }
                entry_id = ACL_NEXT_ENTRY;
            }
            Ok(())
        }

        fn clear_errno() {
            // SAFETY: `__error` returns the calling thread's writable errno slot on Darwin.
            unsafe { *libc::__error() = 0 };
        }

        fn current_errno() -> libc::c_int {
            // SAFETY: `__error` returns the calling thread's readable errno slot on Darwin.
            unsafe { *libc::__error() }
        }

        fn last_acl_error(context: &'static str) -> io::Error {
            let errno = current_errno();
            if errno == 0 {
                io::Error::other(context)
            } else {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("{context}: {}", io::Error::from_raw_os_error(errno)),
                )
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn validate_sticky_temporary_base(directory: &OwnedFd) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        directory_identity(&metadata)?;
        let mode = metadata.st_mode as u32;
        if metadata.st_uid != 0 || mode & 0o1000 == 0 || mode & 0o002 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary fallback must be root-owned, sticky, and world-writable",
            ));
        }
        Ok(())
    }

    fn directory_identity(stat: &rustix::fs::Stat) -> io::Result<DirectoryIdentity> {
        if !FileType::from_raw_mode(stat.st_mode).is_dir() {
            return Err(io::Error::other("path is not a directory"));
        }
        let device = stat.st_dev as u64;
        let inode = stat.st_ino as u64;
        if device == 0 || inode == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem returned an unstable zero directory identity",
            ));
        }
        Ok(DirectoryIdentity { device, inode })
    }

    fn validate_leaf(name: &OsStr) -> io::Result<()> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&0)
        {
            Err(invalid_path())
        } else {
            Ok(())
        }
    }

    fn invalid_path() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private root contains an invalid or escaping path component",
        )
    }

    #[cfg(target_os = "macos")]
    fn darwin_user_temporary_directory() -> io::Result<PathBuf> {
        // SAFETY: the null-buffer call is the documented size query for confstr.
        let required =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if required == 0 || required > 64 * 1024 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u8; required];
        // SAFETY: `buffer` is writable for its full declared length.
        let returned = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_TEMP_DIR,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if returned == 0 || returned > buffer.len() || buffer[returned - 1] != 0 {
            return Err(io::Error::other(
                "confstr returned an invalid Darwin user temporary directory",
            ));
        }
        buffer.truncate(returned - 1);
        if buffer.is_empty() {
            return Err(io::Error::other(
                "confstr returned an empty Darwin user temporary directory",
            ));
        }
        Ok(PathBuf::from(OsString::from_vec(buffer)))
    }

    #[cfg(target_os = "macos")]
    fn effective_home_directory(expected_uid: u32) -> Result<PathBuf, PrivateRootsError> {
        use std::ffi::CStr;
        use std::mem::MaybeUninit;

        const MAX_PASSWD_BUFFER: usize = 1024 * 1024;
        // SAFETY: sysconf has no pointer preconditions.
        let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
        let initial = usize::try_from(suggested).unwrap_or(16 * 1024).max(1024);
        let mut size = initial.min(MAX_PASSWD_BUFFER);
        loop {
            let mut passwd = MaybeUninit::<libc::passwd>::zeroed();
            let mut result = std::ptr::null_mut();
            let mut buffer = vec![0_u8; size];
            // SAFETY: every output points to writable storage retained through result handling.
            let status = unsafe {
                libc::getpwuid_r(
                    expected_uid,
                    passwd.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &raw mut result,
                )
            };
            if status == libc::ERANGE && size < MAX_PASSWD_BUFFER {
                size = size.saturating_mul(2).min(MAX_PASSWD_BUFFER);
                continue;
            }
            if status != 0 {
                return Err(filesystem_error(
                    PrivateRootKind::Cache,
                    "resolve effective-user home",
                    Path::new("<effective-user-home>"),
                    io::Error::from_raw_os_error(status),
                ));
            }
            if result.is_null() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            // SAFETY: getpwuid_r succeeded and the passwd record points into retained `buffer`.
            let passwd = unsafe { passwd.assume_init() };
            if passwd.pw_dir.is_null() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            // SAFETY: POSIX guarantees a NUL-terminated pw_dir within the supplied buffer.
            let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
            if bytes.is_empty() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
            if !path.is_absolute() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            return Ok(path);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        #[cfg(target_os = "macos")]
        use std::process::Command;

        use super::*;

        #[test]
        fn private_child_is_exactly_private_and_revalidation_detects_widening() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = open_path(temporary.path()).unwrap();
            let path = temporary.path().join("private");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let private = create_or_open_private_child(
                &base,
                OsStr::new("private"),
                security_context.effective_uid(),
            )
            .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            private.revalidate(&path, &security_context).unwrap();

            fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
            assert_eq!(
                private
                    .revalidate(&path, &security_context)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn private_root_rejects_a_linked_component() {
            let temporary = tempfile::tempdir().unwrap();
            let target = temporary.path().join("target");
            let linked = temporary.path().join("linked");
            fs::create_dir(&target).unwrap();
            symlink(&target, &linked).unwrap();
            assert!(open_path(&linked).is_err());
        }

        #[test]
        fn strict_override_rejects_a_non_sticky_world_writable_ancestor() {
            let temporary = tempfile::tempdir().unwrap();
            let unsafe_parent = temporary.path().join("unsafe-parent");
            fs::create_dir(&unsafe_parent).unwrap();
            fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
            let override_path = unsafe_parent.join("index-base");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();

            let error = open_or_create_private_path(&override_path, &security_context)
                .err()
                .unwrap();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(!override_path.exists());
        }

        #[test]
        fn strict_override_revalidation_detects_ancestor_permission_widening() {
            let temporary = tempfile::tempdir().unwrap();
            let stable_parent = temporary.path().join("stable-parent");
            fs::create_dir(&stable_parent).unwrap();
            fs::set_permissions(&stable_parent, fs::Permissions::from_mode(0o700)).unwrap();
            let override_path = stable_parent.join("index-base");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let private = open_or_create_private_path(&override_path, &security_context).unwrap();

            fs::set_permissions(&stable_parent, fs::Permissions::from_mode(0o777)).unwrap();

            assert_eq!(
                private
                    .revalidate(&override_path, &security_context)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn existing_insecure_child_is_rejected_without_permission_repair() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = open_path(temporary.path()).unwrap();
            let child = temporary.path().join("insecure");
            fs::create_dir(&child).unwrap();
            fs::set_permissions(&child, fs::Permissions::from_mode(0o750)).unwrap();

            assert_eq!(
                create_or_open_private_child(
                    &base,
                    OsStr::new("insecure"),
                    CurrentSecurityContextSnapshot::current()
                        .unwrap()
                        .effective_uid(),
                )
                .err()
                .unwrap()
                .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::metadata(&child).unwrap().permissions().mode() & 0o777,
                0o750
            );
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn private_directory_rejects_an_extended_acl_even_with_mode_0700() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = open_path(temporary.path()).unwrap();
            let path = temporary.path().join("private");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let private = create_or_open_private_child(
                &base,
                OsStr::new("private"),
                security_context.effective_uid(),
            )
            .unwrap();
            add_macos_acl(&path, "everyone allow list,search");

            let error = private.revalidate(&path, &security_context).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn strict_override_rejects_an_ancestor_allow_acl_before_creation() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let ancestor = temporary.path().join("acl-ancestor");
            fs::create_dir(&ancestor).unwrap();
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
            add_macos_acl(
                &ancestor,
                "everyone allow add_file,add_subdirectory,delete_child,file_inherit,directory_inherit",
            );
            let override_path = ancestor.join("index-base");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();

            let error = open_or_create_private_path(&override_path, &security_context)
                .err()
                .unwrap();

            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(!override_path.exists());
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn strict_override_allows_a_deny_only_ancestor_acl() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let ancestor = temporary.path().join("acl-ancestor");
            fs::create_dir(&ancestor).unwrap();
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
            add_macos_acl(&ancestor, "everyone deny delete");
            let override_path = ancestor.join("index-base");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();

            let private = open_or_create_private_path(&override_path, &security_context).unwrap();

            private
                .revalidate(&override_path, &security_context)
                .unwrap();
        }

        #[cfg(target_os = "macos")]
        fn add_macos_acl(path: &Path, entry: &str) {
            let status = Command::new("chmod")
                .arg("+a")
                .arg(entry)
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "failed to install test ACL: {entry}");
        }

        #[test]
        fn environment_roots_must_be_absolute_and_nonempty() {
            assert!(matches!(
                environment_path("XDG_CACHE_HOME", OsString::new()),
                Err(PrivateRootsError::InvalidEnvironment { .. })
            ));
            assert!(matches!(
                environment_path("XDG_CACHE_HOME", OsString::from("relative")),
                Err(PrivateRootsError::InvalidEnvironment { .. })
            ));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_xdg_roots_are_created_under_validated_bases() {
            let temporary = tempfile::tempdir().unwrap();
            let runtime_base = temporary.path().join("runtime");
            let cache_base = temporary.path().join("cache");
            fs::create_dir(&runtime_base).unwrap();
            fs::create_dir(&cache_base).unwrap();
            fs::set_permissions(&runtime_base, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&cache_base, fs::Permissions::from_mode(0o755)).unwrap();

            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let expected_uid = security_context.effective_uid();
            let (runtime_path, runtime) =
                discover_linux_runtime(Some(runtime_base.into_os_string()), expected_uid).unwrap();
            let (cache_path, cache) =
                discover_linux_cache(Some(cache_base.into_os_string()), None, expected_uid)
                    .unwrap();

            assert_eq!(
                runtime_path.file_name(),
                Some(OsStr::new(PRODUCT_DIRECTORY))
            );
            assert_eq!(cache_path.file_name(), Some(OsStr::new(PRODUCT_DIRECTORY)));
            assert_eq!(
                fs::metadata(&runtime_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            runtime
                .revalidate(&runtime_path, &security_context)
                .unwrap();
            cache.revalidate(&cache_path, &security_context).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn invalid_explicit_xdg_runtime_never_falls_back() {
            let temporary = tempfile::tempdir().unwrap();
            let runtime_base = temporary.path().join("runtime");
            fs::create_dir(&runtime_base).unwrap();
            fs::set_permissions(&runtime_base, fs::Permissions::from_mode(0o755)).unwrap();

            assert!(
                discover_linux_runtime(
                    Some(runtime_base.clone().into_os_string()),
                    CurrentSecurityContextSnapshot::current()
                        .unwrap()
                        .effective_uid(),
                )
                .is_err()
            );
            assert!(!runtime_base.join(PRODUCT_DIRECTORY).exists());
        }
    }
}

#[cfg(windows)]
#[path = "roots_windows.rs"]
mod platform;

#[cfg(windows)]
pub(crate) use platform::{
    PrivateSecurityDescriptor as WindowsPrivateSecurityDescriptor, WINDOWS_NAMED_PIPE_CLIENT_ACCESS,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    use super::{DiscoveredRoots, PrivateRootsError};
    use crate::security_context::CurrentSecurityContextSnapshot;

    pub(super) struct PrivateDirectory;

    impl PrivateDirectory {
        pub(super) fn canonical_path(&self, requested: &Path) -> PathBuf {
            requested.to_path_buf()
        }

        pub(super) fn revalidate(
            &self,
            _path: &Path,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn create_private_child(
            &self,
            _name: &std::ffi::OsStr,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn create_private_file(
            &self,
            _directory_path: &Path,
            _name: &std::ffi::OsStr,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<File> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn open_private_file(
            &self,
            _directory_path: &Path,
            _name: &std::ffi::OsStr,
            _security_context: &CurrentSecurityContextSnapshot,
            _writable: bool,
        ) -> io::Result<File> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn rename_private_file(
            &self,
            _directory_path: &Path,
            _source: &std::ffi::OsStr,
            _destination: &std::ffi::OsStr,
            _replace: bool,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn remove_private_file(
            &self,
            _directory_path: &Path,
            _name: &std::ffi::OsStr,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }

        pub(super) fn sync(&self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }
    }

    pub(super) fn open_or_create_private_path(
        _path: &Path,
        _security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<PrivateDirectory> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private local roots are unsupported on this platform",
        ))
    }

    pub(super) fn discover(
        _security_context: &CurrentSecurityContextSnapshot,
    ) -> Result<DiscoveredRoots, PrivateRootsError> {
        Err(PrivateRootsError::UnsupportedPlatform)
    }
}
