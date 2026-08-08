use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{align_of, size_of};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::FileExt as _;
use std::os::windows::io::{
    AsHandle, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, RawHandle,
};
use std::path::{Component, Components, Path, Prefix};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_DIRECTORY_FILE, FILE_DIRECTORY_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileDirectoryInformation, NtCreateFile,
    NtQueryDirectoryFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, GENERIC_READ, HANDLE,
    INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_NO_MORE_FILES, STATUS_NOT_A_DIRECTORY,
    STATUS_REPARSE_POINT_ENCOUNTERED, STATUS_STOPPED_ON_SYMLINK, STATUS_SUCCESS,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE,
    FileAttributeTagInfo, FileBasicInfo, FileCaseSensitiveInfo, FileIdInfo, FileStandardInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING, SYNCHRONIZE,
};
#[cfg(test)]
use windows_sys::Win32::Storage::FileSystem::{FILE_WRITE_ATTRIBUTES, SetFileInformationByHandle};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{AnchoredFsError, DirectoryEntryHint, EntryKindHint, OpenPolicy};

const DIRECTORY_ACCESS: u32 =
    FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const MAX_WINDOWS_ROOT_UTF16_UNITS: usize = 1_024;
const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;
const WINDOWS_ROOT_BUFFER_UTF16_UNITS: usize = MAX_WINDOWS_ROOT_UTF16_UNITS + 2;
const DIRECTORY_BUFFER_WORDS: usize = 512;

const fn directory_share(policy: OpenPolicy) -> u32 {
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
    if policy.allows_concurrent_replacement() {
        share | FILE_SHARE_DELETE
    } else {
        share
    }
}

const fn regular_share(policy: OpenPolicy) -> u32 {
    if policy.allows_concurrent_replacement() {
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    } else {
        FILE_SHARE_READ
    }
}

pub(super) struct ReadDirectory {
    handle: OwnedHandle,
}

impl AsHandle for ReadDirectory {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        // SAFETY: the returned borrow cannot outlive this directory's owned live handle.
        unsafe { BorrowedHandle::borrow_raw(self.handle.raw() as RawHandle) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
    last_write_time: i64,
    change_time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryObjectIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
    length: u64,
    last_write_time: i64,
    change_time: i64,
}

impl FileIdentity {
    pub(super) const fn length(self) -> u64 {
        self.length
    }
}

struct OwnedHandle(HANDLE);

// SAFETY: this wrapper uniquely owns a Windows kernel handle. All operations exposed through it
// are immutable handle queries, relative opens, or synchronous positional reads.
unsafe impl Send for OwnedHandle {}
// SAFETY: see the `Send` justification; no operation relies on a shared file cursor.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    const fn raw(&self) -> HANDLE {
        self.0
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

pub(super) struct DirectoryEntries<'directory> {
    handle: OwnedHandle,
    buffer: [u64; DIRECTORY_BUFFER_WORDS],
    returned: usize,
    offset: usize,
    restart_scan: bool,
    finished: bool,
    _authority: std::marker::PhantomData<&'directory ReadDirectory>,
}

pub(super) struct DirectoryNames<'directory>(DirectoryEntries<'directory>);

impl Iterator for DirectoryNames<'_> {
    type Item = Result<OsString, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0
            .next()
            .map(|entry| entry.map(DirectoryEntryHint::into_name))
    }
}

impl Iterator for DirectoryEntries<'_> {
    type Item = Result<DirectoryEntryHint, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }
            if self.offset >= self.returned {
                match self.refill() {
                    Ok(true) => {}
                    Ok(false) => return None,
                    Err(error) => {
                        self.finished = true;
                        return Some(Err(error));
                    }
                }
            }
            match self.parse_entry() {
                Ok(entry)
                    if entry.name() == OsStr::new(".") || entry.name() == OsStr::new("..") => {}
                Ok(entry) => return Some(Ok(entry)),
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

impl DirectoryEntries<'_> {
    fn refill(&mut self) -> Result<bool, AnchoredFsError> {
        validate_directory_case_sensitivity(self.handle.raw())?;
        let buffer_bytes = size_of::<[u64; DIRECTORY_BUFFER_WORDS]>();
        let buffer_length = u32::try_from(buffer_bytes)
            .map_err(|_| invalid_data("Windows directory enumeration buffer exceeds u32"))?;
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtQueryDirectoryFile(
                self.handle.raw(),
                std::ptr::null_mut(),
                None,
                std::ptr::null(),
                &raw mut io_status,
                self.buffer.as_mut_ptr().cast(),
                buffer_length,
                FileDirectoryInformation,
                false,
                std::ptr::null(),
                self.restart_scan,
            )
        };
        self.restart_scan = false;
        if status == STATUS_NO_MORE_FILES {
            self.finished = true;
            return Ok(false);
        }
        if status != STATUS_SUCCESS {
            return Err(ntstatus_error(status));
        }
        let returned = io_status.Information;
        if returned == 0 || returned > buffer_bytes {
            return Err(invalid_data(
                "Windows directory enumeration returned an invalid byte count",
            ));
        }
        self.returned = returned;
        self.offset = 0;
        Ok(true)
    }

    fn parse_entry(&mut self) -> Result<DirectoryEntryHint, AnchoredFsError> {
        const NEXT_ENTRY_OFFSET: usize =
            std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, NextEntryOffset);
        const FILE_ATTRIBUTES_OFFSET: usize =
            std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileAttributes);
        const FILE_NAME_LENGTH_OFFSET: usize =
            std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileNameLength);
        const FILE_NAME_OFFSET: usize = std::mem::offset_of!(FILE_DIRECTORY_INFORMATION, FileName);

        let remaining = self
            .returned
            .checked_sub(self.offset)
            .ok_or_else(|| invalid_data("Windows directory enumeration offset overflowed"))?;
        if remaining < FILE_NAME_OFFSET {
            return Err(invalid_data(
                "Windows directory enumeration record is truncated",
            ));
        }
        let base = self.buffer.as_ptr().cast::<u8>();
        let (next_entry_offset, attributes, file_name_length) = unsafe {
            (
                std::ptr::read_unaligned(base.add(self.offset + NEXT_ENTRY_OFFSET).cast::<u32>()),
                std::ptr::read_unaligned(
                    base.add(self.offset + FILE_ATTRIBUTES_OFFSET).cast::<u32>(),
                ),
                std::ptr::read_unaligned(
                    base.add(self.offset + FILE_NAME_LENGTH_OFFSET)
                        .cast::<u32>(),
                ),
            )
        };
        let name_bytes = usize::try_from(file_name_length)
            .map_err(|_| invalid_data("Windows directory entry name length is unsupported"))?;
        if name_bytes % size_of::<u16>() != 0 {
            return Err(invalid_data(
                "Windows directory entry name is not UTF-16 aligned",
            ));
        }
        let record_bytes = FILE_NAME_OFFSET
            .checked_add(name_bytes)
            .ok_or_else(|| invalid_data("Windows directory entry length overflowed"))?;
        if record_bytes > remaining {
            return Err(invalid_data(
                "Windows directory entry extends beyond the query buffer",
            ));
        }
        let name = unsafe {
            std::slice::from_raw_parts(
                base.add(self.offset + FILE_NAME_OFFSET).cast::<u16>(),
                name_bytes / size_of::<u16>(),
            )
        };
        let name = OsString::from_wide(name);
        let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            EntryKindHint::LinkOrReparse
        } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            EntryKindHint::Directory
        } else {
            EntryKindHint::RegularFile
        };

        let next = usize::try_from(next_entry_offset)
            .map_err(|_| invalid_data("Windows directory entry offset is unsupported"))?;
        if next == 0 {
            self.offset = self.returned;
        } else {
            if next < record_bytes || next > remaining || next % align_of::<u64>() != 0 {
                return Err(invalid_data("Windows directory entry offset is invalid"));
            }
            self.offset = self
                .offset
                .checked_add(next)
                .ok_or_else(|| invalid_data("Windows directory entry offset overflowed"))?;
        }
        Ok(DirectoryEntryHint::new(name, kind))
    }
}

pub(super) fn open_directory(
    path: &Path,
    policy: OpenPolicy,
) -> Result<ReadDirectory, AnchoredFsError> {
    let mut path = AbsolutePathParts::new(path)?;
    let mut directory = open_root(path.root(), policy)?;
    while let Some(name) = path.next_component()? {
        directory = open_directory_handle_at(directory.raw(), name, policy)?;
    }
    Ok(ReadDirectory { handle: directory })
}

pub(super) fn open_directory_at(
    parent: &ReadDirectory,
    name: &OsStr,
    policy: OpenPolicy,
) -> Result<ReadDirectory, AnchoredFsError> {
    validate_leaf(name)?;
    open_directory_handle_at(parent.handle.raw(), name, policy)
        .map(|handle| ReadDirectory { handle })
}

pub(super) fn open_regular_at(
    parent: &ReadDirectory,
    name: &OsStr,
    policy: OpenPolicy,
) -> Result<(File, FileIdentity), AnchoredFsError> {
    validate_directory_case_sensitivity(parent.handle.raw())?;
    validate_leaf(name)?;
    let handle = nt_create_at(
        parent.handle.raw(),
        name,
        GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        regular_share(policy),
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_NORMAL,
    )?;
    validate_regular_handle(handle.raw(), policy)?;
    let identity = file_identity(handle.raw())?;
    Ok((handle.into_file(), identity))
}

pub(super) fn opened_file_identity(
    file: &File,
    policy: OpenPolicy,
) -> Result<FileIdentity, AnchoredFsError> {
    let handle = file.as_raw_handle();
    validate_regular_handle(handle, policy)?;
    file_identity(handle)
}

pub(super) fn opened_directory_identity(
    directory: &ReadDirectory,
) -> Result<DirectoryIdentity, AnchoredFsError> {
    let handle = directory.handle.raw();
    validate_directory_handle(handle)?;
    directory_identity(handle)
}

pub(super) fn opened_directory_object_identity(
    directory: &ReadDirectory,
) -> Result<DirectoryObjectIdentity, AnchoredFsError> {
    let identity = opened_directory_identity(directory)?;
    Ok(DirectoryObjectIdentity {
        volume_serial_number: identity.volume_serial_number,
        file_id: identity.file_id,
    })
}

pub(super) fn read_directory(
    directory: &ReadDirectory,
    _policy: OpenPolicy,
) -> Result<DirectoryEntries<'_>, AnchoredFsError> {
    let process = unsafe { GetCurrentProcess() };
    let mut handle = INVALID_HANDLE_VALUE;
    let duplicated = unsafe {
        DuplicateHandle(
            process,
            directory.handle.raw(),
            process,
            &raw mut handle,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if duplicated == 0 || handle == INVALID_HANDLE_VALUE {
        return Err(AnchoredFsError::Io(io::Error::last_os_error()));
    }
    let handle = OwnedHandle(handle);
    validate_directory_handle(handle.raw())?;
    Ok(DirectoryEntries {
        handle,
        buffer: [0_u64; DIRECTORY_BUFFER_WORDS],
        returned: 0,
        offset: 0,
        restart_scan: true,
        finished: false,
        _authority: std::marker::PhantomData,
    })
}

pub(super) fn read_directory_names(
    directory: &ReadDirectory,
    policy: OpenPolicy,
) -> Result<DirectoryNames<'_>, AnchoredFsError> {
    read_directory(directory, policy).map(DirectoryNames)
}

pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(buffer, offset)
}

#[cfg(test)]
pub(crate) fn try_enable_case_sensitive_directory_for_test(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "case-sensitive directory test path must be absolute",
        ));
    }
    let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if encoded.is_empty() || encoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "case-sensitive directory test path is invalid",
        ));
    }
    encoded.push(0);

    let raw_handle = unsafe {
        CreateFileW(
            encoded.as_ptr(),
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if raw_handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(raw_handle);
    let information = FILE_CASE_SENSITIVE_INFO {
        Flags: FILE_CS_FLAG_CASE_SENSITIVE_DIR,
    };
    let information_size = u32::try_from(size_of::<FILE_CASE_SENSITIVE_INFO>())
        .map_err(|_| io::Error::other("Windows case-sensitivity information exceeds u32"))?;
    let succeeded = unsafe {
        SetFileInformationByHandle(
            handle.raw(),
            FileCaseSensitiveInfo,
            (&raw const information).cast(),
            information_size,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = query_case_sensitivity_flags(handle.raw())?;
    if flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR == 0 {
        return Err(io::Error::other(
            "Windows accepted the case-sensitivity update without setting the directory flag",
        ));
    }
    Ok(())
}

fn open_root(root: &OsStr, policy: OpenPolicy) -> Result<OwnedHandle, AnchoredFsError> {
    let mut path = [0_u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS];
    encode_root(root, &mut path)?;
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            DIRECTORY_ACCESS,
            directory_share(policy),
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(AnchoredFsError::Io(io::Error::last_os_error()));
    }
    let handle = OwnedHandle(handle);
    validate_directory_handle(handle.raw())?;
    Ok(handle)
}

fn open_directory_handle_at(
    parent: HANDLE,
    name: &OsStr,
    policy: OpenPolicy,
) -> Result<OwnedHandle, AnchoredFsError> {
    validate_directory_case_sensitivity(parent)?;
    let handle = nt_create_at(
        parent,
        name,
        DIRECTORY_ACCESS,
        directory_share(policy),
        FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_ATTRIBUTE_DIRECTORY,
    )?;
    validate_directory_handle(handle.raw())?;
    Ok(handle)
}

fn nt_create_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    share: u32,
    options: u32,
    attributes: u32,
) -> Result<OwnedHandle, AnchoredFsError> {
    let mut encoded_name = [0_u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS];
    let name_length = encode_leaf(name, &mut encoded_name)?;
    let name_bytes = name_length
        .checked_mul(size_of::<u16>())
        .and_then(|bytes| u16::try_from(bytes).ok())
        .ok_or_else(invalid_component)?;
    let unicode = windows_sys::Win32::Foundation::UNICODE_STRING {
        Length: name_bytes,
        MaximumLength: name_bytes,
        Buffer: encoded_name.as_mut_ptr(),
    };
    let object_attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).map_err(|_| invalid_component())?,
        RootDirectory: parent,
        ObjectName: &raw const unicode,
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: std::ptr::null(),
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
            FILE_OPEN,
            options,
            std::ptr::null(),
            0,
        )
    };
    if let Err(error) = ntstatus_result(status) {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(handle);
            }
        }
        return Err(error);
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(AnchoredFsError::Io(io::Error::other(
            "NtCreateFile succeeded without returning a valid handle",
        )));
    }
    Ok(OwnedHandle(handle))
}

fn validate_directory_handle(handle: HANDLE) -> Result<(), AnchoredFsError> {
    validate_non_reparse(handle)?;
    if file_standard_information(handle)?.Directory {
        validate_directory_case_sensitivity(handle)
    } else {
        Err(AnchoredFsError::NotDirectory)
    }
}

fn validate_directory_case_sensitivity(handle: HANDLE) -> Result<(), AnchoredFsError> {
    validate_case_sensitivity_query(query_case_sensitivity_flags(handle))
}

fn query_case_sensitivity_flags(handle: HANDLE) -> io::Result<u32> {
    let mut information = FILE_CASE_SENSITIVE_INFO::default();
    let information_size = u32::try_from(size_of::<FILE_CASE_SENSITIVE_INFO>())
        .map_err(|_| io::Error::other("Windows case-sensitivity information exceeds u32"))?;
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileCaseSensitiveInfo,
            (&raw mut information).cast(),
            information_size,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(information.Flags)
    }
}

fn validate_case_sensitivity_query(result: io::Result<u32>) -> Result<(), AnchoredFsError> {
    let flags = result.map_err(AnchoredFsError::Io)?;
    if flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR == 0 {
        Ok(())
    } else {
        Err(AnchoredFsError::UnsupportedCaseSensitiveDirectory)
    }
}

fn validate_regular_handle(handle: HANDLE, policy: OpenPolicy) -> Result<(), AnchoredFsError> {
    validate_non_reparse(handle)?;
    let information = file_standard_information(handle)?;
    if information.Directory {
        Err(AnchoredFsError::NotRegular)
    } else if policy.requires_single_link() && information.NumberOfLinks != 1 {
        Err(AnchoredFsError::IdentityChanged)
    } else {
        Ok(())
    }
}

fn validate_non_reparse(handle: HANDLE) -> Result<(), AnchoredFsError> {
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
        return Err(AnchoredFsError::Io(io::Error::last_os_error()));
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(AnchoredFsError::LinkOrReparse)
    } else {
        Ok(())
    }
}

fn file_standard_information(handle: HANDLE) -> Result<FILE_STANDARD_INFO, AnchoredFsError> {
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
        Err(AnchoredFsError::Io(io::Error::last_os_error()))
    } else {
        Ok(information)
    }
}

fn file_id_information(handle: HANDLE) -> Result<FILE_ID_INFO, AnchoredFsError> {
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
        Err(AnchoredFsError::Io(io::Error::last_os_error()))
    } else {
        Ok(information)
    }
}

fn file_basic_information(handle: HANDLE) -> Result<FILE_BASIC_INFO, AnchoredFsError> {
    let mut information = FILE_BASIC_INFO::default();
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            (&raw mut information).cast(),
            fixed_structure_size::<FILE_BASIC_INFO>()?,
        )
    };
    if succeeded == 0 {
        Err(AnchoredFsError::Io(io::Error::last_os_error()))
    } else {
        Ok(information)
    }
}

fn file_identity(handle: HANDLE) -> Result<FileIdentity, AnchoredFsError> {
    let standard = file_standard_information(handle)?;
    if standard.Directory {
        return Err(AnchoredFsError::NotRegular);
    }
    let length = u64::try_from(standard.EndOfFile).map_err(|_| {
        AnchoredFsError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "anchored regular file has a negative length",
        ))
    })?;
    let information = file_id_information(handle)?;
    let basic = file_basic_information(handle)?;
    Ok(FileIdentity {
        volume_serial_number: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
        length,
        last_write_time: basic.LastWriteTime,
        change_time: basic.ChangeTime,
    })
}

fn directory_identity(handle: HANDLE) -> Result<DirectoryIdentity, AnchoredFsError> {
    if !file_standard_information(handle)?.Directory {
        return Err(AnchoredFsError::NotDirectory);
    }
    let information = file_id_information(handle)?;
    let basic = file_basic_information(handle)?;
    Ok(DirectoryIdentity {
        volume_serial_number: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
        last_write_time: basic.LastWriteTime,
        change_time: basic.ChangeTime,
    })
}

fn fixed_structure_size<T>() -> Result<u32, AnchoredFsError> {
    u32::try_from(size_of::<T>()).map_err(|_| {
        AnchoredFsError::Io(io::Error::other(
            "Windows file information structure exceeds u32",
        ))
    })
}

fn ntstatus_result(status: NTSTATUS) -> Result<(), AnchoredFsError> {
    if status >= 0 {
        return Ok(());
    }
    Err(ntstatus_error(status))
}

fn ntstatus_error(status: NTSTATUS) -> AnchoredFsError {
    if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
        return AnchoredFsError::LinkOrReparse;
    }
    if status == STATUS_NOT_A_DIRECTORY {
        return AnchoredFsError::NotDirectory;
    }
    if status == STATUS_FILE_IS_A_DIRECTORY {
        return AnchoredFsError::NotRegular;
    }
    let win32 = unsafe { RtlNtStatusToDosError(status) };
    let raw = match i32::try_from(win32) {
        Ok(raw) => raw,
        Err(_) => {
            return AnchoredFsError::Io(io::Error::other(format!(
                "Windows filesystem operation failed with NTSTATUS {status:#010x} and unmapped Win32 code {win32}"
            )));
        }
    };
    AnchoredFsError::Io(io::Error::from_raw_os_error(raw))
}

struct AbsolutePathParts<'path> {
    root: &'path OsStr,
    components: Components<'path>,
}

impl<'path> AbsolutePathParts<'path> {
    fn new(path: &'path Path) -> Result<Self, AnchoredFsError> {
        if !path.is_absolute() {
            return Err(AnchoredFsError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored Windows directory path must be absolute",
            )));
        }
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(invalid_root());
        };
        match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(_, _) => {}
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => return Err(invalid_root()),
        }
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(invalid_root());
        }
        let root = prefix.as_os_str();
        validate_root(root)?;
        for component in components.clone() {
            match component {
                Component::Normal(name) => {
                    validate_leaf(name)?;
                }
                Component::CurDir
                | Component::ParentDir
                | Component::Prefix(_)
                | Component::RootDir => return Err(invalid_component()),
            }
        }
        Ok(Self { root, components })
    }

    const fn root(&self) -> &'path OsStr {
        self.root
    }

    fn next_component(&mut self) -> Result<Option<&'path OsStr>, AnchoredFsError> {
        match self.components.next() {
            Some(Component::Normal(name)) => Ok(Some(name)),
            Some(
                Component::CurDir
                | Component::ParentDir
                | Component::Prefix(_)
                | Component::RootDir,
            ) => Err(invalid_component()),
            None => Ok(None),
        }
    }
}

fn validate_root(root: &OsStr) -> Result<(), AnchoredFsError> {
    let mut length = 0_usize;
    for unit in root.encode_wide() {
        if unit == 0 {
            return Err(invalid_root());
        }
        length = length.checked_add(1).ok_or_else(invalid_root)?;
        if length > MAX_WINDOWS_ROOT_UTF16_UNITS {
            return Err(invalid_root());
        }
    }
    if length == 0 {
        return Err(invalid_root());
    }
    Ok(())
}

fn encode_root(
    root: &OsStr,
    buffer: &mut [u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS],
) -> Result<(), AnchoredFsError> {
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

fn validate_leaf(name: &OsStr) -> Result<usize, AnchoredFsError> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(invalid_component());
    }
    let mut length = 0_usize;
    for unit in name.encode_wide() {
        if unit == 0
            || unit == u16::from(b':')
            || unit == u16::from(b'/')
            || unit == u16::from(b'\\')
        {
            return Err(invalid_component());
        }
        length = length.checked_add(1).ok_or_else(invalid_component)?;
        if length > MAX_WINDOWS_COMPONENT_UTF16_UNITS {
            return Err(invalid_component());
        }
    }
    Ok(length)
}

fn encode_leaf(
    name: &OsStr,
    buffer: &mut [u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS],
) -> Result<usize, AnchoredFsError> {
    let length = validate_leaf(name)?;
    for (index, unit) in name.encode_wide().enumerate() {
        buffer[index] = unit;
    }
    Ok(length)
}

fn invalid_root() -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        "anchored Windows path has an invalid or unsupported root",
    ))
}

fn invalid_component() -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        "anchored child name is not a single Windows path component",
    ))
}

fn invalid_data(message: &'static str) -> AnchoredFsError {
    AnchoredFsError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

#[cfg(test)]
mod tests {
    use std::io;

    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, STATUS_FILE_IS_A_DIRECTORY, STATUS_NOT_A_DIRECTORY,
    };
    use windows_sys::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;

    use super::{AnchoredFsError, ntstatus_error, validate_case_sensitivity_query};

    #[test]
    fn maps_ntstatus_type_mismatches_to_typed_errors() {
        assert!(matches!(
            ntstatus_error(STATUS_NOT_A_DIRECTORY),
            AnchoredFsError::NotDirectory
        ));
        assert!(matches!(
            ntstatus_error(STATUS_FILE_IS_A_DIRECTORY),
            AnchoredFsError::NotRegular
        ));
    }

    #[test]
    fn maps_case_sensitivity_query_results_to_typed_errors() {
        assert!(validate_case_sensitivity_query(Ok(0)).is_ok());
        assert!(matches!(
            validate_case_sensitivity_query(Ok(FILE_CS_FLAG_CASE_SENSITIVE_DIR)),
            Err(AnchoredFsError::UnsupportedCaseSensitiveDirectory)
        ));
        assert!(matches!(
            validate_case_sensitivity_query(Ok(FILE_CS_FLAG_CASE_SENSITIVE_DIR | 0x8000_0000)),
            Err(AnchoredFsError::UnsupportedCaseSensitiveDirectory)
        ));

        let error = validate_case_sensitivity_query(Err(io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32,
        )))
        .unwrap_err();
        assert!(matches!(
            error,
            AnchoredFsError::Io(source)
                if source.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
        ));
    }
}
