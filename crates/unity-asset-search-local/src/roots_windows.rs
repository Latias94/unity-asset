use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Component, Components, Path, PathBuf, Prefix};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GENERIC_ALL, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, NTSTATUS, OBJ_CASE_INSENSITIVE, OBJ_DONT_REPARSE, RtlNtStatusToDosError,
    STATUS_REPARSE_POINT_ENCOUNTERED, STATUS_STOPPED_ON_SYMLINK,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACE_INHERITED_OBJECT_TYPE_PRESENT, ACE_OBJECT_TYPE_PRESENT,
    ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetKernelObjectSecurity, GetLengthSid,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    INHERIT_ONLY_ACE, InitializeAcl, InitializeSecurityDescriptor, IsValidAcl,
    IsValidSecurityDescriptor, IsValidSid, IsWellKnownSid, OBJECT_INHERIT_ACE,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_DESCRIPTOR,
    SECURITY_MAX_SID_SIZE, SID, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, WinBuiltinAdministratorsSid, WinCreatorOwnerRightsSid,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DELETE_CHILD, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_TRAVERSE, FILE_WRITE_DATA, FileAttributeTagInfo, FileDispositionInfo, FileIdInfo,
    FileStandardInfo, GetDriveTypeW, GetFileInformationByHandleEx, OPEN_EXISTING, READ_CONTROL,
    SYNCHRONIZE, SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, ACCESS_ALLOWED_CALLBACK_ACE_TYPE,
    ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_ALLOWED_COMPOUND_ACE_TYPE,
    ACCESS_ALLOWED_OBJECT_ACE_TYPE, ACCESS_DENIED_ACE_TYPE, ACCESS_DENIED_CALLBACK_ACE_TYPE,
    ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE, ACCESS_DENIED_OBJECT_ACE_TYPE,
    SECURITY_DESCRIPTOR_REVISION,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_NO_ROOT_DIR, DRIVE_REMOTE, DRIVE_UNKNOWN,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};

use super::{DiscoveredRoots, PRODUCT_DIRECTORY, PrivateRootsError};
use crate::security_context::CurrentSecurityContextSnapshot;

const DIRECTORY_SHARE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const DIRECTORY_TRAVERSE_ACCESS: u32 = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const DIRECTORY_CREATE_ACCESS: u32 =
    DIRECTORY_TRAVERSE_ACCESS | FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY;
const PRIVATE_DIRECTORY_ACCESS: u32 = DIRECTORY_CREATE_ACCESS | READ_CONTROL | WRITE_DAC;
const STABLE_PARENT_DIRECTORY_ACCESS: u32 =
    FILE_ADD_SUBDIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE;
const PRIVATE_OBJECT_ACCESS: u32 = FILE_ALL_ACCESS & !WRITE_OWNER;
pub(crate) const WINDOWS_NAMED_PIPE_CLIENT_ACCESS: u32 =
    FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const MAX_WINDOWS_ROOT_UTF16_UNITS: usize = 1_024;
const MAX_WINDOWS_COMPONENT_UTF16_UNITS: usize = 255;
const WINDOWS_ROOT_BUFFER_UTF16_UNITS: usize = MAX_WINDOWS_ROOT_UTF16_UNITS + 2;
const MAX_SECURITY_DESCRIPTOR_BYTES: usize = 128 * 1024;
const MAX_KNOWN_FOLDER_UTF16_UNITS: usize = 32_767;
const FILE_OPENED_INFORMATION: usize = 1;
const FILE_CREATED_INFORMATION: usize = 2;
const SID_HEADER_BYTES: usize = size_of::<SID>() - size_of::<u32>();
const PARENT_MUTATION_ACCESS: u32 = FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_DELETE_CHILD
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | GENERIC_WRITE
    | GENERIC_ALL;

pub(super) struct PrivateDirectory {
    handle: OwnedHandle,
    identity: DirectoryIdentity,
}

struct StableParentDirectory {
    handle: OwnedHandle,
}

impl StableParentDirectory {
    fn create_stable_child(
        &self,
        name: &OsStr,
        security: &PrivateSecurityDescriptor,
    ) -> io::Result<Self> {
        create_or_open_stable_parent_directory(self.handle.raw(), name, security)
    }

    fn create_private_child(
        &self,
        name: &OsStr,
        security: &PrivateSecurityDescriptor,
    ) -> io::Result<PrivateDirectory> {
        create_or_open_private_directory(self.handle.raw(), name, security)
    }
}

impl PrivateDirectory {
    pub(super) fn revalidate(
        &self,
        path: &Path,
        security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<()> {
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        if validate_secured_directory(self.handle.raw(), &security)? != self.identity {
            return Err(io::Error::other(
                "private directory identity changed during revalidation",
            ));
        }
        let reopened = open_directory_path(path, PRIVATE_DIRECTORY_ACCESS)?;
        if validate_secured_directory(reopened.raw(), &security)? != self.identity {
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
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        create_or_open_private_directory(self.handle.raw(), name, &security)
    }

    pub(super) fn create_private_file(
        &self,
        directory_path: &Path,
        name: &OsStr,
        security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<File> {
        self.revalidate(directory_path, security_context)?;
        validate_leaf_component(name)?;
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        let (handle, information) = nt_create_file_at(
            self.handle.raw(),
            name,
            PRIVATE_OBJECT_ACCESS,
            FILE_CREATE,
            Some(security.as_ptr()),
        )?;
        if information != FILE_CREATED_INFORMATION {
            return Err(io::Error::other(
                "Windows private file returned an unexpected create disposition",
            ));
        }
        let file = handle.into_file();
        validate_private_file(&file, &security)?;
        self.revalidate(directory_path, security_context)?;
        Ok(file)
    }

    pub(super) fn open_private_file(
        &self,
        directory_path: &Path,
        name: &OsStr,
        security_context: &CurrentSecurityContextSnapshot,
        writable: bool,
    ) -> io::Result<File> {
        self.revalidate(directory_path, security_context)?;
        validate_leaf_component(name)?;
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        let access = FILE_READ_ATTRIBUTES
            | windows_sys::Win32::Foundation::GENERIC_READ
            | READ_CONTROL
            | if writable {
                windows_sys::Win32::Foundation::GENERIC_WRITE
            } else {
                0
            };
        let (handle, _) = nt_create_file_at(self.handle.raw(), name, access, FILE_OPEN, None)?;
        let file = handle.into_file();
        validate_private_file(&file, &security)?;
        self.revalidate(directory_path, security_context)?;
        Ok(file)
    }

    pub(super) fn rename_private_file(
        &self,
        directory_path: &Path,
        source: &OsStr,
        destination: &OsStr,
        replace: bool,
        security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<()> {
        self.revalidate(directory_path, security_context)?;
        validate_leaf_component(source)?;
        validate_leaf_component(destination)?;
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        let (source, _) = nt_create_file_at(
            self.handle.raw(),
            source,
            DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL,
            FILE_OPEN,
            None,
        )?;
        let source = source.into_file();
        validate_private_file(&source, &security)?;
        rename_file_at(
            &source,
            self.handle.raw(),
            destination,
            if replace {
                RenameBehavior::Replace
            } else {
                RenameBehavior::NoReplace
            },
        )?;
        self.revalidate(directory_path, security_context)?;
        Ok(())
    }

    pub(super) fn remove_private_file(
        &self,
        directory_path: &Path,
        name: &OsStr,
        security_context: &CurrentSecurityContextSnapshot,
    ) -> io::Result<()> {
        self.revalidate(directory_path, security_context)?;
        validate_leaf_component(name)?;
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )?;
        let (file, _) = nt_create_file_at(
            self.handle.raw(),
            name,
            DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL,
            FILE_OPEN,
            None,
        )?;
        let file = file.into_file();
        validate_private_file(&file, &security)?;
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the live file handle has DELETE access and the input structure is exact.
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const disposition).cast(),
                structure_size::<FILE_DISPOSITION_INFO>()?,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        drop(file);
        self.revalidate(directory_path, security_context)
    }

    pub(super) fn sync(&self) -> io::Result<()> {
        // Windows directory handles do not provide a portable durability primitive. Descriptor
        // publication uses write-through replacement; retaining this method keeps the commit
        // boundary explicit across platforms.
        Ok(())
    }
}

fn validate_leaf_component(name: &OsStr) -> io::Result<()> {
    let mut encoded = [0_u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS];
    let _ = encode_leaf(name, &mut encoded)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum RenameBehavior {
    Replace,
    NoReplace,
}

fn rename_file_at(
    source: &File,
    destination_parent: HANDLE,
    destination: &OsStr,
    behavior: RenameBehavior,
) -> io::Result<()> {
    let mut encoded_name = [0_u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS];
    let name_length = encode_leaf(destination, &mut encoded_name)?;
    let name_bytes = name_length
        .checked_mul(size_of::<u16>())
        .ok_or_else(invalid_component)?;
    let buffer_bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or_else(|| io::Error::other("Windows rename buffer size overflow"))?;
    let mut buffer = AlignedStorage::zeroed(buffer_bytes, "Windows file rename information")?;
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: the aligned allocation is large enough for the fixed structure and complete name.
    unsafe {
        (*information).Anonymous.ReplaceIfExists = matches!(behavior, RenameBehavior::Replace);
        (*information).RootDirectory = destination_parent;
        (*information).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| io::Error::other("Windows rename name is too large"))?;
        std::ptr::copy_nonoverlapping(
            encoded_name.as_ptr(),
            (*information).FileName.as_mut_ptr(),
            name_length,
        );
    }
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: the source handle has DELETE access and the variable-sized input is initialized.
    ntstatus_result(unsafe {
        NtSetInformationFile(
            source.as_raw_handle(),
            &raw mut io_status,
            information.cast(),
            u32::try_from(buffer_bytes)
                .map_err(|_| io::Error::other("Windows rename buffer is too large"))?,
            FileRenameInformation,
        )
    })
}

fn validate_private_file(file: &File, security: &PrivateSecurityDescriptor) -> io::Result<()> {
    let handle = file.as_raw_handle();
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the file handle is live and the output buffer is exact.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut attributes).cast(),
            structure_size::<FILE_ATTRIBUTE_TAG_INFO>()?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private namespace file is a reparse point",
        ));
    }
    let mut standard = FILE_STANDARD_INFO::default();
    // SAFETY: the file handle is live and the output buffer is exact.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&raw mut standard).cast(),
            structure_size::<FILE_STANDARD_INFO>()?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if standard.Directory {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private namespace entry is not a regular file",
        ));
    }
    let identity = file_identity(handle)?;
    if identity.volume_serial_number == 0 || identity.file_id.iter().all(|byte| *byte == 0) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private namespace file has no stable identity",
        ));
    }
    security.verify(handle)
}

fn file_identity(handle: HANDLE) -> io::Result<DirectoryIdentity> {
    directory_identity(handle)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirectoryIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

pub(super) fn discover(
    security_context: &CurrentSecurityContextSnapshot,
) -> Result<DiscoveredRoots, PrivateRootsError> {
    let security_context_id = security_context.id();
    let base_path = local_app_data_path().map_err(|source| PrivateRootsError::Filesystem {
        kind: super::PrivateRootKind::Runtime,
        operation: "resolve LocalAppData",
        path: PathBuf::from("<LocalAppData>"),
        source,
    })?;
    let private_security = PrivateSecurityDescriptor::new(
        security_context.windows_user_sid(),
        security_context.windows_logon_sid(),
    )
    .map_err(|source| PrivateRootsError::Filesystem {
        kind: super::PrivateRootKind::Runtime,
        operation: "construct private Windows security descriptor",
        path: base_path.clone(),
        source,
    })?;
    let stable_parent_security = PrivateSecurityDescriptor::for_shared_parent(
        security_context.windows_user_sid(),
    )
    .map_err(|source| PrivateRootsError::Filesystem {
        kind: super::PrivateRootKind::Runtime,
        operation: "construct stable Windows parent security descriptor",
        path: base_path.clone(),
        source,
    })?;
    let base =
        open_directory_path(&base_path, STABLE_PARENT_DIRECTORY_ACCESS).map_err(|source| {
            PrivateRootsError::Filesystem {
                kind: super::PrivateRootKind::Runtime,
                operation: "open LocalAppData",
                path: base_path.clone(),
                source,
            }
        })?;
    stable_parent_security
        .verify_owner_controlled_parent(base.raw())
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: super::PrivateRootKind::Runtime,
            operation: "validate LocalAppData ownership and DACL",
            path: base_path.clone(),
            source,
        })?;

    let product_path = base_path.join(PRODUCT_DIRECTORY);
    let product = create_or_open_stable_parent_directory(
        base.raw(),
        OsStr::new(PRODUCT_DIRECTORY),
        &stable_parent_security,
    )
    .map_err(|source| PrivateRootsError::Filesystem {
        kind: super::PrivateRootKind::Runtime,
        operation: "create stable product root",
        path: product_path.clone(),
        source,
    })?;

    let runtime_parent_path = product_path.join("runtime");
    let runtime_parent = product
        .create_stable_child(OsStr::new("runtime"), &stable_parent_security)
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: super::PrivateRootKind::Runtime,
            operation: "create stable runtime namespace",
            path: runtime_parent_path.clone(),
            source,
        })?;
    let cache_parent_path = product_path.join("cache");
    let cache_parent = product
        .create_stable_child(OsStr::new("cache"), &stable_parent_security)
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: super::PrivateRootKind::Cache,
            operation: "create stable cache namespace",
            path: cache_parent_path.clone(),
            source,
        })?;

    let context_component = security_context_id.path_component();
    let context_name = OsStr::new(&context_component);
    let runtime_path = runtime_parent_path.join(context_name);
    let runtime = runtime_parent
        .create_private_child(context_name, &private_security)
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: super::PrivateRootKind::Runtime,
            operation: "create security-context runtime root",
            path: runtime_path.clone(),
            source,
        })?;
    let cache_path = cache_parent_path.join(context_name);
    let cache = cache_parent
        .create_private_child(context_name, &private_security)
        .map_err(|source| PrivateRootsError::Filesystem {
            kind: super::PrivateRootKind::Cache,
            operation: "create security-context cache root",
            path: cache_path.clone(),
            source,
        })?;

    Ok(DiscoveredRoots {
        runtime_path,
        runtime,
        cache_path,
        cache,
    })
}

fn local_app_data_path() -> io::Result<PathBuf> {
    let folder_id = FOLDERID_LocalAppData;
    let mut pointer = std::ptr::null_mut();
    // SAFETY: the folder identifier and output pointer satisfy SHGetKnownFolderPath's contract.
    let status = unsafe {
        SHGetKnownFolderPath(
            &raw const folder_id,
            0,
            std::ptr::null_mut(),
            &raw mut pointer,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath failed with HRESULT {status:#010x}"
        )));
    }
    if pointer.is_null() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath succeeded without returning a path",
        ));
    }
    let allocation = CoTaskMemPath(pointer);
    let mut length = 0_usize;
    // SAFETY: SHGetKnownFolderPath returns a NUL-terminated allocated string. The explicit cap
    // bounds traversal if the operating-system contract is violated.
    while length < MAX_KNOWN_FOLDER_UTF16_UNITS && unsafe { *allocation.0.add(length) } != 0 {
        length += 1;
    }
    if length == 0 || length == MAX_KNOWN_FOLDER_UTF16_UNITS {
        return Err(io::Error::other(
            "SHGetKnownFolderPath returned an empty or unterminated path",
        ));
    }
    // SAFETY: `length` initialized UTF-16 units precede the observed terminator.
    let units = unsafe { std::slice::from_raw_parts(allocation.0, length) };
    let path = PathBuf::from(OsString::from_wide(units));
    if !path.is_absolute() {
        return Err(io::Error::other("LocalAppData is not an absolute path"));
    }
    Ok(path)
}

struct CoTaskMemPath(*mut u16);

impl Drop for CoTaskMemPath {
    fn drop(&mut self) {
        // SAFETY: SHGetKnownFolderPath transfers this allocation to the caller.
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

fn open_directory_path(path: &Path, final_access: u32) -> io::Result<OwnedHandle> {
    let mut parts = AbsolutePathParts::new(path)?;
    let mut directory = open_root(parts.root())?;
    while let Some(name) = parts.next_component()? {
        let access = if parts.has_more_components()? {
            DIRECTORY_TRAVERSE_ACCESS
        } else {
            final_access
        };
        directory = open_directory_at(directory.raw(), name, access)?;
    }
    Ok(directory)
}

fn create_or_open_private_directory(
    parent: HANDLE,
    name: &OsStr,
    security: &PrivateSecurityDescriptor,
) -> io::Result<PrivateDirectory> {
    let (handle, identity) =
        create_or_open_secured_directory(parent, name, PRIVATE_DIRECTORY_ACCESS, security)?;
    Ok(PrivateDirectory { handle, identity })
}

fn create_or_open_stable_parent_directory(
    parent: HANDLE,
    name: &OsStr,
    security: &PrivateSecurityDescriptor,
) -> io::Result<StableParentDirectory> {
    let (handle, _) =
        create_or_open_secured_directory(parent, name, STABLE_PARENT_DIRECTORY_ACCESS, security)?;
    Ok(StableParentDirectory { handle })
}

fn create_or_open_secured_directory(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    security: &PrivateSecurityDescriptor,
) -> io::Result<(OwnedHandle, DirectoryIdentity)> {
    let (handle, information) =
        nt_create_directory_at(parent, name, access, FILE_OPEN_IF, Some(security.as_ptr()))?;
    if information != FILE_CREATED_INFORMATION && information != FILE_OPENED_INFORMATION {
        return Err(io::Error::other(
            "Windows secured directory returned an unexpected create disposition",
        ));
    }
    let identity = validate_secured_directory(handle.raw(), security)?;
    Ok((handle, identity))
}

fn open_directory_at(parent: HANDLE, name: &OsStr, access: u32) -> io::Result<OwnedHandle> {
    nt_create_directory_at(parent, name, access, FILE_OPEN, None).map(|(handle, _)| handle)
}

fn nt_create_directory_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    disposition: u32,
    security: Option<*const SECURITY_DESCRIPTOR>,
) -> io::Result<(OwnedHandle, usize)> {
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
        SecurityDescriptor: security.unwrap_or(std::ptr::null()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: all pointers reference initialized storage for the duration of the call.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access | SYNCHRONIZE,
            &raw const object_attributes,
            &raw mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_DIRECTORY,
            DIRECTORY_SHARE,
            disposition,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if let Err(error) = ntstatus_result(status) {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: a failed NtCreateFile may still return a handle requiring closure.
            unsafe { CloseHandle(handle) };
        }
        return Err(error);
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(
            "NtCreateFile succeeded without returning a directory handle",
        ));
    }
    let handle = OwnedHandle(handle);
    validate_directory_handle(handle.raw())?;
    Ok((handle, io_status.Information))
}

fn nt_create_file_at(
    parent: HANDLE,
    name: &OsStr,
    access: u32,
    disposition: u32,
    security: Option<*const SECURITY_DESCRIPTOR>,
) -> io::Result<(OwnedHandle, usize)> {
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
        SecurityDescriptor: security.unwrap_or(std::ptr::null()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: all pointers reference initialized storage for the duration of the call.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            access | SYNCHRONIZE,
            &raw const object_attributes,
            &raw mut io_status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            DIRECTORY_SHARE,
            disposition,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if let Err(error) = ntstatus_result(status) {
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            // SAFETY: a failed NtCreateFile may still return a handle requiring closure.
            unsafe { CloseHandle(handle) };
        }
        return Err(error);
    }
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::other(
            "NtCreateFile succeeded without returning a file handle",
        ));
    }
    Ok((OwnedHandle(handle), io_status.Information))
}

fn open_root(root: &OsStr) -> io::Result<OwnedHandle> {
    let mut path = [0_u16; WINDOWS_ROOT_BUFFER_UTF16_UNITS];
    encode_root(root, &mut path)?;
    // SAFETY: `path` is a NUL-terminated volume root.
    match unsafe { GetDriveTypeW(path.as_ptr()) } {
        DRIVE_REMOTE | DRIVE_UNKNOWN | DRIVE_NO_ROOT_DIR => return Err(invalid_root()),
        _ => {}
    }
    // SAFETY: `path` is NUL terminated and the remaining arguments open a directory root.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            DIRECTORY_TRAVERSE_ACCESS,
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

fn validate_secured_directory(
    handle: HANDLE,
    security: &PrivateSecurityDescriptor,
) -> io::Result<DirectoryIdentity> {
    validate_directory_handle(handle)?;
    let identity = directory_identity(handle)?;
    security.verify(handle)?;
    Ok(identity)
}

fn validate_directory_handle(handle: HANDLE) -> io::Result<()> {
    let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: the output is writable for the exact structure size.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileAttributeTagInfo,
            (&raw mut attributes).cast(),
            structure_size::<FILE_ATTRIBUTE_TAG_INFO>()?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("directory is a reparse point"));
    }
    let mut standard = FILE_STANDARD_INFO::default();
    // SAFETY: the output is writable for the exact structure size.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&raw mut standard).cast(),
            structure_size::<FILE_STANDARD_INFO>()?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if !standard.Directory {
        return Err(io::Error::other("path is not a directory"));
    }
    Ok(())
}

fn directory_identity(handle: HANDLE) -> io::Result<DirectoryIdentity> {
    let mut information = FILE_ID_INFO::default();
    // SAFETY: the output is writable for the exact structure size.
    if unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&raw mut information).cast(),
            structure_size::<FILE_ID_INFO>()?,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
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

pub(crate) struct PrivateSecurityDescriptor {
    owner: AlignedStorage,
    _principal: AlignedStorage,
    _owner_rights: AlignedStorage,
    dacl: AlignedStorage,
    descriptor: SECURITY_DESCRIPTOR,
}

// SAFETY: every pointer embedded in `descriptor` targets an owned heap allocation retained by
// this value. Construction completes before publication, the allocations do not move with the
// struct, and all subsequent access is read-only through Windows security APIs.
unsafe impl Send for PrivateSecurityDescriptor {}
// SAFETY: the same immutable retained allocations may be read concurrently; no API exposes a
// mutable pointer or mutates the descriptor after construction.
unsafe impl Sync for PrivateSecurityDescriptor {}

impl PrivateSecurityDescriptor {
    pub(crate) fn new(owner_bytes: &[u8], principal_bytes: &[u8]) -> io::Result<Self> {
        Self::with_access(
            owner_bytes,
            principal_bytes,
            PRIVATE_OBJECT_ACCESS,
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        )
    }

    pub(crate) fn for_named_pipe(owner_bytes: &[u8], principal_bytes: &[u8]) -> io::Result<Self> {
        Self::with_access(
            owner_bytes,
            principal_bytes,
            WINDOWS_NAMED_PIPE_CLIENT_ACCESS,
            0,
        )
    }

    pub(crate) fn for_shared_parent(user_bytes: &[u8]) -> io::Result<Self> {
        Self::with_access(user_bytes, user_bytes, STABLE_PARENT_DIRECTORY_ACCESS, 0)
    }

    fn with_access(
        owner_bytes: &[u8],
        principal_bytes: &[u8],
        principal_access: u32,
        inheritance_flags: u32,
    ) -> io::Result<Self> {
        let owner = AlignedStorage::from_bytes(owner_bytes, "Windows owner SID")?;
        let owner_sid = owner.as_ptr().cast_mut().cast();
        // SAFETY: the copied bytes came from a validated effective-token SID.
        if unsafe { IsValidSid(owner_sid) } == 0 {
            return Err(io::Error::other("effective Windows user SID is invalid"));
        }
        // SAFETY: IsValidSid succeeded for the retained owner allocation.
        let sid_length = unsafe { GetLengthSid(owner_sid) } as usize;
        if sid_length != owner_bytes.len() {
            return Err(io::Error::other(
                "effective Windows user SID has an inconsistent length",
            ));
        }
        let principal = AlignedStorage::from_bytes(principal_bytes, "Windows principal SID")?;
        let principal_sid = principal.as_ptr().cast_mut().cast();
        // SAFETY: AlignedStorage retains all copied SID bytes.
        if unsafe { IsValidSid(principal_sid) } == 0 {
            return Err(io::Error::other("Windows principal SID is invalid"));
        }
        // SAFETY: IsValidSid succeeded.
        let principal_length = unsafe { GetLengthSid(principal_sid) } as usize;
        if principal_length != principal_bytes.len() {
            return Err(io::Error::other(
                "Windows principal SID has an inconsistent length",
            ));
        }

        let mut owner_rights = AlignedStorage::zeroed(
            usize::try_from(SECURITY_MAX_SID_SIZE)
                .map_err(|_| io::Error::other("maximum Windows SID size does not fit usize"))?,
            "Windows OWNER_RIGHTS SID",
        )?;
        let mut owner_rights_length = owner_rights.capacity_u32()?;
        // SAFETY: the output allocation is writable for `owner_rights_length` bytes.
        if unsafe {
            CreateWellKnownSid(
                WinCreatorOwnerRightsSid,
                std::ptr::null_mut(),
                owner_rights.as_mut_ptr().cast(),
                &raw mut owner_rights_length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let owner_rights_sid = owner_rights.as_ptr().cast_mut().cast();
        // SAFETY: CreateWellKnownSid initialized a valid SID on success.
        if unsafe { IsValidSid(owner_rights_sid) } == 0
            || unsafe { GetLengthSid(owner_rights_sid) } != owner_rights_length
        {
            return Err(io::Error::other(
                "Windows OWNER_RIGHTS SID has an inconsistent length",
            ));
        }

        let owner_rights_ace_bytes = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .and_then(|base| {
                base.checked_add(usize::try_from(owner_rights_length).unwrap_or(usize::MAX))
            })
            .ok_or_else(|| io::Error::other("protected Windows DACL size overflow"))?;
        let principal_ace_bytes = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .and_then(|base| base.checked_add(principal_length))
            .ok_or_else(|| io::Error::other("protected Windows DACL size overflow"))?;
        let dacl_bytes = size_of::<ACL>()
            .checked_add(owner_rights_ace_bytes)
            .and_then(|bytes| bytes.checked_add(principal_ace_bytes))
            .ok_or_else(|| io::Error::other("protected Windows DACL size overflow"))?;
        let mut dacl = AlignedStorage::zeroed(dacl_bytes, "protected Windows DACL")?;
        let dacl_ptr = dacl.as_mut_ptr().cast::<ACL>();
        let dacl_length = u32::try_from(dacl_bytes)
            .map_err(|_| io::Error::other("protected Windows DACL is too large"))?;
        // SAFETY: `dacl_ptr` is aligned and writable for `dacl_length` bytes.
        if unsafe { InitializeAcl(dacl_ptr, dacl_length, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Any OWNER_RIGHTS ACE suppresses the owner's implicit READ_CONTROL and WRITE_DAC. Grant
        // only READ_CONTROL here; all other rights require the explicitly named principal.
        // SAFETY: the DACL allocation includes the complete retained OWNER_RIGHTS SID.
        if unsafe {
            AddAccessAllowedAceEx(
                dacl_ptr,
                ACL_REVISION,
                inheritance_flags,
                READ_CONTROL,
                owner_rights_sid,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // A single explicit principal avoids accidentally OR-combining unrelated token SIDs.
        // SAFETY: the DACL allocation was sized for the retained principal SID.
        if unsafe {
            AddAccessAllowedAceEx(
                dacl_ptr,
                ACL_REVISION,
                inheritance_flags,
                principal_access,
                principal_sid,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        // SAFETY: `descriptor_ptr` is aligned and writable.
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the retained owner and DACL allocations outlive the descriptor.
        if unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner_sid, 0) } == 0
            || unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, dacl_ptr, 0) } == 0
            || unsafe {
                SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: all embedded pointers refer to retained initialized allocations.
        if unsafe { IsValidSecurityDescriptor(descriptor_ptr) } == 0 {
            return Err(io::Error::other(
                "constructed protected Windows descriptor is invalid",
            ));
        }
        Ok(Self {
            owner,
            _principal: principal,
            _owner_rights: owner_rights,
            dacl,
            descriptor,
        })
    }

    pub(crate) fn as_ptr(&self) -> *const SECURITY_DESCRIPTOR {
        &raw const self.descriptor
    }

    pub(crate) fn verify_handle(&self, handle: HANDLE) -> io::Result<()> {
        self.verify(handle)
    }

    fn verify_owner_controlled_parent(&self, handle: HANDLE) -> io::Result<()> {
        let snapshot = SecuritySnapshot::capture(handle)?;
        let view = snapshot.view()?;
        // SAFETY: both SIDs are validated and retained by their respective allocations.
        if unsafe { EqualSid(view.owner, self.owner_sid()) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows directory is not owned by the effective user",
            ));
        }
        verify_parent_dacl(&snapshot, &view, self.owner_sid())
    }

    fn verify(&self, handle: HANDLE) -> io::Result<()> {
        let snapshot = SecuritySnapshot::capture(handle)?;
        let view = snapshot.view()?;
        // SAFETY: both owner SIDs were validated and remain retained.
        if unsafe { EqualSid(view.owner, self.owner_sid()) } == 0
            || !view.dacl_protected
            || !acl_equal(view.dacl, self.dacl.as_ptr().cast::<ACL>())?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows object does not have the expected protected principal DACL",
            ));
        }
        Ok(())
    }

    fn owner_sid(&self) -> PSID {
        self.owner.as_ptr().cast_mut().cast()
    }
}

fn verify_parent_dacl(
    snapshot: &SecuritySnapshot,
    view: &SecurityView,
    owner_sid: PSID,
) -> io::Result<()> {
    // SAFETY: `view.dacl` belongs to the validated retained security descriptor.
    let ace_count = u32::from(unsafe { (*view.dacl).AceCount });
    for index in 0..ace_count {
        let mut ace = std::ptr::null_mut();
        // SAFETY: the ACL is valid and `ace` is a writable output pointer.
        if unsafe { GetAce(view.dacl, index, &raw mut ace) } == 0 || ace.is_null() {
            return Err(io::Error::other(
                "Windows parent DACL contains an unreadable ACE",
            ));
        }
        let ace = ace.cast::<u8>();
        if !snapshot.contains(ace, size_of::<ACE_HEADER>()) {
            return Err(io::Error::other(
                "Windows parent DACL contains an out-of-bounds ACE header",
            ));
        }
        // SAFETY: the complete header was checked to lie in the retained descriptor.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        let ace_len = usize::from(header.AceSize);
        if ace_len < size_of::<ACE_HEADER>() || !snapshot.contains(ace, ace_len) {
            return Err(io::Error::other(
                "Windows parent DACL contains an invalid ACE length",
            ));
        }
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }

        let ace_type = u32::from(header.AceType);
        let access_allowed = matches!(
            ace_type,
            ACCESS_ALLOWED_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                | ACCESS_ALLOWED_OBJECT_ACE_TYPE
                | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
                | ACCESS_ALLOWED_COMPOUND_ACE_TYPE
        );
        if !access_allowed
            && matches!(
                ace_type,
                ACCESS_DENIED_ACE_TYPE
                    | ACCESS_DENIED_CALLBACK_ACE_TYPE
                    | ACCESS_DENIED_OBJECT_ACE_TYPE
                    | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE
            )
        {
            continue;
        }
        if !access_allowed {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows parent DACL contains an unsupported ACE type",
            ));
        }
        let mask = read_ace_u32(ace, ace_len, size_of::<ACE_HEADER>())?;
        if mask & PARENT_MUTATION_ACCESS == 0 {
            continue;
        }
        if ace_type == ACCESS_ALLOWED_COMPOUND_ACE_TYPE {
            return Err(untrusted_parent_ace());
        }

        let sid_offset = if matches!(
            ace_type,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
        ) {
            let flags = read_ace_u32(ace, ace_len, size_of::<ACE_HEADER>() + size_of::<u32>())?;
            let mut offset = size_of::<ACE_HEADER>() + size_of::<u32>() * 2;
            if flags & ACE_OBJECT_TYPE_PRESENT != 0 {
                offset = offset
                    .checked_add(size_of::<windows_sys::core::GUID>())
                    .ok_or_else(untrusted_parent_ace)?;
            }
            if flags & ACE_INHERITED_OBJECT_TYPE_PRESENT != 0 {
                offset = offset
                    .checked_add(size_of::<windows_sys::core::GUID>())
                    .ok_or_else(untrusted_parent_ace)?;
            }
            offset
        } else {
            size_of::<ACE_HEADER>() + size_of::<u32>()
        };
        let sid = validated_ace_sid(ace, ace_len, sid_offset)?;
        // SAFETY: every SID was validated and remains in a retained allocation.
        let trusted = unsafe { EqualSid(sid, owner_sid) } != 0
            || unsafe { IsWellKnownSid(sid, WinLocalSystemSid) } != 0
            || unsafe { IsWellKnownSid(sid, WinBuiltinAdministratorsSid) } != 0;
        if !trusted {
            return Err(untrusted_parent_ace());
        }
    }
    Ok(())
}

fn read_ace_u32(ace: *const u8, ace_len: usize, offset: usize) -> io::Result<u32> {
    let end = offset
        .checked_add(size_of::<u32>())
        .ok_or_else(untrusted_parent_ace)?;
    if end > ace_len {
        return Err(untrusted_parent_ace());
    }
    // SAFETY: the requested four bytes lie inside the validated ACE.
    Ok(unsafe { ace.add(offset).cast::<u32>().read_unaligned() })
}

fn validated_ace_sid(ace: *const u8, ace_len: usize, offset: usize) -> io::Result<PSID> {
    let header_end = offset
        .checked_add(SID_HEADER_BYTES)
        .ok_or_else(untrusted_parent_ace)?;
    if header_end > ace_len {
        return Err(untrusted_parent_ace());
    }
    // SAFETY: the SID revision and sub-authority count are inside the validated ACE.
    let sub_authorities = unsafe { *ace.add(offset + 1) } as usize;
    let sid_len = SID_HEADER_BYTES
        .checked_add(
            sub_authorities
                .checked_mul(size_of::<u32>())
                .ok_or_else(untrusted_parent_ace)?,
        )
        .ok_or_else(untrusted_parent_ace)?;
    if offset.checked_add(sid_len).is_none_or(|end| end > ace_len) {
        return Err(untrusted_parent_ace());
    }
    // SAFETY: the complete candidate SID lies inside the validated ACE.
    let sid = unsafe { ace.add(offset) }.cast_mut().cast();
    // SAFETY: the candidate SID is bounded by the retained ACE.
    if unsafe { IsValidSid(sid) } == 0 || unsafe { GetLengthSid(sid) } as usize != sid_len {
        return Err(untrusted_parent_ace());
    }
    Ok(sid)
}

fn untrusted_parent_ace() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows parent DACL grants mutation rights to an untrusted principal",
    )
}

struct SecuritySnapshot {
    storage: AlignedStorage,
    returned_len: usize,
}

impl SecuritySnapshot {
    fn capture(handle: HANDLE) -> io::Result<Self> {
        let requested = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut required = 0_u32;
        // SAFETY: this is the documented size query and `required` is writable.
        let first = unsafe {
            GetKernelObjectSecurity(
                handle,
                requested,
                std::ptr::null_mut(),
                0,
                &raw mut required,
            )
        };
        let first_error = io::Error::last_os_error();
        if first != 0
            || first_error.raw_os_error()
                != Some(i32::try_from(ERROR_INSUFFICIENT_BUFFER).expect("Win32 code fits i32"))
        {
            return Err(first_error);
        }
        let required = usize::try_from(required)
            .map_err(|_| io::Error::other("Windows descriptor size does not fit usize"))?;
        if required == 0 || required > MAX_SECURITY_DESCRIPTOR_BYTES {
            return Err(io::Error::other(
                "Windows security descriptor has an unsupported size",
            ));
        }
        let mut storage = AlignedStorage::zeroed(required, "Windows security descriptor")?;
        let capacity = storage.capacity_u32()?;
        let mut returned = capacity;
        // SAFETY: the aligned output allocation is writable for `capacity` bytes.
        if unsafe {
            GetKernelObjectSecurity(
                handle,
                requested,
                storage.as_mut_ptr().cast(),
                capacity,
                &raw mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let returned_len = if returned == 0 {
            required
        } else {
            usize::try_from(returned)
                .map_err(|_| io::Error::other("Windows descriptor length does not fit usize"))?
        };
        if returned_len > capacity as usize {
            return Err(io::Error::other(
                "Windows returned an invalid security descriptor length",
            ));
        }
        let snapshot = Self {
            storage,
            returned_len,
        };
        // SAFETY: the API populated the retained storage as a security descriptor.
        if unsafe { IsValidSecurityDescriptor(snapshot.as_descriptor()) } == 0 {
            return Err(io::Error::other(
                "Windows returned an invalid security descriptor",
            ));
        }
        Ok(snapshot)
    }

    fn as_descriptor(&self) -> PSECURITY_DESCRIPTOR {
        self.storage.as_ptr().cast_mut().cast()
    }

    fn embedded_bytes(&self, pointer: *const u8, length: usize) -> Option<&[u8]> {
        let start = self.storage.as_ptr() as usize;
        let offset = (pointer as usize).checked_sub(start)?;
        let end = offset.checked_add(length)?;
        if end > self.returned_len {
            return None;
        }
        // SAFETY: the checked offset and length lie inside the retained allocation. Deriving the
        // slice from the allocation pointer preserves its provenance even for an untrusted input
        // pointer value.
        Some(unsafe {
            std::slice::from_raw_parts(self.storage.as_ptr().cast::<u8>().add(offset), length)
        })
    }

    fn contains(&self, pointer: *const u8, length: usize) -> bool {
        self.embedded_bytes(pointer, length).is_some()
    }

    fn validate_embedded_sid(&self, sid: PSID) -> io::Result<usize> {
        if sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows directory has no owner SID",
            ));
        }
        let header = self
            .embedded_bytes(sid.cast(), SID_HEADER_BYTES)
            .ok_or_else(|| {
                io::Error::other("Windows directory owner SID header is outside its descriptor")
            })?;
        let sid_length = usize::from(header[1])
            .checked_mul(size_of::<u32>())
            .and_then(|sub_authorities| SID_HEADER_BYTES.checked_add(sub_authorities))
            .ok_or_else(|| io::Error::other("Windows directory owner SID length overflow"))?;
        self.embedded_bytes(sid.cast(), sid_length).ok_or_else(|| {
            io::Error::other("Windows directory owner SID is outside its descriptor")
        })?;
        // SAFETY: the candidate SID's count-derived complete length is inside retained storage.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows directory has no valid owner SID",
            ));
        }
        // SAFETY: IsValidSid succeeded for the fully bounded SID.
        let validated_length = unsafe { GetLengthSid(sid) } as usize;
        if validated_length != sid_length {
            return Err(io::Error::other(
                "Windows directory owner SID has an inconsistent length",
            ));
        }
        Ok(sid_length)
    }

    fn validate_embedded_acl(&self, acl: *mut ACL) -> io::Result<usize> {
        if acl.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows object has no ACL",
            ));
        }
        let header = self
            .embedded_bytes(acl.cast(), size_of::<ACL>())
            .ok_or_else(|| {
                io::Error::other("Windows object ACL header is outside its descriptor")
            })?;
        // SAFETY: the byte slice contains a complete ACL header and unaligned reads are allowed.
        let header = unsafe { header.as_ptr().cast::<ACL>().read_unaligned() };
        let acl_length = usize::from(header.AclSize);
        if acl_length < size_of::<ACL>() {
            return Err(io::Error::other("Windows object ACL is truncated"));
        }
        self.embedded_bytes(acl.cast(), acl_length)
            .ok_or_else(|| io::Error::other("Windows object ACL is outside its descriptor"))?;
        // SAFETY: the complete ACL length declared by its bounded header lies in retained storage.
        if unsafe { IsValidAcl(acl) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows object has no valid ACL",
            ));
        }
        Ok(acl_length)
    }

    fn view(&self) -> io::Result<SecurityView> {
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0;
        // SAFETY: the snapshot is a validated retained descriptor.
        if unsafe {
            GetSecurityDescriptorOwner(
                self.as_descriptor(),
                &raw mut owner,
                &raw mut owner_defaulted,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        self.validate_embedded_sid(owner)?;

        let mut dacl_present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut dacl_defaulted = 0;
        // SAFETY: the snapshot is a validated retained descriptor.
        if unsafe {
            GetSecurityDescriptorDacl(
                self.as_descriptor(),
                &raw mut dacl_present,
                &raw mut dacl,
                &raw mut dacl_defaulted,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if dacl_present == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Windows directory has no DACL",
            ));
        }
        self.validate_embedded_acl(dacl)?;

        let mut control = 0;
        let mut revision = 0;
        // SAFETY: the snapshot is a validated descriptor and both outputs are writable.
        if unsafe {
            GetSecurityDescriptorControl(self.as_descriptor(), &raw mut control, &raw mut revision)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(SecurityView {
            owner,
            dacl,
            dacl_protected: control & SE_DACL_PROTECTED != 0,
        })
    }
}

struct SecurityView {
    owner: PSID,
    dacl: *mut ACL,
    dacl_protected: bool,
}

struct AlignedStorage {
    units: Vec<MaybeUninit<usize>>,
}

impl AlignedStorage {
    fn zeroed(byte_len: usize, description: &'static str) -> io::Result<Self> {
        let units = byte_len.div_ceil(size_of::<usize>());
        let mut storage = Vec::new();
        storage.try_reserve_exact(units).map_err(|source| {
            io::Error::other(format!("could not allocate {description}: {source}"))
        })?;
        storage.resize_with(units, MaybeUninit::zeroed);
        Ok(Self { units: storage })
    }

    fn from_bytes(bytes: &[u8], description: &'static str) -> io::Result<Self> {
        if bytes.is_empty() {
            return Err(io::Error::other(format!("{description} is empty")));
        }
        let mut storage = Self::zeroed(bytes.len(), description)?;
        // SAFETY: the allocation has at least `bytes.len()` writable bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), storage.as_mut_ptr().cast(), bytes.len())
        };
        Ok(storage)
    }

    fn as_ptr(&self) -> *const MaybeUninit<usize> {
        self.units.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut MaybeUninit<usize> {
        self.units.as_mut_ptr()
    }

    fn capacity_u32(&self) -> io::Result<u32> {
        self.units
            .len()
            .checked_mul(size_of::<usize>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("Windows aligned buffer size overflow"))
    }
}

fn acl_equal(left: *const ACL, right: *const ACL) -> io::Result<bool> {
    let left_len = acl_length(left)?;
    let right_len = acl_length(right)?;
    if left_len != right_len {
        return Ok(false);
    }
    // SAFETY: acl_length validated both ACL headers and lengths.
    let left = unsafe { std::slice::from_raw_parts(left.cast::<u8>(), left_len) };
    // SAFETY: same invariant as `left`.
    let right = unsafe { std::slice::from_raw_parts(right.cast::<u8>(), right_len) };
    Ok(left == right)
}

fn acl_length(acl: *const ACL) -> io::Result<usize> {
    if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
        return Err(io::Error::other("Windows DACL is invalid"));
    }
    // SAFETY: IsValidAcl succeeded, so the ACL header is readable.
    let length = usize::from(unsafe { (*acl).AclSize });
    if length < size_of::<ACL>() {
        Err(io::Error::other("Windows DACL is truncated"))
    } else {
        Ok(length)
    }
}

fn structure_size<T>() -> io::Result<u32> {
    u32::try_from(size_of::<T>())
        .map_err(|_| io::Error::other("Windows structure size exceeds u32"))
}

fn ntstatus_result(status: NTSTATUS) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    if status == STATUS_REPARSE_POINT_ENCOUNTERED || status == STATUS_STOPPED_ON_SYMLINK {
        return Err(io::Error::other("path contains a reparse point"));
    }
    // SAFETY: translating an NTSTATUS has no pointer preconditions.
    let code = unsafe { RtlNtStatusToDosError(status) };
    let code = i32::try_from(code).map_err(|_| {
        io::Error::other(format!(
            "NtCreateFile failed with NTSTATUS {status:#010x} and unmapped Win32 code {code}"
        ))
    })?;
    Err(io::Error::from_raw_os_error(code))
}

struct OwnedHandle(HANDLE);

unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    const fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_file(mut self) -> File {
        let handle = std::mem::replace(&mut self.0, std::ptr::null_mut());
        // SAFETY: ownership of the live handle moves from this wrapper into `File` exactly once.
        unsafe { File::from_raw_handle(handle) }
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

    fn has_more_components(&self) -> io::Result<bool> {
        match self.components.clone().next() {
            Some(Component::Normal(_)) => Ok(true),
            Some(_) => Err(invalid_component()),
            None => Ok(false),
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

fn encode_leaf(
    name: &OsStr,
    buffer: &mut [u16; MAX_WINDOWS_COMPONENT_UTF16_UNITS],
) -> io::Result<usize> {
    let mut length = 0_usize;
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(invalid_component());
    }
    for (index, unit) in name.encode_wide().enumerate() {
        if unit == 0
            || unit == u16::from(b':')
            || unit == u16::from(b'/')
            || unit == u16::from(b'\\')
            || index >= buffer.len()
        {
            return Err(invalid_component());
        }
        buffer[index] = unit;
        length = index + 1;
    }
    if length == 0 {
        Err(invalid_component())
    } else {
        Ok(length)
    }
}

fn invalid_root() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Windows private root has an invalid, remote, or unsupported volume root",
    )
}

fn invalid_component() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Windows private root contains an invalid or escaping path component",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_snapshot_rejects_pointers_outside_returned_data() {
        let snapshot = SecuritySnapshot {
            storage: AlignedStorage::zeroed(size_of::<ACL>(), "test descriptor").unwrap(),
            returned_len: size_of::<ACL>(),
        };
        // SAFETY: creating a one-past-the-end pointer is valid; the production helpers must reject
        // it before any dereference or Windows validation call.
        let outside = unsafe {
            snapshot
                .storage
                .as_ptr()
                .cast::<u8>()
                .add(snapshot.returned_len)
                .cast_mut()
        };

        assert!(snapshot.validate_embedded_sid(outside.cast()).is_err());
        assert!(snapshot.validate_embedded_acl(outside.cast()).is_err());
    }

    #[test]
    fn security_snapshot_rejects_truncated_sid_header() {
        let snapshot = SecuritySnapshot {
            storage: AlignedStorage::zeroed(SID_HEADER_BYTES, "test SID").unwrap(),
            returned_len: SID_HEADER_BYTES - 1,
        };
        let sid = snapshot.storage.as_ptr().cast_mut().cast();

        assert!(snapshot.validate_embedded_sid(sid).is_err());
    }

    #[test]
    fn security_snapshot_rejects_sid_length_past_returned_data() {
        let mut storage = AlignedStorage::zeroed(SID_HEADER_BYTES, "test SID").unwrap();
        let sid_bytes = storage.as_mut_ptr().cast::<u8>();
        // SAFETY: the test allocation is writable for the complete SID header.
        unsafe {
            sid_bytes.write(1);
            sid_bytes.add(1).write(1);
        }
        let snapshot = SecuritySnapshot {
            storage,
            returned_len: SID_HEADER_BYTES,
        };
        let sid = snapshot.storage.as_ptr().cast_mut().cast();

        assert!(snapshot.validate_embedded_sid(sid).is_err());
    }

    #[test]
    fn security_snapshot_rejects_acl_length_past_returned_data() {
        let mut storage = AlignedStorage::zeroed(size_of::<ACL>(), "test ACL").unwrap();
        let header = ACL {
            AclRevision: u8::try_from(ACL_REVISION).unwrap(),
            AclSize: u16::try_from(size_of::<ACL>() + 1).unwrap(),
            ..ACL::default()
        };
        // SAFETY: the aligned test allocation is writable for one ACL header.
        unsafe { storage.as_mut_ptr().cast::<ACL>().write(header) };
        let snapshot = SecuritySnapshot {
            storage,
            returned_len: size_of::<ACL>(),
        };
        let dacl = snapshot.storage.as_ptr().cast_mut().cast();

        assert!(snapshot.validate_embedded_acl(dacl).is_err());
    }

    #[test]
    fn stable_parent_grants_only_cross_session_directory_creation_rights() {
        let temporary = tempfile::tempdir().unwrap();
        let base = open_directory_path(temporary.path(), DIRECTORY_CREATE_ACCESS).unwrap();
        let security_context = CurrentSecurityContextSnapshot::current().unwrap();
        let security =
            PrivateSecurityDescriptor::for_shared_parent(security_context.windows_user_sid())
                .unwrap();
        let stable =
            create_or_open_stable_parent_directory(base.raw(), OsStr::new("stable"), &security)
                .unwrap();
        let stable_path = temporary.path().join("stable");

        security.verify_handle(stable.handle.raw()).unwrap();
        open_directory_path(&stable_path, STABLE_PARENT_DIRECTORY_ACCESS).unwrap();
        for forbidden in [
            FILE_LIST_DIRECTORY,
            FILE_ADD_FILE,
            FILE_DELETE_CHILD,
            DELETE,
            WRITE_DAC,
            WRITE_OWNER,
        ] {
            assert!(
                open_directory_path(&stable_path, forbidden).is_err(),
                "stable parent unexpectedly granted access mask {forbidden:#010x}"
            );
        }
    }

    #[test]
    fn stable_parent_creates_an_exact_logon_private_context_child() {
        let temporary = tempfile::tempdir().unwrap();
        let base = open_directory_path(temporary.path(), DIRECTORY_CREATE_ACCESS).unwrap();
        let security_context = CurrentSecurityContextSnapshot::current().unwrap();
        let stable_security =
            PrivateSecurityDescriptor::for_shared_parent(security_context.windows_user_sid())
                .unwrap();
        let private_security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )
        .unwrap();
        let stable = create_or_open_stable_parent_directory(
            base.raw(),
            OsStr::new("stable"),
            &stable_security,
        )
        .unwrap();
        let private = stable
            .create_private_child(OsStr::new("context"), &private_security)
            .unwrap();
        let private_path = temporary.path().join("stable").join("context");

        private_security
            .verify_handle(private.handle.raw())
            .unwrap();
        assert!(stable_security.verify_handle(private.handle.raw()).is_err());
        let reopened = open_directory_path(&private_path, PRIVATE_DIRECTORY_ACCESS).unwrap();
        assert_eq!(
            validate_secured_directory(reopened.raw(), &private_security).unwrap(),
            private.identity
        );
    }

    #[test]
    fn private_windows_child_revalidates_without_exposing_its_handle() {
        let temporary = tempfile::tempdir().unwrap();
        let base = open_directory_path(temporary.path(), DIRECTORY_CREATE_ACCESS).unwrap();
        let security_context = CurrentSecurityContextSnapshot::current().unwrap();
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )
        .unwrap();
        let private =
            create_or_open_private_directory(base.raw(), OsStr::new("private"), &security).unwrap();
        private
            .revalidate(&temporary.path().join("private"), &security_context)
            .unwrap();
    }

    #[test]
    fn existing_insecure_windows_child_is_rejected_without_acl_repair() {
        let temporary = tempfile::tempdir().unwrap();
        let base = open_directory_path(temporary.path(), DIRECTORY_CREATE_ACCESS).unwrap();
        let security_context = CurrentSecurityContextSnapshot::current().unwrap();
        let security = PrivateSecurityDescriptor::new(
            security_context.windows_user_sid(),
            security_context.windows_logon_sid(),
        )
        .unwrap();
        std::fs::create_dir(temporary.path().join("private")).unwrap();
        assert!(
            create_or_open_private_directory(base.raw(), OsStr::new("private"), &security).is_err()
        );
    }
}
