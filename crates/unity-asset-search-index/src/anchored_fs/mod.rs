//! Identity-bound, no-follow filesystem access rooted in opened directory handles.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, BorrowedHandle};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use unix as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;
#[cfg(all(test, windows))]
pub(crate) use windows::try_enable_case_sensitive_directory_for_test;

#[cfg(all(test, windows))]
pub(crate) fn case_sensitivity_test_is_unsupported(error: &io::Error) -> bool {
    // Older Windows versions, filesystems without per-directory case sensitivity, and callers
    // without the required privilege may legitimately reject this test-only setup operation.
    matches!(error.raw_os_error(), Some(1 | 5 | 50 | 87 | 1_314))
}

/// A security-relevant failure while opening or validating an anchored path.
#[derive(Debug)]
pub(crate) enum AnchoredFsError {
    Io(io::Error),
    #[cfg_attr(
        any(target_os = "linux", target_os = "macos", windows),
        allow(dead_code)
    )]
    UnsupportedPlatform,
    LinkOrReparse,
    UnsupportedCaseSensitiveDirectory,
    NotDirectory,
    NotRegular,
    IdentityChanged,
}

impl std::fmt::Display for AnchoredFsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::UnsupportedPlatform => formatter
                .write_str("identity-bound filesystem access is unsupported on this platform"),
            Self::LinkOrReparse => {
                formatter.write_str("anchored path contains a symbolic link or reparse point")
            }
            Self::UnsupportedCaseSensitiveDirectory => formatter
                .write_str("per-directory case-sensitive Windows directories are unsupported"),
            Self::NotDirectory => formatter.write_str("anchored path is not a directory"),
            Self::NotRegular => formatter.write_str("anchored entry is not a regular file"),
            Self::IdentityChanged => formatter
                .write_str("anchored file identity, link count, or length is unsafe or changed"),
        }
    }
}

impl std::error::Error for AnchoredFsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::UnsupportedPlatform
            | Self::LinkOrReparse
            | Self::UnsupportedCaseSensitiveDirectory
            | Self::NotDirectory
            | Self::NotRegular
            | Self::IdentityChanged => None,
        }
    }
}

impl From<io::Error> for AnchoredFsError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// Security and sharing semantics for files opened below an anchored directory.
///
/// Persisted state is immutable once published. Project sources are live inputs and allow
/// concurrent replacement. Both authority-bearing policies reject hard-link aliases so an
/// anchored project or generation root remains a real content boundary. `RecoveryAlias` is a
/// private repair capability used only to compare the two exact names of an interrupted
/// hard-link publication before restoring the single-link invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenPolicy {
    PersistedState,
    ProjectSource,
    RecoveryAlias,
}

impl OpenPolicy {
    pub(super) const fn requires_single_link(self) -> bool {
        !matches!(self, Self::RecoveryAlias)
    }

    #[cfg(windows)]
    pub(super) const fn allows_concurrent_replacement(self) -> bool {
        match self {
            Self::PersistedState | Self::RecoveryAlias => false,
            Self::ProjectSource => true,
        }
    }
}

/// An untrusted type observation returned by directory enumeration.
///
/// The caller must use a relative no-follow open before treating this hint as authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKindHint {
    Directory,
    RegularFile,
    LinkOrReparse,
    #[cfg_attr(windows, allow(dead_code))]
    Other,
    Unknown,
}

/// One untrusted child name and type hint from a handle-relative directory enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryEntryHint {
    name: OsString,
    kind: EntryKindHint,
}

impl DirectoryEntryHint {
    pub(super) const fn new(name: OsString, kind: EntryKindHint) -> Self {
        Self { name, kind }
    }

    #[must_use]
    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> EntryKindHint {
        self.kind
    }

    /// Heap capacity retained by the owned entry name.
    #[must_use]
    pub(crate) fn name_capacity(&self) -> usize {
        self.name.capacity()
    }

    #[must_use]
    pub(crate) fn into_name(self) -> OsString {
        self.name
    }
}

/// One already-opened directory used as the authority for descendant lookups.
pub(crate) struct ReadDirectory {
    inner: platform::ReadDirectory,
    policy: OpenPolicy,
}

impl ReadDirectory {
    pub(crate) fn open(path: &Path, policy: OpenPolicy) -> Result<Self, AnchoredFsError> {
        if !path.is_absolute() {
            return Err(invalid_relative_path(
                "anchored root directory path must be absolute",
            ));
        }
        let inner = platform::open_directory(path, policy)?;
        Ok(Self { inner, policy })
    }

    /// Opens a non-empty, multi-component relative directory without following links.
    pub(crate) fn open_directory<P>(&self, relative: &P) -> Result<Self, AnchoredFsError>
    where
        P: AsRef<OsStr> + ?Sized,
    {
        let relative = Path::new(relative.as_ref());
        let mut current = None;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid_relative_path(
                    "anchored relative directory contains an escaping component",
                ));
            };
            let parent = current.as_ref().unwrap_or(&self.inner);
            current = Some(platform::open_directory_at(parent, name, self.policy)?);
        }
        let inner = current
            .ok_or_else(|| invalid_relative_path("anchored relative directory path is empty"))?;
        Ok(Self {
            inner,
            policy: self.policy,
        })
    }

    /// Opens a non-empty, multi-component relative regular file without following links.
    pub(crate) fn open_regular<P>(&self, relative: &P) -> Result<RegularFile, AnchoredFsError>
    where
        P: AsRef<OsStr> + ?Sized,
    {
        let relative = Path::new(relative.as_ref());
        let mut components = relative.components().peekable();
        if components.peek().is_none() {
            return Err(invalid_relative_path(
                "anchored relative regular-file path is empty",
            ));
        }

        let mut current = None;
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(invalid_relative_path(
                    "anchored relative regular-file path contains an escaping component",
                ));
            };
            let parent = current.as_ref().unwrap_or(&self.inner);
            if components.peek().is_none() {
                let (file, identity) = platform::open_regular_at(parent, name, self.policy)?;
                return Ok(RegularFile {
                    file,
                    length: identity.length(),
                    identity,
                    policy: self.policy,
                });
            }
            current = Some(platform::open_directory_at(parent, name, self.policy)?);
        }
        Err(invalid_relative_path(
            "anchored relative regular-file path has no leaf",
        ))
    }

    /// Validates the anchored lookup semantics for every parent of a relative leaf path.
    ///
    /// The leaf is intentionally not opened. Workspace sources can retain authoritative bytes
    /// after their original file disappears, but a project coordinate is only valid when its
    /// parent namespace still follows this authority's no-link and case-equivalence contract.
    pub(crate) fn validate_parent_lookup<P>(&self, relative: &P) -> Result<(), AnchoredFsError>
    where
        P: AsRef<OsStr> + ?Sized,
    {
        let relative = Path::new(relative.as_ref());
        let mut components = relative.components().peekable();
        if components.peek().is_none() {
            return Err(invalid_relative_path(
                "anchored relative leaf path is empty",
            ));
        }

        let mut current = None;
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(invalid_relative_path(
                    "anchored relative leaf path contains an escaping component",
                ));
            };
            let parent = current.as_ref().unwrap_or(self);
            if components.peek().is_none() {
                parent.stable_identity()?;
                return Ok(());
            }
            current = Some(parent.open_directory(name)?);
        }
        Err(invalid_relative_path(
            "anchored relative leaf path has no leaf",
        ))
    }

    /// Enumerates this exact opened directory handle.
    ///
    /// Entries are intentionally unsorted and unbudgeted. The caller owns both ledgers and must
    /// treat names and types as discovery hints until a relative open succeeds.
    pub(crate) fn entries(&self) -> Result<DirectoryEntries<'_>, AnchoredFsError> {
        platform::read_directory(&self.inner, self.policy).map(DirectoryEntries)
    }

    /// Enumerates only child names from this exact opened directory handle.
    ///
    /// This is the preflight form: callers that do not consume type hints avoid platform metadata
    /// lookups while retaining the same handle-relative namespace boundary.
    pub(crate) fn entry_names(&self) -> Result<DirectoryNames<'_>, AnchoredFsError> {
        platform::read_directory_names(&self.inner, self.policy).map(DirectoryNames)
    }

    /// Captures this exact directory handle's namespace identity and metadata version.
    pub(crate) fn stable_identity(&self) -> Result<StableDirectoryIdentity, AnchoredFsError> {
        platform::opened_directory_identity(&self.inner).map(StableDirectoryIdentity)
    }

    /// Captures only this directory object's stable filesystem identity.
    ///
    /// Unlike [`Self::stable_identity`], this token intentionally excludes the directory's
    /// metadata version so it remains valid while legitimate children are added or removed.
    pub(crate) fn object_identity(&self) -> Result<StableDirectoryObjectIdentity, AnchoredFsError> {
        platform::opened_directory_object_identity(&self.inner).map(StableDirectoryObjectIdentity)
    }

    /// Revalidates this handle against a prior handle-derived directory snapshot.
    pub(crate) fn same_identity(
        &self,
        expected: StableDirectoryIdentity,
    ) -> Result<bool, AnchoredFsError> {
        self.stable_identity().map(|actual| actual == expected)
    }

    pub(crate) fn ensure_identity(
        &self,
        expected: StableDirectoryIdentity,
    ) -> Result<(), AnchoredFsError> {
        if self.same_identity(expected)? {
            Ok(())
        } else {
            Err(AnchoredFsError::IdentityChanged)
        }
    }

    pub(crate) fn same_object(
        &self,
        expected: StableDirectoryObjectIdentity,
    ) -> Result<bool, AnchoredFsError> {
        self.object_identity().map(|actual| actual == expected)
    }

    pub(crate) fn ensure_object(
        &self,
        expected: StableDirectoryObjectIdentity,
    ) -> Result<(), AnchoredFsError> {
        if self.same_object(expected)? {
            Ok(())
        } else {
            Err(AnchoredFsError::IdentityChanged)
        }
    }
}

impl std::fmt::Debug for ReadDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadDirectory")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl AsFd for ReadDirectory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.as_fd()
    }
}

#[cfg(windows)]
impl AsHandle for ReadDirectory {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.inner.as_handle()
    }
}

/// A streaming, handle-relative directory iterator.
pub(crate) struct DirectoryEntries<'directory>(platform::DirectoryEntries<'directory>);

impl Iterator for DirectoryEntries<'_> {
    type Item = Result<DirectoryEntryHint, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl std::fmt::Debug for DirectoryEntries<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DirectoryEntries(..)")
    }
}

/// A streaming, handle-relative iterator that retains only directory entry names.
pub(crate) struct DirectoryNames<'directory>(platform::DirectoryNames<'directory>);

impl Iterator for DirectoryNames<'_> {
    type Item = Result<OsString, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

impl std::fmt::Debug for DirectoryNames<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DirectoryNames(..)")
    }
}

/// An opaque, process-local snapshot of one directory's identity and metadata version.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct StableDirectoryIdentity(platform::DirectoryIdentity);

impl std::fmt::Debug for StableDirectoryIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StableDirectoryIdentity(..)")
    }
}

/// An opaque, process-local identity for one directory object across metadata changes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct StableDirectoryObjectIdentity(platform::DirectoryObjectIdentity);

impl std::fmt::Debug for StableDirectoryObjectIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StableDirectoryObjectIdentity(..)")
    }
}

/// An opaque, process-local snapshot of one regular file's identity and metadata version.
///
/// The token is intentionally not serializable. It only compares two handle-derived snapshots
/// during one scan or persisted-state operation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct StableFileIdentity(platform::FileIdentity);

impl std::fmt::Debug for StableFileIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StableFileIdentity(..)")
    }
}

/// A regular file whose identity and length came from this exact open handle.
pub(crate) struct RegularFile {
    file: File,
    identity: platform::FileIdentity,
    length: u64,
    policy: OpenPolicy,
}

impl RegularFile {
    #[must_use]
    pub(crate) const fn length(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub(crate) const fn stable_identity(&self) -> StableFileIdentity {
        StableFileIdentity(self.identity)
    }

    /// Revalidates this handle and compares it with a prior handle-derived snapshot.
    pub(crate) fn same_identity(
        &self,
        expected: StableFileIdentity,
    ) -> Result<bool, AnchoredFsError> {
        let actual = platform::opened_file_identity(&self.file, self.policy)?;
        Ok(actual == expected.0)
    }

    /// Provides cursor-based compatibility for existing persisted readers.
    ///
    /// Callers must invoke [`Self::ensure_unchanged`] after consuming bytes.
    pub(crate) const fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn ensure_unchanged(&self) -> Result<(), AnchoredFsError> {
        if self.same_identity(self.stable_identity())? {
            Ok(())
        } else {
            Err(AnchoredFsError::IdentityChanged)
        }
    }

    /// Reads one exact positional range and revalidates identity and length afterwards.
    pub(crate) fn read_exact_at(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> Result<(), AnchoredFsError> {
        let length = u64::try_from(output.len()).map_err(|_| {
            AnchoredFsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored range length exceeds u64",
            ))
        })?;
        let mut range = self.range(offset, length)?;
        io::Read::read_exact(&mut range, output)?;
        self.ensure_unchanged()
    }

    pub(crate) fn range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<RegularFileRange<'_>, AnchoredFsError> {
        self.ensure_unchanged()?;
        let end = offset.checked_add(length).ok_or_else(|| {
            AnchoredFsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("anchored file range offset {offset} plus length {length} overflows u64"),
            ))
        })?;
        if end > self.length {
            return Err(AnchoredFsError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "anchored file range {offset}+{length} exceeds the opened file length {}",
                    self.length
                ),
            )));
        }
        Ok(RegularFileRange {
            file: self,
            next_offset: offset,
            remaining: length,
        })
    }
}

impl std::fmt::Debug for RegularFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RegularFile")
            .field("length", &self.length)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// A bounded positional view over an already-opened regular file.
#[derive(Debug)]
pub(crate) struct RegularFileRange<'file> {
    file: &'file RegularFile,
    next_offset: u64,
    remaining: u64,
}

impl io::Read for RegularFileRange<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.remaining == 0 {
            return Ok(0);
        }
        let buffer_length = u64::try_from(buffer.len())
            .map_err(|_| io::Error::other("read buffer length exceeds u64"))?;
        let requested = usize::try_from(self.remaining.min(buffer_length))
            .map_err(|_| io::Error::other("bounded read length exceeds usize"))?;
        let read = platform::read_at(&self.file.file, &mut buffer[..requested], self.next_offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "opened regular file ended before its validated range",
            ));
        }
        let read_u64 =
            u64::try_from(read).map_err(|_| io::Error::other("read byte count exceeds u64"))?;
        self.next_offset = self
            .next_offset
            .checked_add(read_u64)
            .ok_or_else(|| io::Error::other("positional read offset overflows u64"))?;
        self.remaining = self
            .remaining
            .checked_sub(read_u64)
            .ok_or_else(|| io::Error::other("positional read exceeded its validated range"))?;
        Ok(read)
    }
}

fn invalid_relative_path(message: &'static str) -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(io::ErrorKind::InvalidInput, message))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::{AnchoredFsError, EntryKindHint, OpenPolicy, ReadDirectory};
    #[cfg(windows)]
    use super::{
        case_sensitivity_test_is_unsupported, try_enable_case_sensitive_directory_for_test,
    };

    #[cfg(windows)]
    fn enable_case_sensitivity_or_skip(path: &Path) -> bool {
        match try_enable_case_sensitive_directory_for_test(path) {
            Ok(()) => true,
            Err(error) if case_sensitivity_test_is_unsupported(&error) => {
                eprintln!(
                    "skipping per-directory case-sensitivity contract test for {}: {error}",
                    path.display()
                );
                false
            }
            Err(error) => panic!(
                "unexpected failure enabling per-directory case sensitivity for {}: {error}",
                path.display()
            ),
        }
    }

    #[test]
    fn opens_multi_component_descendants_from_the_root_handle() {
        let temporary = tempdir().unwrap();
        let nested = temporary.path().join("one").join("two");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("asset.bin"), b"asset bytes").unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        root.open_directory(Path::new("one/two")).unwrap();
        let file = root.open_regular(Path::new("one/two/asset.bin")).unwrap();
        let mut bytes = [0_u8; 5];
        file.read_exact_at(6, &mut bytes).unwrap();

        assert_eq!(&bytes, b"bytes");
    }

    #[test]
    fn validates_parent_lookup_without_requiring_the_leaf() {
        let temporary = tempdir().unwrap();
        fs::create_dir_all(temporary.path().join("one/two")).unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        root.validate_parent_lookup(Path::new("one/two/missing.asset"))
            .unwrap();

        let error = root
            .validate_parent_lookup(Path::new("one/missing/asset.bin"))
            .unwrap_err();
        assert!(matches!(error, AnchoredFsError::Io(_)));
    }

    #[test]
    fn rejects_empty_absolute_and_escaping_relative_paths() {
        let temporary = tempdir().unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        for path in [Path::new(""), Path::new("."), Path::new("../asset.bin")] {
            let error = root.open_regular(path).unwrap_err();
            assert!(matches!(error, AnchoredFsError::Io(_)));
        }
        let absolute = temporary.path().join("asset.bin");
        let error = root.open_regular(&absolute).unwrap_err();
        assert!(matches!(error, AnchoredFsError::Io(_)));
    }

    #[test]
    fn rejects_a_relative_root_on_every_platform() {
        let error = ReadDirectory::open(Path::new("."), OpenPolicy::ProjectSource).unwrap_err();

        assert!(matches!(error, AnchoredFsError::Io(_)));
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_windows_directories_support_opening_and_enumeration() {
        let temporary = tempdir().unwrap();
        fs::create_dir(temporary.path().join("Assets")).unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        root.open_directory(OsStr::new("Assets")).unwrap();
        let entries = root
            .entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name(), OsStr::new("Assets"));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_case_sensitive_windows_root_when_supported() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("project");
        fs::create_dir(&root).unwrap();
        if !enable_case_sensitivity_or_skip(&root) {
            return;
        }

        let error = ReadDirectory::open(&root, OpenPolicy::ProjectSource).unwrap_err();

        assert!(matches!(
            error,
            AnchoredFsError::UnsupportedCaseSensitiveDirectory
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_case_sensitive_windows_scan_root_ancestor_when_supported() {
        let temporary = tempdir().unwrap();
        let scan_root_ancestor = temporary.path().join("Assets");
        fs::create_dir(&scan_root_ancestor).unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();
        if !enable_case_sensitivity_or_skip(&scan_root_ancestor) {
            return;
        }
        fs::create_dir(scan_root_ancestor.join("Nested")).unwrap();

        let error = root.open_directory(Path::new("Assets/Nested")).unwrap_err();

        assert!(matches!(
            error,
            AnchoredFsError::UnsupportedCaseSensitiveDirectory
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rechecks_windows_case_sensitivity_before_enumeration_when_supported() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("Assets");
        fs::create_dir(&path).unwrap();
        let directory = ReadDirectory::open(&path, OpenPolicy::ProjectSource).unwrap();
        if !enable_case_sensitivity_or_skip(&path) {
            return;
        }

        let error = directory.entries().unwrap_err();

        assert!(matches!(
            error,
            AnchoredFsError::UnsupportedCaseSensitiveDirectory
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rechecks_windows_parent_case_sensitivity_before_opening_a_regular_file_when_supported() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("Assets");
        fs::create_dir(&path).unwrap();
        let directory = ReadDirectory::open(&path, OpenPolicy::ProjectSource).unwrap();
        if !enable_case_sensitivity_or_skip(&path) {
            return;
        }
        fs::write(path.join("Example.asset"), b"asset").unwrap();

        let error = directory
            .open_regular(Path::new("Example.asset"))
            .unwrap_err();

        assert!(matches!(
            error,
            AnchoredFsError::UnsupportedCaseSensitiveDirectory
        ));
    }

    #[test]
    fn directory_entries_are_unsorted_hints_until_reopened() {
        let temporary = tempdir().unwrap();
        fs::create_dir(temporary.path().join("directory")).unwrap();
        fs::write(temporary.path().join("file.bin"), b"bytes").unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        let mut entries = root
            .entries()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_unstable_by(|left, right| left.name().cmp(right.name()));

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name(), OsStr::new("directory"));
        assert_eq!(entries[0].kind(), EntryKindHint::Directory);
        assert_eq!(entries[1].name(), OsStr::new("file.bin"));
        assert_eq!(entries[1].kind(), EntryKindHint::RegularFile);
        root.open_directory(entries[0].name()).unwrap();
        root.open_regular(entries[1].name()).unwrap();
    }

    #[test]
    fn directory_name_preflight_matches_full_enumeration() {
        let temporary = tempdir().unwrap();
        fs::create_dir(temporary.path().join("directory")).unwrap();
        fs::write(temporary.path().join("file.bin"), b"bytes").unwrap();
        let root = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();

        let mut names = root
            .entry_names()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        names.sort_unstable();

        assert_eq!(
            names,
            [OsString::from("directory"), OsString::from("file.bin")]
        );
    }

    #[test]
    fn reopened_directory_identity_detects_namespace_replacement() {
        let temporary = tempdir().unwrap();
        let active = temporary.path().join("active");
        let replacement = temporary.path().join("replacement");
        let displaced = temporary.path().join("displaced");
        fs::create_dir(&active).unwrap();
        fs::create_dir(&replacement).unwrap();
        let opened = ReadDirectory::open(&active, OpenPolicy::ProjectSource).unwrap();
        let expected = opened.stable_identity().unwrap();

        fs::rename(&active, &displaced).unwrap();
        fs::rename(&replacement, &active).unwrap();

        let reopened = ReadDirectory::open(&active, OpenPolicy::ProjectSource).unwrap();
        assert!(!reopened.same_identity(expected).unwrap());
    }

    #[test]
    fn every_open_policy_rejects_hard_link_aliases() {
        let temporary = tempdir().unwrap();
        fs::write(temporary.path().join("state.json"), b"original").unwrap();
        fs::hard_link(
            temporary.path().join("state.json"),
            temporary.path().join("alias.json"),
        )
        .unwrap();

        let persisted = ReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap();
        let error = persisted
            .open_regular(OsStr::new("state.json"))
            .unwrap_err();
        assert!(matches!(error, AnchoredFsError::IdentityChanged));

        let project = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();
        let error = project.open_regular(OsStr::new("state.json")).unwrap_err();
        assert!(matches!(error, AnchoredFsError::IdentityChanged));
    }

    #[test]
    fn recovery_alias_compares_exact_hard_link_names_without_weakening_authority_policies() {
        let temporary = tempdir().unwrap();
        fs::write(temporary.path().join("staging.json"), b"original").unwrap();
        fs::hard_link(
            temporary.path().join("staging.json"),
            temporary.path().join("committed.json"),
        )
        .unwrap();

        let recovery = ReadDirectory::open(temporary.path(), OpenPolicy::RecoveryAlias).unwrap();
        let staging = recovery.open_regular(OsStr::new("staging.json")).unwrap();
        let committed = recovery.open_regular(OsStr::new("committed.json")).unwrap();

        assert!(committed.same_identity(staging.stable_identity()).unwrap());
        staging.ensure_unchanged().unwrap();
    }

    #[test]
    fn range_rejects_a_file_truncated_after_open() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("asset.bin");
        fs::write(&path, b"original").unwrap();
        let directory = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();
        let opened = directory.open_regular(OsStr::new("asset.bin")).unwrap();

        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(4)
            .unwrap();
        let error = opened.range(0, 4).unwrap_err();

        assert!(matches!(error, AnchoredFsError::IdentityChanged));
    }

    #[test]
    fn stable_identity_detects_same_length_in_place_modification() {
        use std::fs::FileTimes;
        use std::time::{Duration, UNIX_EPOCH};

        let temporary = tempdir().unwrap();
        let path = temporary.path().join("asset.bin");
        fs::write(&path, b"original").unwrap();
        let directory = ReadDirectory::open(temporary.path(), OpenPolicy::ProjectSource).unwrap();
        let opened = directory.open_regular(OsStr::new("asset.bin")).unwrap();
        let original_identity = opened.stable_identity();
        assert!(opened.same_identity(original_identity).unwrap());

        fs::write(&path, b"modified").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(946_684_800)))
            .unwrap();

        assert!(!opened.same_identity(original_identity).unwrap());
        assert!(matches!(
            opened.ensure_unchanged(),
            Err(AnchoredFsError::IdentityChanged)
        ));
        let reopened = directory.open_regular(OsStr::new("asset.bin")).unwrap();
        assert_ne!(reopened.stable_identity(), original_identity);
    }

    #[cfg(unix)]
    #[test]
    fn pinned_parent_cannot_be_redirected_before_leaf_open() {
        use std::io::Read as _;

        let temporary = tempdir().unwrap();
        let parent = temporary.path().join("activations");
        let replacement = temporary.path().join("replacement");
        let displaced = temporary.path().join("displaced");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(parent.join("head.json"), b"original").unwrap();
        fs::write(replacement.join("head.json"), b"replacement").unwrap();
        let directory = ReadDirectory::open(&parent, OpenPolicy::PersistedState).unwrap();

        fs::rename(&parent, &displaced).unwrap();
        fs::rename(&replacement, &parent).unwrap();
        let mut opened = directory.open_regular(OsStr::new("head.json")).unwrap();
        let mut actual = String::new();
        opened.file_mut().read_to_string(&mut actual).unwrap();

        assert_eq!(actual, "original");
        opened.ensure_unchanged().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symbolic_link_leaf() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        fs::write(temporary.path().join("target.json"), b"{}").unwrap();
        symlink("target.json", temporary.path().join("state.json")).unwrap();
        let directory = ReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap();

        let error = directory
            .open_regular(OsStr::new("state.json"))
            .unwrap_err();
        assert!(matches!(error, AnchoredFsError::LinkOrReparse));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn rejects_fifo_without_waiting_for_a_writer() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        use rustix::fs::{CWD, Mode, mkfifoat};

        let temporary = tempdir().unwrap();
        let fifo = temporary.path().join("state.json");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let directory = ReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            sender
                .send(directory.open_regular(OsStr::new("state.json")))
                .unwrap();
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("no-follow regular-file validation blocked on a FIFO");
        worker.join().unwrap();
        assert!(matches!(result, Err(AnchoredFsError::NotRegular)));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_unix_socket_as_a_non_regular_entry() {
        use std::os::unix::net::UnixListener;

        let temporary = tempdir().unwrap();
        let socket = temporary.path().join("state.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let directory = ReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap();

        let error = directory
            .open_regular(OsStr::new("state.sock"))
            .unwrap_err();

        assert!(matches!(error, AnchoredFsError::NotRegular));
    }
}
