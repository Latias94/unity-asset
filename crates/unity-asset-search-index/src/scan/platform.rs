use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};
    use std::sync::Arc;

    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};

    const DIRECTORY_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    const REGULAR_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;

    #[derive(Debug, Clone)]
    pub(super) struct ProjectReadRoot {
        directory: Arc<File>,
    }

    impl ProjectReadRoot {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            if !path.is_absolute() {
                return Err(invalid_path("project read root must be absolute"));
            }
            let mut directory = openat(
                rustix::fs::CWD,
                Path::new("/"),
                DIRECTORY_FLAGS,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            for component in path.components() {
                match component {
                    Component::RootDir | Component::CurDir => {}
                    Component::Normal(name) => {
                        directory = open_directory_at(&directory, name)?;
                    }
                    Component::ParentDir | Component::Prefix(_) => {
                        return Err(invalid_path(
                            "project read root contains an escaping component",
                        ));
                    }
                }
            }
            Ok(Self {
                directory: Arc::new(File::from(directory)),
            })
        }

        pub(super) fn open_relative(&self, relative: &Path) -> io::Result<File> {
            let mut components = relative.components().peekable();
            if components.peek().is_none() {
                return Err(invalid_path("project-relative file path is empty"));
            }

            let mut directory: Option<OwnedFd> = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(invalid_path(
                        "project-relative file path contains an escaping component",
                    ));
                };
                let is_leaf = components.peek().is_none();
                let opened = match directory.as_ref() {
                    Some(parent) if is_leaf => open_regular_at(parent, name)?,
                    Some(parent) => open_directory_at(parent, name)?,
                    None if is_leaf => open_regular_at(self.directory.as_ref(), name)?,
                    None => open_directory_at(self.directory.as_ref(), name)?,
                };
                if is_leaf {
                    return Ok(File::from(opened));
                }
                directory = Some(opened);
            }
            Err(invalid_path("project-relative file path has no leaf"))
        }
    }

    fn open_directory_at<Fd: std::os::fd::AsFd>(
        parent: Fd,
        name: &std::ffi::OsStr,
    ) -> io::Result<OwnedFd> {
        openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(map_open_error)
    }

    fn open_regular_at<Fd: std::os::fd::AsFd>(
        parent: Fd,
        name: &std::ffi::OsStr,
    ) -> io::Result<OwnedFd> {
        let descriptor =
            openat(parent, name, REGULAR_FLAGS, Mode::empty()).map_err(map_open_error)?;
        let metadata = fstat(&descriptor).map_err(io::Error::from)?;
        if !FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(invalid_path("project source is not a regular file"));
        }
        Ok(descriptor)
    }

    fn map_open_error(error: rustix::io::Errno) -> io::Error {
        if error == rustix::io::Errno::LOOP {
            invalid_path("project source path contains a symbolic link")
        } else {
            error.into()
        }
    }

    fn invalid_path(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::path::{Component, Path, Prefix};
    use std::sync::Arc;

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE,
        OBJ_DONT_REPARSE, RtlNtStatusToDosError, STATUS_REPARSE_POINT_ENCOUNTERED,
        STATUS_STOPPED_ON_SYMLINK,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FILE_TRAVERSE, FileAttributeTagInfo, FileStandardInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const DIRECTORY_ACCESS: u32 = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const FILE_ACCESS: u32 = GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
    const MAX_ROOT_UTF16_UNITS: usize = 1_024;
    const ROOT_BUFFER_UTF16_UNITS: usize = MAX_ROOT_UTF16_UNITS + 2;
    const MAX_COMPONENT_UTF16_UNITS: usize = 255;

    #[derive(Debug, Clone)]
    pub(super) struct ProjectReadRoot {
        directory: Arc<OwnedHandle>,
    }

    impl ProjectReadRoot {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            let (root, mut components) = absolute_parts(path)?;
            let mut directory = open_root(root)?;
            for component in &mut components {
                let Component::Normal(name) = component else {
                    return Err(invalid_path(
                        "project read root contains an invalid component",
                    ));
                };
                directory = open_directory_at(raw(&directory), name)?;
            }
            Ok(Self {
                directory: Arc::new(directory),
            })
        }

        pub(super) fn open_relative(&self, relative: &Path) -> io::Result<File> {
            let mut components = relative.components().peekable();
            if components.peek().is_none() {
                return Err(invalid_path("project-relative file path is empty"));
            }

            let mut directory: Option<OwnedHandle> = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(invalid_path(
                        "project-relative file path contains an escaping component",
                    ));
                };
                validate_component(name)?;
                let parent = directory
                    .as_ref()
                    .map_or_else(|| raw(self.directory.as_ref()), raw);
                if components.peek().is_none() {
                    let handle = open_regular_at(parent, name)?;
                    return Ok(File::from(handle));
                }
                directory = Some(open_directory_at(parent, name)?);
            }
            Err(invalid_path("project-relative file path has no leaf"))
        }
    }

    fn absolute_parts(path: &Path) -> io::Result<(&OsStr, std::path::Components<'_>)> {
        if !path.is_absolute() {
            return Err(invalid_path("project read root must be absolute"));
        }
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(invalid_path("Windows project root has no supported prefix"));
        };
        match prefix.kind() {
            Prefix::Disk(_)
            | Prefix::UNC(_, _)
            | Prefix::VerbatimDisk(_)
            | Prefix::VerbatimUNC(_, _) => {}
            Prefix::DeviceNS(_) | Prefix::Verbatim(_) => {
                return Err(invalid_path(
                    "Windows project root uses an unsupported device namespace",
                ));
            }
        }
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(invalid_path("Windows project root is not rooted"));
        }
        for component in components.clone() {
            let Component::Normal(name) = component else {
                return Err(invalid_path(
                    "Windows project root contains an invalid component",
                ));
            };
            validate_component(name)?;
        }
        Ok((prefix.as_os_str(), components))
    }

    fn open_root(root: &OsStr) -> io::Result<OwnedHandle> {
        let mut encoded = [0_u16; ROOT_BUFFER_UTF16_UNITS];
        let mut length = 0_usize;
        for unit in root.encode_wide() {
            if unit == 0 || length >= MAX_ROOT_UTF16_UNITS {
                return Err(invalid_path("Windows project root is invalid or too long"));
            }
            encoded[length] = unit;
            length += 1;
        }
        if length == 0 {
            return Err(invalid_path("Windows project root is empty"));
        }
        if encoded[length - 1] != u16::from(b'\\') {
            encoded[length] = u16::from(b'\\');
            length += 1;
        }
        encoded[length] = 0;
        let handle = unsafe {
            CreateFileW(
                encoded.as_ptr(),
                DIRECTORY_ACCESS,
                SHARE_ALL,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        validate_directory(raw(&handle))?;
        Ok(handle)
    }

    fn open_directory_at(parent: HANDLE, name: &OsStr) -> io::Result<OwnedHandle> {
        let handle = nt_open_at(
            parent,
            name,
            DIRECTORY_ACCESS,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_ATTRIBUTE_DIRECTORY,
        )?;
        validate_directory(raw(&handle))?;
        Ok(handle)
    }

    fn open_regular_at(parent: HANDLE, name: &OsStr) -> io::Result<OwnedHandle> {
        let handle = nt_open_at(
            parent,
            name,
            FILE_ACCESS,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_ATTRIBUTE_NORMAL,
        )?;
        validate_regular(raw(&handle))?;
        Ok(handle)
    }

    fn nt_open_at(
        parent: HANDLE,
        name: &OsStr,
        access: u32,
        options: u32,
        attributes: u32,
    ) -> io::Result<OwnedHandle> {
        let mut encoded = [0_u16; MAX_COMPONENT_UTF16_UNITS];
        let length = encode_component(name, &mut encoded)?;
        let byte_length = u16::try_from(
            length
                .checked_mul(size_of::<u16>())
                .ok_or_else(|| invalid_path("Windows path component is too long"))?,
        )
        .map_err(|_| invalid_path("Windows path component is too long"))?;
        let unicode = windows_sys::Win32::Foundation::UNICODE_STRING {
            Length: byte_length,
            MaximumLength: byte_length,
            Buffer: encoded.as_mut_ptr(),
        };
        let object_attributes = OBJECT_ATTRIBUTES {
            Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
                .map_err(|_| invalid_path("Windows object attributes are unsupported"))?,
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
                SHARE_ALL,
                FILE_OPEN,
                options,
                std::ptr::null(),
                0,
            )
        };
        if let Err(error) = ntstatus_result(status) {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                drop(unsafe { OwnedHandle::from_raw_handle(handle) });
            }
            return Err(error);
        }
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::other(
                "NtCreateFile succeeded without returning a valid handle",
            ));
        }
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }

    fn validate_directory(handle: HANDLE) -> io::Result<()> {
        validate_non_reparse(handle)?;
        if !standard_information(handle)?.Directory {
            return Err(invalid_path("project path component is not a directory"));
        }
        Ok(())
    }

    fn validate_regular(handle: HANDLE) -> io::Result<()> {
        validate_non_reparse(handle)?;
        if standard_information(handle)?.Directory {
            return Err(invalid_path("project source is not a regular file"));
        }
        Ok(())
    }

    fn validate_non_reparse(handle: HANDLE) -> io::Result<()> {
        let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileAttributeTagInfo,
                (&raw mut attributes).cast(),
                fixed_size::<FILE_ATTRIBUTE_TAG_INFO>()?,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid_path("project source path contains a reparse point"));
        }
        Ok(())
    }

    fn standard_information(handle: HANDLE) -> io::Result<FILE_STANDARD_INFO> {
        let mut information = FILE_STANDARD_INFO::default();
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                (&raw mut information).cast(),
                fixed_size::<FILE_STANDARD_INFO>()?,
            )
        };
        if succeeded == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(information)
        }
    }

    fn encode_component(
        name: &OsStr,
        output: &mut [u16; MAX_COMPONENT_UTF16_UNITS],
    ) -> io::Result<usize> {
        let length = validate_component(name)?;
        for (index, unit) in name.encode_wide().enumerate() {
            output[index] = unit;
        }
        Ok(length)
    }

    fn validate_component(name: &OsStr) -> io::Result<usize> {
        if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
            return Err(invalid_path("Windows path component is invalid"));
        }
        let mut length = 0_usize;
        for unit in name.encode_wide() {
            if unit == 0
                || unit == u16::from(b':')
                || unit == u16::from(b'/')
                || unit == u16::from(b'\\')
            {
                return Err(invalid_path("Windows path component is invalid"));
            }
            length = length
                .checked_add(1)
                .ok_or_else(|| invalid_path("Windows path component is too long"))?;
            if length > MAX_COMPONENT_UTF16_UNITS {
                return Err(invalid_path("Windows path component is too long"));
            }
        }
        Ok(length)
    }

    fn ntstatus_result(status: NTSTATUS) -> io::Result<()> {
        if status >= 0 {
            return Ok(());
        }
        if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
            return Err(invalid_path("project source path contains a reparse point"));
        }
        let win32 = unsafe { RtlNtStatusToDosError(status) };
        let raw = i32::try_from(win32).map_err(|_| {
            io::Error::other(format!(
                "open Windows project source failed with NTSTATUS {status:#010x}"
            ))
        })?;
        Err(io::Error::from_raw_os_error(raw))
    }

    fn fixed_size<T>() -> io::Result<u32> {
        u32::try_from(size_of::<T>())
            .map_err(|_| io::Error::other("fixed Windows structure is too large"))
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        handle.as_raw_handle()
    }

    fn invalid_path(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::path::{Component, Path, PathBuf};

    #[derive(Debug, Clone)]
    pub(super) struct ProjectReadRoot {
        path: PathBuf,
    }

    impl ProjectReadRoot {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            Ok(Self {
                path: path.to_path_buf(),
            })
        }

        pub(super) fn open_relative(&self, relative: &Path) -> io::Result<File> {
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "project-relative file path contains an escaping component",
                ));
            }
            File::open(self.path.join(relative))
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ProjectReadRoot(imp::ProjectReadRoot);

impl ProjectReadRoot {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        imp::ProjectReadRoot::open(path).map(Self)
    }

    pub(super) fn open_relative(&self, relative: &Path) -> io::Result<File> {
        self.0.open_relative(relative)
    }
}
