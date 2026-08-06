use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt as _;
use std::path::{Component, Path};

use rustix::fs::CWD;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat, fstat, openat, statat};
use rustix::io::Errno;

use super::{AnchoredFsError, DirectoryEntryHint, EntryKindHint, OpenPolicy};

const DIRECTORY_FLAGS: OFlags =
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
const REGULAR_FILE_FLAGS: OFlags =
    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;

pub(super) struct ReadDirectory {
    descriptor: OwnedFd,
}

impl AsFd for ReadDirectory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.descriptor.as_fd()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryIdentity {
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanoseconds: u32,
    changed_seconds: i64,
    changed_nanoseconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryObjectIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: u32,
    changed_seconds: i64,
    changed_nanoseconds: u32,
}

impl FileIdentity {
    pub(super) const fn length(self) -> u64 {
        self.length
    }
}

pub(super) struct DirectoryEntries<'directory> {
    entries: Dir,
    _authority: PhantomData<&'directory ReadDirectory>,
}

impl Iterator for DirectoryEntries<'_> {
    type Item = Result<DirectoryEntryHint, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        let (name, kind) = match next_directory_entry(&mut self.entries)? {
            Ok(entry) => entry,
            Err(error) => return Some(Err(error)),
        };
        Some(Ok(DirectoryEntryHint::new(name, kind)))
    }
}

pub(super) struct DirectoryNames<'directory> {
    entries: Dir,
    _authority: PhantomData<&'directory ReadDirectory>,
}

impl Iterator for DirectoryNames<'_> {
    type Item = Result<OsString, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        next_directory_entry(&mut self.entries).map(|entry| entry.map(|(name, _)| name))
    }
}

pub(super) fn open_directory(
    path: &Path,
    _policy: OpenPolicy,
) -> Result<ReadDirectory, AnchoredFsError> {
    let mut descriptor =
        openat(CWD, Path::new("/"), DIRECTORY_FLAGS, Mode::empty()).map_err(map_open_error)?;
    validate_directory(&descriptor)?;
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                descriptor = open_directory_descriptor(&descriptor, name)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid_path(
                    "anchored directory path contains an escaping component",
                ));
            }
        }
    }
    Ok(ReadDirectory { descriptor })
}

pub(super) fn open_directory_at(
    parent: &ReadDirectory,
    name: &OsStr,
    _policy: OpenPolicy,
) -> Result<ReadDirectory, AnchoredFsError> {
    validate_leaf(name)?;
    let descriptor = open_directory_descriptor(&parent.descriptor, name)?;
    Ok(ReadDirectory { descriptor })
}

pub(super) fn open_regular_at(
    parent: &ReadDirectory,
    name: &OsStr,
    policy: OpenPolicy,
) -> Result<(File, FileIdentity), AnchoredFsError> {
    validate_leaf(name)?;
    let descriptor = openat(&parent.descriptor, name, REGULAR_FILE_FLAGS, Mode::empty())
        .map_err(|source| classify_regular_open_error(&parent.descriptor, name, source))?;
    let metadata = fstat(&descriptor).map_err(io_error)?;
    let identity = regular_file_identity(&metadata, policy)?;
    let named = statat(&parent.descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| AnchoredFsError::IdentityChanged)?;
    let named_identity =
        regular_file_identity(&named, policy).map_err(|_| AnchoredFsError::IdentityChanged)?;
    if named_identity != identity {
        return Err(AnchoredFsError::IdentityChanged);
    }
    Ok((descriptor.into(), identity))
}

pub(super) fn opened_file_identity(
    file: &File,
    policy: OpenPolicy,
) -> Result<FileIdentity, AnchoredFsError> {
    let metadata = fstat(file.as_fd()).map_err(io_error)?;
    regular_file_identity(&metadata, policy)
}

pub(super) fn opened_directory_identity(
    directory: &ReadDirectory,
) -> Result<DirectoryIdentity, AnchoredFsError> {
    let metadata = fstat(&directory.descriptor).map_err(io_error)?;
    directory_identity(&metadata)
}

pub(super) fn opened_directory_object_identity(
    directory: &ReadDirectory,
) -> Result<DirectoryObjectIdentity, AnchoredFsError> {
    let identity = opened_directory_identity(directory)?;
    Ok(DirectoryObjectIdentity {
        device: identity.device,
        inode: identity.inode,
    })
}

pub(super) fn read_directory(
    directory: &ReadDirectory,
    _policy: OpenPolicy,
) -> Result<DirectoryEntries<'_>, AnchoredFsError> {
    let entries = Dir::read_from(&directory.descriptor).map_err(io_error)?;
    Ok(DirectoryEntries {
        entries,
        _authority: PhantomData,
    })
}

pub(super) fn read_directory_names(
    directory: &ReadDirectory,
    _policy: OpenPolicy,
) -> Result<DirectoryNames<'_>, AnchoredFsError> {
    let entries = Dir::read_from(&directory.descriptor).map_err(io_error)?;
    Ok(DirectoryNames {
        entries,
        _authority: PhantomData,
    })
}

pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(buffer, offset)
}

fn open_directory_descriptor(parent: &OwnedFd, name: &OsStr) -> Result<OwnedFd, AnchoredFsError> {
    validate_leaf(name)?;
    let descriptor =
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(map_open_error)?;
    let opened = fstat(&descriptor).map_err(io_error)?;
    if !FileType::from_raw_mode(opened.st_mode).is_dir() {
        return Err(AnchoredFsError::NotDirectory);
    }
    let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| AnchoredFsError::IdentityChanged)?;
    if !FileType::from_raw_mode(named.st_mode).is_dir()
        || named.st_dev != opened.st_dev
        || named.st_ino != opened.st_ino
    {
        return Err(AnchoredFsError::IdentityChanged);
    }
    Ok(descriptor)
}

fn validate_directory(descriptor: &OwnedFd) -> Result<(), AnchoredFsError> {
    let metadata = fstat(descriptor).map_err(io_error)?;
    if FileType::from_raw_mode(metadata.st_mode).is_dir() {
        Ok(())
    } else {
        Err(AnchoredFsError::NotDirectory)
    }
}

fn regular_file_identity(
    metadata: &Stat,
    policy: OpenPolicy,
) -> Result<FileIdentity, AnchoredFsError> {
    if !FileType::from_raw_mode(metadata.st_mode).is_file() {
        return Err(AnchoredFsError::NotRegular);
    }
    if policy.requires_single_link() && metadata.st_nlink != 1 {
        return Err(AnchoredFsError::IdentityChanged);
    }
    let length = u64::try_from(metadata.st_size).map_err(|_| {
        AnchoredFsError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "anchored regular file has a negative length",
        ))
    })?;
    let modified_seconds = checked_time_seconds(
        metadata.st_mtime,
        "anchored file modification time exceeds i64",
    )?;
    let modified_nanoseconds = checked_time_nanoseconds(
        metadata.st_mtime_nsec,
        "anchored file modification nanoseconds are invalid",
    )?;
    let changed_seconds =
        checked_time_seconds(metadata.st_ctime, "anchored file change time exceeds i64")?;
    let changed_nanoseconds = checked_time_nanoseconds(
        metadata.st_ctime_nsec,
        "anchored file change nanoseconds are invalid",
    )?;
    Ok(FileIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
        length,
        modified_seconds,
        modified_nanoseconds,
        changed_seconds,
        changed_nanoseconds,
    })
}

fn directory_identity(metadata: &Stat) -> Result<DirectoryIdentity, AnchoredFsError> {
    if !FileType::from_raw_mode(metadata.st_mode).is_dir() {
        return Err(AnchoredFsError::NotDirectory);
    }
    let modified_seconds = checked_time_seconds(
        metadata.st_mtime,
        "anchored directory modification time exceeds i64",
    )?;
    let modified_nanoseconds = checked_time_nanoseconds(
        metadata.st_mtime_nsec,
        "anchored directory modification nanoseconds are invalid",
    )?;
    let changed_seconds = checked_time_seconds(
        metadata.st_ctime,
        "anchored directory change time exceeds i64",
    )?;
    let changed_nanoseconds = checked_time_nanoseconds(
        metadata.st_ctime_nsec,
        "anchored directory change nanoseconds are invalid",
    )?;
    Ok(DirectoryIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
        modified_seconds,
        modified_nanoseconds,
        changed_seconds,
        changed_nanoseconds,
    })
}

fn entry_kind(file_type: FileType) -> EntryKindHint {
    if file_type.is_dir() {
        EntryKindHint::Directory
    } else if file_type.is_file() {
        EntryKindHint::RegularFile
    } else if file_type.is_symlink() {
        EntryKindHint::LinkOrReparse
    } else if file_type == FileType::Unknown {
        EntryKindHint::Unknown
    } else {
        EntryKindHint::Other
    }
}

fn next_directory_entry(
    entries: &mut Dir,
) -> Option<Result<(OsString, EntryKindHint), AnchoredFsError>> {
    loop {
        let entry = match entries.next()? {
            Ok(entry) => entry,
            Err(source) => return Some(Err(io_error(source))),
        };
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name != OsStr::new(".") && name != OsStr::new("..") {
            return Some(Ok((name.to_os_string(), entry_kind(entry.file_type()))));
        }
    }
}

fn validate_leaf(name: &OsStr) -> Result<(), AnchoredFsError> {
    let name = name.as_bytes();
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(invalid_path(
            "anchored child name is not a single path component",
        ));
    }
    Ok(())
}

fn map_open_error(source: Errno) -> AnchoredFsError {
    if source == Errno::LOOP {
        AnchoredFsError::LinkOrReparse
    } else if source == Errno::NOTDIR {
        AnchoredFsError::NotDirectory
    } else {
        io_error(source)
    }
}

fn classify_regular_open_error(parent: &OwnedFd, name: &OsStr, source: Errno) -> AnchoredFsError {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => match entry_kind(FileType::from_raw_mode(metadata.st_mode)) {
            EntryKindHint::LinkOrReparse => AnchoredFsError::LinkOrReparse,
            EntryKindHint::Directory | EntryKindHint::Other => AnchoredFsError::NotRegular,
            EntryKindHint::RegularFile | EntryKindHint::Unknown => map_open_error(source),
        },
        Err(_) => map_open_error(source),
    }
}

fn io_error(source: Errno) -> AnchoredFsError {
    AnchoredFsError::Io(source.into())
}

fn invalid_path(message: &'static str) -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn invalid_identity(message: &'static str) -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn checked_time_seconds<T>(value: T, message: &'static str) -> Result<i64, AnchoredFsError>
where
    T: TryInto<i64>,
{
    value.try_into().map_err(|_| invalid_identity(message))
}

fn checked_time_nanoseconds<T>(value: T, message: &'static str) -> Result<u32, AnchoredFsError>
where
    T: TryInto<u32>,
{
    let value = value.try_into().map_err(|_| invalid_identity(message))?;
    if value < 1_000_000_000 {
        Ok(value)
    } else {
        Err(invalid_identity(message))
    }
}
