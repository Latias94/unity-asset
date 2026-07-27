//! Identity-bound, no-follow reads for persisted generation contracts.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::Path;

/// A security-relevant failure while opening or validating persisted state.
#[derive(Debug)]
pub(crate) enum SecureReadError {
    Io(io::Error),
    LinkOrReparse,
    NotDirectory,
    NotRegular,
    IdentityChanged,
}

impl std::fmt::Display for SecureReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => source.fmt(formatter),
            Self::LinkOrReparse => {
                formatter.write_str("persisted path contains a symbolic link or reparse point")
            }
            Self::NotDirectory => formatter.write_str("persisted path is not a directory"),
            Self::NotRegular => formatter.write_str("persisted entry is not a regular file"),
            Self::IdentityChanged => formatter
                .write_str("persisted file identity, link count, or length is unsafe or changed"),
        }
    }
}

impl std::error::Error for SecureReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::LinkOrReparse | Self::NotDirectory | Self::NotRegular | Self::IdentityChanged => {
                None
            }
        }
    }
}

impl From<io::Error> for SecureReadError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// One already-opened directory used as the authority for child lookups.
pub(crate) struct ReadDirectory(platform::ReadDirectory);

impl ReadDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, SecureReadError> {
        platform::open_directory(path).map(Self)
    }

    pub(crate) fn open_directory(&self, name: &OsStr) -> Result<Self, SecureReadError> {
        platform::open_directory_at(&self.0, name).map(Self)
    }

    pub(crate) fn open_regular(&self, name: &OsStr) -> Result<RegularFile, SecureReadError> {
        let (file, identity) = platform::open_regular_at(&self.0, name)?;
        let length = identity.length();
        Ok(RegularFile {
            file,
            identity,
            length,
        })
    }
}

impl std::fmt::Debug for ReadDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReadDirectory(..)")
    }
}

/// A regular file whose identity and length came from this exact open handle.
pub(crate) struct RegularFile {
    file: File,
    identity: platform::FileIdentity,
    length: u64,
}

impl RegularFile {
    #[must_use]
    pub(crate) const fn length(&self) -> u64 {
        self.length
    }

    pub(crate) const fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn ensure_unchanged(&self) -> Result<(), SecureReadError> {
        let actual = platform::opened_file_identity(&self.file)?;
        if actual == self.identity {
            Ok(())
        } else {
            Err(SecureReadError::IdentityChanged)
        }
    }

    pub(crate) fn range(
        &self,
        offset: u64,
        length: u64,
    ) -> Result<RegularFileRange<'_>, SecureReadError> {
        self.ensure_unchanged()?;
        let end = offset.checked_add(length).ok_or_else(|| {
            SecureReadError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("persisted file range offset {offset} plus length {length} overflows u64"),
            ))
        })?;
        if end > self.length {
            return Err(SecureReadError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "persisted file range {offset}+{length} exceeds the opened file length {}",
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

#[cfg(unix)]
mod platform {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::FileExt as _;
    use std::path::{Component, Path};

    use rustix::fs::CWD;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat, fstat, openat, statat};
    use rustix::io::Errno;

    use super::SecureReadError;

    const DIRECTORY_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    const REGULAR_FILE_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;

    pub(super) struct ReadDirectory {
        descriptor: OwnedFd,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct FileIdentity {
        device: u64,
        inode: u64,
        length: u64,
    }

    impl FileIdentity {
        pub(super) const fn length(self) -> u64 {
            self.length
        }
    }

    pub(super) fn open_directory(path: &Path) -> Result<ReadDirectory, SecureReadError> {
        let start = if path.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        };
        let mut descriptor =
            openat(CWD, start, DIRECTORY_FLAGS, Mode::empty()).map_err(map_open_error)?;
        validate_directory(&descriptor)?;
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    descriptor = open_directory_descriptor(&descriptor, name)?;
                }
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(SecureReadError::Io(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "persisted directory path contains an escaping component",
                    )));
                }
            }
        }
        Ok(ReadDirectory { descriptor })
    }

    pub(super) fn open_directory_at(
        parent: &ReadDirectory,
        name: &OsStr,
    ) -> Result<ReadDirectory, SecureReadError> {
        validate_leaf(name)?;
        let descriptor = open_directory_descriptor(&parent.descriptor, name)?;
        Ok(ReadDirectory { descriptor })
    }

    pub(super) fn open_regular_at(
        parent: &ReadDirectory,
        name: &OsStr,
    ) -> Result<(File, FileIdentity), SecureReadError> {
        validate_leaf(name)?;
        let descriptor = openat(&parent.descriptor, name, REGULAR_FILE_FLAGS, Mode::empty())
            .map_err(map_open_error)?;
        let metadata = fstat(&descriptor).map_err(io_error)?;
        if !FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(SecureReadError::NotRegular);
        }
        if metadata.st_nlink != 1 {
            return Err(SecureReadError::IdentityChanged);
        }
        let identity = file_identity(&metadata)?;
        let named = statat(&parent.descriptor, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| SecureReadError::IdentityChanged)?;
        if !FileType::from_raw_mode(named.st_mode).is_file() || file_identity(&named)? != identity {
            return Err(SecureReadError::IdentityChanged);
        }
        Ok((descriptor.into(), identity))
    }

    pub(super) fn opened_file_identity(file: &File) -> Result<FileIdentity, SecureReadError> {
        let metadata = fstat(file.as_fd()).map_err(io_error)?;
        if !FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(SecureReadError::NotRegular);
        }
        if metadata.st_nlink != 1 {
            return Err(SecureReadError::IdentityChanged);
        }
        file_identity(&metadata)
    }

    pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        file.read_at(buffer, offset)
    }

    fn open_directory_descriptor(
        parent: &OwnedFd,
        name: &OsStr,
    ) -> Result<OwnedFd, SecureReadError> {
        validate_leaf(name)?;
        let descriptor =
            openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(map_open_error)?;
        let opened = fstat(&descriptor).map_err(io_error)?;
        if !FileType::from_raw_mode(opened.st_mode).is_dir() {
            return Err(SecureReadError::NotDirectory);
        }
        let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| SecureReadError::IdentityChanged)?;
        if !FileType::from_raw_mode(named.st_mode).is_dir()
            || named.st_dev != opened.st_dev
            || named.st_ino != opened.st_ino
        {
            return Err(SecureReadError::IdentityChanged);
        }
        Ok(descriptor)
    }

    fn validate_directory(descriptor: &OwnedFd) -> Result<(), SecureReadError> {
        let metadata = fstat(descriptor).map_err(io_error)?;
        if FileType::from_raw_mode(metadata.st_mode).is_dir() {
            Ok(())
        } else {
            Err(SecureReadError::NotDirectory)
        }
    }

    fn file_identity(metadata: &Stat) -> Result<FileIdentity, SecureReadError> {
        let length = u64::try_from(metadata.st_size).map_err(|_| {
            SecureReadError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted regular file has a negative length",
            ))
        })?;
        Ok(FileIdentity {
            device: metadata.st_dev as u64,
            inode: metadata.st_ino as u64,
            length,
        })
    }

    fn validate_leaf(name: &OsStr) -> Result<(), SecureReadError> {
        let name = name.as_bytes();
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(SecureReadError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "persisted child name is not a single path component",
            )));
        }
        Ok(())
    }

    fn map_open_error(source: Errno) -> SecureReadError {
        if source == Errno::LOOP {
            SecureReadError::LinkOrReparse
        } else if source == Errno::NOTDIR {
            SecureReadError::NotDirectory
        } else {
            io_error(source)
        }
    }

    fn io_error(source: Errno) -> SecureReadError {
        SecureReadError::Io(source.into())
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::FileExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Component, Components, Path, Prefix};

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE,
        OBJ_DONT_REPARSE, RtlNtStatusToDosError, STATUS_REPARSE_POINT_ENCOUNTERED,
        STATUS_STOPPED_ON_SYMLINK,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FILE_TRAVERSE, FileAttributeTagInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    use super::SecureReadError;

    const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
    const FILE_SHARE: u32 = FILE_SHARE_READ;
    const DIRECTORY_ACCESS: u32 =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const MAX_WINDOWS_ROOT_UTF16_UNITS: usize = 1_024;
    const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;
    const WINDOWS_ROOT_BUFFER_UTF16_UNITS: usize = MAX_WINDOWS_ROOT_UTF16_UNITS + 2;

    pub(super) struct ReadDirectory {
        handle: OwnedHandle,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct FileIdentity {
        volume_serial_number: u64,
        file_id: [u8; 16],
        length: u64,
    }

    impl FileIdentity {
        pub(super) const fn length(self) -> u64 {
            self.length
        }
    }

    struct OwnedHandle(HANDLE);

    // SAFETY: this wrapper uniquely owns a Windows kernel handle. All operations exposed through
    // it are immutable handle queries or relative opens, which Windows permits from multiple
    // threads, and `Drop` closes the handle only after the last owner is gone.
    unsafe impl Send for OwnedHandle {}
    // SAFETY: see the `Send` justification above; no operation relies on a shared file cursor.
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

    pub(super) fn open_directory(path: &Path) -> Result<ReadDirectory, SecureReadError> {
        let mut path = AbsolutePathParts::new(path)?;
        let mut directory = open_root(path.root())?;
        while let Some(name) = path.next_component()? {
            directory = open_directory_handle_at(directory.raw(), name)?;
        }
        Ok(ReadDirectory { handle: directory })
    }

    pub(super) fn open_directory_at(
        parent: &ReadDirectory,
        name: &OsStr,
    ) -> Result<ReadDirectory, SecureReadError> {
        validate_leaf(name)?;
        open_directory_handle_at(parent.handle.raw(), name).map(|handle| ReadDirectory { handle })
    }

    pub(super) fn open_regular_at(
        parent: &ReadDirectory,
        name: &OsStr,
    ) -> Result<(File, FileIdentity), SecureReadError> {
        validate_leaf(name)?;
        let handle = nt_create_at(
            parent.handle.raw(),
            name,
            GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_ATTRIBUTE_NORMAL,
        )?;
        validate_regular_handle(handle.raw())?;
        let identity = file_identity(handle.raw())?;
        Ok((handle.into_file(), identity))
    }

    pub(super) fn opened_file_identity(file: &File) -> Result<FileIdentity, SecureReadError> {
        let handle = file.as_raw_handle();
        validate_regular_handle(handle)?;
        file_identity(handle)
    }

    pub(super) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        file.seek_read(buffer, offset)
    }

    fn open_root(root: &OsStr) -> Result<OwnedHandle, SecureReadError> {
        let mut path = [0_u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS];
        encode_root(root, &mut path)?;
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                DIRECTORY_ACCESS,
                DIRECTORY_SHARE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(SecureReadError::Io(io::Error::last_os_error()));
        }
        let handle = OwnedHandle(handle);
        validate_directory_handle(handle.raw())?;
        Ok(handle)
    }

    fn open_directory_handle_at(
        parent: HANDLE,
        name: &OsStr,
    ) -> Result<OwnedHandle, SecureReadError> {
        let handle = nt_create_at(
            parent,
            name,
            DIRECTORY_ACCESS,
            DIRECTORY_SHARE,
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
    ) -> Result<OwnedHandle, SecureReadError> {
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
            Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
                .map_err(|_| invalid_component())?,
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
            return Err(SecureReadError::Io(io::Error::other(
                "NtCreateFile succeeded without returning a valid handle",
            )));
        }
        Ok(OwnedHandle(handle))
    }

    fn validate_directory_handle(handle: HANDLE) -> Result<(), SecureReadError> {
        validate_non_reparse(handle)?;
        if file_standard_information(handle)?.Directory {
            Ok(())
        } else {
            Err(SecureReadError::NotDirectory)
        }
    }

    fn validate_regular_handle(handle: HANDLE) -> Result<(), SecureReadError> {
        validate_non_reparse(handle)?;
        let information = file_standard_information(handle)?;
        if information.Directory {
            Err(SecureReadError::NotRegular)
        } else if information.NumberOfLinks != 1 {
            Err(SecureReadError::IdentityChanged)
        } else {
            Ok(())
        }
    }

    fn validate_non_reparse(handle: HANDLE) -> Result<(), SecureReadError> {
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
            return Err(SecureReadError::Io(io::Error::last_os_error()));
        }
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Err(SecureReadError::LinkOrReparse)
        } else {
            Ok(())
        }
    }

    fn file_standard_information(handle: HANDLE) -> Result<FILE_STANDARD_INFO, SecureReadError> {
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
            Err(SecureReadError::Io(io::Error::last_os_error()))
        } else {
            Ok(information)
        }
    }

    fn file_id_information(handle: HANDLE) -> Result<FILE_ID_INFO, SecureReadError> {
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
            Err(SecureReadError::Io(io::Error::last_os_error()))
        } else {
            Ok(information)
        }
    }

    fn file_identity(handle: HANDLE) -> Result<FileIdentity, SecureReadError> {
        let standard = file_standard_information(handle)?;
        if standard.Directory {
            return Err(SecureReadError::NotRegular);
        }
        let length = u64::try_from(standard.EndOfFile).map_err(|_| {
            SecureReadError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted regular file has a negative length",
            ))
        })?;
        let information = file_id_information(handle)?;
        Ok(FileIdentity {
            volume_serial_number: information.VolumeSerialNumber,
            file_id: information.FileId.Identifier,
            length,
        })
    }

    fn fixed_structure_size<T>() -> Result<u32, SecureReadError> {
        u32::try_from(size_of::<T>()).map_err(|_| {
            SecureReadError::Io(io::Error::other(
                "Windows file information structure exceeds u32",
            ))
        })
    }

    fn ntstatus_result(status: NTSTATUS) -> Result<(), SecureReadError> {
        if status >= 0 {
            return Ok(());
        }
        if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
            return Err(SecureReadError::LinkOrReparse);
        }
        let win32 = unsafe { RtlNtStatusToDosError(status) };
        let raw = i32::try_from(win32).map_err(|_| {
            SecureReadError::Io(io::Error::other(format!(
                "NtCreateFile failed with NTSTATUS {status:#010x} and unmapped Win32 code {win32}"
            )))
        })?;
        Err(SecureReadError::Io(io::Error::from_raw_os_error(raw)))
    }

    struct AbsolutePathParts<'path> {
        root: &'path OsStr,
        components: Components<'path>,
    }

    impl<'path> AbsolutePathParts<'path> {
        fn new(path: &'path Path) -> Result<Self, SecureReadError> {
            if !path.is_absolute() {
                return Err(SecureReadError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "persisted Windows directory path must be absolute",
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

        fn next_component(&mut self) -> Result<Option<&'path OsStr>, SecureReadError> {
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

    fn validate_root(root: &OsStr) -> Result<(), SecureReadError> {
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
    ) -> Result<(), SecureReadError> {
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

    fn validate_leaf(name: &OsStr) -> Result<usize, SecureReadError> {
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
    ) -> Result<usize, SecureReadError> {
        let length = validate_leaf(name)?;
        for (index, unit) in name.encode_wide().enumerate() {
            buffer[index] = unit;
        }
        Ok(length)
    }

    fn invalid_root() -> SecureReadError {
        SecureReadError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted Windows path has an invalid or unsupported root",
        ))
    }

    fn invalid_component() -> SecureReadError {
        SecureReadError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "persisted child name is not a single Windows path component",
        ))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    use super::SecureReadError;

    pub(super) struct ReadDirectory;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct FileIdentity;

    impl FileIdentity {
        pub(super) const fn length(self) -> u64 {
            0
        }
    }

    pub(super) fn open_directory(_: &Path) -> Result<ReadDirectory, SecureReadError> {
        Err(unsupported())
    }

    pub(super) fn open_directory_at(
        _: &ReadDirectory,
        _: &OsStr,
    ) -> Result<ReadDirectory, SecureReadError> {
        Err(unsupported())
    }

    pub(super) fn open_regular_at(
        _: &ReadDirectory,
        _: &OsStr,
    ) -> Result<(File, FileIdentity), SecureReadError> {
        Err(unsupported())
    }

    pub(super) fn opened_file_identity(_: &File) -> Result<FileIdentity, SecureReadError> {
        Err(unsupported())
    }

    pub(super) fn read_at(_: &File, _: &mut [u8], _: u64) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional persisted reads are unsupported on this platform",
        ))
    }

    fn unsupported() -> SecureReadError {
        SecureReadError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound persisted reads are unsupported on this platform",
        ))
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::io::Read as _;
    use std::os::unix::fs::symlink;

    use tempfile::tempdir;

    use super::{ReadDirectory, SecureReadError};

    #[test]
    fn rejects_symbolic_link_leaf() {
        let temporary = tempdir().unwrap();
        let directory = temporary.path().join("state");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("target.json"), b"{}").unwrap();
        symlink("target.json", directory.join("source-state-v1.json")).unwrap();

        let directory = ReadDirectory::open(&directory).unwrap();
        let error = directory
            .open_regular(OsStr::new("source-state-v1.json"))
            .unwrap_err();

        assert!(matches!(error, SecureReadError::LinkOrReparse));
    }

    #[test]
    fn pinned_parent_cannot_be_redirected_before_leaf_open() {
        let temporary = tempdir().unwrap();
        let parent = temporary.path().join("activations");
        let replacement = temporary.path().join("replacement");
        let displaced = temporary.path().join("displaced");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&replacement).unwrap();
        fs::write(parent.join("00000000000000000001.json"), b"original").unwrap();
        fs::write(
            replacement.join("00000000000000000001.json"),
            b"replacement",
        )
        .unwrap();
        let directory = ReadDirectory::open(&parent).unwrap();

        let mut opened = directory
            .open_regular_after(OsStr::new("00000000000000000001.json"), || {
                fs::rename(&parent, &displaced).unwrap();
                fs::rename(&replacement, &parent).unwrap();
            })
            .unwrap();
        let mut actual = String::new();
        opened.file_mut().read_to_string(&mut actual).unwrap();

        assert_eq!(actual, "original");
        opened.ensure_unchanged().unwrap();
    }

    #[test]
    fn range_rejects_a_file_truncated_after_open() {
        let temporary = tempdir().unwrap();
        let directory_path = temporary.path().join("state");
        fs::create_dir(&directory_path).unwrap();
        let file_path = directory_path.join("source-state-v1.json");
        fs::write(&file_path, b"original").unwrap();
        let directory = ReadDirectory::open(&directory_path).unwrap();
        let opened = directory
            .open_regular(OsStr::new("source-state-v1.json"))
            .unwrap();

        fs::OpenOptions::new()
            .write(true)
            .open(&file_path)
            .unwrap()
            .set_len(4)
            .unwrap();
        let error = opened.range(0, 4).unwrap_err();

        assert!(matches!(error, SecureReadError::IdentityChanged));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn rejects_fifo_without_waiting_for_a_writer() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        use rustix::fs::{CWD, Mode, mkfifoat};

        let temporary = tempdir().unwrap();
        let directory_path = temporary.path().join("state");
        fs::create_dir(&directory_path).unwrap();
        let fifo = directory_path.join("source-state-v1.json");
        mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        let directory = ReadDirectory::open(&directory_path).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            sender
                .send(directory.open_regular(OsStr::new("source-state-v1.json")))
                .unwrap();
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("no-follow regular-file validation blocked on a FIFO");
        worker.join().unwrap();

        assert!(matches!(result, Err(SecureReadError::NotRegular)));
    }
}
