use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::collections::TryReserveError;
use std::error::Error as StdError;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(windows)]
use std::ffi::OsStr;

use unity_asset_core::{DigestBuildError, DigestV1, DigestV1Builder};
use unity_asset_search_protocol::{MAX_PORTABLE_PATH_BYTES, ProjectId};

const PROJECT_PATH_IDENTITY_DOMAIN: &[u8] = b"unity-asset:project-path:v1\0";

/// Lexical coordinate system for one already-verified project root.
///
/// This type owns path equivalence only. It does not authorize filesystem access; scanners must
/// still reopen paths through their anchored project handle before consuming bytes.
/// On Windows, one project uses ordinal ignore-case identity throughout. NTFS directories with
/// per-directory case-sensitive lookup enabled are outside this coordinate contract.
#[derive(Clone)]
pub struct ProjectPathSpace {
    inner: Arc<ProjectPathSpaceInner>,
}

struct ProjectPathSpaceInner {
    project_id: ProjectId,
    root: PathBuf,
}

impl ProjectPathSpace {
    pub(crate) fn new(root: PathBuf, project_id: ProjectId) -> Result<Self, ProjectPathError> {
        if !root.is_absolute() {
            return Err(ProjectPathError::InvalidRoot { root });
        }
        Ok(Self {
            inner: Arc::new(ProjectPathSpaceInner { project_id, root }),
        })
    }

    #[must_use]
    pub fn project_id(&self) -> ProjectId {
        self.inner.project_id
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Resolves an absolute or project-relative lexical path into this project coordinate space.
    ///
    /// `Ok(None)` represents the project root itself. Paths outside the root or paths whose
    /// lexical parent traversal escapes it are rejected.
    pub fn resolve(&self, supplied: &Path) -> Result<Option<ProjectPath>, ProjectPathError> {
        let relative = if supplied.is_absolute() {
            strip_prefix(self.root(), supplied).map_err(|()| ProjectPathError::OutsideProject {
                path: supplied.to_path_buf(),
                project_root: self.root().to_path_buf(),
            })?
        } else {
            supplied
        };
        let relative = normalize_relative_path(relative, supplied, self.root())?;
        if relative.as_os_str().is_empty() {
            return Ok(None);
        }
        ProjectPath::new(self.project_id(), relative).map(Some)
    }

    pub fn resolve_set<I, P>(&self, paths: I) -> Result<ProjectPathSet, ProjectPathError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut resolved = ProjectPathSet::new(self);
        for path in paths {
            let supplied = path.as_ref();
            let Some(path) = self.resolve(supplied)? else {
                return Err(ProjectPathError::ProjectRootChangedPath {
                    path: supplied.to_path_buf(),
                });
            };
            resolved.insert(path)?;
        }
        Ok(resolved)
    }
}

impl fmt::Debug for ProjectPathSpace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectPathSpace")
            .field("project_id", &self.project_id())
            .field("root", &self.root())
            .finish()
    }
}

impl PartialEq for ProjectPathSpace {
    fn eq(&self, other: &Self) -> bool {
        self.project_id() == other.project_id()
    }
}

impl Eq for ProjectPathSpace {}

/// One normalized, non-root lexical coordinate inside a [`ProjectPathSpace`].
#[derive(Clone)]
pub struct ProjectPath {
    project_id: ProjectId,
    relative: PathBuf,
    key: ProjectPathKey,
    identity: DigestV1,
}

impl ProjectPath {
    fn new(project_id: ProjectId, relative: PathBuf) -> Result<Self, ProjectPathError> {
        validate_portable_relative_path(&relative)?;
        let key = ProjectPathKey::from_relative(&relative)?;
        let identity = project_path_identity(project_id, &key)?;
        Ok(Self {
            project_id,
            relative,
            key,
            identity,
        })
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub fn as_relative_path(&self) -> &Path {
        &self.relative
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&std::ffi::OsStr> {
        self.relative.file_name()
    }

    #[must_use]
    pub const fn identity(&self) -> DigestV1 {
        self.identity
    }

    #[must_use]
    pub fn is_at_or_below(&self, prefix: &Self) -> bool {
        self.project_id == prefix.project_id && self.key.starts_with(&prefix.key)
    }
}

impl fmt::Debug for ProjectPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectPath")
            .field("project_id", &self.project_id)
            .field("relative", &self.relative)
            .field("identity", &self.identity)
            .finish()
    }
}

impl PartialEq for ProjectPath {
    fn eq(&self, other: &Self) -> bool {
        self.project_id == other.project_id && self.key == other.key
    }
}

impl Eq for ProjectPath {}

impl PartialOrd for ProjectPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ProjectPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.project_id
            .cmp(&other.project_id)
            .then_with(|| self.key.cmp(&other.key))
    }
}

impl Hash for ProjectPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.project_id.hash(state);
        self.key.hash(state);
    }
}

/// Sorted, deduplicated changed paths that are proven to belong to one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPathSet {
    project_id: ProjectId,
    paths: BTreeSet<ProjectPath>,
}

impl ProjectPathSet {
    #[must_use]
    pub fn new(space: &ProjectPathSpace) -> Self {
        Self {
            project_id: space.project_id(),
            paths: BTreeSet::new(),
        }
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.project_id
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn insert(&mut self, path: ProjectPath) -> Result<bool, ProjectPathError> {
        if path.project_id != self.project_id {
            return Err(ProjectPathError::DifferentProject {
                expected: self.project_id,
                actual: path.project_id,
            });
        }
        Ok(self.paths.insert(path))
    }

    pub fn extend(&mut self, other: Self) -> Result<(), ProjectPathError> {
        if other.project_id != self.project_id {
            return Err(ProjectPathError::DifferentProject {
                expected: self.project_id,
                actual: other.project_id,
            });
        }
        self.paths.extend(other.paths);
        Ok(())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &ProjectPath> {
        self.paths.iter()
    }

    pub fn into_paths(self) -> impl ExactSizeIterator<Item = ProjectPath> {
        self.paths.into_iter()
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ProjectPathError {
    InvalidRoot {
        root: PathBuf,
    },
    OutsideProject {
        path: PathBuf,
        project_root: PathBuf,
    },
    InvalidComponent {
        path: PathBuf,
    },
    ProjectRootChangedPath {
        path: PathBuf,
    },
    DifferentProject {
        expected: ProjectId,
        actual: ProjectId,
    },
    SizeOverflow {
        resource: &'static str,
    },
    Allocation {
        resource: &'static str,
        requested: usize,
        source: TryReserveError,
    },
    Digest(DigestBuildError),
}

impl fmt::Display for ProjectPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot { root } => {
                write!(
                    formatter,
                    "project path space root must be absolute: {}",
                    root.display()
                )
            }
            Self::OutsideProject { path, project_root } => write!(
                formatter,
                "path {} is outside project root {}",
                path.display(),
                project_root.display()
            ),
            Self::InvalidComponent { path } => {
                write!(
                    formatter,
                    "path contains an invalid project component: {}",
                    path.display()
                )
            }
            Self::ProjectRootChangedPath { path } => write!(
                formatter,
                "project root {} must be represented by a reconcile intent, not a changed path",
                path.display()
            ),
            Self::DifferentProject { expected, actual } => write!(
                formatter,
                "project path belongs to {actual}, but this path space belongs to {expected}"
            ),
            Self::SizeOverflow { resource } => {
                write!(formatter, "project path {resource} size overflow")
            }
            Self::Allocation {
                resource,
                requested,
                ..
            } => write!(
                formatter,
                "could not reserve {requested} units for project path {resource}"
            ),
            Self::Digest(error) => {
                write!(formatter, "could not derive project path identity: {error}")
            }
        }
    }
}

impl StdError for ProjectPathError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Allocation { source, .. } => Some(source),
            Self::Digest(error) => Some(error),
            _ => None,
        }
    }
}

impl From<DigestBuildError> for ProjectPathError {
    fn from(error: DigestBuildError) -> Self {
        Self::Digest(error)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ProjectPathKey {
    #[cfg(windows)]
    units: Vec<u16>,
    #[cfg(not(windows))]
    units: Vec<u8>,
}

impl ProjectPathKey {
    fn from_relative(relative: &Path) -> Result<Self, ProjectPathError> {
        #[cfg(windows)]
        let mut units = Vec::<u16>::new();
        #[cfg(not(windows))]
        let mut units = Vec::<u8>::new();

        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                return Err(ProjectPathError::InvalidComponent {
                    path: relative.to_path_buf(),
                });
            };
            append_component_key(&mut units, component)?;
        }
        Ok(Self { units })
    }

    fn starts_with(&self, prefix: &Self) -> bool {
        self.units.starts_with(&prefix.units)
    }

    fn encoded_bytes(&self) -> Result<u64, ProjectPathError> {
        #[cfg(windows)]
        let unit_bytes = std::mem::size_of::<u16>();
        #[cfg(not(windows))]
        let unit_bytes = std::mem::size_of::<u8>();
        self.units
            .len()
            .checked_mul(unit_bytes)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(ProjectPathError::SizeOverflow {
                resource: "identity",
            })
    }

    fn update_digest(&self, digest: &mut DigestV1Builder) -> Result<(), DigestBuildError> {
        #[cfg(windows)]
        for unit in &self.units {
            digest.update(&unit.to_le_bytes())?;
        }
        #[cfg(not(windows))]
        digest.update(&self.units)?;
        Ok(())
    }
}

fn project_path_identity(
    project_id: ProjectId,
    key: &ProjectPathKey,
) -> Result<DigestV1, ProjectPathError> {
    let key_bytes = key.encoded_bytes()?;
    let declared_length = u64::try_from(PROJECT_PATH_IDENTITY_DOMAIN.len())
        .ok()
        .and_then(|length| length.checked_add(project_id.as_bytes().len() as u64))
        .and_then(|length| length.checked_add(key_bytes))
        .ok_or(ProjectPathError::SizeOverflow {
            resource: "identity",
        })?;
    let mut digest = DigestV1Builder::new(declared_length);
    digest.update(PROJECT_PATH_IDENTITY_DOMAIN)?;
    digest.update(project_id.as_bytes())?;
    key.update_digest(&mut digest)?;
    Ok(digest.finalize()?)
}

fn normalize_relative_path(
    relative: &Path,
    supplied: &Path,
    project_root: &Path,
) -> Result<PathBuf, ProjectPathError> {
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return Err(ProjectPathError::OutsideProject {
                        path: supplied.to_path_buf(),
                        project_root: project_root.to_path_buf(),
                    });
                }
            }
            std::path::Component::Normal(component) if component == std::ffi::OsStr::new(".") => {}
            std::path::Component::Normal(component) if component == std::ffi::OsStr::new("..") => {
                if !normalized.pop() {
                    return Err(ProjectPathError::OutsideProject {
                        path: supplied.to_path_buf(),
                        project_root: project_root.to_path_buf(),
                    });
                }
            }
            std::path::Component::Normal(component) => {
                if component_contains_nul(component) {
                    return Err(ProjectPathError::InvalidComponent {
                        path: supplied.to_path_buf(),
                    });
                }
                normalized.push(component);
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(ProjectPathError::OutsideProject {
                    path: supplied.to_path_buf(),
                    project_root: project_root.to_path_buf(),
                });
            }
        }
    }
    Ok(normalized)
}

fn validate_portable_relative_path(relative: &Path) -> Result<(), ProjectPathError> {
    let mut encoded_bytes = 0_usize;
    for (index, component) in relative.components().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(ProjectPathError::InvalidComponent {
                path: relative.to_path_buf(),
            });
        };
        let Some(component) = component.to_str() else {
            return Err(ProjectPathError::InvalidComponent {
                path: relative.to_path_buf(),
            });
        };
        if !is_portable_component(component) {
            return Err(ProjectPathError::InvalidComponent {
                path: relative.to_path_buf(),
            });
        }
        encoded_bytes = encoded_bytes
            .checked_add(usize::from(index != 0))
            .and_then(|bytes| bytes.checked_add(component.len()))
            .ok_or(ProjectPathError::SizeOverflow {
                resource: "portable coordinate",
            })?;
        if encoded_bytes > MAX_PORTABLE_PATH_BYTES {
            return Err(ProjectPathError::InvalidComponent {
                path: relative.to_path_buf(),
            });
        }
    }
    Ok(())
}

pub(crate) fn is_portable_component(component: &str) -> bool {
    !component.is_empty()
        && !matches!(component, "." | "..")
        && !component
            .chars()
            .any(|character| matches!(character, '\\' | '\0' | ':'))
}

#[cfg(unix)]
fn component_contains_nul(component: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    component.as_bytes().contains(&0)
}

#[cfg(windows)]
fn component_contains_nul(component: &std::ffi::OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt as _;

    component.encode_wide().any(|unit| unit == 0)
}

#[cfg(not(any(unix, windows)))]
fn component_contains_nul(component: &std::ffi::OsStr) -> bool {
    component.to_string_lossy().contains('\0')
}

#[cfg(unix)]
fn append_component_key(
    key: &mut Vec<u8>,
    component: &std::ffi::OsStr,
) -> Result<(), ProjectPathError> {
    use std::os::unix::ffi::OsStrExt as _;

    let encoded = component.as_bytes();
    let requested = encoded
        .len()
        .checked_add(1)
        .ok_or(ProjectPathError::SizeOverflow { resource: "key" })?;
    key.try_reserve(requested)
        .map_err(|source| ProjectPathError::Allocation {
            resource: "key",
            requested,
            source,
        })?;
    key.extend_from_slice(encoded);
    key.push(0);
    Ok(())
}

#[cfg(windows)]
fn append_component_key(
    key: &mut Vec<u16>,
    component: &std::ffi::OsStr,
) -> Result<(), ProjectPathError> {
    use std::os::windows::ffi::OsStrExt as _;
    let units = component.encode_wide().count();
    let requested = units
        .checked_add(1)
        .ok_or(ProjectPathError::SizeOverflow { resource: "key" })?;
    key.try_reserve(requested)
        .map_err(|source| ProjectPathError::Allocation {
            resource: "key",
            requested,
            source,
        })?;
    for unit in component.encode_wide() {
        key.push(windows_ordinal_upcase(unit));
    }
    key.push(0);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn append_component_key(
    key: &mut Vec<u8>,
    component: &std::ffi::OsStr,
) -> Result<(), ProjectPathError> {
    let Some(encoded) = component.to_str() else {
        return Err(ProjectPathError::InvalidComponent {
            path: PathBuf::from(component),
        });
    };
    let requested = encoded
        .len()
        .checked_add(1)
        .ok_or(ProjectPathError::SizeOverflow { resource: "key" })?;
    key.try_reserve(requested)
        .map_err(|source| ProjectPathError::Allocation {
            resource: "key",
            requested,
            source,
        })?;
    key.extend_from_slice(encoded.as_bytes());
    key.push(0);
    Ok(())
}

pub(crate) fn compare_portable_paths(left: &str, right: &str) -> Ordering {
    let mut left_components = left.split('/');
    let mut right_components = right.split('/');
    loop {
        match (left_components.next(), right_components.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = portable_component_cmp(left, right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn portable_component_cmp(left: &str, right: &str) -> Ordering {
    left.cmp(right)
}

#[cfg(windows)]
fn portable_component_cmp(left: &str, right: &str) -> Ordering {
    windows_component_cmp(OsStr::new(left), OsStr::new(right))
}

#[cfg(not(windows))]
pub(crate) fn strip_prefix<'path>(prefix: &Path, path: &'path Path) -> Result<&'path Path, ()> {
    path.strip_prefix(prefix).map_err(|_| ())
}

#[cfg(windows)]
pub(crate) fn strip_prefix<'path>(prefix: &Path, path: &'path Path) -> Result<&'path Path, ()> {
    use std::path::Component;

    let mut prefix_components = prefix.components();
    let mut path_components = path.components();
    let (Some(Component::Prefix(prefix_root)), Some(Component::Prefix(path_root))) =
        (prefix_components.next(), path_components.next())
    else {
        return Err(());
    };
    if !windows_prefix_eq(prefix_root.kind(), path_root.kind())
        || !matches!(prefix_components.next(), Some(Component::RootDir))
        || !matches!(path_components.next(), Some(Component::RootDir))
    {
        return Err(());
    }
    for component in prefix_components {
        let Component::Normal(expected) = component else {
            return Err(());
        };
        let Some(Component::Normal(actual)) = path_components.next() else {
            return Err(());
        };
        if !windows_component_eq(expected, actual) {
            return Err(());
        }
    }
    Ok(path_components.as_path())
}

#[cfg(windows)]
fn windows_prefix_eq(left: std::path::Prefix<'_>, right: std::path::Prefix<'_>) -> bool {
    use std::path::Prefix;

    match (left, right) {
        (Prefix::Disk(left), Prefix::Disk(right))
        | (Prefix::Disk(left), Prefix::VerbatimDisk(right))
        | (Prefix::VerbatimDisk(left), Prefix::Disk(right))
        | (Prefix::VerbatimDisk(left), Prefix::VerbatimDisk(right)) => {
            left.eq_ignore_ascii_case(&right)
        }
        (Prefix::UNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (Prefix::UNC(left_server, left_share), Prefix::VerbatimUNC(right_server, right_share))
        | (Prefix::VerbatimUNC(left_server, left_share), Prefix::UNC(right_server, right_share))
        | (
            Prefix::VerbatimUNC(left_server, left_share),
            Prefix::VerbatimUNC(right_server, right_share),
        ) => {
            windows_component_eq(left_server, right_server)
                && windows_component_eq(left_share, right_share)
        }
        _ => false,
    }
}

#[cfg(windows)]
pub(crate) fn windows_component_eq(left: &OsStr, right: &OsStr) -> bool {
    windows_component_cmp(left, right) == Ordering::Equal
}

#[cfg(windows)]
pub(crate) fn windows_component_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt as _;

    let mut left = left.encode_wide().map(windows_ordinal_upcase);
    let mut right = right.encode_wide().map(windows_ordinal_upcase);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = left.cmp(&right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(windows)]
fn windows_ordinal_upcase(unit: u16) -> u16 {
    use windows_sys::Wdk::System::SystemServices::RtlUpcaseUnicodeChar;

    // SAFETY: the API accepts any UTF-16 code unit by value and does not dereference caller
    // memory. Windows ordinal ignore-case comparison uses this same one-unit upcase table.
    unsafe { RtlUpcaseUnicodeChar(unit) }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use unity_asset_search_protocol::{MAX_PORTABLE_PATH_BYTES, ProjectId};

    use super::{ProjectPathError, ProjectPathSpace};

    #[cfg(windows)]
    use super::{windows_component_cmp, windows_ordinal_upcase};
    #[cfg(windows)]
    use std::cmp::Ordering;
    #[cfg(windows)]
    use std::ffi::OsString;

    #[cfg(windows)]
    fn native_windows_component_cmp(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> Ordering {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Globalization::{
            CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
        };

        let left = left.encode_wide().collect::<Vec<_>>();
        let right = right.encode_wide().collect::<Vec<_>>();
        let left_len = i32::try_from(left.len()).unwrap();
        let right_len = i32::try_from(right.len()).unwrap();
        // SAFETY: both slices stay live for the call and the explicit lengths are exact.
        match unsafe { CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) }
        {
            CSTR_LESS_THAN => Ordering::Less,
            CSTR_EQUAL => Ordering::Equal,
            CSTR_GREATER_THAN => Ordering::Greater,
            value => panic!("CompareStringOrdinal returned {value}"),
        }
    }

    fn project_id(seed: u8) -> ProjectId {
        ProjectId::from_bytes([seed; 32])
    }

    #[cfg(windows)]
    fn space() -> ProjectPathSpace {
        ProjectPathSpace::new(r"C:\Project".into(), project_id(1)).unwrap()
    }

    #[cfg(not(windows))]
    fn space() -> ProjectPathSpace {
        ProjectPathSpace::new("/Project".into(), project_id(1)).unwrap()
    }

    #[test]
    fn relative_and_absolute_coordinates_share_one_identity() {
        let space = space();
        let relative = space
            .resolve(Path::new("Assets/Hero.prefab"))
            .unwrap()
            .unwrap();
        let absolute = space
            .resolve(&space.root().join("Assets/Hero.prefab"))
            .unwrap()
            .unwrap();

        assert_eq!(relative, absolute);
        assert_eq!(relative.identity(), absolute.identity());
        assert_eq!(
            space.root().join(relative.as_relative_path()),
            space.root().join("Assets/Hero.prefab")
        );
    }

    #[test]
    fn lexical_parent_escape_is_rejected_at_the_path_space_boundary() {
        let error = space().resolve(Path::new("../Outside.asset")).unwrap_err();

        assert!(matches!(error, ProjectPathError::OutsideProject { .. }));
    }

    #[test]
    fn non_portable_components_are_rejected_at_the_path_space_boundary() {
        let error = space()
            .resolve(Path::new("Assets/Bad:Name.asset"))
            .unwrap_err();

        assert!(matches!(error, ProjectPathError::InvalidComponent { .. }));
    }

    #[test]
    fn portable_path_byte_limit_is_exact() {
        let exact = "a".repeat(MAX_PORTABLE_PATH_BYTES);
        assert!(space().resolve(Path::new(&exact)).unwrap().is_some());

        let oversized = "a".repeat(MAX_PORTABLE_PATH_BYTES + 1);
        let error = space().resolve(Path::new(&oversized)).unwrap_err();
        assert!(matches!(error, ProjectPathError::InvalidComponent { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_coordinates_are_rejected_at_the_path_space_boundary() {
        use std::os::unix::ffi::OsStringExt as _;

        let invalid = std::path::PathBuf::from("Assets")
            .join(std::ffi::OsString::from_vec(b"invalid-\xFF.asset".to_vec()));
        let error = space().resolve(&invalid).unwrap_err();

        assert!(matches!(error, ProjectPathError::InvalidComponent { .. }));
    }

    #[test]
    fn path_sets_are_sorted_deduplicated_and_project_bound() {
        let space = space();
        let paths = space
            .resolve_set([
                Path::new("Assets/Z.asset"),
                Path::new("Assets/A.asset"),
                Path::new("Assets/Z.asset"),
            ])
            .unwrap();
        let actual = paths
            .iter()
            .map(|path| path.as_relative_path().to_path_buf())
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            ["Assets/A.asset", "Assets/Z.asset"]
                .into_iter()
                .map(Into::into)
                .collect::<Vec<std::path::PathBuf>>()
        );

        let other = ProjectPathSpace::new(space.root().to_path_buf(), project_id(2)).unwrap();
        let foreign = other.resolve(Path::new("Assets/B.asset")).unwrap().unwrap();
        let mut paths = paths;
        assert!(matches!(
            paths.insert(foreign),
            Err(ProjectPathError::DifferentProject { .. })
        ));
    }

    #[test]
    fn prefix_matching_respects_component_boundaries() {
        let space = space();
        let prefix = space.resolve(Path::new("Assets/Foo")).unwrap().unwrap();
        let child = space
            .resolve(Path::new("Assets/Foo/Child.asset"))
            .unwrap()
            .unwrap();
        let sibling = space
            .resolve(Path::new("Assets/Foobar.asset"))
            .unwrap()
            .unwrap();

        assert!(prefix.is_at_or_below(&prefix));
        assert!(child.is_at_or_below(&prefix));
        assert!(!sibling.is_at_or_below(&prefix));
    }

    #[cfg(windows)]
    #[test]
    fn ordinal_comparison_handles_long_components_without_allocation() {
        let upper = OsString::from("A".repeat(257));
        let lower = OsString::from("a".repeat(257));

        assert_eq!(windows_component_cmp(&upper, &lower), Ordering::Equal);
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_key_order_matches_ordinal_ignore_case() {
        use std::ffi::OsStr;

        let space = space();
        let names = [
            "A", "a", "Ä", "ä", "\u{0526}", "\u{0527}", "\u{13A1}", "\u{AB71}", "ß", "ẞ", "I", "i",
            "İ", "ı", "K", "K", "S", "ſ", "Σ", "σ", "ς",
        ];
        for left in names {
            let left_path = space.resolve(Path::new(left)).unwrap().unwrap();
            for right in names {
                let right_path = space.resolve(Path::new(right)).unwrap().unwrap();
                let expected = native_windows_component_cmp(OsStr::new(left), OsStr::new(right));
                assert_eq!(
                    left_path.cmp(&right_path),
                    expected,
                    "project path key order differs for {left:?} and {right:?}"
                );
                assert_eq!(
                    left_path == right_path,
                    expected == Ordering::Equal,
                    "project path equality differs for {left:?} and {right:?}"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_ordinal_key_matches_compare_string_ordinal_for_all_code_units() {
        use windows_sys::Win32::Globalization::{
            CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN, CompareStringOrdinal,
        };

        let mut units = (0_u16..=u16::MAX).collect::<Vec<_>>();
        units.sort_unstable_by_key(|unit| (windows_ordinal_upcase(*unit), *unit));
        for pair in units.windows(2) {
            let [left, right] = pair else {
                unreachable!("windows(2) always returns two elements")
            };
            // SAFETY: both pointers refer to one live UTF-16 code unit for the full call.
            let expected = unsafe { CompareStringOrdinal(left, 1, right, 1, 1) };
            let expected = match expected {
                CSTR_LESS_THAN => Ordering::Less,
                CSTR_EQUAL => Ordering::Equal,
                CSTR_GREATER_THAN => Ordering::Greater,
                value => panic!("CompareStringOrdinal returned {value} for {left:#06x}"),
            };
            assert_eq!(
                windows_ordinal_upcase(*left).cmp(&windows_ordinal_upcase(*right)),
                expected,
                "ordinal key differs for {left:#06x} and {right:#06x}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_verbatim_and_case_aliases_share_path_identity() {
        let space = space();
        let ordinary = space
            .resolve(Path::new(r"c:\project\assets\Hero.prefab"))
            .unwrap()
            .unwrap();
        let verbatim = space
            .resolve(Path::new(r"\\?\C:\PROJECT\Assets\hero.PREFAB"))
            .unwrap()
            .unwrap();

        assert_eq!(ordinary, verbatim);
        assert_eq!(ordinary.identity(), verbatim.identity());
        assert_eq!(
            space
                .resolve_set([ordinary.as_relative_path()])
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_and_verbatim_unc_aliases_share_path_identity() {
        let space = ProjectPathSpace::new(r"\\Server\Share\Project".into(), project_id(3)).unwrap();
        let ordinary = space
            .resolve(Path::new(r"\\server\share\project\Assets\Hero.prefab"))
            .unwrap()
            .unwrap();
        let verbatim = space
            .resolve(Path::new(
                r"\\?\UNC\SERVER\SHARE\PROJECT\assets\hero.PREFAB",
            ))
            .unwrap()
            .unwrap();

        assert_eq!(ordinary, verbatim);
        assert_eq!(ordinary.identity(), verbatim.identity());
        assert!(matches!(
            space.resolve(Path::new(r"\\server\other\project\Assets\Hero.prefab")),
            Err(ProjectPathError::OutsideProject { .. })
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_case_variants_are_distinct_coordinates() {
        let space = space();
        let upper = space
            .resolve(Path::new("Assets/Hero.asset"))
            .unwrap()
            .unwrap();
        let lower = space
            .resolve(Path::new("assets/hero.asset"))
            .unwrap()
            .unwrap();

        assert_ne!(upper, lower);
        assert_ne!(upper.identity(), lower.identity());
    }
}
