use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use thiserror::Error;
use unity_asset_search_protocol::ProjectId;

const PROJECT_ID_DOMAIN: &[u8] = b"unity-asset:project-identity:v1\0";
const PROJECT_MARKERS: [&str; 2] = ["Assets", "ProjectSettings"];

pub struct ProjectLocatorV1 {
    root: PathBuf,
    identity: ProjectIdentityV1,
    platform_identity: platform::DirectoryIdentity,
    authority: platform::ReadDirectory,
}

impl ProjectLocatorV1 {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ProjectLocatorError> {
        let root = absolute_path(root.as_ref())?;
        let authority =
            platform::ReadDirectory::open(&root).map_err(|source| map_root_error(&root, source))?;
        let platform_identity = authority
            .identity()
            .map_err(|source| map_root_error(&root, source))?;
        validate_markers(&authority, &root)?;
        let identity = ProjectIdentityV1 {
            project_id: derive_project_id(platform_identity)?,
        };
        Ok(Self {
            root,
            identity,
            platform_identity,
            authority,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn identity(&self) -> &ProjectIdentityV1 {
        &self.identity
    }

    #[must_use]
    pub const fn project_id(&self) -> ProjectId {
        self.identity.project_id
    }

    pub fn revalidate(&self) -> Result<(), ProjectLocatorError> {
        let pinned = self
            .authority
            .identity()
            .map_err(|source| map_root_error(&self.root, source))?;
        if pinned != self.platform_identity {
            return Err(ProjectLocatorError::IdentityChanged {
                path: self.root.clone(),
            });
        }
        let reopened = platform::ReadDirectory::open(&self.root)
            .map_err(|source| map_root_error(&self.root, source))?;
        let rebound = reopened
            .identity()
            .map_err(|source| map_root_error(&self.root, source))?;
        if rebound != self.platform_identity {
            return Err(ProjectLocatorError::IdentityChanged {
                path: self.root.clone(),
            });
        }
        validate_markers(&reopened, &self.root)
    }
}

impl std::fmt::Debug for ProjectLocatorV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectLocatorV1")
            .field("root", &self.root)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProjectIdentityV1 {
    project_id: ProjectId,
}

impl ProjectIdentityV1 {
    /// Derives a stable identity for an existing local directory without asserting that it is a
    /// Unity project. This is intended for path-scoped local state; callers that need Unity
    /// project validation must use [`ProjectLocatorV1::open`].
    pub fn for_existing_root(root: impl AsRef<Path>) -> Result<Self, ProjectLocatorError> {
        let root = absolute_path(root.as_ref())?;
        let authority =
            platform::ReadDirectory::open(&root).map_err(|source| map_root_error(&root, source))?;
        let platform_identity = authority
            .identity()
            .map_err(|source| map_root_error(&root, source))?;
        let reopened =
            platform::ReadDirectory::open(&root).map_err(|source| map_root_error(&root, source))?;
        let rebound = reopened
            .identity()
            .map_err(|source| map_root_error(&root, source))?;
        if rebound != platform_identity {
            return Err(ProjectLocatorError::IdentityChanged { path: root });
        }
        Ok(Self {
            project_id: derive_project_id(platform_identity)?,
        })
    }

    #[must_use]
    pub const fn project_id(self) -> ProjectId {
        self.project_id
    }
}

fn validate_markers(
    authority: &platform::ReadDirectory,
    root: &Path,
) -> Result<(), ProjectLocatorError> {
    for marker in PROJECT_MARKERS {
        authority
            .open_directory(OsStr::new(marker))
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    ProjectLocatorError::MissingMarker {
                        root: root.to_path_buf(),
                        marker,
                    }
                } else {
                    ProjectLocatorError::InvalidMarker {
                        root: root.to_path_buf(),
                        marker,
                        source,
                    }
                }
            })?;
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, ProjectLocatorError> {
    if path.as_os_str().is_empty() {
        return Err(ProjectLocatorError::EmptyRoot);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|source| ProjectLocatorError::Io {
            operation: "resolve current directory",
            path: path.to_path_buf(),
            source,
        })
}

fn derive_project_id(
    identity: platform::DirectoryIdentity,
) -> Result<ProjectId, ProjectLocatorError> {
    let mut hasher = Sha256::new();
    hasher.update(PROJECT_ID_DOMAIN);
    identity.update_digest(&mut hasher);
    let bytes: [u8; 32] = hasher.finalize().into();
    if bytes.iter().all(|byte| *byte == 0) {
        Err(ProjectLocatorError::InvalidDerivedIdentity)
    } else {
        Ok(ProjectId::from_bytes(bytes))
    }
}

fn map_root_error(path: &Path, source: io::Error) -> ProjectLocatorError {
    match source.kind() {
        io::ErrorKind::NotFound => ProjectLocatorError::RootNotFound {
            path: path.to_path_buf(),
        },
        io::ErrorKind::Unsupported
            if source
                .get_ref()
                .is_some_and(|error| error.is::<UnsupportedPlatformIo>()) =>
        {
            ProjectLocatorError::UnsupportedPlatform
        }
        io::ErrorKind::Unsupported => ProjectLocatorError::UnsupportedFilesystem {
            path: path.to_path_buf(),
            source,
        },
        _ => ProjectLocatorError::InvalidRoot {
            path: path.to_path_buf(),
            source,
        },
    }
}

#[derive(Debug)]
struct UnsupportedPlatformIo;

impl std::fmt::Display for UnsupportedPlatformIo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stable project identity is unsupported on this platform")
    }
}

impl std::error::Error for UnsupportedPlatformIo {}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn unsupported_platform_io() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, UnsupportedPlatformIo)
}

#[cfg(windows)]
fn unsupported_filesystem(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, reason)
}

#[derive(Debug, Error)]
pub enum ProjectLocatorError {
    #[error("project root must not be empty")]
    EmptyRoot,
    #[error("project root does not exist: {path}")]
    RootNotFound { path: PathBuf },
    #[error("project root is not an ordinary no-follow directory: {path}: {source}")]
    InvalidRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Unity project root {root} is missing the {marker} directory")]
    MissingMarker { root: PathBuf, marker: &'static str },
    #[error("Unity marker {marker} is not an ordinary no-follow directory under {root}: {source}")]
    InvalidMarker {
        root: PathBuf,
        marker: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("project root identity changed while locating {path}")]
    IdentityChanged { path: PathBuf },
    #[error("the derived project identity was the reserved zero value")]
    InvalidDerivedIdentity,
    #[error(
        "project root filesystem cannot provide a supported stable local identity at {path}: {source}"
    )]
    UnsupportedFilesystem {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("stable project identity is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("failed to {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
mod tests {
    use std::fs;

    use super::ProjectIdentityV1;

    #[test]
    fn existing_root_identity_survives_rename_but_not_copy() {
        let temporary = tempfile::tempdir().unwrap();
        let original = temporary.path().join("original");
        let renamed = temporary.path().join("renamed");
        let copied = temporary.path().join("copied");
        fs::create_dir(&original).unwrap();
        fs::write(original.join("asset.txt"), b"content").unwrap();

        let first = ProjectIdentityV1::for_existing_root(&original).unwrap();
        fs::rename(&original, &renamed).unwrap();
        let after_rename = ProjectIdentityV1::for_existing_root(&renamed).unwrap();
        fs::create_dir(&copied).unwrap();
        fs::copy(renamed.join("asset.txt"), copied.join("asset.txt")).unwrap();
        let after_copy = ProjectIdentityV1::for_existing_root(&copied).unwrap();

        assert_eq!(first.project_id(), after_rename.project_id());
        assert_ne!(first.project_id(), after_copy.project_id());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path};

    use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fstat, openat, statat};
    use sha2::Digest as _;
    use sha2::Sha256;

    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    pub(super) struct ReadDirectory {
        descriptor: OwnedFd,
    }

    impl ReadDirectory {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            let start = if path.is_absolute() {
                Path::new("/")
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "project root must be absolute",
                ));
            };
            let mut descriptor =
                openat(CWD, start, directory_flags(), Mode::empty()).map_err(io::Error::from)?;
            for component in path.components() {
                match component {
                    Component::RootDir => {}
                    Component::Normal(name) => {
                        descriptor = open_directory_at(&descriptor, name)?;
                    }
                    Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                        return Err(invalid_component());
                    }
                }
            }
            validate_directory(&descriptor)?;
            validate_local_filesystem(&descriptor)?;
            Ok(Self { descriptor })
        }

        pub(super) fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
            open_directory_at(&self.descriptor, name).map(|descriptor| Self { descriptor })
        }

        pub(super) fn identity(&self) -> io::Result<DirectoryIdentity> {
            let stat = fstat(self.descriptor.as_fd()).map_err(io::Error::from)?;
            directory_identity(&stat)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct DirectoryIdentity {
        device: u64,
        inode: u64,
    }

    impl DirectoryIdentity {
        pub(super) fn update_digest(self, digest: &mut Sha256) {
            #[cfg(target_os = "linux")]
            digest.update(b"linux\0");
            #[cfg(target_os = "macos")]
            digest.update(b"macos\0");
            digest.update(self.device.to_le_bytes());
            digest.update(self.inode.to_le_bytes());
        }
    }

    fn open_directory_at(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
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

    fn validate_directory(descriptor: &OwnedFd) -> io::Result<()> {
        let stat = fstat(descriptor).map_err(io::Error::from)?;
        directory_identity(&stat).map(|_| ())
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

    fn validate_local_filesystem(descriptor: &OwnedFd) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(descriptor)
    }

    fn validate_leaf(name: &OsStr) -> io::Result<()> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&0)
        {
            return Err(invalid_component());
        }
        Ok(())
    }

    fn invalid_component() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "project path contains an escaping or invalid component",
        )
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::path::{Component, Components, Path, Prefix};

    use sha2::{Digest as _, Sha256};
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, HANDLE, INVALID_HANDLE_VALUE,
        NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
        STATUS_REPARSE_POINT_ENCOUNTERED, STATUS_STOPPED_ON_SYMLINK,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE, FileAttributeTagInfo,
        FileIdInfo, FileStandardInfo, GetDriveTypeW, GetFileInformationByHandleEx, OPEN_EXISTING,
        SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_UNKNOWN,
    };

    const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const DIRECTORY_ACCESS: u32 =
        FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const MAX_WINDOWS_ROOT_UTF16_UNITS: usize = 1_024;
    const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;
    const WINDOWS_ROOT_BUFFER_UTF16_UNITS: usize = MAX_WINDOWS_ROOT_UTF16_UNITS + 2;

    pub(super) struct ReadDirectory {
        handle: OwnedHandle,
    }

    impl ReadDirectory {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            let mut path = AbsolutePathParts::new(path)?;
            let mut directory = open_root(path.root())?;
            while let Some(name) = path.next_component()? {
                directory = open_directory_handle_at(directory.raw(), name)?;
            }
            Ok(Self { handle: directory })
        }

        pub(super) fn open_directory(&self, name: &OsStr) -> io::Result<Self> {
            open_directory_handle_at(self.handle.raw(), name).map(|handle| Self { handle })
        }

        pub(super) fn identity(&self) -> io::Result<DirectoryIdentity> {
            directory_identity(self.handle.raw())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct DirectoryIdentity {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    impl DirectoryIdentity {
        pub(super) fn update_digest(self, digest: &mut Sha256) {
            digest.update(b"windows\0");
            digest.update(self.volume_serial_number.to_le_bytes());
            digest.update(self.file_id);
        }
    }

    struct OwnedHandle(HANDLE);

    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    impl OwnedHandle {
        const fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: this wrapper uniquely owns the live kernel handle.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn open_root(root: &OsStr) -> io::Result<OwnedHandle> {
        let mut path = [0_u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS];
        encode_root(root, &mut path)?;
        // SAFETY: `path` is a NUL-terminated volume root.
        match unsafe { GetDriveTypeW(path.as_ptr()) } {
            DRIVE_REMOTE => {
                return Err(super::unsupported_filesystem(
                    "project root is on a mapped remote volume",
                ));
            }
            DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR => return Err(invalid_root()),
            _ => {}
        }
        // SAFETY: `path` is NUL terminated and all remaining arguments follow CreateFileW's
        // directory-open contract.
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
            return Err(io::Error::last_os_error());
        }
        let handle = OwnedHandle(handle);
        validate_directory_handle(handle.raw())?;
        crate::windows_volume::validate_local_volume(handle.raw())?;
        Ok(handle)
    }

    fn open_directory_handle_at(parent: HANDLE, name: &OsStr) -> io::Result<OwnedHandle> {
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
    ) -> io::Result<OwnedHandle> {
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
        // SAFETY: every pointer references initialized storage for the duration of the call.
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
                // SAFETY: a failed call may still return a handle that the caller must close.
                unsafe { CloseHandle(handle) };
            }
            return Err(error);
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(
                "NtCreateFile succeeded without returning a valid handle",
            ));
        }
        Ok(OwnedHandle(handle))
    }

    fn validate_directory_handle(handle: HANDLE) -> io::Result<()> {
        let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
        // SAFETY: `attributes` is writable for the exact structure size.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&raw mut attributes).cast(),
                fixed_structure_size::<FILE_ATTRIBUTE_TAG_INFO>()?,
            )
        } == 0
        {
            return Err(required_metadata_error(io::Error::last_os_error()));
        }
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("directory is a reparse point"));
        }
        let standard = file_standard_information(handle)?;
        if !standard.Directory {
            return Err(io::Error::other("path is not a directory"));
        }
        directory_identity(handle).map(|_| ())
    }

    fn directory_identity(handle: HANDLE) -> io::Result<DirectoryIdentity> {
        let mut information = FILE_ID_INFO::default();
        // SAFETY: `information` is writable for the exact structure size.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut information).cast(),
                fixed_structure_size::<FILE_ID_INFO>()?,
            )
        } == 0
        {
            return Err(required_metadata_error(io::Error::last_os_error()));
        }
        if information.VolumeSerialNumber == 0
            || information.FileId.Identifier.iter().all(|byte| *byte == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem returned an unstable zero directory identity",
            ));
        }
        Ok(DirectoryIdentity {
            volume_serial_number: information.VolumeSerialNumber,
            file_id: information.FileId.Identifier,
        })
    }

    fn file_standard_information(handle: HANDLE) -> io::Result<FILE_STANDARD_INFO> {
        let mut information = FILE_STANDARD_INFO::default();
        // SAFETY: `information` is writable for the exact structure size.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&raw mut information).cast(),
                fixed_structure_size::<FILE_STANDARD_INFO>()?,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(information)
        }
    }

    fn fixed_structure_size<T>() -> io::Result<u32> {
        u32::try_from(size_of::<T>())
            .map_err(|_| io::Error::other("Windows structure size exceeds u32"))
    }

    fn required_metadata_error(error: io::Error) -> io::Error {
        let raw = error.raw_os_error();
        if raw == Some(i32::try_from(ERROR_NOT_SUPPORTED).expect("Win32 code fits i32"))
            || raw == Some(i32::try_from(ERROR_INVALID_PARAMETER).expect("Win32 code fits i32"))
        {
            super::unsupported_filesystem(
                "project root filesystem does not expose the required stable identity metadata",
            )
        } else {
            error
        }
    }

    fn ntstatus_result(status: NTSTATUS) -> io::Result<()> {
        if status >= 0 {
            return Ok(());
        }
        if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
            return Err(io::Error::other("path contains a reparse point"));
        }
        // SAFETY: translating an NTSTATUS has no pointer preconditions.
        let win32 = unsafe { RtlNtStatusToDosError(status) };
        let raw = i32::try_from(win32).map_err(|_| {
            io::Error::other(format!(
                "NtCreateFile failed with NTSTATUS {status:#010x} and unmapped Win32 code {win32}"
            ))
        })?;
        Err(io::Error::from_raw_os_error(raw))
    }

    struct AbsolutePathParts<'path> {
        root: &'path OsStr,
        components: Components<'path>,
    }

    impl<'path> AbsolutePathParts<'path> {
        fn new(path: &'path Path) -> io::Result<Self> {
            if !path.is_absolute() {
                return Err(invalid_root());
            }
            let mut components = path.components();
            let Some(Component::Prefix(prefix)) = components.next() else {
                return Err(invalid_root());
            };
            match prefix.kind() {
                Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
                Prefix::UNC(_, _)
                | Prefix::VerbatimUNC(_, _)
                | Prefix::DeviceNS(_)
                | Prefix::Verbatim(_) => return Err(invalid_root()),
            }
            if !matches!(components.next(), Some(Component::RootDir)) {
                return Err(invalid_root());
            }
            let root = prefix.as_os_str();
            validate_root(root)?;
            for component in components.clone() {
                if !matches!(component, Component::Normal(_)) {
                    return Err(invalid_component());
                }
            }
            Ok(Self { root, components })
        }

        const fn root(&self) -> &'path OsStr {
            self.root
        }

        fn next_component(&mut self) -> io::Result<Option<&'path OsStr>> {
            match self.components.next() {
                Some(Component::Normal(name)) => Ok(Some(name)),
                Some(_) => Err(invalid_component()),
                None => Ok(None),
            }
        }
    }

    fn validate_root(root: &OsStr) -> io::Result<()> {
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
            Err(invalid_root())
        } else {
            Ok(())
        }
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
    ) -> io::Result<usize> {
        let length = validate_leaf(name)?;
        for (index, unit) in name.encode_wide().enumerate() {
            buffer[index] = unit;
        }
        Ok(length)
    }

    fn invalid_root() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows project root has an invalid or unsupported root",
        )
    }

    fn invalid_component() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "project path contains an escaping or invalid Windows component",
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn remote_roots_are_rejected() {
            assert!(AbsolutePathParts::new(Path::new(r"\\server\share\project")).is_err());
            assert!(AbsolutePathParts::new(Path::new(r"\\?\UNC\server\share\project")).is_err());
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    use sha2::Sha256;

    use super::unsupported_platform_io;

    pub(super) struct ReadDirectory;

    impl ReadDirectory {
        pub(super) fn open(_path: &Path) -> io::Result<Self> {
            Err(unsupported_platform_io())
        }

        pub(super) fn open_directory(&self, _name: &OsStr) -> io::Result<Self> {
            Err(unsupported_platform_io())
        }

        pub(super) fn identity(&self) -> io::Result<DirectoryIdentity> {
            Err(unsupported_platform_io())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) struct DirectoryIdentity;

    impl DirectoryIdentity {
        pub(super) fn update_digest(self, _digest: &mut Sha256) {}
    }
}
