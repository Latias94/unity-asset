//! Windows publication primitives.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Seek as _, SeekFrom};
use std::mem::{MaybeUninit, align_of, size_of, size_of_val};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Component, Components, Path, Prefix};

use serde::{Deserialize, Serialize};
use unity_asset_core::{AssetLoadBudget, DigestV1};

use super::super::journal::{RECOVERY_DIRECTORY, RECOVERY_VERSION_DIRECTORY};
use super::{
    COMMIT_LOCK_FILE, DirectoryEntryName, DirectoryVisitError, LEGACY_COMMIT_LOCK_DIRECTORY,
    SecurityMetadataError,
};

pub(super) const DIRECTORY_VISIT_SETUP_BYTES: u64 = 0;
pub(super) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = 0;
pub(super) const SECURITY_METADATA_COPY_RESERVATION_BYTES: u64 = 256 * 1024;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DIRECTORY_INFORMATION, FILE_DISPOSITION_INFORMATION,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_0, FILE_SYNCHRONOUS_IO_NONALERT,
    FileDirectoryInformation, FileDispositionInformation, FileRenameInformation, NtCreateFile,
    NtQueryDirectoryFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_LOCK_VIOLATION, ERROR_NO_TOKEN,
    ERROR_NOT_SAME_DEVICE, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
    STATUS_NO_MORE_FILES, STATUS_REPARSE_POINT_ENCOUNTERED, STATUS_STOPPED_ON_SYMLINK,
    STATUS_SUCCESS,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, EqualSid, GROUP_SECURITY_INFORMATION, GetKernelObjectSecurity,
    GetLengthSid, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetSecurityDescriptorGroup, GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, IsValidAcl, IsValidSecurityDescriptor, IsValidSid,
    OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR, SetKernelObjectSecurity,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_QUERY, TOKEN_USER, TokenUser, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FileAttributeTagInfo, FileIdInfo, FileStandardInfo,
    FlushFileBuffers, GetFileInformationByHandleEx, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
    WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};
use windows_sys::Win32::System::WindowsProgramming::{FILE_CREATED, FILE_OPENED};

const PINNED_DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const PINNED_FILE_SHARE: u32 = FILE_SHARE_READ;
const DIRECTORY_TRAVERSE_ACCESS: u32 =
    FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const DIRECTORY_DESTINATION_ACCESS: u32 =
    DIRECTORY_TRAVERSE_ACCESS | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY;
const DIRECTORY_SYNC_ACCESS: u32 = DIRECTORY_TRAVERSE_ACCESS | GENERIC_READ | GENERIC_WRITE;
const DIRECTORY_DESTINATION_SYNC_ACCESS: u32 =
    DIRECTORY_DESTINATION_ACCESS | GENERIC_READ | GENERIC_WRITE;
const PRIVATE_FILE_ACCESS: u32 = GENERIC_READ
    | GENERIC_WRITE
    | READ_CONTROL
    | WRITE_DAC
    | WRITE_OWNER
    | FILE_READ_ATTRIBUTES
    | SYNCHRONIZE;
const PRIVATE_DIRECTORY_ACCESS: u32 =
    DIRECTORY_TRAVERSE_ACCESS | READ_CONTROL | WRITE_DAC | WRITE_OWNER;
const SECURITY_METADATA_INFORMATION: u32 =
    OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
const PRIVATE_SECURITY_INFORMATION: u32 =
    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
const MAX_WINDOWS_ROOT_UTF16_UNITS: usize = 1_024;
const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;
const WINDOWS_ROOT_BUFFER_UTF16_UNITS: usize = MAX_WINDOWS_ROOT_UTF16_UNITS + 2;
const MAX_WINDOWS_SECURITY_DESCRIPTOR_BYTES: usize = 128 * 1024;

/// Stable identity of one opened regular file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileIdentity {
    #[serde(with = "super::hex_u64")]
    volume_serial_number: u64,
    #[serde(with = "super::hex_16_bytes")]
    file_id: [u8; 16],
    #[serde(with = "super::hex_u64")]
    length: u64,
}

impl FileIdentity {
    #[must_use]
    pub(super) const fn new(volume_serial_number: u64, file_id: [u8; 16], length: u64) -> Self {
        Self {
            volume_serial_number,
            file_id,
            length,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn volume_serial_number(&self) -> u64 {
        self.volume_serial_number
    }

    #[must_use]
    #[cfg(test)]
    pub(super) const fn file_id(&self) -> [u8; 16] {
        self.file_id
    }

    #[must_use]
    pub(super) const fn length(&self) -> u64 {
        self.length
    }
}

/// Stable identity of one opened directory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryIdentity {
    #[serde(with = "super::hex_u64")]
    volume_serial_number: u64,
    #[serde(with = "super::hex_16_bytes")]
    file_id: [u8; 16],
}

impl DirectoryIdentity {
    #[must_use]
    pub(super) const fn new(volume_serial_number: u64, file_id: [u8; 16]) -> Self {
        Self {
            volume_serial_number,
            file_id,
        }
    }
}

pub(super) struct CommitRoot {
    directory: OpenedDirectory,
}

pub(super) struct JournalNamespace {
    recovery: OpenedDirectory,
    version: OpenedDirectory,
}

pub(super) struct JournalDirectory {
    directory: OpenedDirectory,
}

pub(super) fn open_commit_root(
    path: &Path,
    expected: &DirectoryIdentity,
) -> io::Result<CommitRoot> {
    let directory = open_directory(path, DIRECTORY_DESTINATION_SYNC_ACCESS)?;
    verify_directory_identity(
        directory.raw(),
        expected,
        "publication root changed before writer lock acquisition",
    )?;
    Ok(CommitRoot { directory })
}

pub(super) fn acquire_commit_locks(root: &CommitRoot) -> io::Result<(File, File)> {
    let directory_security = PrivateSecurityDescriptor::new(true)?;
    let recovery = open_or_create_private_directory_at(
        root.directory.raw(),
        OsStr::new(RECOVERY_DIRECTORY),
        &directory_security,
    )?;
    let legacy = open_or_create_private_directory_at(
        recovery.raw(),
        OsStr::new(LEGACY_COMMIT_LOCK_DIRECTORY),
        &directory_security,
    )?;

    // Journal v1 placed its only writer lock under v1/. Newer protocols
    // retain that lock and also take a version-independent lock so old and
    // future binaries cannot publish concurrently into the same root.
    let legacy_file = acquire_lock_at(legacy.raw(), OsStr::new(COMMIT_LOCK_FILE))?;
    let stable_file = acquire_lock_at(recovery.raw(), OsStr::new(COMMIT_LOCK_FILE))?;
    Ok((legacy_file, stable_file))
}

pub(super) fn open_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    let directory_security = PrivateSecurityDescriptor::new(true)?;
    let recovery = open_or_create_private_directory_at(
        root.directory.raw(),
        OsStr::new(RECOVERY_DIRECTORY),
        &directory_security,
    )?;
    let version = open_or_create_private_directory_at(
        recovery.raw(),
        OsStr::new(RECOVERY_VERSION_DIRECTORY),
        &directory_security,
    )?;
    Ok(JournalNamespace {
        recovery: OpenedDirectory { handle: recovery },
        version: OpenedDirectory { handle: version },
    })
}

pub(super) fn open_existing_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    let (recovery, _) = open_directory_at(
        root.directory.raw(),
        OsStr::new(RECOVERY_DIRECTORY),
        DIRECTORY_DESTINATION_SYNC_ACCESS,
        FILE_OPEN,
        None,
    )?;
    let (version, _) = open_directory_at(
        recovery.raw(),
        OsStr::new(RECOVERY_VERSION_DIRECTORY),
        DIRECTORY_DESTINATION_SYNC_ACCESS,
        FILE_OPEN,
        None,
    )?;
    Ok(JournalNamespace {
        recovery: OpenedDirectory { handle: recovery },
        version: OpenedDirectory { handle: version },
    })
}

pub(super) fn open_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    open_journal_directory_at(namespace.version.raw(), name)
}

pub(super) fn open_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    open_journal_directory_at(parent.directory.raw(), name)
}

pub(super) fn create_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    create_journal_directory_at(namespace.version.raw(), name)
}

pub(super) fn create_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<JournalDirectory> {
    create_journal_directory_at(parent.directory.raw(), name)
}

pub(super) fn journal_directory_identity(
    directory: &JournalDirectory,
) -> io::Result<DirectoryIdentity> {
    directory_identity(directory.directory.raw())
}

pub(super) fn open_journal_regular(namespace: &JournalNamespace, name: &OsStr) -> io::Result<File> {
    open_journal_regular_at(namespace.version.raw(), name)
}

pub(super) fn open_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<File> {
    open_journal_regular_at(parent.directory.raw(), name)
}

pub(super) fn create_journal_regular(
    namespace: &JournalNamespace,
    name: &OsStr,
) -> io::Result<File> {
    create_journal_regular_at(namespace.version.raw(), name)
}

pub(super) fn create_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
) -> io::Result<File> {
    create_journal_regular_at(parent.directory.raw(), name)
}

pub(super) fn remove_journal_regular(
    namespace: &JournalNamespace,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    remove_journal_regular_at(namespace.version.raw(), name, expected)
}

pub(super) fn remove_journal_regular_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    remove_journal_regular_at(parent.directory.raw(), name, expected)
}

pub(super) fn remove_journal_directory(
    namespace: &JournalNamespace,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    remove_journal_directory_at(namespace.version.raw(), name, expected)
}

pub(super) fn remove_journal_directory_in_directory(
    parent: &JournalDirectory,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    remove_journal_directory_at(parent.directory.raw(), name, expected)
}

pub(super) fn atomic_replace_journal_regular(
    namespace: &JournalNamespace,
    source: &OsStr,
    destination: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_journal_regular_at(
        namespace.version.raw(),
        source,
        destination,
        replace_existing,
    )
}

pub(super) fn atomic_replace_journal_regular_in_directory(
    parent: &JournalDirectory,
    source: &OsStr,
    destination: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    atomic_replace_journal_regular_at(
        parent.directory.raw(),
        source,
        destination,
        replace_existing,
    )
}

pub(super) fn sync_journal_directory(directory: &JournalDirectory) -> io::Result<()> {
    flush_handle(directory.directory.raw())
}

pub(super) fn visit_journal_directory_entries<S, E>(
    directory: &JournalDirectory,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
    const DIRECTORY_BUFFER_WORDS: usize = DIRECTORY_BUFFER_BYTES / size_of::<u64>();
    const NEXT_ENTRY_OFFSET: usize =
        std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, NextEntryOffset);
    const FILE_NAME_LENGTH_OFFSET: usize =
        std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileNameLength);
    const FILE_NAME_OFFSET: usize = std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileName);

    let mut buffer = [0_u64; DIRECTORY_BUFFER_WORDS];
    let buffer_bytes = size_of::<[u64; DIRECTORY_BUFFER_WORDS]>();
    let buffer_length = u32::try_from(buffer_bytes).expect("directory enumeration buffer fits u32");
    let mut restart_scan = true;
    loop {
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: `directory` is an opened directory handle, the stack buffer
        // is aligned and writable for the announced length, and synchronous I/O
        // leaves no outstanding references after this call returns.
        let status = unsafe {
            NtQueryDirectoryFile(
                directory.directory.raw(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut io_status,
                buffer.as_mut_ptr().cast(),
                buffer_length,
                FileDirectoryInformation,
                false,
                std::ptr::null(),
                restart_scan,
            )
        };
        restart_scan = false;
        if status == STATUS_NO_MORE_FILES {
            break;
        }
        if status != STATUS_SUCCESS {
            return Err(DirectoryVisitError::Io(io::Error::from_raw_os_error(
                i32::try_from(unsafe { RtlNtStatusToDosError(status) }).unwrap_or(i32::MAX),
            )));
        }
        let returned = io_status.Information;
        if returned == 0 || returned > buffer_bytes {
            return Err(DirectoryVisitError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows directory enumeration returned an invalid byte count",
            )));
        }

        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0_usize;
        loop {
            let remaining = returned.checked_sub(offset).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory enumeration offset overflowed",
                ))
            })?;
            if remaining < FILE_NAME_OFFSET {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory enumeration record is truncated",
                )));
            }
            // SAFETY: `remaining` covers the fixed header, including both
            // u32 fields. Reading the generated C struct itself would also
            // read its one-element flexible-array placeholder and any Rust
            // tail padding beyond a short final record.
            let (next_entry_offset, file_name_length) = unsafe {
                (
                    std::ptr::read_unaligned(base.add(offset + NEXT_ENTRY_OFFSET).cast::<u32>()),
                    std::ptr::read_unaligned(
                        base.add(offset + FILE_NAME_LENGTH_OFFSET).cast::<u32>(),
                    ),
                )
            };
            let name_bytes = usize::try_from(file_name_length).map_err(|_| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry name length is unsupported",
                ))
            })?;
            if name_bytes % size_of::<u16>() != 0 {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry name is not UTF-16 aligned",
                )));
            }
            let record_bytes = FILE_NAME_OFFSET.checked_add(name_bytes).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry length overflowed",
                ))
            })?;
            if record_bytes > remaining {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry extends beyond the query buffer",
                )));
            }
            // SAFETY: `record_bytes` bounds the UTF-16 payload and the query
            // buffer alignment is sufficient for `u16` reads.
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + FILE_NAME_OFFSET).cast::<u16>(),
                    name_bytes / size_of::<u16>(),
                )
            };
            before_entry(state).map_err(DirectoryVisitError::Visitor)?;
            visitor(state, DirectoryEntryName::Windows(name))
                .map_err(DirectoryVisitError::Visitor)?;

            let next = usize::try_from(next_entry_offset).map_err(|_| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset is unsupported",
                ))
            })?;
            if next == 0 {
                break;
            }
            if next < record_bytes || next > remaining || next % align_of::<u64>() != 0 {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset is invalid",
                )));
            }
            offset = offset.checked_add(next).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset overflowed",
                ))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn journal_namespace_version_identity(
    namespace: &JournalNamespace,
) -> io::Result<DirectoryIdentity> {
    directory_identity(namespace.version.raw())
}

pub(super) fn sync_journal_namespace(
    root: &CommitRoot,
    namespace: &JournalNamespace,
) -> io::Result<()> {
    flush_handle(namespace.version.raw())?;
    flush_handle(namespace.recovery.raw())?;
    flush_handle(root.directory.raw())
}

fn open_journal_directory_at(parent: HANDLE, name: &OsStr) -> io::Result<JournalDirectory> {
    let (handle, _) = open_directory_at(
        parent,
        name,
        DIRECTORY_DESTINATION_SYNC_ACCESS,
        FILE_OPEN,
        None,
    )?;
    Ok(JournalDirectory {
        directory: OpenedDirectory { handle },
    })
}

fn create_journal_directory_at(parent: HANDLE, name: &OsStr) -> io::Result<JournalDirectory> {
    let private_security = PrivateSecurityDescriptor::new(true)?;
    let (directory, information) = open_directory_at(
        parent,
        name,
        PRIVATE_DIRECTORY_ACCESS | DIRECTORY_DESTINATION_SYNC_ACCESS,
        FILE_CREATE,
        Some(private_security.as_ptr()),
    )?;
    if information != FILE_CREATED as usize {
        return Err(io::Error::other(
            "Windows exclusive private journal directory returned an unexpected create disposition",
        ));
    }
    private_security.apply_and_verify(directory.raw())?;
    Ok(JournalDirectory {
        directory: OpenedDirectory { handle: directory },
    })
}

fn open_journal_regular_at(parent: HANDLE, name: &OsStr) -> io::Result<File> {
    let opened = open_regular_at(
        parent,
        name,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "journal entry",
    )?;
    reject_mutable_hardlink(opened.raw(), "opened journal entry")?;
    Ok(opened.into_file())
}

fn create_journal_regular_at(parent: HANDLE, name: &OsStr) -> io::Result<File> {
    let private_security = PrivateSecurityDescriptor::new(false)?;
    let (handle, information) = nt_create_at(
        parent,
        name,
        PRIVATE_FILE_ACCESS,
        0,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        Some(private_security.as_ptr()),
    )?;
    validate_regular_handle(handle.raw(), "Windows private journal file")?;
    if information != FILE_CREATED as usize {
        return Err(io::Error::other(
            "Windows exclusive private journal file returned an unexpected create disposition",
        ));
    }
    private_security.apply_and_verify(handle.raw())?;
    reject_mutable_hardlink(handle.raw(), "created journal entry")?;
    Ok(handle.into_file())
}

fn remove_journal_regular_at(
    parent: HANDLE,
    name: &OsStr,
    expected: &FileIdentity,
) -> io::Result<()> {
    let (file, _) = nt_create_at(
        parent,
        name,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        PINNED_FILE_SHARE,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    validate_regular_handle(
        file.raw(),
        "owned journal cleanup file is not a regular file",
    )?;
    verify_file_identity(
        file.raw(),
        expected,
        "owned journal cleanup file no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(file.raw(), "owned journal cleanup file")?;
    mark_for_delete_on_close(file.raw())?;
    file.close()?;
    flush_handle(parent)
}

fn remove_journal_directory_at(
    parent: HANDLE,
    name: &OsStr,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    let (directory, _) =
        open_directory_at(parent, name, DELETE | FILE_READ_ATTRIBUTES, FILE_OPEN, None)?;
    verify_directory_identity(
        directory.raw(),
        expected,
        "owned journal directory no longer matches its captured identity",
    )?;
    mark_for_delete_on_close(directory.raw())?;
    directory.close()?;
    flush_handle(parent)
}

fn atomic_replace_journal_regular_at(
    parent: HANDLE,
    source_name: &OsStr,
    destination_name: &OsStr,
    replace_existing: bool,
) -> Result<(), super::AtomicMoveError> {
    let mut moved = false;
    let result = (|| {
        let source = open_regular_at(
            parent,
            source_name,
            DELETE | FILE_READ_ATTRIBUTES,
            PINNED_FILE_SHARE,
            "journal temporary entry",
        )?;
        let expected = file_identity(source.raw())?;
        reject_mutable_hardlink(source.raw(), "journal temporary entry")?;
        let rename = RenameInformation::new(destination_name, parent, replace_existing)?;
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                source.raw(),
                &raw mut io_status,
                rename.as_ptr(),
                rename.byte_len(),
                FileRenameInformation,
            )
        };
        ntstatus_result(status, "rename journal temporary entry")?;
        moved = true;
        verify_file_identity(
            source.raw(),
            &expected,
            "journal destination does not match the promoted temporary entry",
        )?;
        reject_mutable_hardlink(source.raw(), "promoted journal entry")?;
        flush_handle(parent)
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
pub(super) fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut path = AbsolutePathParts::new(path)?;
    if !path.has_components() {
        return Err(invalid_path(
            "private directory path must name a child directory",
        ));
    }

    let private_security = PrivateSecurityDescriptor::new(true)?;
    let mut directory = open_root(path.root(), DIRECTORY_TRAVERSE_ACCESS)?;

    let mut private_scope = false;
    while let Some(name) = path.next_component()? {
        let is_last = !path.has_components();
        let should_be_private = private_scope || name == OsStr::new(RECOVERY_DIRECTORY) || is_last;

        let child = if should_be_private {
            private_scope = true;
            open_or_create_private_directory_at(directory.raw(), name, &private_security)?
        } else {
            match open_directory_at(
                directory.raw(),
                name,
                DIRECTORY_TRAVERSE_ACCESS,
                FILE_OPEN,
                None,
            ) {
                Ok((handle, _)) => handle,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    private_scope = true;
                    open_or_create_private_directory_at(directory.raw(), name, &private_security)?
                }
                Err(error) => return Err(error),
            }
        };
        // A child opened relative to `directory` remains bound to that exact
        // object after its parent handle is released.
        directory = child;
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn create_private_directory_exclusive(path: &Path) -> io::Result<DirectoryIdentity> {
    let (parent_path, _) = split_leaf(path)?;
    let expected_parent = observe_directory_identity(parent_path)?;
    create_private_directory_exclusive_in_parent(path, &expected_parent)
}

#[cfg(test)]
pub(super) fn create_private_directory_exclusive_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<DirectoryIdentity> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_ACCESS)?;
    let actual_parent = directory_identity(parent.raw())?;
    if &actual_parent != expected_parent {
        return Err(identity_changed(
            "private directory parent no longer matches its captured identity",
        ));
    }
    let private_security = PrivateSecurityDescriptor::new(true)?;
    let (directory, information) = open_directory_at(
        parent.raw(),
        name,
        PRIVATE_DIRECTORY_ACCESS,
        FILE_CREATE,
        Some(private_security.as_ptr()),
    )?;
    if information != FILE_CREATED as usize {
        return Err(io::Error::other(
            "Windows exclusive private directory returned an unexpected create disposition",
        ));
    }
    private_security.apply_and_verify(directory.raw())?;
    directory_identity(directory.raw())
}

#[cfg(test)]
pub(super) fn create_private_file(path: &Path) -> io::Result<File> {
    let private_security = PrivateSecurityDescriptor::new(false)?;
    let opened = create_or_open_private_file(path, FILE_CREATE, &private_security)?;
    private_security.verify(opened.handle.raw())?;
    reject_mutable_hardlink(opened.handle.raw(), "created private file")?;
    Ok(opened.handle.into_file())
}

#[cfg(test)]
pub(super) fn create_private_file_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_ACCESS)?;
    let actual_parent = directory_identity(parent.raw())?;
    if &actual_parent != expected_parent {
        return Err(identity_changed(
            "private file parent no longer matches its captured identity",
        ));
    }

    let private_security = PrivateSecurityDescriptor::new(false)?;
    let (handle, information) = nt_create_at(
        parent.raw(),
        name,
        PRIVATE_FILE_ACCESS,
        0,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        Some(private_security.as_ptr()),
    )?;
    validate_regular_handle(handle.raw(), "Windows private file")?;
    if information != FILE_CREATED as usize {
        return Err(io::Error::other(
            "Windows exclusive private file returned an unexpected create disposition",
        ));
    }
    private_security.apply_and_verify(handle.raw())?;
    reject_mutable_hardlink(handle.raw(), "created private file")?;
    Ok(handle.into_file())
}

#[cfg(test)]
pub(super) fn remove_owned_file_in_parent(
    path: &Path,
    expected_file: &FileIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_SYNC_ACCESS)?;
    verify_directory_identity(
        parent.raw(),
        expected_parent,
        "owned file parent no longer matches its captured identity",
    )?;
    let (file, _) = nt_create_at(
        parent.raw(),
        name,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        PINNED_FILE_SHARE,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    validate_regular_handle(file.raw(), "owned cleanup file is not a regular file")?;
    verify_file_identity(
        file.raw(),
        expected_file,
        "owned cleanup file no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(file.raw(), "owned cleanup file")?;
    mark_for_delete_on_close(file.raw())?;
    file.close()?;
    flush_handle(parent.raw())
}

#[cfg(test)]
pub(super) fn remove_owned_empty_directory_in_parent(
    path: &Path,
    expected_directory: &DirectoryIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_SYNC_ACCESS)?;
    verify_directory_identity(
        parent.raw(),
        expected_parent,
        "owned directory parent no longer matches its captured identity",
    )?;
    let (directory, _) = open_directory_at(
        parent.raw(),
        name,
        DELETE | FILE_READ_ATTRIBUTES,
        FILE_OPEN,
        None,
    )?;
    verify_directory_identity(
        directory.raw(),
        expected_directory,
        "owned cleanup directory no longer matches its captured identity",
    )?;
    mark_for_delete_on_close(directory.raw())?;
    directory.close()?;
    flush_handle(parent.raw())
}

pub(super) fn open_readonly_regular_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_TRAVERSE_ACCESS)?;
    verify_directory_identity(
        parent.raw(),
        expected_parent,
        "opened journal entry parent no longer matches its captured identity",
    )?;
    let opened = open_regular_at(
        parent.raw(),
        name,
        GENERIC_READ | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "journal entry",
    )?;
    reject_mutable_hardlink(opened.raw(), "opened journal entry")?;
    Ok(opened.into_file())
}

#[cfg(test)]
pub(super) fn acquire_lock(path: &Path) -> io::Result<File> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_ACCESS)?;
    acquire_lock_at(parent.raw(), name)
}

fn acquire_lock_at(parent: HANDLE, name: &OsStr) -> io::Result<File> {
    let private_security = PrivateSecurityDescriptor::new(false)?;
    let handle = create_or_open_private_file_at(parent, name, FILE_OPEN_IF, &private_security)
        .map_err(normalize_lock_contention)?;
    private_security.apply_and_verify(handle.raw())?;
    reject_mutable_hardlink(handle.raw(), "publication lock")?;
    Ok(handle.into_file())
}

/// Takes an existing lock with the same no-reparse and sharing guarantees as
/// publication, without creating or changing the lock file's security state.
pub(super) fn acquire_existing_lock(path: &Path) -> io::Result<File> {
    let opened = open_regular(
        path,
        GENERIC_READ | GENERIC_WRITE,
        PINNED_FILE_SHARE,
        "publication lock",
    )
    .map_err(normalize_lock_contention)?;
    reject_mutable_hardlink(opened.handle.raw(), "publication lock")?;
    Ok(opened.handle.into_file())
}

fn normalize_lock_contention(error: io::Error) -> io::Error {
    let is_contention = matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_LOCK_VIOLATION as i32
    );
    if is_contention {
        io::Error::new(io::ErrorKind::WouldBlock, error)
    } else {
        error
    }
}

pub(super) fn visit_existing_directory_entries<S, E>(
    path: &Path,
    expected: &DirectoryIdentity,
    state: &mut S,
    mut before_entry: impl FnMut(&mut S) -> Result<(), E>,
    mut visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    const DIRECTORY_BUFFER_BYTES: usize = 64 * 1024;
    const DIRECTORY_BUFFER_WORDS: usize = DIRECTORY_BUFFER_BYTES / size_of::<u64>();
    const NEXT_ENTRY_OFFSET: usize =
        std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, NextEntryOffset);
    const FILE_NAME_LENGTH_OFFSET: usize =
        std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileNameLength);
    const FILE_NAME_OFFSET: usize = std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileName);

    let directory =
        open_directory(path, DIRECTORY_TRAVERSE_ACCESS).map_err(DirectoryVisitError::Io)?;
    verify_directory_identity(
        directory.raw(),
        expected,
        "directory changed before recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)?;

    let mut buffer = [0_u64; DIRECTORY_BUFFER_WORDS];
    let buffer_bytes = size_of::<[u64; DIRECTORY_BUFFER_WORDS]>();
    let buffer_length = u32::try_from(buffer_bytes).expect("directory enumeration buffer fits u32");
    let mut restart_scan = true;
    loop {
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: `directory` is an opened directory handle, the stack buffer
        // is aligned and writable for the announced length, and synchronous I/O
        // leaves no outstanding references after this call returns.
        let status = unsafe {
            NtQueryDirectoryFile(
                directory.raw(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut io_status,
                buffer.as_mut_ptr().cast(),
                buffer_length,
                FileDirectoryInformation,
                false,
                std::ptr::null(),
                restart_scan,
            )
        };
        restart_scan = false;
        if status == STATUS_NO_MORE_FILES {
            break;
        }
        if status != STATUS_SUCCESS {
            return Err(DirectoryVisitError::Io(io::Error::from_raw_os_error(
                i32::try_from(unsafe { RtlNtStatusToDosError(status) }).unwrap_or(i32::MAX),
            )));
        }
        let returned = io_status.Information;
        if returned == 0 || returned > buffer_bytes {
            return Err(DirectoryVisitError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows directory enumeration returned an invalid byte count",
            )));
        }

        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0_usize;
        loop {
            let remaining = returned.checked_sub(offset).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory enumeration offset overflowed",
                ))
            })?;
            if remaining < FILE_NAME_OFFSET {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory enumeration record is truncated",
                )));
            }
            // SAFETY: `remaining` covers the fixed header, including both
            // u32 fields. Reading the generated C struct itself would also
            // read its one-element flexible-array placeholder and any Rust
            // tail padding beyond a short final record.
            let (next_entry_offset, file_name_length) = unsafe {
                (
                    std::ptr::read_unaligned(base.add(offset + NEXT_ENTRY_OFFSET).cast::<u32>()),
                    std::ptr::read_unaligned(
                        base.add(offset + FILE_NAME_LENGTH_OFFSET).cast::<u32>(),
                    ),
                )
            };
            let name_bytes = usize::try_from(file_name_length).map_err(|_| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry name length is unsupported",
                ))
            })?;
            if name_bytes % size_of::<u16>() != 0 {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry name is not UTF-16 aligned",
                )));
            }
            let record_bytes = FILE_NAME_OFFSET.checked_add(name_bytes).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry length overflowed",
                ))
            })?;
            if record_bytes > remaining {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry extends beyond the query buffer",
                )));
            }
            // SAFETY: `record_bytes` bounds the UTF-16 payload and the query
            // buffer alignment is sufficient for `u16` reads.
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + FILE_NAME_OFFSET).cast::<u16>(),
                    name_bytes / size_of::<u16>(),
                )
            };
            before_entry(state).map_err(DirectoryVisitError::Visitor)?;
            visitor(state, DirectoryEntryName::Windows(name))
                .map_err(DirectoryVisitError::Visitor)?;

            let next = usize::try_from(next_entry_offset).map_err(|_| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset is unsupported",
                ))
            })?;
            if next == 0 {
                break;
            }
            if next < record_bytes || next > remaining || next % align_of::<u64>() != 0 {
                return Err(DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset is invalid",
                )));
            }
            offset = offset.checked_add(next).ok_or_else(|| {
                DirectoryVisitError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows directory entry offset overflowed",
                ))
            })?;
        }
    }

    verify_directory_identity(
        directory.raw(),
        expected,
        "directory changed during recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)?;
    let reopened =
        open_directory(path, DIRECTORY_TRAVERSE_ACCESS).map_err(DirectoryVisitError::Io)?;
    verify_directory_identity(
        reopened.raw(),
        expected,
        "directory path changed during recovery discovery enumeration",
    )
    .map_err(DirectoryVisitError::Io)
}

#[cfg(test)]
pub(super) fn observe_file_identity(path: &Path) -> io::Result<FileIdentity> {
    let opened = open_regular(
        path,
        FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "file identity source",
    )?;
    file_identity(opened.handle.raw())
}

pub(super) fn observe_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    let opened = open_directory(path, DIRECTORY_TRAVERSE_ACCESS)?;
    directory_identity(opened.raw())
}

pub(super) fn opened_file_identity(file: &File) -> io::Result<FileIdentity> {
    file_identity(file.as_raw_handle())
}

#[cfg(test)]
pub(super) fn ensure_same_filesystem(first: &Path, second: &Path) -> io::Result<()> {
    let first = open_existing_node(first)?;
    let second = open_existing_node(second)?;
    let first_volume = file_id_information(first.handle.raw())?.VolumeSerialNumber;
    let second_volume = file_id_information(second.handle.raw())?.VolumeSerialNumber;
    ensure_same_volume(first_volume, second_volume)
}

pub(super) fn ensure_journal_directory_same_filesystem(
    directory: &JournalDirectory,
    anchor: &Path,
) -> io::Result<()> {
    let anchor = open_existing_node(anchor)?;
    let directory_volume = file_id_information(directory.directory.raw())?.VolumeSerialNumber;
    let anchor_volume = file_id_information(anchor.handle.raw())?.VolumeSerialNumber;
    ensure_same_volume(directory_volume, anchor_volume)
}

pub(super) fn ensure_single_hardlink(path: &Path) -> io::Result<()> {
    let opened = open_regular(
        path,
        FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "hard-link validation source",
    )?;
    reject_mutable_hardlink(opened.handle.raw(), "publication source")
}

#[cfg(test)]
pub(super) fn copy_security_metadata(
    source: &Path,
    destination: &Path,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination: &FileIdentity,
    expected_destination_parent: &DirectoryIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let (source_parent_path, source_name) = split_leaf(source)?;
    let (destination_parent_path, destination_name) = split_leaf(destination)?;
    let source_parent = open_directory(source_parent_path, DIRECTORY_TRAVERSE_ACCESS)?;
    let destination_parent = open_directory(destination_parent_path, DIRECTORY_TRAVERSE_ACCESS)?;
    verify_directory_identity(
        source_parent.raw(),
        expected_source_parent,
        "security metadata source parent no longer matches its captured identity",
    )?;
    verify_directory_identity(
        destination_parent.raw(),
        expected_destination_parent,
        "security metadata destination parent no longer matches its captured identity",
    )?;
    let source = open_regular_at(
        source_parent.raw(),
        source_name,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "security metadata source",
    )?;
    let destination = open_regular_at(
        destination_parent.raw(),
        destination_name,
        GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "security metadata destination",
    )?;
    verify_file_identity(
        source.raw(),
        expected_source,
        "security metadata source no longer matches its captured identity",
    )?;
    verify_file_identity(
        destination.raw(),
        expected_destination,
        "security metadata destination no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(source.raw(), "security metadata source")?;
    reject_mutable_hardlink(destination.raw(), "security metadata destination")?;

    let metadata = SecuritySnapshot::capture_budgeted(source.raw(), budget).map_err(|error| {
        map_security_metadata_error("read owner, group, and DACL from source", error)
    })?;
    metadata
        .apply_and_verify_budgeted(destination.raw(), budget)
        .map_err(|error| {
            map_security_metadata_error("apply owner, group, and DACL to destination", error)
        })?;
    Ok(flush_handle(destination.raw())?)
}

pub(super) fn copy_security_metadata_between_journal_directories(
    source_directory: &JournalDirectory,
    source_name: &OsStr,
    destination_directory: &JournalDirectory,
    destination_name: &OsStr,
    expected_source: &FileIdentity,
    expected_destination: &FileIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let source = open_regular_at(
        source_directory.directory.raw(),
        source_name,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "journal security metadata source",
    )?;
    let destination = open_regular_at(
        destination_directory.directory.raw(),
        destination_name,
        GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "journal security metadata destination",
    )?;
    verify_file_identity(
        source.raw(),
        expected_source,
        "journal security metadata source no longer matches its captured identity",
    )?;
    verify_file_identity(
        destination.raw(),
        expected_destination,
        "journal security metadata destination no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(source.raw(), "journal security metadata source")?;
    reject_mutable_hardlink(destination.raw(), "journal security metadata destination")?;

    let metadata = SecuritySnapshot::capture_budgeted(source.raw(), budget).map_err(|error| {
        map_security_metadata_error("read owner, group, and DACL from journal source", error)
    })?;
    metadata
        .apply_and_verify_budgeted(destination.raw(), budget)
        .map_err(|error| {
            map_security_metadata_error(
                "apply owner, group, and DACL to journal destination",
                error,
            )
        })?;
    Ok(flush_handle(destination.raw())?)
}

pub(super) fn copy_security_metadata_external_to_journal_directory(
    source: &Path,
    destination_directory: &JournalDirectory,
    destination_name: &OsStr,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination: &FileIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    let (source_parent_path, source_name) = split_leaf(source)?;
    let source_parent = open_directory(source_parent_path, DIRECTORY_SYNC_ACCESS)?;
    verify_directory_identity(
        source_parent.raw(),
        expected_source_parent,
        "security metadata source parent no longer matches its captured identity",
    )?;
    let source = open_regular_at(
        source_parent.raw(),
        source_name,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "security metadata source",
    )?;
    let destination = open_regular_at(
        destination_directory.directory.raw(),
        destination_name,
        GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        PINNED_FILE_SHARE,
        "journal security metadata destination",
    )?;
    verify_file_identity(
        source.raw(),
        expected_source,
        "security metadata source no longer matches its captured identity",
    )?;
    verify_file_identity(
        destination.raw(),
        expected_destination,
        "journal security metadata destination no longer matches its captured identity",
    )?;
    reject_mutable_hardlink(source.raw(), "security metadata source")?;
    reject_mutable_hardlink(destination.raw(), "journal security metadata destination")?;

    let metadata = SecuritySnapshot::capture_budgeted(source.raw(), budget).map_err(|error| {
        map_security_metadata_error("read owner, group, and DACL from source", error)
    })?;
    metadata
        .apply_and_verify_budgeted(destination.raw(), budget)
        .map_err(|error| {
            map_security_metadata_error(
                "apply owner, group, and DACL to journal destination",
                error,
            )
        })?;
    Ok(flush_handle(destination.raw())?)
}

#[cfg(test)]
pub(super) fn capture_existing(
    source: &Path,
    destination: &Path,
    expected_file: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    match expected_digest {
        Some(expected_digest) => rename_verified_digest(
            source,
            destination,
            false,
            expected_file,
            expected_digest,
            expected_source_parent,
            expected_destination_parent,
        ),
        None => rename_verified(
            source,
            destination,
            false,
            expected_file,
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
    let source_parent = open_directory(source_parent_path, DIRECTORY_SYNC_ACCESS)?;
    rename_verified_opened(
        &source_parent,
        source_name,
        &destination.directory,
        destination_name,
        false,
        RenameVerification {
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
    let destination_parent =
        open_directory(destination_parent_path, DIRECTORY_DESTINATION_SYNC_ACCESS)?;
    rename_verified_opened(
        &source.directory,
        source_name,
        &destination_parent,
        destination_name,
        false,
        RenameVerification {
            expected_source: Some(expected_source),
            expected_digest,
            expected_source_parent: None,
            expected_destination_parent: Some(expected_destination_parent),
        },
    )
    .map_err(super::AtomicMoveError::into_error)
}

#[cfg(test)]
pub(super) fn atomic_replace_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), super::AtomicMoveError> {
    rename_verified_tracked(
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
fn rename_verified(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    rename_verified_tracked(
        source,
        destination,
        replace_existing,
        Some(expected),
        None,
        expected_source_parent,
        expected_destination_parent,
    )
    .map_err(super::AtomicMoveError::into_error)
}

#[cfg(test)]
fn rename_verified_digest(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected: &FileIdentity,
    expected_digest: DigestV1,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    rename_verified_tracked(
        source,
        destination,
        replace_existing,
        Some(expected),
        Some(expected_digest),
        expected_source_parent,
        expected_destination_parent,
    )
    .map_err(super::AtomicMoveError::into_error)
}

#[cfg(test)]
fn rename_verified_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected: Option<&FileIdentity>,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), super::AtomicMoveError> {
    let (source_parent_path, source_name) = match split_leaf(source) {
        Ok(parts) => parts,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let source_parent = match open_directory(source_parent_path, DIRECTORY_SYNC_ACCESS) {
        Ok(directory) => directory,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let (destination_parent_path, destination_name) = match split_leaf(destination) {
        Ok(parts) => parts,
        Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
    };
    let destination_parent =
        match open_directory(destination_parent_path, DIRECTORY_DESTINATION_SYNC_ACCESS) {
            Ok(directory) => directory,
            Err(error) => return Err(super::AtomicMoveError::not_moved(error)),
        };
    rename_verified_opened(
        &source_parent,
        source_name,
        &destination_parent,
        destination_name,
        replace_existing,
        RenameVerification {
            expected_source: expected,
            expected_digest,
            expected_source_parent: Some(expected_source_parent),
            expected_destination_parent: Some(expected_destination_parent),
        },
    )
}

struct RenameVerification<'a> {
    expected_source: Option<&'a FileIdentity>,
    expected_digest: Option<DigestV1>,
    expected_source_parent: Option<&'a DirectoryIdentity>,
    expected_destination_parent: Option<&'a DirectoryIdentity>,
}

fn rename_verified_opened(
    source_parent: &OpenedDirectory,
    source_name: &OsStr,
    destination_parent: &OpenedDirectory,
    destination_name: &OsStr,
    replace_existing: bool,
    verification: RenameVerification<'_>,
) -> Result<(), super::AtomicMoveError> {
    let mut moved = false;
    let result = (|| {
        if let Some(expected_source_parent) = verification.expected_source_parent {
            verify_directory_identity(
                source_parent.raw(),
                expected_source_parent,
                "atomic publication source parent no longer matches its captured identity",
            )?;
        }
        let source_access = DELETE
            | FILE_READ_ATTRIBUTES
            | if verification.expected_digest.is_some() {
                GENERIC_READ
            } else {
                0
            };
        let source = open_regular_at(
            source_parent.raw(),
            source_name,
            source_access,
            PINNED_FILE_SHARE,
            "atomic publication source",
        )?;
        let actual = file_identity(source.raw())?;
        reject_mutable_hardlink(source.raw(), "atomic publication source")?;
        let expected = verification.expected_source.unwrap_or(&actual);
        verify_file_identity(
            source.raw(),
            expected,
            "atomic publication source no longer matches its captured identity",
        )?;
        let mut source_reader = match verification.expected_digest {
            Some(expected_digest) => {
                let mut reader = duplicate_handle_for_identity(source.raw())?.into_file();
                validate_opened_digest(
                    &mut reader,
                    expected_digest,
                    expected.length,
                    "atomic publication source content changed before rename",
                )?;
                verify_file_identity(
                    source.raw(),
                    expected,
                    "atomic publication source changed while its content was verified",
                )?;
                Some(reader)
            }
            None => None,
        };

        let actual_destination_parent = directory_identity(destination_parent.raw())?;
        if let Some(expected_destination_parent) = verification.expected_destination_parent
            && &actual_destination_parent != expected_destination_parent
        {
            return Err(identity_changed(
                "atomic publication destination parent no longer matches its captured identity",
            ));
        }
        ensure_same_volume(
            actual.volume_serial_number,
            actual_destination_parent.volume_serial_number,
        )?;

        let rename =
            RenameInformation::new(destination_name, destination_parent.raw(), replace_existing)?;
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtSetInformationFile(
                source.raw(),
                &raw mut io_status,
                rename.as_ptr(),
                rename.byte_len(),
                FileRenameInformation,
            )
        };
        ntstatus_result(status, "rename publication source")?;
        moved = true;

        verify_file_identity(
            source.raw(),
            expected,
            "atomic publication source identity changed during rename",
        )?;
        reject_mutable_hardlink(source.raw(), "promoted publication source")?;
        if let (Some(expected_digest), Some(reader)) =
            (verification.expected_digest, &mut source_reader)
        {
            validate_opened_digest(
                reader,
                expected_digest,
                expected.length,
                "atomic publication source content changed during rename",
            )?;
        }
        if let Some(expected_source_parent) = verification.expected_source_parent {
            verify_directory_identity(
                source_parent.raw(),
                expected_source_parent,
                "atomic publication source parent changed during publication",
            )?;
        }
        if let Some(expected_destination_parent) = verification.expected_destination_parent {
            verify_directory_identity(
                destination_parent.raw(),
                expected_destination_parent,
                "atomic publication destination parent changed during publication",
            )?;
        }
        flush_handle(source_parent.raw())?;
        flush_handle(destination_parent.raw())
    })();
    result.map_err(|source| {
        if moved {
            super::AtomicMoveError::moved_or_unknown(source)
        } else {
            super::AtomicMoveError::not_moved(source)
        }
    })
}

struct AbsolutePathParts<'path> {
    root: &'path OsStr,
    components: Components<'path>,
}

impl<'path> AbsolutePathParts<'path> {
    fn new(path: &'path Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid_path("Windows publication path must be absolute"));
        }

        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(invalid_path(
                "Windows publication path has no supported prefix",
            ));
        };
        match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(_, _) => {}
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                return Err(invalid_path(
                    "Windows publication path uses an unsupported device namespace",
                ));
            }
        }
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(invalid_path(
                "Windows publication path must be rooted after its prefix",
            ));
        }

        let root = prefix.as_os_str();
        validate_root(root)?;
        for component in components.clone() {
            match component {
                Component::Normal(name) => {
                    validate_leaf(name)?;
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(invalid_path(
                        "Windows publication path contains a relative component",
                    ));
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(invalid_path(
                        "Windows publication path contains a second root",
                    ));
                }
            }
        }
        Ok(Self { root, components })
    }

    fn root(&self) -> &'path OsStr {
        self.root
    }

    fn has_components(&self) -> bool {
        self.components.clone().next().is_some()
    }

    fn next_component(&mut self) -> io::Result<Option<&'path OsStr>> {
        match self.components.next() {
            Some(Component::Normal(name)) => Ok(Some(name)),
            Some(Component::CurDir | Component::ParentDir) => Err(invalid_path(
                "Windows publication path contains a relative component",
            )),
            Some(Component::Prefix(_) | Component::RootDir) => Err(invalid_path(
                "Windows publication path contains a second root",
            )),
            None => Ok(None),
        }
    }
}

struct OpenedDirectory {
    handle: OwnedHandle,
}

impl OpenedDirectory {
    fn raw(&self) -> HANDLE {
        self.handle.raw()
    }
}

struct OpenedNode {
    _parent: Option<OpenedDirectory>,
    handle: OwnedHandle,
}

struct OpenedRegular {
    _parent: OpenedDirectory,
    handle: OwnedHandle,
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }

    fn close(mut self) -> io::Result<()> {
        let raw = std::mem::replace(&mut self.0, INVALID_HANDLE_VALUE);
        let succeeded = unsafe { CloseHandle(raw) };
        if succeeded == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn into_file(self) -> File {
        let raw = self.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn open_directory(path: &Path, final_access: u32) -> io::Result<OpenedDirectory> {
    let mut path = AbsolutePathParts::new(path)?;
    let root_access = if path.has_components() {
        DIRECTORY_TRAVERSE_ACCESS
    } else {
        final_access
    };
    let mut directory = open_root(path.root(), root_access)?;

    while let Some(name) = path.next_component()? {
        let access = if path.has_components() {
            DIRECTORY_TRAVERSE_ACCESS
        } else {
            final_access
        };
        let (child, _) = open_directory_at(directory.raw(), name, access, FILE_OPEN, None)?;
        // `child` was opened relative to the currently pinned parent, then
        // became its own stable handle. Retaining every ancestor only creates
        // an unbounded handle-chain allocation without adding protection.
        directory = child;
    }
    Ok(OpenedDirectory { handle: directory })
}

fn open_root(root: &OsStr, access: u32) -> io::Result<OwnedHandle> {
    let mut path = [0_u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS];
    encode_root(root, &mut path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            PINNED_DIRECTORY_SHARE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(handle);
    validate_directory_handle(handle.raw(), "Windows publication root")?;
    Ok(handle)
}

fn open_directory_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    disposition: u32,
    security: Option<*const SECURITY_DESCRIPTOR>,
) -> io::Result<(OwnedHandle, usize)> {
    let (handle, information) = nt_create_at(
        parent,
        name,
        access | SYNCHRONIZE,
        PINNED_DIRECTORY_SHARE,
        disposition,
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_DIRECTORY,
        security,
    )?;
    validate_directory_handle(handle.raw(), "Windows publication directory")?;
    Ok((handle, information))
}

fn open_or_create_private_directory_at(
    parent: HANDLE,
    name: &OsStr,
    security: &PrivateSecurityDescriptor,
) -> io::Result<OwnedHandle> {
    let (handle, information) = open_directory_at(
        parent,
        name,
        PRIVATE_DIRECTORY_ACCESS | DIRECTORY_DESTINATION_SYNC_ACCESS,
        FILE_OPEN_IF,
        Some(security.as_ptr()),
    )?;
    if information != FILE_CREATED as usize && information != FILE_OPENED as usize {
        return Err(io::Error::other(
            "Windows private directory returned an unexpected create disposition",
        ));
    }
    security.apply_and_verify(handle.raw())?;
    Ok(handle)
}

#[cfg(test)]
fn create_or_open_private_file(
    path: &Path,
    disposition: u32,
    security: &PrivateSecurityDescriptor,
) -> io::Result<OpenedRegular> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_DESTINATION_ACCESS)?;
    let handle = create_or_open_private_file_at(parent.raw(), name, disposition, security)?;
    Ok(OpenedRegular {
        _parent: parent,
        handle,
    })
}

fn create_or_open_private_file_at(
    parent: HANDLE,
    name: &OsStr,
    disposition: u32,
    security: &PrivateSecurityDescriptor,
) -> io::Result<OwnedHandle> {
    let (handle, information) = nt_create_at(
        parent,
        name,
        PRIVATE_FILE_ACCESS,
        0,
        disposition,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        Some(security.as_ptr()),
    )?;
    validate_regular_handle(handle.raw(), "Windows private file")?;
    if disposition == FILE_OPEN_IF
        && information != FILE_CREATED as usize
        && information != FILE_OPENED as usize
    {
        return Err(io::Error::other(
            "Windows private file returned an unexpected create disposition",
        ));
    }
    Ok(handle)
}

fn open_regular(
    path: &Path,
    access: u32,
    share: u32,
    description: &'static str,
) -> io::Result<OpenedRegular> {
    open_regular_with_parent_access(path, access, share, description, DIRECTORY_TRAVERSE_ACCESS)
}

fn open_regular_with_parent_access(
    path: &Path,
    access: u32,
    share: u32,
    description: &'static str,
    parent_access: u32,
) -> io::Result<OpenedRegular> {
    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, parent_access)?;
    let handle = open_regular_at(parent.raw(), name, access, share, description)?;
    Ok(OpenedRegular {
        _parent: parent,
        handle,
    })
}

fn open_regular_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    share: u32,
    description: &'static str,
) -> io::Result<OwnedHandle> {
    let (handle, _) = nt_create_at(
        parent,
        name,
        access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        share,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    validate_regular_handle(handle.raw(), description)?;
    Ok(handle)
}

fn open_existing_node(path: &Path) -> io::Result<OpenedNode> {
    if path.file_name().is_none() {
        let directory = open_directory(path, DIRECTORY_TRAVERSE_ACCESS)?;
        let duplicate = duplicate_handle_for_identity(directory.raw())?;
        return Ok(OpenedNode {
            _parent: Some(directory),
            handle: duplicate,
        });
    }

    let (parent_path, name) = split_leaf(path)?;
    let parent = open_directory(parent_path, DIRECTORY_TRAVERSE_ACCESS)?;
    let (handle, _) = nt_create_at(
        parent.raw(),
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        PINNED_DIRECTORY_SHARE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
        None,
    )?;
    validate_non_reparse(handle.raw(), "filesystem identity entry is a reparse point")?;
    let standard = file_standard_information(handle.raw())?;
    if !standard.Directory && standard.EndOfFile < 0 {
        return Err(invalid_path(
            "filesystem identity entry has a negative length",
        ));
    }
    Ok(OpenedNode {
        _parent: Some(parent),
        handle,
    })
}

fn duplicate_handle_for_identity(handle: HANDLE) -> io::Result<OwnedHandle> {
    use windows_sys::Win32::Foundation::DUPLICATE_SAME_ACCESS;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let process = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    let succeeded = unsafe {
        DuplicateHandle(
            process,
            handle,
            process,
            &raw mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(duplicate))
    }
}

fn nt_create_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    attributes: u32,
    security: Option<*const SECURITY_DESCRIPTOR>,
) -> io::Result<(OwnedHandle, usize)> {
    let mut encoded_name = [0_u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS];
    let name_length = encode_leaf(name, &mut encoded_name)?;
    let name_bytes = name_length
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| invalid_path("Windows path component is too long"))?;
    let name_bytes = u16::try_from(name_bytes)
        .map_err(|_| invalid_path("Windows path component is too long"))?;
    let unicode = windows_sys::Win32::Foundation::UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: name_bytes,
        Buffer: encoded_name.as_mut_ptr(),
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
            .map_err(|_| invalid_path("Windows object attributes size is unsupported"))?,
        RootDirectory: parent,
        ObjectName: &raw const unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: security.unwrap_or(std::ptr::null()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access,
            &raw const object_attributes,
            &raw mut io_status,
            std::ptr::null(),
            attributes,
            share,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if let Err(error) = ntstatus_result(status, "open Windows path component") {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
        }
        return Err(error);
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(
            "NtCreateFile succeeded without returning a valid handle",
        ));
    }
    Ok((OwnedHandle(handle), io_status.Information))
}

fn split_leaf(path: &Path) -> io::Result<(&Path, &OsStr)> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid_path("atomic publication path has no regular leaf name"))?;
    validate_leaf(name)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid_path("atomic publication path has no parent"))?;
    Ok((parent, name))
}

fn validate_root(root: &OsStr) -> io::Result<()> {
    let mut length = 0_usize;
    for unit in root.encode_wide() {
        if unit == 0 {
            return Err(invalid_path("Windows publication root contains a NUL"));
        }
        length = length
            .checked_add(1)
            .ok_or_else(|| invalid_path("Windows publication root is too long"))?;
        if length > MAX_WINDOWS_ROOT_UTF16_UNITS {
            return Err(invalid_path("Windows publication root is too long"));
        }
    }
    if length == 0 {
        return Err(invalid_path("Windows publication root is empty"));
    }
    Ok(())
}

fn encode_root(
    root: &OsStr,
    buffer: &mut [u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS],
) -> io::Result<()> {
    validate_root(root)?;
    let mut length = 0_usize;
    for unit in root.encode_wide() {
        buffer[length] = unit;
        length += 1;
    }
    if buffer[length - 1] != u16::from(b'\\') {
        buffer[length] = u16::from(b'\\');
        length += 1;
    }
    buffer[length] = 0;
    Ok(())
}

fn validate_leaf(name: &OsStr) -> io::Result<usize> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(invalid_path(
            "atomic publication path has an invalid leaf name",
        ));
    }

    let mut length = 0_usize;
    for unit in name.encode_wide() {
        if unit == 0
            || unit == u16::from(b':')
            || unit == u16::from(b'/')
            || unit == u16::from(b'\\')
        {
            return Err(invalid_path(
                "atomic publication path has an invalid leaf name",
            ));
        }
        length = length
            .checked_add(1)
            .ok_or_else(|| invalid_path("Windows path component is too long"))?;
        if length > MAX_WINDOWS_COMPONENT_UTF16_UNITS {
            return Err(invalid_path("Windows path component is too long"));
        }
    }
    Ok(length)
}

fn encode_leaf(
    name: &OsStr,
    buffer: &mut [u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS],
) -> io::Result<usize> {
    let length = validate_leaf(name)?;
    for (index, unit) in name.encode_wide().enumerate() {
        buffer[index] = unit;
    }
    Ok(length)
}

fn validate_non_reparse(handle: HANDLE, message: &'static str) -> io::Result<()> {
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut attributes).cast(),
            fixed_structure_size::<FILE_ATTRIBUTE_TAG_INFO>()?,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_path(message));
    }
    Ok(())
}

fn validate_directory_handle(handle: HANDLE, description: &'static str) -> io::Result<()> {
    validate_non_reparse(handle, "Windows publication directory is a reparse point")?;
    if !file_standard_information(handle)?.Directory {
        return Err(invalid_path(description));
    }
    Ok(())
}

fn validate_regular_handle(handle: HANDLE, description: &'static str) -> io::Result<()> {
    validate_non_reparse(handle, "Windows publication file is a reparse point")?;
    if file_standard_information(handle)?.Directory {
        return Err(invalid_path(description));
    }
    Ok(())
}

fn reject_mutable_hardlink(handle: HANDLE, description: &'static str) -> io::Result<()> {
    if file_standard_information(handle)?.NumberOfLinks != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{description} must have exactly one hard link"),
        ));
    }
    Ok(())
}

fn file_standard_information(handle: HANDLE) -> io::Result<FILE_STANDARD_INFO> {
    let mut information = FILE_STANDARD_INFO::default();
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&raw mut information).cast(),
            fixed_structure_size::<FILE_STANDARD_INFO>()?,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(information)
    }
}

fn file_id_information(handle: HANDLE) -> io::Result<FILE_ID_INFO> {
    let mut information = FILE_ID_INFO::default();
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut information).cast(),
            fixed_structure_size::<FILE_ID_INFO>()?,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(information)
    }
}

fn file_identity(handle: HANDLE) -> io::Result<FileIdentity> {
    let standard = file_standard_information(handle)?;
    if standard.Directory {
        return Err(invalid_path("file identity source is not a regular file"));
    }
    let length = u64::try_from(standard.EndOfFile)
        .map_err(|_| invalid_path("file identity source has a negative length"))?;
    let information = file_id_information(handle)?;
    Ok(FileIdentity::new(
        information.VolumeSerialNumber,
        information.FileId.Identifier,
        length,
    ))
}

fn verify_file_identity(
    handle: HANDLE,
    expected: &FileIdentity,
    message: &'static str,
) -> io::Result<()> {
    if &file_identity(handle)? == expected {
        Ok(())
    } else {
        Err(identity_changed(message))
    }
}

fn validate_opened_digest(
    file: &mut File,
    expected: DigestV1,
    length: u64,
    changed_message: &'static str,
) -> io::Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let actual = DigestV1::hash_reader(&mut *file, length)?;
    if actual != expected {
        return Err(io::Error::new(io::ErrorKind::InvalidData, changed_message));
    }
    Ok(())
}

fn directory_identity(handle: HANDLE) -> io::Result<DirectoryIdentity> {
    if !file_standard_information(handle)?.Directory {
        return Err(invalid_path("directory identity source is not a directory"));
    }
    let information = file_id_information(handle)?;
    Ok(DirectoryIdentity::new(
        information.VolumeSerialNumber,
        information.FileId.Identifier,
    ))
}

fn verify_directory_identity(
    handle: HANDLE,
    expected: &DirectoryIdentity,
    message: &'static str,
) -> io::Result<()> {
    if &directory_identity(handle)? == expected {
        Ok(())
    } else {
        Err(identity_changed(message))
    }
}

fn ensure_same_volume(first: u64, second: u64) -> io::Result<()> {
    if first == second {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(
            i32::try_from(ERROR_NOT_SAME_DEVICE).expect("Win32 error fits i32"),
        ))
    }
}

struct PrivateSecurityDescriptor {
    _token_information: Vec<MaybeUninit<usize>>,
    acl: Vec<MaybeUninit<usize>>,
    descriptor: SECURITY_DESCRIPTOR,
}

impl PrivateSecurityDescriptor {
    fn new(directory: bool) -> io::Result<Self> {
        let token = open_effective_token()?;
        let mut required = 0_u32;
        let first = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut required,
            )
        };
        if first != 0
            || io::Error::last_os_error().raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).expect("Win32 error fits i32"))
        {
            return Err(io::Error::last_os_error());
        }
        let mut token_information = aligned_storage(
            usize::try_from(required)
                .map_err(|_| io::Error::other("token information size does not fit usize"))?,
            "Windows token information",
        )?;
        let token_capacity = storage_bytes_u32(&token_information)?;
        let succeeded = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                token_information.as_mut_ptr().cast(),
                token_capacity,
                &raw mut required,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let token_user = token_information.as_ptr().cast::<TOKEN_USER>();
        let owner = unsafe { (*token_user).User.Sid };
        if owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
            return Err(io::Error::other(
                "effective Windows token returned an invalid user SID",
            ));
        }
        let sid_length = unsafe { GetLengthSid(owner) };
        if sid_length == 0 {
            return Err(io::Error::last_os_error());
        }

        let ace_bytes = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .and_then(|value| value.checked_add(usize::try_from(sid_length).ok()?))
            .ok_or_else(|| io::Error::other("owner-only ACL size overflow"))?;
        let acl_bytes = size_of::<ACL>()
            .checked_add(ace_bytes)
            .ok_or_else(|| io::Error::other("owner-only ACL size overflow"))?;
        let mut acl = aligned_storage(acl_bytes, "owner-only Windows ACL")?;
        let acl_length = u32::try_from(acl_bytes)
            .map_err(|_| io::Error::other("owner-only Windows ACL is too large"))?;
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        if unsafe { InitializeAcl(acl_ptr, acl_length, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let ace_flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        if unsafe {
            AddAccessAllowedAceEx(acl_ptr, ACL_REVISION, ace_flags, FILE_ALL_ACCESS, owner)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if unsafe { IsValidSecurityDescriptor(descriptor_ptr) } == 0 {
            return Err(io::Error::other(
                "constructed owner-only Windows security descriptor is invalid",
            ));
        }

        Ok(Self {
            _token_information: token_information,
            acl,
            descriptor,
        })
    }

    fn as_ptr(&self) -> *const SECURITY_DESCRIPTOR {
        &raw const self.descriptor
    }

    fn apply_and_verify(&self, handle: HANDLE) -> io::Result<()> {
        let mut descriptor = self.descriptor;
        let succeeded = unsafe {
            SetKernelObjectSecurity(
                handle,
                PRIVATE_SECURITY_INFORMATION,
                (&raw mut descriptor).cast(),
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        self.verify(handle)
    }

    fn verify(&self, handle: HANDLE) -> io::Result<()> {
        let snapshot = SecuritySnapshot::capture_with(
            handle,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
        )?;
        let view = snapshot.private_view()?;
        if unsafe { EqualSid(view.owner, self.descriptor.Owner) } == 0
            || !view.dacl_protected
            || !acl_equal(view.dacl, self.acl.as_ptr().cast::<ACL>())?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows private object did not retain its owner-only DACL",
            ));
        }
        Ok(())
    }
}

fn open_effective_token() -> io::Result<OwnedHandle> {
    let mut handle = std::ptr::null_mut();
    let thread_opened =
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut handle) };
    if thread_opened != 0 {
        return Ok(OwnedHandle(handle));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(i32::try_from(ERROR_NO_TOKEN).expect("Win32 error fits i32")) {
        return Err(error);
    }

    let process_opened =
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut handle) };
    if process_opened == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

struct SecuritySnapshot {
    storage: Vec<MaybeUninit<usize>>,
}

impl SecuritySnapshot {
    #[cfg(test)]
    fn capture(handle: HANDLE) -> io::Result<Self> {
        Self::capture_with(handle, SECURITY_METADATA_INFORMATION)
    }

    fn capture_with(handle: HANDLE, information: u32) -> io::Result<Self> {
        let mut required = 0_u32;
        let first = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                std::ptr::null_mut(),
                0,
                &raw mut required,
            )
        };
        if first != 0
            || io::Error::last_os_error().raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).expect("Win32 error fits i32"))
        {
            return Err(io::Error::last_os_error());
        }
        let mut storage = aligned_storage(
            usize::try_from(required)
                .map_err(|_| io::Error::other("security descriptor size does not fit usize"))?,
            "Windows security descriptor",
        )?;
        let capacity = storage_bytes_u32(&storage)?;
        let succeeded = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                storage.as_mut_ptr().cast(),
                capacity,
                &raw mut required,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let snapshot = Self { storage };
        if unsafe { IsValidSecurityDescriptor(snapshot.as_ptr()) } == 0 {
            return Err(io::Error::other(
                "Windows returned an invalid security descriptor",
            ));
        }
        Ok(snapshot)
    }

    fn capture_budgeted(
        handle: HANDLE,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SecurityMetadataError> {
        Self::capture_with_budgeted(handle, SECURITY_METADATA_INFORMATION, budget)
    }

    fn capture_with_budgeted(
        handle: HANDLE,
        information: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SecurityMetadataError> {
        let mut required = 0_u32;
        let first = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                std::ptr::null_mut(),
                0,
                &raw mut required,
            )
        };
        if first != 0
            || io::Error::last_os_error().raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).expect("Win32 error fits i32"))
        {
            return Err(io::Error::last_os_error().into());
        }
        let required_bytes = usize::try_from(required)
            .map_err(|_| io::Error::other("security descriptor size does not fit usize"))?;
        if required_bytes > MAX_WINDOWS_SECURITY_DESCRIPTOR_BYTES {
            return Err(unsupported_descriptor(
                "Windows security descriptor exceeds the supported bounded size",
            )
            .into());
        }
        let mut storage =
            aligned_storage_budgeted(required_bytes, "Windows security descriptor", budget)?;
        let capacity = storage_bytes_u32(&storage)?;
        let succeeded = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                storage.as_mut_ptr().cast(),
                capacity,
                &raw mut required,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let snapshot = Self { storage };
        if unsafe { IsValidSecurityDescriptor(snapshot.as_ptr()) } == 0 {
            return Err(io::Error::other("Windows returned an invalid security descriptor").into());
        }
        Ok(snapshot)
    }

    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.storage.as_ptr().cast_mut().cast()
    }

    fn private_view(&self) -> io::Result<PrivateSecurityView> {
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0;
        if unsafe {
            GetSecurityDescriptorOwner(self.as_ptr(), &raw mut owner, &raw mut owner_defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
            return Err(unsupported_descriptor(
                "Windows security descriptor has no valid owner",
            ));
        }

        let mut dacl_present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut dacl_defaulted = 0;
        if unsafe {
            GetSecurityDescriptorDacl(
                self.as_ptr(),
                &raw mut dacl_present,
                &raw mut dacl,
                &raw mut dacl_defaulted,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if dacl_present == 0 || dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
            return Err(unsupported_descriptor(
                "Windows security descriptor has a missing or null DACL",
            ));
        }

        let mut control = 0;
        let mut revision = 0;
        if unsafe {
            GetSecurityDescriptorControl(self.as_ptr(), &raw mut control, &raw mut revision)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(PrivateSecurityView {
            owner,
            dacl,
            dacl_protected: control & SE_DACL_PROTECTED != 0,
        })
    }

    fn full_view(&self) -> io::Result<SecurityView> {
        let private = self.private_view()?;
        let mut group = std::ptr::null_mut();
        let mut group_defaulted = 0;
        if unsafe {
            GetSecurityDescriptorGroup(self.as_ptr(), &raw mut group, &raw mut group_defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if group.is_null() || unsafe { IsValidSid(group) } == 0 {
            return Err(unsupported_descriptor(
                "Windows security descriptor has no valid primary group",
            ));
        }
        Ok(SecurityView {
            owner: private.owner,
            group,
            dacl: private.dacl,
            dacl_protected: private.dacl_protected,
        })
    }

    fn apply_and_verify_budgeted(
        &self,
        handle: HANDLE,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SecurityMetadataError> {
        let source = self.full_view()?;
        let protection = if source.dacl_protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        let succeeded = unsafe {
            SetKernelObjectSecurity(
                handle,
                SECURITY_METADATA_INFORMATION | protection,
                self.as_ptr(),
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let applied = Self::capture_budgeted(handle, budget)?;
        if !self.equivalent_to(&applied)? {
            return Err(unsupported_descriptor(
                "Windows owner, group, or DACL could not be preserved exactly",
            )
            .into());
        }
        Ok(())
    }

    fn equivalent_to(&self, other: &Self) -> io::Result<bool> {
        let left = self.full_view()?;
        let right = other.full_view()?;
        Ok(unsafe { EqualSid(left.owner, right.owner) } != 0
            && unsafe { EqualSid(left.group, right.group) } != 0
            && acl_equal(left.dacl, right.dacl)?
            && left.dacl_protected == right.dacl_protected)
    }
}

struct PrivateSecurityView {
    owner: PSID,
    dacl: *mut ACL,
    dacl_protected: bool,
}

struct SecurityView {
    owner: PSID,
    group: PSID,
    dacl: *mut ACL,
    dacl_protected: bool,
}

fn acl_equal(left: *const ACL, right: *const ACL) -> io::Result<bool> {
    let left_length = acl_length(left)?;
    let right_length = acl_length(right)?;
    if left_length != right_length {
        return Ok(false);
    }
    let left = unsafe { std::slice::from_raw_parts(left.cast::<u8>(), left_length) };
    let right = unsafe { std::slice::from_raw_parts(right.cast::<u8>(), right_length) };
    Ok(left == right)
}

fn acl_length(acl: *const ACL) -> io::Result<usize> {
    if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
        return Err(unsupported_descriptor("Windows DACL is invalid"));
    }
    let length = usize::from(unsafe { (*acl).AclSize });
    if length < size_of::<ACL>() {
        return Err(unsupported_descriptor("Windows DACL is truncated"));
    }
    Ok(length)
}

fn aligned_storage(
    byte_len: usize,
    description: &'static str,
) -> io::Result<Vec<MaybeUninit<usize>>> {
    if byte_len == 0 {
        return Err(io::Error::other(format!(
            "{description} requested zero bytes"
        )));
    }
    let units = byte_len.div_ceil(size_of::<usize>());
    let mut storage = Vec::new();
    storage
        .try_reserve_exact(units)
        .map_err(|error| allocation_error(description, error))?;
    storage.resize_with(units, MaybeUninit::zeroed);
    Ok(storage)
}

fn aligned_storage_budgeted(
    byte_len: usize,
    description: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<MaybeUninit<usize>>, SecurityMetadataError> {
    if byte_len == 0 {
        return Err(io::Error::other(format!("{description} requested zero bytes")).into());
    }
    let units = byte_len.div_ceil(size_of::<usize>());
    let requested = units
        .checked_mul(size_of::<MaybeUninit<usize>>())
        .ok_or_else(|| io::Error::other("Windows aligned storage size overflow"))?;
    let requested = u64::try_from(requested)
        .map_err(|_| io::Error::other("Windows aligned storage is too large"))?;
    budget.check_bytes(requested)?;

    let mut storage = Vec::new();
    storage
        .try_reserve_exact(units)
        .map_err(|error| allocation_error(description, error))?;
    let actual = storage
        .capacity()
        .checked_mul(size_of::<MaybeUninit<usize>>())
        .ok_or_else(|| io::Error::other("Windows aligned storage size overflow"))?;
    let actual = u64::try_from(actual)
        .map_err(|_| io::Error::other("Windows aligned storage is too large"))?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    storage.resize_with(units, MaybeUninit::zeroed);
    Ok(storage)
}

fn storage_bytes_u32(storage: &[MaybeUninit<usize>]) -> io::Result<u32> {
    let bytes = storage
        .len()
        .checked_mul(size_of::<usize>())
        .ok_or_else(|| io::Error::other("Windows aligned storage size overflow"))?;
    u32::try_from(bytes).map_err(|_| io::Error::other("Windows aligned storage is too large"))
}

const RENAME_INFORMATION_STORAGE_UNITS: usize = (size_of::<FILE_RENAME_INFORMATION>()
    + MAX_WINDOWS_COMPONENT_UTF16_UNITS * size_of::<u16>())
.div_ceil(size_of::<FILE_RENAME_INFORMATION>());

struct RenameInformation {
    storage: [MaybeUninit<FILE_RENAME_INFORMATION>; RENAME_INFORMATION_STORAGE_UNITS],
    byte_len: u32,
}

impl RenameInformation {
    fn new(destination_name: &OsStr, root: HANDLE, replace_existing: bool) -> io::Result<Self> {
        let mut encoded_name = [0_u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS];
        let name_length = encode_leaf(destination_name, &mut encoded_name)?;
        let name_bytes = name_length
            .checked_mul(size_of::<u16>())
            .ok_or_else(|| invalid_path("destination leaf name is too long"))?;
        let required_bytes = size_of::<FILE_RENAME_INFORMATION>()
            .checked_add(name_bytes)
            .ok_or_else(|| invalid_path("destination leaf name is too long"))?;
        let byte_len = u32::try_from(required_bytes)
            .map_err(|_| invalid_path("destination leaf name is too long"))?;
        let mut storage: [MaybeUninit<FILE_RENAME_INFORMATION>; RENAME_INFORMATION_STORAGE_UNITS] =
            std::array::from_fn(|_| MaybeUninit::zeroed());
        debug_assert!(required_bytes <= size_of_val(&storage));

        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*information).Anonymous = FILE_RENAME_INFORMATION_0 {
                ReplaceIfExists: replace_existing,
            };
            (*information).RootDirectory = root;
            (*information).FileNameLength = u32::try_from(name_bytes)
                .map_err(|_| invalid_path("destination leaf name is too long"))?;
            std::ptr::copy_nonoverlapping(
                encoded_name.as_ptr(),
                (*information).FileName.as_mut_ptr(),
                name_length,
            );
        }
        debug_assert_eq!(
            (information as usize) % align_of::<FILE_RENAME_INFORMATION>(),
            0
        );
        Ok(Self { storage, byte_len })
    }

    fn as_ptr(&self) -> *const std::ffi::c_void {
        self.storage.as_ptr().cast()
    }

    const fn byte_len(&self) -> u32 {
        self.byte_len
    }
}

fn flush_handle(handle: HANDLE) -> io::Result<()> {
    let succeeded = unsafe { FlushFileBuffers(handle) };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn mark_for_delete_on_close(handle: HANDLE) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtSetInformationFile(
            handle,
            &raw mut io_status,
            (&raw const disposition).cast(),
            fixed_structure_size::<FILE_DISPOSITION_INFORMATION>()?,
            FileDispositionInformation,
        )
    };
    ntstatus_result(status, "mark owned publication entry for deletion")
}

fn fixed_structure_size<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("fixed Windows structure is too large"))
}

fn invalid_path(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn identity_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, message)
}

fn unsupported_descriptor(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

fn unsupported_security(operation: &'static str, error: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("Windows cannot safely {operation}: {error}"),
    )
}

fn map_security_metadata_error(
    operation: &'static str,
    error: SecurityMetadataError,
) -> SecurityMetadataError {
    match error {
        SecurityMetadataError::Budget(error) => SecurityMetadataError::Budget(error),
        SecurityMetadataError::Io(error) => {
            SecurityMetadataError::Io(unsupported_security(operation, error))
        }
    }
}

fn allocation_error(description: &'static str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("failed to allocate {description}: {error}"))
}

fn ntstatus_result(status: NTSTATUS, operation: &'static str) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
        return Err(invalid_path(
            "Windows publication path contains a reparse point",
        ));
    }
    let win32 = unsafe { RtlNtStatusToDosError(status) };
    let raw = i32::try_from(win32).map_err(|_| {
        io::Error::other(format!(
            "{operation} failed with NTSTATUS {status:#010x} (unmapped Win32 code {win32})"
        ))
    })?;
    Err(io::Error::from_raw_os_error(raw))
}

#[cfg(test)]
mod tests {
    use super::{
        AbsolutePathParts, DIRECTORY_TRAVERSE_ACCESS, ERROR_NOT_SAME_DEVICE, FileIdentity,
        MAX_WINDOWS_COMPONENT_UTF16_UNITS, MAX_WINDOWS_ROOT_UTF16_UNITS, PINNED_FILE_SHARE,
        PrivateSecurityDescriptor, READ_CONTROL, RenameInformation, SecurityMetadataError,
        SecuritySnapshot, WRITE_DAC, WRITE_OWNER, atomic_replace, capture_existing,
        capture_external_regular_in_journal_directory, copy_security_metadata,
        create_journal_directory, create_private_directory, create_private_directory_exclusive,
        create_private_directory_exclusive_in_parent, create_private_file,
        create_private_file_in_parent, ensure_same_filesystem, ensure_same_volume,
        ensure_single_hardlink, observe_directory_identity, observe_file_identity,
        open_commit_root, open_directory, open_existing_journal_namespace, open_journal_namespace,
        open_readonly_regular_in_parent, open_regular, promote_journal_regular_to_external,
        remove_owned_empty_directory_in_parent, remove_owned_file_in_parent,
        sync_journal_namespace,
    };
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::os::windows::fs::symlink_dir;
    use std::path::Path;
    use unity_asset_core::{AssetLoadBudget, DigestV1};

    #[test]
    fn absolute_path_parts_borrow_and_bound_components() {
        let mut path =
            AbsolutePathParts::new(Path::new(r"C:\alpha\beta")).expect("absolute path components");
        assert_eq!(path.root(), std::ffi::OsStr::new("C:"));
        assert_eq!(
            path.next_component().unwrap(),
            Some(std::ffi::OsStr::new("alpha"))
        );
        assert!(path.has_components());
        assert_eq!(
            path.next_component().unwrap(),
            Some(std::ffi::OsStr::new("beta"))
        );
        assert!(!path.has_components());

        let accepted = Path::new(r"C:\").join("a".repeat(MAX_WINDOWS_COMPONENT_UTF16_UNITS));
        assert!(AbsolutePathParts::new(&accepted).is_ok());
        let rejected = Path::new(r"C:\").join("a".repeat(MAX_WINDOWS_COMPONENT_UTF16_UNITS + 1));
        assert!(AbsolutePathParts::new(&rejected).is_err());
    }

    #[test]
    fn absolute_path_parts_bound_windows_roots() {
        let accepted_share = "a".repeat(MAX_WINDOWS_ROOT_UTF16_UNITS - 4);
        let accepted = format!(r"\\s\{accepted_share}\leaf");
        assert!(AbsolutePathParts::new(Path::new(&accepted)).is_ok());

        let rejected_share = "a".repeat(MAX_WINDOWS_ROOT_UTF16_UNITS - 3);
        let rejected = format!(r"\\s\{rejected_share}\leaf");
        assert!(AbsolutePathParts::new(Path::new(&rejected)).is_err());
    }

    #[test]
    fn rename_information_bounds_destination_leaf_storage() {
        let accepted = OsString::from("a".repeat(MAX_WINDOWS_COMPONENT_UTF16_UNITS));
        assert!(RenameInformation::new(&accepted, std::ptr::null_mut(), false).is_ok());

        let rejected = OsString::from("a".repeat(MAX_WINDOWS_COMPONENT_UTF16_UNITS + 1));
        assert!(RenameInformation::new(&rejected, std::ptr::null_mut(), false).is_err());
    }

    #[test]
    fn moves_a_file_without_replacing_the_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, b"new").expect("source");

        atomic_replace(&source, &destination, false).expect("atomic move");

        assert!(!source.exists());
        assert_eq!(fs::read(&destination).expect("destination"), b"new");
    }

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
    fn journal_namespace_handles_support_directory_sync() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root_identity = observe_directory_identity(directory.path()).expect("root identity");
        let root = open_commit_root(directory.path(), &root_identity).expect("commit root");

        let namespace = open_journal_namespace(&root).expect("new journal namespace");
        sync_journal_namespace(&root, &namespace).expect("sync new journal namespace");
        drop(namespace);

        let reopened = open_existing_journal_namespace(&root).expect("existing journal namespace");
        sync_journal_namespace(&root, &reopened).expect("sync existing journal namespace");
    }

    #[test]
    fn ancestor_directory_reparse_is_rejected_before_child_creation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let external = tempfile::tempdir().expect("external directory");
        let recovery = directory.path().join(".unity-asset-recovery");
        if let Err(error) = symlink_dir(external.path(), &recovery) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("directory reparse point: {error}");
        }

        create_private_directory(&recovery.join("v1")).expect_err("reparse rejected");

        assert!(!external.path().join("v1").exists());
    }

    #[test]
    fn captured_identity_rejects_a_byte_identical_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("source");
        let destination = directory.path().join("destination");
        fs::write(&source, b"same bytes").expect("source");
        let expected = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(directory.path()).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(directory.path()).expect("destination parent identity");
        fs::remove_file(&source).expect("remove original");
        fs::write(&source, b"same bytes").expect("replacement");

        let error = capture_existing(
            &source,
            &destination,
            &expected,
            None,
            &expected_source_parent,
            &expected_destination_parent,
        )
        .expect_err("replacement identity rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&source).expect("replacement remains"),
            b"same bytes"
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
    fn captured_destination_parent_rejects_a_path_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_parent = directory.path().join("source-parent");
        let destination_parent = directory.path().join("destination-parent");
        let displaced_parent = directory.path().join("displaced-parent");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination_parent).expect("destination parent");
        let source = source_parent.join("source");
        let destination = destination_parent.join("destination");
        fs::write(&source, b"source").expect("source");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(&source_parent).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&destination_parent).expect("destination parent identity");
        fs::rename(&destination_parent, &displaced_parent).expect("displace destination parent");
        fs::create_dir(&destination_parent).expect("replacement destination parent");

        let error = capture_existing(
            &source,
            &destination,
            &expected_source,
            None,
            &expected_source_parent,
            &expected_destination_parent,
        )
        .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(&source).expect("source remains"), b"source");
        assert!(!destination.exists());
        assert_ne!(
            observe_directory_identity(&destination_parent).expect("replacement identity"),
            expected_destination_parent
        );
    }

    #[test]
    fn captured_source_parent_rejects_a_path_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_parent = directory.path().join("source-parent");
        let destination_parent = directory.path().join("destination-parent");
        let displaced_parent = directory.path().join("displaced-parent");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination_parent).expect("destination parent");
        let source = source_parent.join("source");
        let destination = destination_parent.join("destination");
        fs::write(&source, b"source").expect("source");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_source_parent =
            observe_directory_identity(&source_parent).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&destination_parent).expect("destination parent identity");
        fs::rename(&source_parent, &displaced_parent).expect("displace source parent");
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
            fs::read(displaced_parent.join("source")).expect("captured source remains"),
            b"source"
        );
        assert!(!destination.exists());
    }

    #[test]
    fn parent_bound_read_rejects_a_replaced_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced = directory.path().join("displaced");
        fs::create_dir(&parent).expect("parent");
        let file = parent.join("file");
        fs::write(&file, b"captured").expect("captured file");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::rename(&parent, &displaced).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");
        fs::write(&file, b"replacement").expect("replacement file");

        let error = open_readonly_regular_in_parent(&file, &expected_parent)
            .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&file).expect("replacement remains"),
            b"replacement"
        );
        assert_eq!(
            fs::read(displaced.join("file")).expect("captured file remains"),
            b"captured"
        );
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
    fn file_identity_contains_volume_file_id_and_length() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::write(&first, b"first").expect("first");
        fs::write(&second, b"second").expect("second");

        ensure_same_filesystem(&first, directory.path()).expect("same filesystem");
        let identity = observe_file_identity(&first).expect("identity");
        assert_ne!(identity.file_id(), [0; 16]);
        assert_ne!(identity.volume_serial_number(), 0);
        assert_eq!(identity.length(), 5);
        assert_ne!(
            identity,
            observe_file_identity(&second).expect("second identity")
        );
        assert_eq!(
            identity,
            FileIdentity::new(
                identity.volume_serial_number(),
                identity.file_id(),
                identity.length()
            )
        );
    }

    #[test]
    fn cross_volume_preflight_returns_not_same_device() {
        let error = ensure_same_volume(1, 2).expect_err("different volumes");
        assert_eq!(
            error.raw_os_error(),
            Some(i32::try_from(ERROR_NOT_SAME_DEVICE).expect("Win32 error fits i32"))
        );
    }

    #[test]
    fn private_files_and_directories_have_owner_only_dacls() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recovery = directory.path().join(".unity-asset-recovery").join("v1");
        create_private_directory(&recovery).expect("private directory");
        let journal = recovery.join("journal");
        drop(create_private_file(&journal).expect("private file"));

        let directory_handle = open_directory(&recovery, DIRECTORY_TRAVERSE_ACCESS | READ_CONTROL)
            .expect("open private directory");
        PrivateSecurityDescriptor::new(true)
            .expect("directory descriptor")
            .verify(directory_handle.raw())
            .expect("directory DACL");
        let file_handle = open_regular(&journal, READ_CONTROL, PINNED_FILE_SHARE, "private file")
            .expect("open private file");
        PrivateSecurityDescriptor::new(false)
            .expect("file descriptor")
            .verify(file_handle.handle.raw())
            .expect("file DACL");
    }

    #[test]
    fn exclusive_private_directory_does_not_claim_an_existing_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recovery = directory.path().join(".unity-asset-recovery").join("v1");
        create_private_directory(&recovery).expect("private recovery root");
        let transaction = recovery.join("transaction");
        let created = create_private_directory_exclusive(&transaction).expect("transaction");
        assert_eq!(
            observe_directory_identity(&transaction).expect("transaction identity"),
            created
        );
        let unknown = transaction.join("unknown");
        fs::write(&unknown, b"must remain").expect("unknown entry");

        let error = create_private_directory_exclusive(&transaction)
            .expect_err("existing transaction rejected");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(unknown).expect("unknown entry remains"),
            b"must remain"
        );
    }

    #[test]
    fn parent_bound_private_directory_creation_rejects_a_replaced_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced = directory.path().join("displaced");
        fs::create_dir(&parent).expect("parent");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::rename(&parent, &displaced).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");
        let transaction = parent.join("transaction");

        let error = create_private_directory_exclusive_in_parent(&transaction, &expected_parent)
            .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!transaction.exists());
        assert!(!displaced.join("transaction").exists());
    }

    #[test]
    fn parent_bound_private_file_creation_is_exclusive_and_rejects_a_replaced_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced = directory.path().join("displaced");
        fs::create_dir(&parent).expect("parent");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        let file = parent.join("file");
        drop(create_private_file_in_parent(&file, &expected_parent).expect("private file"));
        fs::write(&file, b"known").expect("known file contents");

        let existing = create_private_file_in_parent(&file, &expected_parent)
            .expect_err("existing leaf rejected");
        assert_eq!(existing.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&file).expect("known file remains"), b"known");

        fs::rename(&parent, &displaced).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");
        let replacement_file = parent.join("replacement");
        let error = create_private_file_in_parent(&replacement_file, &expected_parent)
            .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!replacement_file.exists());
        assert_eq!(
            fs::read(displaced.join("file")).expect("original file remains"),
            b"known"
        );
    }

    #[test]
    fn owned_file_cleanup_requires_the_captured_parent_and_file_identities() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let displaced = directory.path().join("displaced");
        fs::create_dir(&parent).expect("parent");
        let file = parent.join("file");
        fs::write(&file, b"owned").expect("owned file");
        let expected_file = observe_file_identity(&file).expect("file identity");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");

        fs::rename(&parent, &displaced).expect("displace parent");
        fs::create_dir(&parent).expect("replacement parent");
        let error =
            remove_owned_file_in_parent(&parent.join("file"), &expected_file, &expected_parent)
                .expect_err("replacement parent rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(displaced.join("file")).expect("owned file remains"),
            b"owned"
        );

        fs::remove_dir(&parent).expect("remove replacement parent");
        fs::rename(&displaced, &parent).expect("restore captured parent");
        remove_owned_file_in_parent(&file, &expected_file, &expected_parent)
            .expect("delete owned file");
        assert!(!file.exists());
    }

    #[test]
    fn owned_file_cleanup_rejects_a_same_parent_replacement() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let file = parent.join("file");
        let displaced = parent.join("displaced");
        fs::create_dir(&parent).expect("parent");
        fs::write(&file, b"original").expect("original file");
        let expected_file = observe_file_identity(&file).expect("file identity");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::rename(&file, &displaced).expect("displace original file");
        fs::write(&file, b"replacement").expect("replacement file");

        let error = remove_owned_file_in_parent(&file, &expected_file, &expected_parent)
            .expect_err("replacement file rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&file).expect("replacement remains"),
            b"replacement"
        );
        assert_eq!(fs::read(&displaced).expect("original remains"), b"original");
    }

    #[test]
    fn owned_file_cleanup_rejects_a_hard_linked_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let file = parent.join("file");
        let alias = parent.join("alias");
        fs::create_dir(&parent).expect("parent");
        fs::write(&file, b"owned").expect("owned file");
        let expected_file = observe_file_identity(&file).expect("file identity");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        fs::hard_link(&file, &alias).expect("hard link");

        let error = remove_owned_file_in_parent(&file, &expected_file, &expected_parent)
            .expect_err("hard linked file rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&file).expect("file remains"), b"owned");
        assert_eq!(fs::read(&alias).expect("alias remains"), b"owned");
    }

    #[test]
    fn owned_directory_cleanup_only_removes_an_empty_captured_directory() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let parent = directory.path().join("parent");
        let child = parent.join("child");
        fs::create_dir(&parent).expect("parent");
        fs::create_dir(&child).expect("child");
        let expected_parent = observe_directory_identity(&parent).expect("parent identity");
        let expected_child = observe_directory_identity(&child).expect("child identity");
        let unknown = child.join("unknown");
        fs::write(&unknown, b"must remain").expect("unknown child entry");

        remove_owned_empty_directory_in_parent(&child, &expected_child, &expected_parent)
            .expect_err("non-empty child rejected");
        assert_eq!(
            fs::read(&unknown).expect("unknown child remains"),
            b"must remain"
        );

        fs::remove_file(&unknown).expect("remove known test entry");
        remove_owned_empty_directory_in_parent(&child, &expected_child, &expected_parent)
            .expect("remove empty owned directory");
        assert!(!child.exists());
    }

    #[test]
    fn security_metadata_copy_preserves_owner_group_and_dacl() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recovery = directory.path().join(".unity-asset-recovery").join("v1");
        create_private_directory(&recovery).expect("private directory");
        let source = recovery.join("source");
        let destination = recovery.join("destination");
        drop(create_private_file(&source).expect("source"));
        drop(create_private_file(&destination).expect("destination"));

        let source_handle = open_regular(
            &source,
            READ_CONTROL | WRITE_DAC | WRITE_OWNER,
            PINNED_FILE_SHARE,
            "metadata source",
        )
        .expect("source handle");
        PrivateSecurityDescriptor::new(true)
            .expect("different DACL")
            .apply_and_verify(source_handle.handle.raw())
            .expect("custom source DACL");
        drop(source_handle);
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_destination =
            observe_file_identity(&destination).expect("destination identity");
        let expected_source_parent =
            observe_directory_identity(&recovery).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&recovery).expect("destination parent identity");
        let mut budget = AssetLoadBudget::default();

        copy_security_metadata(
            &source,
            &destination,
            &expected_source,
            &expected_source_parent,
            &expected_destination,
            &expected_destination_parent,
            &mut budget,
        )
        .expect("copy metadata");

        let source_handle =
            open_regular(&source, READ_CONTROL, PINNED_FILE_SHARE, "metadata source")
                .expect("source handle");
        let destination_handle = open_regular(
            &destination,
            READ_CONTROL,
            PINNED_FILE_SHARE,
            "metadata destination",
        )
        .expect("destination handle");
        let source_metadata =
            SecuritySnapshot::capture(source_handle.handle.raw()).expect("source metadata");
        let destination_metadata = SecuritySnapshot::capture(destination_handle.handle.raw())
            .expect("destination metadata");
        assert!(
            source_metadata
                .equivalent_to(&destination_metadata)
                .expect("compare metadata")
        );
    }

    #[test]
    fn security_metadata_copy_rejects_a_replaced_destination_identity() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let recovery = directory.path().join(".unity-asset-recovery").join("v1");
        create_private_directory(&recovery).expect("private directory");
        let source = recovery.join("source");
        let destination = recovery.join("destination");
        let displaced = recovery.join("displaced");
        drop(create_private_file(&source).expect("source"));
        drop(create_private_file(&destination).expect("destination"));
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_destination =
            observe_file_identity(&destination).expect("destination identity");
        let expected_source_parent =
            observe_directory_identity(&recovery).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&recovery).expect("destination parent identity");
        fs::rename(&destination, &displaced).expect("displace destination");
        drop(create_private_file(&destination).expect("replacement destination"));
        let mut budget = AssetLoadBudget::default();

        let error = copy_security_metadata(
            &source,
            &destination,
            &expected_source,
            &expected_source_parent,
            &expected_destination,
            &expected_destination_parent,
            &mut budget,
        )
        .expect_err("replacement identity rejected");

        let SecurityMetadataError::Io(error) = error else {
            panic!("replaced destination must fail with an I/O identity error");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            observe_file_identity(&displaced).expect("displaced identity"),
            expected_destination
        );
        assert_ne!(
            observe_file_identity(&destination).expect("replacement identity"),
            expected_destination
        );
    }

    #[test]
    fn security_metadata_copy_rejects_a_replaced_source_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_parent = directory.path().join("source-parent");
        let destination_parent = directory.path().join("destination-parent");
        let displaced_parent = directory.path().join("displaced-parent");
        fs::create_dir(&source_parent).expect("source parent");
        fs::create_dir(&destination_parent).expect("destination parent");
        let source = source_parent.join("source");
        let destination = destination_parent.join("destination");
        fs::write(&source, b"captured source").expect("source");
        fs::write(&destination, b"destination").expect("destination");
        let expected_source = observe_file_identity(&source).expect("source identity");
        let expected_destination =
            observe_file_identity(&destination).expect("destination identity");
        let expected_source_parent =
            observe_directory_identity(&source_parent).expect("source parent identity");
        let expected_destination_parent =
            observe_directory_identity(&destination_parent).expect("destination parent identity");
        fs::rename(&source_parent, &displaced_parent).expect("displace source parent");
        fs::create_dir(&source_parent).expect("replacement source parent");
        fs::write(&source, b"replacement source").expect("replacement source");
        let mut budget = AssetLoadBudget::default();

        let error = copy_security_metadata(
            &source,
            &destination,
            &expected_source,
            &expected_source_parent,
            &expected_destination,
            &expected_destination_parent,
            &mut budget,
        )
        .expect_err("replacement source parent rejected");

        let SecurityMetadataError::Io(error) = error else {
            panic!("replaced source parent must fail with an I/O identity error");
        };
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(
            fs::read(&destination).expect("destination remains"),
            b"destination"
        );
    }
}
