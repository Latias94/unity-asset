//! Unix publication primitives.

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
use std::ffi::CStr;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Seek as _, SeekFrom};
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::mem::MaybeUninit;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
use std::os::fd::AsRawFd as _;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};

use rustix::fs::CWD;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rustix::fs::RawDir;
use rustix::fs::{
    AtFlags, FileType, FlockOperation, Gid, Mode, OFlags, Stat, Uid, fchmod, fchown, flock, fstat,
    fsync, mkdirat, openat, renameat, statat, unlinkat,
};
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
use rustix::fs::{RenameFlags, renameat_with};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use unity_asset_core::{AssetLoadBudget, DigestV1};

use super::super::journal::{RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY};
use super::{
    COMMIT_LOCK_FILE, DirectoryEntryName, DirectoryVisitError, LEGACY_COMMIT_LOCK_DIRECTORY,
    SecurityMetadataError,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) const DIRECTORY_VISIT_SETUP_BYTES: u64 = 0;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = 0;
pub(super) const SECURITY_METADATA_COPY_RESERVATION_BYTES: u64 = 0;

// `fdopendir` owns one opaque C iterator buffer on Darwin/BSD. The entry
// names themselves remain borrowed from `readdir`, so one bounded setup
// reservation covers the only opaque allocation.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
pub(super) const DIRECTORY_VISIT_SETUP_BYTES: u64 = 64 * 1024;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
pub(super) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = 0;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub(super) const DIRECTORY_VISIT_SETUP_BYTES: u64 = 0;
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub(super) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = 0;

const DIRECTORY_FLAGS: OFlags = OFlags::from_bits_retain(
    OFlags::RDONLY.bits()
        | OFlags::DIRECTORY.bits()
        | OFlags::NOFOLLOW.bits()
        | OFlags::CLOEXEC.bits(),
);
const REGULAR_FILE_FLAGS: OFlags = OFlags::from_bits_retain(
    OFlags::RDONLY.bits() | OFlags::NOFOLLOW.bits() | OFlags::CLOEXEC.bits(),
);

/// Stable Unix identity captured from an opened publication source.
///
/// The token deliberately excludes paths. A later irreversible rename must
/// validate every field against a freshly opened, no-follow source handle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileIdentity {
    #[serde(with = "super::hex_u64")]
    device: u64,
    #[serde(with = "super::hex_u64")]
    inode: u64,
    #[serde(with = "super::hex_u64")]
    length: u64,
}

impl FileIdentity {
    #[must_use]
    pub(super) const fn new(device: u64, inode: u64, length: u64) -> Self {
        Self {
            device,
            inode,
            length,
        }
    }

    #[must_use]
    pub(super) const fn length(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub(super) fn same_object(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
}

/// Stable Unix identity captured from an opened publication directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryIdentity {
    #[serde(with = "super::hex_u64")]
    device: u64,
    #[serde(with = "super::hex_u64")]
    inode: u64,
}

impl DirectoryIdentity {
    #[must_use]
    pub(super) const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }
}

pub(super) struct CommitRoot {
    directory: OwnedFd,
}

pub(super) struct JournalNamespace {
    recovery: OwnedFd,
    version: OwnedFd,
}

pub(super) struct JournalDirectory {
    directory: OwnedFd,
}

pub(super) fn open_commit_root(
    path: &Path,
    expected: &DirectoryIdentity,
) -> io::Result<CommitRoot> {
    let directory = open_directory(path)?;
    validate_expected_directory_identity(
        &directory,
        expected,
        "publication root changed before writer lock acquisition",
    )?;
    Ok(CommitRoot { directory })
}

pub(super) fn acquire_commit_locks(root: &CommitRoot) -> io::Result<(File, File)> {
    let recovery =
        open_or_create_private_directory_at(&root.directory, OsStr::new(RECOVERY_DIRECTORY))?;
    let legacy =
        open_or_create_private_directory_at(&recovery, OsStr::new(LEGACY_COMMIT_LOCK_DIRECTORY))?;

    // Journal v1 placed its only writer lock under v1/. Newer protocols
    // retain that lock and also take a version-independent lock so old and
    // future binaries cannot publish concurrently into the same root.
    let legacy_file = acquire_lock_at(&legacy, OsStr::new(COMMIT_LOCK_FILE))?;
    let stable_file = acquire_lock_at(&recovery, OsStr::new(COMMIT_LOCK_FILE))?;
    Ok((legacy_file, stable_file))
}

pub(super) fn open_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    let recovery =
        open_or_create_private_directory_at(&root.directory, OsStr::new(RECOVERY_DIRECTORY))?;
    let version =
        open_or_create_private_directory_at(&recovery, OsStr::new(RECOVERY_VERSION_DIRECTORY))?;
    Ok(JournalNamespace { recovery, version })
}

pub(super) fn open_existing_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    let recovery =
        open_existing_private_directory_at(&root.directory, OsStr::new(RECOVERY_DIRECTORY))?;
    let version =
        open_existing_private_directory_at(&recovery, OsStr::new(RECOVERY_VERSION_DIRECTORY))?;
    Ok(JournalNamespace { recovery, version })
}

pub(super) fn open_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    open_journal_directory_at(&namespace.version, name)
}

pub(super) fn open_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    open_journal_directory_at(&parent.directory, name)
}

pub(super) fn create_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    create_journal_directory_at(&namespace.version, name)
}

pub(super) fn create_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    create_journal_directory_at(&parent.directory, name)
}

pub(super) fn journal_directory_identity(
    directory: &JournalDirectory,
) -> io::Result<DirectoryIdentity> {
    opened_directory_identity(&directory.directory)
}

pub(super) fn open_journal_regular(namespace: &JournalNamespace, name: &OsStr) -> io::Result<File> {
    open_journal_regular_at(&namespace.version, name)
}

pub(super) fn open_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<File> {
    open_journal_regular_at(&parent.directory, name)
}

pub(super) fn create_journal_regular(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<File> {
    create_journal_regular_at(&namespace.version, name)
}

pub(super) fn create_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<File> {
    create_journal_regular_at(&parent.directory, name)
}

pub(super) fn remove_journal_regular(
    namespace: &JournalNamespace,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    remove_journal_regular_at(&namespace.version, name, expected)
}

pub(super) fn remove_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    remove_journal_regular_at(&parent.directory, name, expected)
}

pub(super) fn remove_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    remove_journal_directory_at(&namespace.version, name, expected)
}

pub(super) fn remove_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    remove_journal_directory_at(&parent.directory, name, expected)
}

pub(super) fn atomic_replace_journal_regular(
    namespace: &JournalNamespace,
    source: &OsStr,
    destination: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_journal_regular_at(&namespace.version, source, destination, replace_existing)
}

pub(super) fn atomic_replace_journal_regular_in_directory(
    parent: &JournalDirectory,
    source: &OsStr,
    destination: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_journal_regular_at(&parent.directory, source, destination, replace_existing)
}

pub(super) fn sync_journal_directory(directory: &JournalDirectory) -> io::Result<()> {
    fsync(&directory.directory).map_err(io::Error::from)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn visit_journal_directory_entries<S, E>(
    directory: &JournalDirectory,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;

    let mut buffer = [MaybeUninit::<u8>::uninit(); DIRECTORY_BUFFER_BYTES];
    let mut entries = RawDir::new(&directory.directory, &mut buffer);
    while let Some(entry) = entries.next() {
        before_entry(state).map_err(DirectoryVisitError::Visitor)?;
        let entry = entry
            .map_err(io::Error::from)
            .map_err(DirectoryVisitError::Io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        visitor(state, DirectoryEntryName::Unix(name)).map_err(DirectoryVisitError::Visitor)?;
    }
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
pub(super) fn visit_journal_directory_entries<S, E>(
    directory: &JournalDirectory,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    let mut entries =
        DirectoryStream::from_directory(&directory.directory).map_err(DirectoryVisitError::Io)?;
    loop {
        let entry = entries.next().map_err(DirectoryVisitError::Io)?;
        let Some(entry) = entry else {
            break;
        };
        before_entry(state).map_err(DirectoryVisitError::Visitor)?;
        visitor(state, DirectoryEntryName::Unix(entry)).map_err(DirectoryVisitError::Visitor)?;
    }
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub(super) fn visit_journal_directory_entries<S, E>(
    _: &JournalDirectory,
    _: &mut S,
    _: impl FnMut(&mut S) -> Result<(), E>,
    _: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    Err(DirectoryVisitError::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "opened journal directory enumeration is unsupported on this target",
    )))
}

#[cfg(test)]
pub(super) fn journal_namespace_version_identity(
    namespace: &JournalNamespace,
) -> io::Result<DirectoryIdentity> {
    opened_directory_identity(&namespace.version)
}

pub(super) fn sync_journal_namespace(
    root: &CommitRoot,
    namespace: &JournalNamespace,
) -> io::Result<()> {
    fsync(&namespace.version).map_err(io::Error::from)?;
    fsync(&namespace.recovery).map_err(io::Error::from)?;
    fsync(&root.directory).map_err(io::Error::from)
}

#[cfg(test)]
pub(super) fn create_private_directory(path: &Path) -> io::Result<()> {
    validate_directory_path(path)?;

    let mut directory = open_start_directory(path)?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                match mkdirat(&directory, name, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                directory = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?;
            }
            Component::ParentDir | Component::Prefix(_) => unreachable!(
                "validate_directory_path rejects escaping components before filesystem writes"
            ),
        }
    }

    // mkdirat is affected by umask and an existing recovery directory may be
    // broader. Tighten the final directory through the verified descriptor.
    fchmod(&directory, Mode::RWXU).map_err(io::Error::from)
}

fn open_or_create_private_directory_at(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    let directory =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    // `mkdirat` is affected by umask and an existing recovery directory may
    // be broader. Tighten the opened descriptor, never the re-resolved path.
    fchmod(&directory, Mode::RWXU).map_err(io::Error::from)?;
    Ok(directory)
}

fn open_existing_private_directory_at(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let directory =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    validate_directory_entry(
        parent,
        name,
        &directory,
        "opened journal directory changed during validation",
    )?;
    Ok(directory)
}

fn open_journal_directory_at(parent: &OwnedFd, name: &OsStr) -> io::Result<JournalDirectory> {
    Ok(JournalDirectory {
        directory: open_existing_private_directory_at(parent, name)?,
    })
}

fn create_journal_directory_at(parent: &OwnedFd, name: &OsStr) -> io::Result<JournalDirectory> {
    mkdirat(parent, name, Mode::RWXU).map_err(io::Error::from)?;
    let directory =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    validate_directory_entry(
        parent,
        name,
        &directory,
        "created journal directory changed during validation",
    )?;
    fchmod(&directory, Mode::RWXU).map_err(io::Error::from)?;
    Ok(JournalDirectory { directory })
}

fn open_journal_regular_at(parent: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let descriptor = open_regular_at(parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        parent,
        name,
        &descriptor,
        "opened journal entry changed during validation",
    )?;
    reject_mutable_hardlink(&metadata, "opened journal entry")?;
    Ok(descriptor.into())
}

fn create_journal_regular_at(parent: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let descriptor = openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    validate_regular_entry(
        parent,
        name,
        &descriptor,
        "created journal entry changed during validation",
    )?;
    Ok(descriptor.into())
}

fn remove_journal_regular_at(
    parent: &OwnedFd,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    let descriptor = open_regular_at(parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        parent,
        name,
        &descriptor,
        "owned journal entry changed during deletion validation",
    )?;
    validate_expected_file_identity(
        &metadata,
        expected,
        "owned journal entry no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&metadata, "owned journal entry")?;
    unlinkat(parent, name, AtFlags::empty()).map_err(io::Error::from)?;
    fsync(parent).map_err(io::Error::from)
}

fn remove_journal_directory_at(
    parent: &OwnedFd,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    let directory =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    validate_directory_entry(
        parent,
        name,
        &directory,
        "owned journal directory changed during deletion validation",
    )?;
    validate_expected_directory_identity(
        &directory,
        expected,
        "owned journal directory no longer matches its captured identity",
    )?;
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
    fsync(parent).map_err(io::Error::from)
}

fn atomic_replace_journal_regular_at(
    parent: &OwnedFd,
    source: &OsStr,
    destination: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    let mut moved = false;
    let result = (|| {
        let source_file = open_regular_at(parent, source, REGULAR_FILE_FLAGS)?;
        let opened = validate_regular_entry(
            parent,
            source,
            &source_file,
            "journal temporary entry changed during replacement",
        )?;
        reject_mutable_hardlink(&opened, "journal temporary entry")?;
        let expected_source = identity(&opened);

        if !replace_existing {
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            {
                renameat_with(parent, source, parent, destination, RenameFlags::NOREPLACE)
                    .map_err(io::Error::from)?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic no-replace journal replacement is unsupported on this Unix target",
                ));
            }
        } else {
            renameat(parent, source, parent, destination).map_err(io::Error::from)?;
        }
        moved = true;

        validate_promoted_source(&source_file, &expected_source)?;
        let promoted =
            statat(parent, destination, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        if identity(&promoted) != expected_source {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "journal destination does not match the promoted temporary entry",
            ));
        }
        fsync(parent).map_err(io::Error::from)
    })();
    result.map_err(|source| {
        if moved {
            super::AtomicMoveError::moved_or_unknown(source)
        } else {
            super::AtomicMoveError::not_moved(source)
        }
    })
}

#[cfg(test)]
pub(super) fn create_private_directory_exclusive(path: &Path) -> io::Result<DirectoryIdentity> {
    create_private_directory_exclusive_with_parent(path, None)
}

#[cfg(test)]
pub(super) fn create_private_directory_exclusive_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<DirectoryIdentity> {
    create_private_directory_exclusive_with_parent(path, Some(expected_parent))
}

#[cfg(test)]
fn create_private_directory_exclusive_with_parent(
    path: &Path,
    expected_parent: Option<&DirectoryIdentity>,
) -> io::Result<DirectoryIdentity> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    if let Some(expected_parent) = expected_parent {
        validate_expected_directory_identity(
            &parent,
            expected_parent,
            "created private directory parent no longer matches its captured identity",
        )?;
    }
    mkdirat(&parent, name, Mode::RWXU).map_err(io::Error::from)?;
    let directory =
        openat(&parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    let observed = validate_directory_entry(
        &parent,
        name,
        &directory,
        "created private directory changed during validation",
    )?;
    fchmod(&directory, Mode::RWXU).map_err(io::Error::from)?;
    Ok(observed)
}

#[cfg(test)]
pub(super) fn create_private_file(path: &Path) -> io::Result<File> {
    create_private_file_with_parent(path, None)
}

pub(super) fn create_private_file_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    create_private_file_with_parent(path, Some(expected_parent))
}

fn create_private_file_with_parent(
    path: &Path,
    expected_parent: Option<&DirectoryIdentity>,
) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    if let Some(expected_parent) = expected_parent {
        validate_expected_directory_identity(
            &parent,
            expected_parent,
            "created private file parent no longer matches its captured identity",
        )?;
    }
    let descriptor = openat(
        &parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "created private file changed during validation",
    )?;
    Ok(descriptor.into())
}

pub(super) fn remove_owned_file_in_parent(
    path: &Path,
    expected_file: &FileIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    validate_expected_directory_identity(
        &parent,
        expected_parent,
        "owned file parent no longer matches its captured identity",
    )?;
    let descriptor = open_regular_at(&parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "owned file changed during deletion validation",
    )?;
    validate_expected_file_identity(
        &metadata,
        expected_file,
        "owned file no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&metadata, "owned file")?;
    unlinkat(&parent, name, AtFlags::empty()).map_err(io::Error::from)?;
    fsync(&parent).map_err(io::Error::from)
}

#[cfg(test)]
pub(super) fn remove_owned_empty_directory_in_parent(
    path: &Path,
    expected_directory: &DirectoryIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    validate_expected_directory_identity(
        &parent,
        expected_parent,
        "owned directory parent no longer matches its captured identity",
    )?;
    let directory =
        openat(&parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
    validate_directory_entry(
        &parent,
        name,
        &directory,
        "owned directory changed during deletion validation",
    )?;
    validate_expected_directory_identity(
        &directory,
        expected_directory,
        "owned directory no longer matches its captured identity",
    )?;
    unlinkat(&parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
    fsync(&parent).map_err(io::Error::from)
}

pub(super) fn open_readonly_regular_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    validate_expected_directory_identity(
        &parent,
        expected_parent,
        "opened journal entry parent no longer matches its captured identity",
    )?;
    let descriptor = open_regular_at(&parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "opened journal entry changed during validation",
    )?;
    reject_mutable_hardlink(&metadata, "opened journal entry")?;
    Ok(descriptor.into())
}

#[cfg(test)]
pub(super) fn acquire_lock(path: &Path) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    acquire_lock_at(&parent, name)
}

pub(super) fn acquire_lock_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    validate_expected_directory_identity(
        &parent,
        expected_parent,
        "publication lock parent no longer matches its captured identity",
    )?;
    acquire_lock_at(&parent, name)
}

fn acquire_lock_at(parent: &OwnedFd, name: &OsStr) -> io::Result<File> {
    let descriptor = openat(
        parent,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication lock changed during validation",
    )?;
    reject_mutable_hardlink(&metadata, "publication lock")?;
    fchmod(&descriptor, Mode::RUSR | Mode::WUSR).map_err(io::Error::from)?;
    flock(&descriptor, FlockOperation::NonBlockingLockExclusive).map_err(io::Error::from)?;

    // A cooperating process must not be able to acquire a different inode by
    // replacing the lock pathname after this process acquired its lock.
    validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication lock path changed after locking",
    )?;
    Ok(descriptor.into())
}

/// Takes an already-existing lock without creating or tightening it.
pub(super) fn acquire_existing_lock(path: &Path) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    let descriptor = openat(
        &parent,
        name,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication lock changed during validation",
    )?;
    reject_mutable_hardlink(&metadata, "publication lock")?;
    flock(&descriptor, FlockOperation::NonBlockingLockExclusive).map_err(io::Error::from)?;
    validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication lock path changed after locking",
    )?;
    Ok(descriptor.into())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn visit_existing_directory_entries<S, E>(
    path: &Path,
    expected: &DirectoryIdentity,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;

    let directory = open_directory(path).map_err(DirectoryVisitError::Io)?;
    validate_expected_directory_identity(
        &directory,
        expected,
        "directory changed before recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)?;

    let mut buffer = [MaybeUninit::<u8>::uninit(); DIRECTORY_BUFFER_BYTES];
    let mut entries = RawDir::new(&directory, &mut buffer);
    while let Some(entry) = entries.next() {
        before_entry(state).map_err(DirectoryVisitError::Visitor)?;
        let entry = entry
            .map_err(io::Error::from)
            .map_err(DirectoryVisitError::Io)?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        visitor(state, DirectoryEntryName::Unix(name)).map_err(DirectoryVisitError::Visitor)?;
    }

    validate_expected_directory_identity(
        &directory,
        expected,
        "directory changed during recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)?;
    let reopened = open_directory(path).map_err(DirectoryVisitError::Io)?;
    validate_expected_directory_identity(
        &reopened,
        expected,
        "directory path changed during recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
pub(super) fn visit_existing_directory_entries<S, E>(
    path: &Path,
    expected: &DirectoryIdentity,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    // `readdir` returns a borrowed `dirent` name. It avoids the opaque
    // `OsString` allocation in `std::fs::DirEntry::file_name`, while the
    // opened descriptor remains available for identity checks during the scan.
    let directory = open_directory(path).map_err(DirectoryVisitError::Io)?;
    validate_expected_directory_identity(
        &directory,
        expected,
        "directory changed before recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)?;

    let mut entries =
        DirectoryStream::from_directory(&directory).map_err(DirectoryVisitError::Io)?;
    loop {
        let entry = entries.next().map_err(DirectoryVisitError::Io)?;
        let Some(entry) = entry else {
            break;
        };
        before_entry(state).map_err(DirectoryVisitError::Visitor)?;
        visitor(state, DirectoryEntryName::Unix(entry)).map_err(DirectoryVisitError::Visitor)?;
        validate_expected_directory_identity(
            &directory,
            expected,
            "directory changed during recovery discovery enumeration",
        )
        .map_err(DirectoryVisitError::Io)?;
    }
    let reopened = open_directory(path).map_err(DirectoryVisitError::Io)?;
    validate_expected_directory_identity(
        &reopened,
        expected,
        "directory path changed during recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
struct DirectoryStream(*mut libc::DIR);

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
impl DirectoryStream {
    fn from_directory(directory: &OwnedFd) -> io::Result<Self> {
        let descriptor = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(descriptor) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(descriptor);
            }
            return Err(error);
        }
        Ok(Self(stream))
    }

    fn next(&mut self) -> io::Result<Option<&OsStr>> {
        clear_directory_errno();
        let entry = unsafe { libc::readdir(self.0) };
        if entry.is_null() {
            let error = directory_errno();
            return if error == 0 {
                Ok(None)
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
        }
        // SAFETY: `readdir` returned a valid pointer owned by `self`, and its
        // `d_name` member is NUL-terminated for the lifetime of this borrow.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = OsStr::from_bytes(name.to_bytes());
        Ok(Some(name))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn directory_errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
fn directory_errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno() }
}

#[cfg(target_os = "dragonfly")]
fn directory_errno_pointer() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
fn clear_directory_errno() {
    unsafe {
        *directory_errno_pointer() = 0;
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
))]
fn directory_errno() -> i32 {
    unsafe { *directory_errno_pointer() }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
)))]
pub(super) fn visit_existing_directory_entries<S, E>(
    _path: &Path,
    _expected: &DirectoryIdentity,
    _state: &mut S,
    _before_entry: impl FnMut(&mut S) -> Result<(), E>,
    _visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    Err(DirectoryVisitError::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "identity-bound directory enumeration is unavailable on this Unix target",
    )))
}

/// Returns whether two existing regular files or directories share a device.
///
/// Every path component and the final object are opened without following
/// symbolic links. This makes the comparison operate on stable handles rather
/// than on path metadata that can be exchanged between two lookups.
#[cfg(test)]
pub(super) fn ensure_same_filesystem(first: &Path, second: &Path) -> io::Result<()> {
    let first = open_existing_file_or_directory(first)?;
    let second = open_existing_file_or_directory(second)?;
    let first = fstat(&first).map_err(io::Error::from)?;
    let second = fstat(&second).map_err(io::Error::from)?;
    if first.st_dev == second.st_dev {
        Ok(())
    } else {
        Err(Errno::XDEV.into())
    }
}

pub(super) fn ensure_journal_directory_same_filesystem(
    directory: &JournalDirectory,
    anchor: &Path,
) -> io::Result<()> {
    let anchor = open_existing_file_or_directory(anchor)?;
    let directory = fstat(&directory.directory).map_err(io::Error::from)?;
    let anchor = fstat(&anchor).map_err(io::Error::from)?;
    if directory.st_dev == anchor.st_dev {
        Ok(())
    } else {
        Err(Errno::XDEV.into())
    }
}

pub(super) fn ensure_single_hardlink(path: &Path) -> io::Result<()> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    let descriptor = open_regular_at(&parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication source changed during hard-link validation",
    )?;
    reject_mutable_hardlink(&metadata, "publication source")
}

/// Captures a stable device/inode token for a no-follow regular source.
pub(super) fn observe_file_identity(path: &Path) -> io::Result<FileIdentity> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    let descriptor = open_regular_at(&parent, name, REGULAR_FILE_FLAGS)?;
    let metadata = validate_regular_entry(
        &parent,
        name,
        &descriptor,
        "publication source changed while capturing identity",
    )?;
    Ok(identity(&metadata))
}

/// Captures a stable device/inode token for a no-follow directory.
pub(super) fn observe_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    let directory = open_directory(path)?;
    opened_directory_identity(&directory)
}

pub(super) fn ensure_directory_no_follow(path: &Path) -> io::Result<DirectoryIdentity> {
    validate_directory_path(path)?;

    let mut directory = open_start_directory(path)?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                match mkdirat(
                    &directory,
                    name,
                    Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH,
                ) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                directory = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?;
            }
            Component::ParentDir | Component::Prefix(_) => unreachable!(
                "validate_directory_path rejects escaping components before filesystem writes"
            ),
        }
    }
    opened_directory_identity(&directory)
}

pub(super) fn opened_file_identity(file: &File) -> io::Result<FileIdentity> {
    let metadata = fstat(file.as_fd()).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(metadata.st_mode).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "opened identity source is not a regular file",
        ));
    }
    Ok(identity(&metadata))
}

/// Copies supported Unix security metadata through no-follow file handles.
///
/// Ownership is copied before the mode because `fchown` may clear set-id bits.
/// The final mode is exactly the source mode, so this operation never grants a
/// permission that the source did not have. If ownership cannot be preserved,
/// the operation fails rather than silently weakening the publication proof.
#[cfg(test)]
pub(super) fn copy_security_metadata(
    source: &Path,
    target: &Path,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_target: &FileIdentity,
    expected_target_parent: &DirectoryIdentity,
    _: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let (source_parent_path, source_name) = split_leaf(source)?;
    let (target_parent_path, target_name) = split_leaf(target)?;
    let source_parent = open_directory(source_parent_path)?;
    let target_parent = open_directory(target_parent_path)?;
    validate_expected_directory_identity(
        &source_parent,
        expected_source_parent,
        "security metadata source parent no longer matches its captured identity",
    )?;
    validate_expected_directory_identity(
        &target_parent,
        expected_target_parent,
        "security metadata target parent no longer matches its captured identity",
    )?;
    let source = open_regular_at(&source_parent, source_name, REGULAR_FILE_FLAGS)?;
    let target = open_regular_at(&target_parent, target_name, REGULAR_FILE_FLAGS)?;
    let source_metadata = validate_regular_entry(
        &source_parent,
        source_name,
        &source,
        "security metadata source changed during validation",
    )?;
    let target_metadata = validate_regular_entry(
        &target_parent,
        target_name,
        &target,
        "security metadata target changed during validation",
    )?;
    validate_expected_file_identity(
        &source_metadata,
        expected_source,
        "security metadata source no longer matches its captured identity",
    )?;
    validate_expected_file_identity(
        &target_metadata,
        expected_target,
        "security metadata target no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&target_metadata, "security metadata target")?;

    if source_metadata.st_uid != target_metadata.st_uid
        || source_metadata.st_gid != target_metadata.st_gid
    {
        fchown(
            &target,
            Some(Uid::from_raw(source_metadata.st_uid)),
            Some(Gid::from_raw(source_metadata.st_gid)),
        )
        .map_err(io::Error::from)?;
    }

    let source_mode = Mode::from_raw_mode(source_metadata.st_mode);
    fchmod(&target, source_mode).map_err(io::Error::from)?;

    let applied = fstat(&target).map_err(io::Error::from)?;
    if applied.st_uid != source_metadata.st_uid
        || applied.st_gid != source_metadata.st_gid
        || Mode::from_raw_mode(applied.st_mode) != source_mode
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unix security metadata could not be preserved exactly",
        ));
    }
    fsync(&target).map_err(io::Error::from)?;
    Ok(())
}

pub(super) fn copy_security_metadata_between_journal_directories(
    source_directory: &JournalDirectory,
    source_name: &OsStr,
    target_directory: &JournalDirectory,
    target_name: &OsStr,
    expected_source: &FileIdentity,
    expected_target: &FileIdentity,
    _: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let source = open_regular_at(&source_directory.directory, source_name, REGULAR_FILE_FLAGS)?;
    let target = open_regular_at(&target_directory.directory, target_name, REGULAR_FILE_FLAGS)?;
    let source_metadata = validate_regular_entry(
        &source_directory.directory,
        source_name,
        &source,
        "journal security metadata source changed during validation",
    )?;
    let target_metadata = validate_regular_entry(
        &target_directory.directory,
        target_name,
        &target,
        "journal security metadata target changed during validation",
    )?;
    validate_expected_file_identity(
        &source_metadata,
        expected_source,
        "journal security metadata source no longer matches its captured identity",
    )?;
    validate_expected_file_identity(
        &target_metadata,
        expected_target,
        "journal security metadata target no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&target_metadata, "journal security metadata target")?;

    if source_metadata.st_uid != target_metadata.st_uid
        || source_metadata.st_gid != target_metadata.st_gid
    {
        fchown(
            &target,
            Some(Uid::from_raw(source_metadata.st_uid)),
            Some(Gid::from_raw(source_metadata.st_gid)),
        )
        .map_err(io::Error::from)?;
    }

    let source_mode = Mode::from_raw_mode(source_metadata.st_mode);
    fchmod(&target, source_mode).map_err(io::Error::from)?;

    let applied = fstat(&target).map_err(io::Error::from)?;
    if applied.st_uid != source_metadata.st_uid
        || applied.st_gid != source_metadata.st_gid
        || Mode::from_raw_mode(applied.st_mode) != source_mode
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unix security metadata could not be preserved exactly",
        )
        .into());
    }
    fsync(&target).map_err(io::Error::from)?;
    Ok(())
}

pub(super) fn copy_security_metadata_external_to_journal_directory(
    source: &Path,
    target_directory: &JournalDirectory,
    target_name: &OsStr,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_target: &FileIdentity,
    _: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let (source_parent_path, source_name) = split_leaf(source)?;
    let source_parent = open_directory(source_parent_path)?;
    validate_expected_directory_identity(
        &source_parent,
        expected_source_parent,
        "security metadata source parent no longer matches its captured identity",
    )?;
    let source = open_regular_at(&source_parent, source_name, REGULAR_FILE_FLAGS)?;
    let target = open_regular_at(&target_directory.directory, target_name, REGULAR_FILE_FLAGS)?;
    let source_metadata = validate_regular_entry(
        &source_parent,
        source_name,
        &source,
        "security metadata source changed during validation",
    )?;
    let target_metadata = validate_regular_entry(
        &target_directory.directory,
        target_name,
        &target,
        "journal security metadata target changed during validation",
    )?;
    validate_expected_file_identity(
        &source_metadata,
        expected_source,
        "security metadata source no longer matches its captured identity",
    )?;
    validate_expected_file_identity(
        &target_metadata,
        expected_target,
        "journal security metadata target no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&target_metadata, "journal security metadata target")?;

    if source_metadata.st_uid != target_metadata.st_uid
        || source_metadata.st_gid != target_metadata.st_gid
    {
        fchown(
            &target,
            Some(Uid::from_raw(source_metadata.st_uid)),
            Some(Gid::from_raw(source_metadata.st_gid)),
        )
        .map_err(io::Error::from)?;
    }

    let source_mode = Mode::from_raw_mode(source_metadata.st_mode);
    fchmod(&target, source_mode).map_err(io::Error::from)?;

    let applied = fstat(&target).map_err(io::Error::from)?;
    if applied.st_uid != source_metadata.st_uid
        || applied.st_gid != source_metadata.st_gid
        || Mode::from_raw_mode(applied.st_mode) != source_mode
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unix security metadata could not be preserved exactly",
        )
        .into());
    }
    fsync(&target).map_err(io::Error::from)?;
    Ok(())
}

#[cfg(test)]
pub(super) fn test_tamper_security_metadata(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))
}

#[cfg(test)]
pub(super) fn test_security_metadata_matches(left: &Path, right: &Path) -> io::Result<bool> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let left = std::fs::metadata(left)?;
    let right = std::fs::metadata(right)?;
    Ok(left.uid() == right.uid()
        && left.gid() == right.gid()
        && left.permissions().mode() == right.permissions().mode())
}

#[cfg(test)]
pub(super) fn atomic_replace_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_verified_tracked(
        source,
        destination,
        replace_existing,
        None,
        None,
        expected_source_parent,
        expected_destination_parent,
    )
}

#[cfg(test)]
pub(super) fn atomic_replace(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> io::Result<()> {
    let (source_parent, _) = split_leaf(source)?;
    let (destination_parent, _) = split_leaf(destination)?;
    let expected_source_parent = observe_directory_identity(source_parent)?;
    let expected_destination_parent = observe_directory_identity(destination_parent)?;
    atomic_replace_tracked(
        source,
        destination,
        replace_existing,
        &expected_source_parent,
        &expected_destination_parent,
    )
    .map_err(super::AtomicMoveError::into_error)
}

#[cfg(test)]
pub(super) fn capture_existing(
    source: &Path,
    destination: &Path,
    expected_source: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    match expected_digest {
        Some(expected_digest) => atomic_replace_verified_digest(
            source,
            destination,
            false,
            expected_source,
            expected_digest,
            expected_source_parent,
            expected_destination_parent,
        ),
        None => atomic_replace_verified(
            source,
            destination,
            false,
            expected_source,
            expected_source_parent,
            expected_destination_parent,
        ),
    }
}

pub(super) fn capture_external_regular_in_journal_directory(
    source: &Path,
    destination: &JournalDirectory,
    destination_name: &OsStr,
    expected_source: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (source_parent_path, source_name) = split_leaf(source)?;
    let source_parent = open_directory(source_parent_path)?;
    atomic_replace_verified_opened(
        &source_parent,
        source_name,
        &destination.directory,
        destination_name,
        false,
        AtomicReplaceVerification {
            expected_source: Some(expected_source),
            expected_digest,
            expected_source_parent: Some(expected_source_parent),
            expected_destination_parent: None,
        },
    )
    .map_err(super::AtomicMoveError::into_error)
}

pub(super) fn promote_journal_regular_to_external(
    source: &JournalDirectory,
    source_name: &OsStr,
    destination: &Path,
    expected_source: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (destination_parent_path, destination_name) = split_leaf(destination)?;
    let destination_parent = open_directory(destination_parent_path)?;
    atomic_replace_verified_opened(
        &source.directory,
        source_name,
        &destination_parent,
        destination_name,
        false,
        AtomicReplaceVerification {
            expected_source: Some(expected_source),
            expected_digest,
            expected_source_parent: None,
            expected_destination_parent: Some(expected_destination_parent),
        },
    )
    .map_err(super::AtomicMoveError::into_error)
}

/// Atomically moves a source whose identity was captured before publication.
///
/// The source file and both parent directories remain open across validation
/// and rename. The source device must equal the destination-parent device, so
/// cross-filesystem publication is rejected as `EXDEV` before mutation.
#[cfg(test)]
pub(super) fn atomic_replace_verified(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    atomic_replace_verified_tracked(
        source,
        destination,
        replace_existing,
        Some(expected_source),
        None,
        expected_source_parent,
        expected_destination_parent,
    )
    .map_err(super::AtomicMoveError::into_error)
}

#[cfg(test)]
fn atomic_replace_verified_digest(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source: &FileIdentity,
    expected_digest: DigestV1,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    atomic_replace_verified_tracked(
        source,
        destination,
        replace_existing,
        Some(expected_source),
        Some(expected_digest),
        expected_source_parent,
        expected_destination_parent,
    )
    .map_err(super::AtomicMoveError::into_error)
}

pub(super) fn atomic_replace_verified_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source: Option<&FileIdentity>,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), super::AtomicMoveError> {
    let (source_parent_path, source_name) = match split_leaf(source) {
        Ok(parts) => parts,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let (destination_parent_path, destination_name) = match split_leaf(destination) {
        Ok(parts) => parts,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let source_parent = match open_directory(source_parent_path) {
        Ok(directory) => directory,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let destination_parent = match open_directory(destination_parent_path) {
        Ok(directory) => directory,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    atomic_replace_verified_opened(
        &source_parent,
        source_name,
        &destination_parent,
        destination_name,
        replace_existing,
        AtomicReplaceVerification {
            expected_source,
            expected_digest,
            expected_source_parent: Some(expected_source_parent),
            expected_destination_parent: Some(expected_destination_parent),
        },
    )
}

pub(super) fn atomic_replace_captured_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source: &FileIdentity,
    expected_digest: DigestV1,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_verified_tracked(
        source,
        destination,
        replace_existing,
        Some(expected_source),
        Some(expected_digest),
        expected_source_parent,
        expected_destination_parent,
    )
}

struct AtomicReplaceVerification<'a> {
    expected_source: Option<&'a FileIdentity>,
    expected_digest: Option<DigestV1>,
    expected_source_parent: Option<&'a DirectoryIdentity>,
    expected_destination_parent: Option<&'a DirectoryIdentity>,
}

fn atomic_replace_verified_opened(
    source_parent: &OwnedFd,
    source_name: &OsStr,
    destination_parent: &OwnedFd,
    destination_name: &OsStr,
    replace_existing: bool,
    verification: AtomicReplaceVerification<'_>,
) -> Result<(), super::AtomicMoveError> {
    let mut moved = false;
    let result = (|| {
        if let Some(expected_source_parent) = verification.expected_source_parent {
            validate_expected_directory_identity(
                source_parent,
                expected_source_parent,
                "atomic publication source parent no longer matches its captured identity",
            )?;
        }
        if let Some(expected_destination_parent) = verification.expected_destination_parent {
            validate_expected_directory_identity(
                destination_parent,
                expected_destination_parent,
                "atomic publication destination parent no longer matches its captured identity",
            )?;
        }
        let source_file = open_regular_at(source_parent, source_name, REGULAR_FILE_FLAGS)?;
        let opened = validate_regular_entry(
            source_parent,
            source_name,
            &source_file,
            "atomic publication source changed during validation",
        )?;
        reject_mutable_hardlink(&opened, "atomic publication source")?;
        let observed_source = identity(&opened);
        let expected_source = verification.expected_source.unwrap_or(&observed_source);
        validate_expected_file_identity(
            &opened,
            expected_source,
            "atomic publication source no longer matches its captured identity",
        )?;
        let mut source_file = File::from(source_file);
        if let Some(expected_digest) = verification.expected_digest {
            validate_opened_digest(
                &mut source_file,
                expected_digest,
                expected_source.length,
                "atomic publication source content changed before rename",
            )?;
            validate_promoted_source(&source_file, expected_source)?;
        }

        let destination_parent_metadata = fstat(destination_parent).map_err(io::Error::from)?;
        if opened.st_dev != destination_parent_metadata.st_dev {
            return Err(Errno::XDEV.into());
        }

        if !replace_existing {
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            {
                renameat_with(
                    source_parent,
                    source_name,
                    destination_parent,
                    destination_name,
                    RenameFlags::NOREPLACE,
                )
                .map_err(io::Error::from)?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "atomic no-replace publication is unsupported on this Unix target",
                ));
            }
        } else {
            renameat(
                source_parent,
                source_name,
                destination_parent,
                destination_name,
            )
            .map_err(io::Error::from)?;
        }
        moved = true;

        validate_promoted_source(&source_file, expected_source)?;
        if let Some(expected_digest) = verification.expected_digest {
            validate_opened_digest(
                &mut source_file,
                expected_digest,
                expected_source.length,
                "atomic publication source content changed during rename",
            )?;
        }

        // POSIX has no general rename-by-FD primitive. Verify that the destination
        // entry now names the opened inode; higher layers retain digest evidence
        // and recover if a hostile same-user process races the final name lookup.
        let promoted = statat(
            destination_parent,
            destination_name,
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(io::Error::from)?;
        if identity(&promoted) != *expected_source {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "atomic publication destination does not match the promoted source identity",
            ));
        }

        if let Some(expected_destination_parent) = verification.expected_destination_parent {
            validate_expected_directory_identity(
                destination_parent,
                expected_destination_parent,
                "atomic publication destination parent handle changed during publication",
            )?;
        }
        if let Some(expected_source_parent) = verification.expected_source_parent {
            validate_expected_directory_identity(
                source_parent,
                expected_source_parent,
                "atomic publication source parent handle changed during publication",
            )?;
        }
        fsync(source_parent).map_err(io::Error::from)?;
        fsync(destination_parent).map_err(io::Error::from)?;
        Ok(())
    })();
    result.map_err(|source| {
        if moved {
            super::AtomicMoveError::moved_or_unknown(source)
        } else {
            super::AtomicMoveError::not_moved(source)
        }
    })
}

fn split_leaf(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic publication path has no regular leaf name",
        )
    })?;
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic publication path has an invalid leaf name",
        ));
    }
    Ok((path.parent().unwrap_or_else(|| Path::new(".")), name))
}

fn validate_directory_path(path: &Path) -> io::Result<()> {
    let mut has_normal_component = false;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(_) => has_normal_component = true,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "private directory path contains an escaping component",
                ));
            }
        }
    }
    if !has_normal_component {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private directory path must name a child directory",
        ));
    }
    Ok(())
}

fn open_start_directory(path: &Path) -> io::Result<OwnedFd> {
    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    openat(CWD, start, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)
}

fn open_directory(path: &Path) -> io::Result<OwnedFd> {
    let mut directory = open_start_directory(path)?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                directory = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "atomic publication parent contains an escaping path component",
                ));
            }
        }
    }
    Ok(directory)
}

fn open_existing_file_or_directory(path: &Path) -> io::Result<OwnedFd> {
    if path.file_name().is_none() {
        return open_directory(path);
    }

    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path)?;
    let descriptor = openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let metadata = fstat(&descriptor).map_err(io::Error::from)?;
    let file_type = FileType::from_raw_mode(metadata.st_mode);
    if !file_type.is_file() && !file_type.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem identity path is not a regular file or directory",
        ));
    }
    let named = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if identity(&metadata) != identity(&named) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "filesystem identity path changed during validation",
        ));
    }
    Ok(descriptor)
}

fn open_regular_at(parent: &OwnedFd, name: &OsStr, flags: OFlags) -> io::Result<OwnedFd> {
    let descriptor = openat(parent, name, flags, Mode::empty()).map_err(io::Error::from)?;
    let metadata = fstat(&descriptor).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(metadata.st_mode).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication entry is not a regular file",
        ));
    }
    Ok(descriptor)
}

fn validate_regular_entry(
    parent: &OwnedFd,
    name: &OsStr,
    descriptor: &OwnedFd,
    changed_message: &'static str,
) -> io::Result<Stat> {
    let opened = fstat(descriptor).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(opened.st_mode).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication entry is not a regular file",
        ));
    }
    let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if identity(&opened) != identity(&named) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, changed_message));
    }
    Ok(opened)
}

fn validate_directory_entry(
    parent: &OwnedFd,
    name: &OsStr,
    descriptor: &OwnedFd,
    changed_message: &'static str,
) -> io::Result<DirectoryIdentity> {
    let opened = opened_directory_identity(descriptor)?;
    let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(named.st_mode).is_dir() || opened != directory_identity(&named) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, changed_message));
    }
    Ok(opened)
}

fn opened_directory_identity(directory: &OwnedFd) -> io::Result<DirectoryIdentity> {
    let metadata = fstat(directory).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(metadata.st_mode).is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication directory identity source is not a directory",
        ));
    }
    Ok(directory_identity(&metadata))
}

fn validate_expected_directory_identity(
    directory: &OwnedFd,
    expected: &DirectoryIdentity,
    changed_message: &'static str,
) -> io::Result<()> {
    let observed = opened_directory_identity(directory)?;
    if &observed != expected {
        return Err(io::Error::new(io::ErrorKind::Interrupted, changed_message));
    }
    Ok(())
}

fn validate_expected_file_identity(
    metadata: &Stat,
    expected: &FileIdentity,
    changed_message: &'static str,
) -> io::Result<()> {
    if &identity(metadata) != expected {
        return Err(io::Error::new(io::ErrorKind::Interrupted, changed_message));
    }
    Ok(())
}

fn validate_promoted_source(
    descriptor: impl std::os::fd::AsFd,
    expected: &FileIdentity,
) -> io::Result<()> {
    let metadata = fstat(descriptor).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(metadata.st_mode).is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "promoted publication source is no longer a regular file",
        ));
    }
    validate_expected_file_identity(
        &metadata,
        expected,
        "promoted publication source no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(&metadata, "promoted publication source")
}

fn validate_opened_digest(
    file: &mut File,
    expected: DigestV1,
    length: u64,
    changed_message: &'static str,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    #[cfg(test)]
    super::super::test_record_verification_hash(length);
    let actual = DigestV1::hash_reader(&mut *file, length)?;
    if actual != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData, changed_message));
    }
    Ok(())
}

fn identity(metadata: &Stat) -> FileIdentity {
    FileIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
        length: metadata.st_size as u64,
    }
}

fn directory_identity(metadata: &Stat) -> DirectoryIdentity {
    DirectoryIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
    }
}

fn reject_mutable_hardlink(metadata: &Stat, description: &'static str) -> io::Result<()> {
    if metadata.st_nlink != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must have exactly one hard link"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        REGULAR_FILE_FLAGS, SecurityMetadataError, atomic_replace, atomic_replace_verified,
        capture_existing, capture_external_regular_in_journal_directory, copy_security_metadata,
        create_journal_directory, create_private_directory, create_private_directory_exclusive,
        create_private_directory_exclusive_in_parent, create_private_file,
        create_private_file_in_parent, ensure_same_filesystem, ensure_single_hardlink,
        observe_directory_identity, observe_file_identity, open_commit_root, open_directory,
        open_journal_namespace, open_readonly_regular_in_parent, open_regular_at,
        promote_journal_regular_to_external, remove_owned_empty_directory_in_parent,
        remove_owned_file_in_parent, validate_promoted_source,
    };
    use rustix::io::Errno;
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::Path;
    use unity_asset_core::{AssetLoadBudget, DigestV1};

    #[test]
    fn no_replace_does_not_overwrite_an_existing_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, b"new").expect("source");
        fs::write(&destination, b"old").expect("destination");

        let error = atomic_replace(&source, &destination, false).expect_err("destination exists");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).expect("source remains"), b"new");
        assert_eq!(fs::read(&destination).expect("destination remains"), b"old");
    }

    #[test]
    fn handle_rooted_journal_moves_preserve_identity_and_content() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, b"journal move").expect("source fixture");

        let root_identity = observe_directory_identity(directory.path()).expect("root identity");
        let source_identity = observe_file_identity(&source).expect("source identity");
        let digest = DigestV1::hash_bytes(b"journal move");
        let root = open_commit_root(directory.path(), &root_identity).expect("commit root");
        let namespace = open_journal_namespace(&root).expect("journal namespace");
        let stage =
            create_journal_directory(&namespace, OsStr::new("stage")).expect("stage directory");

        capture_external_regular_in_journal_directory(
            &source,
            &stage,
            OsStr::new("artifact"),
            &source_identity,
            Some(digest),
            &root_identity,
        )
        .expect("capture source into journal");
        assert!(!source.exists());

        promote_journal_regular_to_external(
            &stage,
            OsStr::new("artifact"),
            &destination,
            &source_identity,
            Some(digest),
            &root_identity,
        )
        .expect("promote source from journal");
        assert_eq!(
            fs::read(&destination).expect("promoted content"),
            b"journal move"
        );
    }

    #[test]
    fn source_symlink_is_rejected_without_touching_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let real = directory.path().join("real");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&real, b"new").expect("real source");
        symlink(&real, &source).expect("source symlink");

        atomic_replace(&source, &destination, false).expect_err("symlink rejected");
        assert!(!destination.exists());
        assert!(
            fs::symlink_metadata(&source)
                .expect("symlink remains")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn preexisting_recovery_symlink_is_rejected_before_child_creation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let external = tempfile::tempdir().expect("external directory");
        let recovery = directory.path().join(".unity-asset-recovery");
        symlink(external.path(), &recovery).expect("recovery symlink");

        create_private_directory(&recovery.join("v1")).expect_err("symlink rejected");

        assert!(!external.path().join("v1").exists());
        assert!(
            fs::symlink_metadata(&recovery)
                .expect("recovery symlink remains")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn private_file_rejects_symlinked_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let external = tempfile::tempdir().expect("external directory");
        let recovery = directory.path().join("recovery");
        symlink(external.path(), &recovery).expect("recovery symlink");

        create_private_file(&recovery.join("journal.json")).expect_err("symlink rejected");

        assert!(!external.path().join("journal.json").exists());
    }

    #[test]
    fn bound_readonly_open_rejects_a_replaced_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced_parent = directory.path().join("displaced-parent");
        let journal = parent.join("journal");
        fs::create_dir(&parent).expect("parent");
        fs::write(&journal, b"original").expect("journal");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::rename(&parent, &displaced_parent).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");
        fs::write(&journal, b"replacement").expect("replacement journal");

        let error = open_readonly_regular_in_parent(&journal, &expected_parent)
            .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&journal).expect("replacement remains"),
            b"replacement"
        );
        assert_eq!(
            fs::read(displaced_parent.join("journal")).expect("original remains"),
            b"original"
        );
    }

    #[test]
    fn exclusive_private_directory_returns_identity_and_rejects_reuse() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recovery = directory.path().join("recovery");
        fs::create_dir(&recovery).expect("recovery parent");
        let transaction = recovery.join("transaction");

        let created = create_private_directory_exclusive(&transaction)
            .expect("exclusive transaction directory");

        assert_eq!(
            created,
            observe_directory_identity(&transaction).expect("transaction identity")
        );
        let mode = fs::metadata(&transaction)
            .expect("transaction metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o700);
        let error = create_private_directory_exclusive(&transaction)
            .expect_err("existing transaction rejected");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn exclusive_private_directory_does_not_create_missing_ancestors() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing_parent = directory.path().join("missing");

        let error = create_private_directory_exclusive(&missing_parent.join("transaction"))
            .expect_err("missing ancestor rejected");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!missing_parent.exists());
    }

    #[test]
    fn bound_private_creation_rejects_a_replaced_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced_parent = directory.path().join("displaced-parent");
        fs::create_dir(&parent).expect("parent");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::rename(&parent, &displaced_parent).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");

        let file_error = create_private_file_in_parent(&parent.join("journal"), &expected_parent)
            .expect_err("replacement parent rejected for file creation");
        let directory_error = create_private_directory_exclusive_in_parent(
            &parent.join("transaction"),
            &expected_parent,
        )
        .expect_err("replacement parent rejected for directory creation");

        assert_eq!(file_error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(directory_error.kind(), io::ErrorKind::Interrupted);
        assert!(!parent.join("journal").exists());
        assert!(!parent.join("transaction").exists());
        assert!(!displaced_parent.join("journal").exists());
        assert!(!displaced_parent.join("transaction").exists());
    }

    #[test]
    fn owned_file_removal_requires_the_captured_parent_and_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let file = parent.join("journal");
        let replacement = parent.join("replacement");
        fs::create_dir(&parent).expect("parent");
        fs::write(&file, b"original").expect("file");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        let expected_file = observe_file_identity(&file).expect("file identity");
        fs::write(&replacement, b"replacement").expect("replacement");
        fs::rename(&replacement, &file).expect("replace file");

        let error = remove_owned_file_in_parent(&file, &expected_file, &expected_parent)
            .expect_err("replacement file rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&file).expect("replacement remains"),
            b"replacement"
        );

        let stable = parent.join("stable");
        fs::write(&stable, b"stable").expect("stable file");
        let stable_identity = observe_file_identity(&stable).expect("stable identity");
        remove_owned_file_in_parent(&stable, &stable_identity, &expected_parent)
            .expect("remove stable file");
        assert!(!stable.exists());
    }

    #[test]
    fn owned_empty_directory_removal_requires_the_captured_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let child = parent.join("transaction");
        let displaced_child = parent.join("displaced-transaction");
        fs::create_dir(&parent).expect("parent");
        fs::create_dir(&child).expect("child");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        let expected_child = observe_directory_identity(&child).expect("child identity");
        fs::rename(&child, &displaced_child).expect("displace child");
        fs::create_dir(&child).expect("replacement child");

        let error =
            remove_owned_empty_directory_in_parent(&child, &expected_child, &expected_parent)
                .expect_err("replacement child rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(child.is_dir());
        assert!(displaced_child.is_dir());

        let stable = parent.join("stable");
        fs::create_dir(&stable).expect("stable child");
        let stable_identity = observe_directory_identity(&stable).expect("stable identity");
        remove_owned_empty_directory_in_parent(&stable, &stable_identity, &expected_parent)
            .expect("remove stable child");
        assert!(!stable.exists());
    }

    #[test]
    fn captured_identity_rejects_a_replaced_source() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let replacement = directory.path().join("replacement");
        let destination = directory.path().join("destination");
        fs::write(&source, b"original").expect("source");
        let expected = observe_file_identity(&source).expect("source identity");
        let expected_parent =
            observe_directory_identity(directory.path()).expect("publication parent identity");
        fs::write(&replacement, b"replacement").expect("replacement");
        fs::rename(&replacement, &source).expect("replace source");

        let error = atomic_replace_verified(
            &source,
            &destination,
            false,
            &expected,
            &expected_parent,
            &expected_parent,
        )
        .expect_err("identity mismatch");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&source).expect("replacement remains"),
            b"replacement"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn captured_digest_rejects_in_place_content_change_before_rename() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, b"original").expect("source");
        let expected = observe_file_identity(&source).expect("source identity");
        let expected_digest = DigestV1::hash_bytes(b"original");
        let expected_parent =
            observe_directory_identity(directory.path()).expect("publication parent identity");
        fs::write(&source, b"external").expect("in-place replacement");

        let error = capture_existing(
            &source,
            &destination,
            &expected,
            Some(expected_digest),
            &expected_parent,
            &expected_parent,
        )
        .expect_err("digest mismatch");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&source).expect("source remains"), b"external");
        assert!(!destination.exists());
    }

    #[test]
    fn captured_parent_identity_rejects_a_replaced_destination_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination_parent = directory.path().join("destination-parent");
        let displaced_parent = directory.path().join("displaced-parent");
        fs::write(&source, b"source").expect("source");
        fs::create_dir(&destination_parent).expect("destination parent");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(directory.path()).expect("source parent identity");
        let expected_parent =
            observe_directory_identity(&destination_parent).expect("destination parent identity");
        fs::rename(&destination_parent, &displaced_parent).expect("displace parent");
        fs::create_dir(&destination_parent).expect("replacement parent");
        let destination = destination_parent.join("destination");

        let error = capture_existing(
            &source,
            &destination,
            &expected_source,
            None,
            &expected_source_parent,
            &expected_parent,
        )
        .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&source).expect("source remains"), b"source");
        assert!(!destination.exists());
        assert!(!displaced_parent.join("destination").exists());
    }

    #[test]
    fn captured_parent_identity_rejects_a_replaced_source_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_parent = directory.path().join("source-parent");
        let displaced_source_parent = directory.path().join("displaced-source-parent");
        let destination_parent = directory.path().join("destination-parent");
        let source = source_parent.join("source");
        let destination = destination_parent.join("destination");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination_parent).expect("destination parent");
        fs::write(&source, b"source").expect("source");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(&source_parent).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&destination_parent).expect("destination parent identity");
        fs::rename(&source_parent, &displaced_source_parent).expect("displace source parent");
        fs::create_dir(&source_parent).expect("replacement source parent");
        fs::write(&source, b"replacement").expect("replacement source");

        let error = capture_existing(
            &source,
            &destination,
            &expected_source,
            None,
            &expected_source_parent,
            &expected_destination_parent,
        )
        .expect_err("replacement source parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&source).expect("replacement remains"),
            b"replacement"
        );
        assert_eq!(
            fs::read(displaced_source_parent.join("source")).expect("original remains"),
            b"source"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn mutable_hardlink_is_rejected_before_publication() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let alias = directory.path().join("alias");
        fs::write(&source, b"source").expect("source");
        fs::hard_link(&source, &alias).expect("hard link");

        let error = ensure_single_hardlink(&source).expect_err("hard link rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(alias).expect("alias remains"), b"source");
    }

    #[test]
    fn promoted_source_rejects_a_hardlink_added_after_rename() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        let alias = directory.path().join("alias");
        fs::write(&source, b"source").expect("source");
        let expected = observe_file_identity(&source).expect("source identity");
        let parent = open_directory(directory.path()).expect("open parent");
        let descriptor = open_regular_at(
            &parent,
            source.file_name().expect("source name"),
            REGULAR_FILE_FLAGS,
        )
        .expect("open source");
        fs::rename(&source, &destination).expect("promote source");
        fs::hard_link(&destination, &alias).expect("late hard link");

        let error =
            validate_promoted_source(&descriptor, &expected).expect_err("late hard link rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read(&destination).expect("destination remains"),
            b"source"
        );
        assert_eq!(fs::read(&alias).expect("alias remains"), b"source");
    }

    #[test]
    fn security_metadata_preserves_mode_exactly() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        fs::write(&source, b"source").expect("source");
        fs::write(&target, b"target").expect("target");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("source mode");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_target = observe_file_identity(&target).expect("target identity");
        let expected_parent =
            observe_directory_identity(directory.path()).expect("metadata parent identity");
        let mut budget = AssetLoadBudget::default();

        copy_security_metadata(
            &source,
            &target,
            &expected_source,
            &expected_parent,
            &expected_target,
            &expected_parent,
            &mut budget,
        )
        .expect("preserve metadata");

        let mode = fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn security_metadata_rejects_a_replaced_target_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let replacement = directory.path().join("replacement");
        fs::write(&source, b"source").expect("source");
        fs::write(&target, b"target").expect("target");
        fs::write(&replacement, b"replacement").expect("replacement");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("source mode");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o604))
            .expect("replacement mode");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_target = observe_file_identity(&target).expect("target identity");
        let expected_parent =
            observe_directory_identity(directory.path()).expect("metadata parent identity");
        fs::rename(&replacement, &target).expect("replace target");
        let mut budget = AssetLoadBudget::default();

        let error = copy_security_metadata(
            &source,
            &target,
            &expected_source,
            &expected_parent,
            &expected_target,
            &expected_parent,
            &mut budget,
        )
        .expect_err("replacement target rejected");

        let SecurityMetadataError::Io(error) = error else {
            panic!("replaced target must fail with an I/O identity error");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let mode = fs::metadata(&target)
            .expect("replacement metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o604);
    }

    #[test]
    fn security_metadata_rejects_a_replaced_source_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_parent = directory.path().join("source-parent");
        let displaced_source_parent = directory.path().join("displaced-source-parent");
        let target_parent = directory.path().join("target-parent");
        let source = source_parent.join("source");
        let target = target_parent.join("target");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&target_parent).expect("target parent");
        fs::write(&source, b"source").expect("source");
        fs::write(&target, b"target").expect("target");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).expect("source mode");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("target mode");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(&source_parent).expect("source parent identity");
        let expected_target = observe_file_identity(&target).expect("target identity");
        let expected_target_parent =
            observe_directory_identity(&target_parent).expect("target parent identity");
        fs::rename(&source_parent, &displaced_source_parent).expect("displace source parent");
        fs::create_dir(&source_parent).expect("replacement source parent");
        fs::write(&source, b"replacement").expect("replacement source");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o604))
            .expect("replacement source mode");
        let mut budget = AssetLoadBudget::default();

        let error = copy_security_metadata(
            &source,
            &target,
            &expected_source,
            &expected_source_parent,
            &expected_target,
            &expected_target_parent,
            &mut budget,
        )
        .expect_err("replacement source parent rejected");

        let SecurityMetadataError::Io(error) = error else {
            panic!("replaced source parent must fail with an I/O identity error");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        let target_mode = fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(target_mode, 0o600);
    }

    #[test]
    fn same_filesystem_uses_opened_handles() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::write(&first, b"first").expect("first");
        fs::write(&second, b"second").expect("second");

        ensure_same_filesystem(&first, &second).expect("filesystem identity");
    }

    #[test]
    fn cross_filesystem_publication_is_classified_as_exdev_when_available() {
        let source_directory = tempfile::tempdir().expect("temporary directory");
        let source = source_directory.path().join("source");
        fs::write(&source, b"source").expect("source");

        let Some(destination_directory) = different_filesystem(source_directory.path()) else {
            return;
        };
        let destination = destination_directory.path().join("destination");
        let expected = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(source_directory.path()).expect("source parent identity");
        let expected_parent = observe_directory_identity(destination_directory.path())
            .expect("destination parent identity");

        let error = ensure_same_filesystem(&source, destination_directory.path())
            .expect_err("different filesystem");
        assert_eq!(error.raw_os_error(), Some(Errno::XDEV.raw_os_error()));
        let error = atomic_replace_verified(
            &source,
            &destination,
            false,
            &expected,
            &expected_source_parent,
            &expected_parent,
        )
        .expect_err("cross-filesystem publication rejected");
        assert_eq!(error.raw_os_error(), Some(Errno::XDEV.raw_os_error()));
        assert_eq!(fs::read(&source).expect("source remains"), b"source");
        assert!(!destination.exists());
    }

    fn different_filesystem(reference: &Path) -> Option<tempfile::TempDir> {
        ["/dev/shm", "/tmp", "/var/tmp"]
            .into_iter()
            .filter_map(|candidate| tempfile::Builder::new().tempdir_in(candidate).ok())
            .find(|candidate| ensure_same_filesystem(reference, candidate.path()).is_err())
    }
}
