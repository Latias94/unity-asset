use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::security_context::CurrentSecurityContextSnapshot;
use crate::{SecurityContextError, SecurityContextIdV1};

const PRODUCT_DIRECTORY: &str = "unity-asset-search";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateRootKind {
    Runtime,
    Cache,
}

impl fmt::Display for PrivateRootKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Runtime => "runtime",
            Self::Cache => "cache",
        })
    }
}

pub struct PrivateRootV1 {
    kind: PrivateRootKind,
    path: PathBuf,
    security_context_id: SecurityContextIdV1,
    authority: platform::PrivateDirectory,
}

impl PrivateRootV1 {
    #[must_use]
    pub const fn kind(&self) -> PrivateRootKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        self.revalidate_for_context(&current)
    }

    fn revalidate_for_context(
        &self,
        current: &CurrentSecurityContextSnapshot,
    ) -> Result<(), PrivateRootsError> {
        if current.id() != self.security_context_id {
            return Err(PrivateRootsError::SecurityContextChanged);
        }
        self.authority
            .revalidate(&self.path, current)
            .map_err(|source| PrivateRootsError::Filesystem {
                kind: self.kind,
                operation: "revalidate",
                path: self.path.clone(),
                source,
            })
    }
}

impl fmt::Debug for PrivateRootV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRootV1")
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("security_context_id", &self.security_context_id)
            .finish_non_exhaustive()
    }
}

pub struct PrivateRootsV1 {
    security_context_id: SecurityContextIdV1,
    runtime: PrivateRootV1,
    cache: PrivateRootV1,
}

impl PrivateRootsV1 {
    pub fn discover_for_current_context() -> Result<Self, PrivateRootsError> {
        let security_context =
            CurrentSecurityContextSnapshot::current().map_err(|error| match error {
                SecurityContextError::UnsupportedPlatform => PrivateRootsError::UnsupportedPlatform,
                error => PrivateRootsError::SecurityContext(error),
            })?;
        let security_context_id = security_context.id();
        let discovered = platform::discover(&security_context)?;
        Ok(Self {
            security_context_id,
            runtime: PrivateRootV1 {
                kind: PrivateRootKind::Runtime,
                path: discovered.runtime_path,
                security_context_id,
                authority: discovered.runtime,
            },
            cache: PrivateRootV1 {
                kind: PrivateRootKind::Cache,
                path: discovered.cache_path,
                security_context_id,
                authority: discovered.cache,
            },
        })
    }

    #[must_use]
    pub const fn security_context_id(&self) -> SecurityContextIdV1 {
        self.security_context_id
    }

    #[must_use]
    pub const fn runtime(&self) -> &PrivateRootV1 {
        &self.runtime
    }

    #[must_use]
    pub const fn cache(&self) -> &PrivateRootV1 {
        &self.cache
    }

    pub fn revalidate(&self) -> Result<(), PrivateRootsError> {
        let current = CurrentSecurityContextSnapshot::current()?;
        self.runtime.revalidate_for_context(&current)?;
        self.cache.revalidate_for_context(&current)
    }
}

impl fmt::Debug for PrivateRootsV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateRootsV1")
            .field("security_context_id", &self.security_context_id)
            .field("runtime", &self.runtime)
            .field("cache", &self.cache)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum PrivateRootsError {
    #[error(transparent)]
    SecurityContext(#[from] SecurityContextError),
    #[error("private local roots are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("environment variable {variable} {reason}")]
    InvalidEnvironment {
        variable: &'static str,
        reason: &'static str,
    },
    #[error("the effective user has no usable home directory")]
    MissingHomeDirectory,
    #[error("the effective security context changed while private roots were in use")]
    SecurityContextChanged,
    #[error("could not {operation} the private {kind} root at {path}: {source}")]
    Filesystem {
        kind: PrivateRootKind,
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

struct DiscoveredRoots {
    runtime_path: PathBuf,
    runtime: platform::PrivateDirectory,
    cache_path: PathBuf,
    cache: platform::PrivateDirectory,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn filesystem_error(
    kind: PrivateRootKind,
    operation: &'static str,
    path: &Path,
    source: io::Error,
) -> PrivateRootsError {
    PrivateRootsError::Filesystem {
        kind,
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    #[cfg(target_os = "linux")]
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::fd::{AsFd as _, OwnedFd};
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
    use std::path::{Component, Path, PathBuf};

    use rustix::fs::{
        AtFlags, CWD, FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat, statat,
    };
    use rustix::io::Errno;

    use super::{
        DiscoveredRoots, PRODUCT_DIRECTORY, PrivateRootKind, PrivateRootsError, filesystem_error,
    };
    use crate::security_context::CurrentSecurityContextSnapshot;

    const DIRECTORY_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    const PRIVATE_MODE: u32 = 0o700;

    pub(super) struct PrivateDirectory {
        descriptor: OwnedFd,
        identity: DirectoryIdentity,
    }

    impl PrivateDirectory {
        pub(super) fn revalidate(
            &self,
            path: &Path,
            security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            let expected_uid = security_context.effective_uid();
            if validate_private_directory(&self.descriptor, expected_uid)? != self.identity {
                return Err(io::Error::other(
                    "private directory identity changed during revalidation",
                ));
            }
            let reopened = open_path(path)?;
            if validate_private_directory(&reopened, expected_uid)? != self.identity {
                return Err(io::Error::other(
                    "private directory identity changed during revalidation",
                ));
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct DirectoryIdentity {
        device: u64,
        inode: u64,
    }

    pub(super) fn discover(
        security_context: &CurrentSecurityContextSnapshot,
    ) -> Result<DiscoveredRoots, PrivateRootsError> {
        let expected_uid = security_context.effective_uid();
        let (runtime_path, runtime) = discover_runtime(expected_uid)?;
        let (cache_path, cache) = discover_cache(expected_uid)?;
        Ok(DiscoveredRoots {
            runtime_path,
            runtime,
            cache_path,
            cache,
        })
    }

    #[cfg(target_os = "linux")]
    fn discover_runtime(
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        discover_linux_runtime(env::var_os("XDG_RUNTIME_DIR"), expected_uid)
    }

    #[cfg(target_os = "linux")]
    fn discover_linux_runtime(
        xdg_runtime_dir: Option<OsString>,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Runtime;
        if let Some(value) = xdg_runtime_dir {
            let base_path = environment_path("XDG_RUNTIME_DIR", value)?;
            let base = open_path(&base_path).map_err(|source| {
                filesystem_error(kind, "open XDG runtime base", &base_path, source)
            })?;
            validate_private_directory(&base, expected_uid).map_err(|source| {
                filesystem_error(kind, "validate XDG runtime base", &base_path, source)
            })?;
            return create_product_root(kind, &base_path, &base, expected_uid);
        }

        let temporary_path = PathBuf::from("/tmp");
        let temporary = open_path(&temporary_path).map_err(|source| {
            filesystem_error(kind, "open /tmp fallback", &temporary_path, source)
        })?;
        validate_sticky_temporary_base(&temporary).map_err(|source| {
            filesystem_error(kind, "validate /tmp fallback", &temporary_path, source)
        })?;
        let name = OsString::from(format!("{PRODUCT_DIRECTORY}-{expected_uid}"));
        let path = temporary_path.join(&name);
        let root = create_or_open_private_child(&temporary, &name, expected_uid)
            .map_err(|source| filesystem_error(kind, "create /tmp runtime root", &path, source))?;
        Ok((path, root))
    }

    #[cfg(target_os = "macos")]
    fn discover_runtime(
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Runtime;
        let base_path = darwin_user_temporary_directory().map_err(|source| {
            filesystem_error(
                kind,
                "resolve user temporary base",
                Path::new("<darwin-user-temp>"),
                source,
            )
        })?;
        let base = open_path(&base_path).map_err(|source| {
            filesystem_error(kind, "open user temporary base", &base_path, source)
        })?;
        validate_private_directory(&base, expected_uid).map_err(|source| {
            filesystem_error(kind, "validate user temporary base", &base_path, source)
        })?;
        create_product_root(kind, &base_path, &base, expected_uid)
    }

    fn discover_cache(expected_uid: u32) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        #[cfg(target_os = "linux")]
        return discover_linux_cache(
            env::var_os("XDG_CACHE_HOME"),
            env::var_os("HOME"),
            expected_uid,
        );

        #[cfg(target_os = "macos")]
        {
            let kind = PrivateRootKind::Cache;
            let base_path = effective_home_directory(expected_uid)?
                .join("Library")
                .join("Caches");
            let base = open_or_create_owner_controlled_base(&base_path, expected_uid)
                .map_err(|source| filesystem_error(kind, "open cache base", &base_path, source))?;
            create_product_root(kind, &base_path, &base, expected_uid)
        }
    }

    #[cfg(target_os = "linux")]
    fn discover_linux_cache(
        xdg_cache_home: Option<OsString>,
        home: Option<OsString>,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let kind = PrivateRootKind::Cache;
        let base_path = match xdg_cache_home {
            Some(value) => environment_path("XDG_CACHE_HOME", value)?,
            None => {
                let home = home.ok_or(PrivateRootsError::MissingHomeDirectory)?;
                environment_path("HOME", home)?.join(".cache")
            }
        };

        let base = open_or_create_owner_controlled_base(&base_path, expected_uid)
            .map_err(|source| filesystem_error(kind, "open cache base", &base_path, source))?;
        create_product_root(kind, &base_path, &base, expected_uid)
    }

    fn create_product_root(
        kind: PrivateRootKind,
        base_path: &Path,
        base: &OwnedFd,
        expected_uid: u32,
    ) -> Result<(PathBuf, PrivateDirectory), PrivateRootsError> {
        let path = base_path.join(PRODUCT_DIRECTORY);
        let root = create_or_open_private_child(base, OsStr::new(PRODUCT_DIRECTORY), expected_uid)
            .map_err(|source| filesystem_error(kind, "create product root", &path, source))?;
        Ok((path, root))
    }

    fn environment_path(
        variable: &'static str,
        value: OsString,
    ) -> Result<PathBuf, PrivateRootsError> {
        if value.is_empty() {
            return Err(PrivateRootsError::InvalidEnvironment {
                variable,
                reason: "must not be empty",
            });
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(PrivateRootsError::InvalidEnvironment {
                variable,
                reason: "must be an absolute path",
            });
        }
        Ok(path)
    }

    fn open_or_create_owner_controlled_base(path: &Path, expected_uid: u32) -> io::Result<OwnedFd> {
        match open_path(path) {
            Ok(directory) => {
                validate_owner_controlled_directory(&directory, expected_uid)?;
                Ok(directory)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent_path = path.parent().ok_or_else(invalid_path)?;
                let name = path.file_name().ok_or_else(invalid_path)?;
                let parent = open_path(parent_path)?;
                validate_owner_controlled_directory(&parent, expected_uid)?;
                let created = create_or_open_private_child(&parent, name, expected_uid)?;
                Ok(created.descriptor)
            }
            Err(error) => Err(error),
        }
    }

    fn create_or_open_private_child(
        parent: &OwnedFd,
        name: &OsStr,
        expected_uid: u32,
    ) -> io::Result<PrivateDirectory> {
        validate_leaf(name)?;
        let created = match mkdirat(parent, name, Mode::RWXU) {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(error) => return Err(error.into()),
        };
        let descriptor = open_named_directory(parent, name)?;
        if created {
            fchmod(&descriptor, Mode::RWXU).map_err(io::Error::from)?;
        }
        let identity = validate_private_directory(&descriptor, expected_uid)?;
        Ok(PrivateDirectory {
            descriptor,
            identity,
        })
    }

    fn open_path(path: &Path) -> io::Result<OwnedFd> {
        if !path.is_absolute() {
            return Err(invalid_path());
        }
        let mut descriptor =
            openat(CWD, Path::new("/"), DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => descriptor = open_named_directory(&descriptor, name)?,
                Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(invalid_path());
                }
            }
        }
        Ok(descriptor)
    }

    fn open_named_directory(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        validate_leaf(name)?;
        let descriptor =
            openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
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

    fn validate_private_directory(
        directory: &OwnedFd,
        expected_uid: u32,
    ) -> io::Result<DirectoryIdentity> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        let identity = directory_identity(&metadata)?;
        if metadata.st_uid != expected_uid || metadata.st_mode as u32 & 0o777 != PRIVATE_MODE {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private directory must be owned by the effective user with mode 0700",
            ));
        }
        Ok(identity)
    }

    fn validate_owner_controlled_directory(
        directory: &OwnedFd,
        expected_uid: u32,
    ) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        directory_identity(&metadata)?;
        if metadata.st_uid != expected_uid || metadata.st_mode as u32 & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cache base must be owned by the effective user and not group/other writable",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn validate_sticky_temporary_base(directory: &OwnedFd) -> io::Result<()> {
        crate::local_filesystem::validate_local_directory(directory)?;
        let metadata = fstat(directory.as_fd()).map_err(io::Error::from)?;
        directory_identity(&metadata)?;
        let mode = metadata.st_mode as u32;
        if metadata.st_uid != 0 || mode & 0o1000 == 0 || mode & 0o002 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary fallback must be root-owned, sticky, and world-writable",
            ));
        }
        Ok(())
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

    fn validate_leaf(name: &OsStr) -> io::Result<()> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.contains(&b'/')
            || bytes.contains(&0)
        {
            Err(invalid_path())
        } else {
            Ok(())
        }
    }

    fn invalid_path() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private root contains an invalid or escaping path component",
        )
    }

    #[cfg(target_os = "macos")]
    fn darwin_user_temporary_directory() -> io::Result<PathBuf> {
        // SAFETY: the null-buffer call is the documented size query for confstr.
        let required =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
        if required == 0 || required > 64 * 1024 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0_u8; required];
        // SAFETY: `buffer` is writable for its full declared length.
        let returned = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_TEMP_DIR,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if returned == 0 || returned > buffer.len() || buffer[returned - 1] != 0 {
            return Err(io::Error::other(
                "confstr returned an invalid Darwin user temporary directory",
            ));
        }
        buffer.truncate(returned - 1);
        if buffer.is_empty() {
            return Err(io::Error::other(
                "confstr returned an empty Darwin user temporary directory",
            ));
        }
        Ok(PathBuf::from(OsString::from_vec(buffer)))
    }

    #[cfg(target_os = "macos")]
    fn effective_home_directory(expected_uid: u32) -> Result<PathBuf, PrivateRootsError> {
        use std::ffi::CStr;
        use std::mem::MaybeUninit;

        const MAX_PASSWD_BUFFER: usize = 1024 * 1024;
        // SAFETY: sysconf has no pointer preconditions.
        let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
        let initial = usize::try_from(suggested).unwrap_or(16 * 1024).max(1024);
        let mut size = initial.min(MAX_PASSWD_BUFFER);
        loop {
            let mut passwd = MaybeUninit::<libc::passwd>::zeroed();
            let mut result = std::ptr::null_mut();
            let mut buffer = vec![0_u8; size];
            // SAFETY: every output points to writable storage retained through result handling.
            let status = unsafe {
                libc::getpwuid_r(
                    expected_uid,
                    passwd.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &raw mut result,
                )
            };
            if status == libc::ERANGE && size < MAX_PASSWD_BUFFER {
                size = size.saturating_mul(2).min(MAX_PASSWD_BUFFER);
                continue;
            }
            if status != 0 {
                return Err(filesystem_error(
                    PrivateRootKind::Cache,
                    "resolve effective-user home",
                    Path::new("<effective-user-home>"),
                    io::Error::from_raw_os_error(status),
                ));
            }
            if result.is_null() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            // SAFETY: getpwuid_r succeeded and the passwd record points into retained `buffer`.
            let passwd = unsafe { passwd.assume_init() };
            if passwd.pw_dir.is_null() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            // SAFETY: POSIX guarantees a NUL-terminated pw_dir within the supplied buffer.
            let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
            if bytes.is_empty() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
            if !path.is_absolute() {
                return Err(PrivateRootsError::MissingHomeDirectory);
            }
            return Ok(path);
        }
    }

    #[cfg(test)]
    mod tests {
        use std::fs;
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        use super::*;

        #[test]
        fn private_child_is_exactly_private_and_revalidation_detects_widening() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = open_path(temporary.path()).unwrap();
            let path = temporary.path().join("private");
            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let private = create_or_open_private_child(
                &base,
                OsStr::new("private"),
                security_context.effective_uid(),
            )
            .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            private.revalidate(&path, &security_context).unwrap();

            fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
            assert_eq!(
                private
                    .revalidate(&path, &security_context)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        #[test]
        fn private_root_rejects_a_linked_component() {
            let temporary = tempfile::tempdir().unwrap();
            let target = temporary.path().join("target");
            let linked = temporary.path().join("linked");
            fs::create_dir(&target).unwrap();
            symlink(&target, &linked).unwrap();
            assert!(open_path(&linked).is_err());
        }

        #[test]
        fn existing_insecure_child_is_rejected_without_permission_repair() {
            let temporary = tempfile::tempdir().unwrap();
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let base = open_path(temporary.path()).unwrap();
            let child = temporary.path().join("insecure");
            fs::create_dir(&child).unwrap();
            fs::set_permissions(&child, fs::Permissions::from_mode(0o750)).unwrap();

            assert_eq!(
                create_or_open_private_child(
                    &base,
                    OsStr::new("insecure"),
                    CurrentSecurityContextSnapshot::current()
                        .unwrap()
                        .effective_uid(),
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::metadata(&child).unwrap().permissions().mode() & 0o777,
                0o750
            );
        }

        #[test]
        fn environment_roots_must_be_absolute_and_nonempty() {
            assert!(matches!(
                environment_path("XDG_CACHE_HOME", OsString::new()),
                Err(PrivateRootsError::InvalidEnvironment { .. })
            ));
            assert!(matches!(
                environment_path("XDG_CACHE_HOME", OsString::from("relative")),
                Err(PrivateRootsError::InvalidEnvironment { .. })
            ));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn linux_xdg_roots_are_created_under_validated_bases() {
            let temporary = tempfile::tempdir().unwrap();
            let runtime_base = temporary.path().join("runtime");
            let cache_base = temporary.path().join("cache");
            fs::create_dir(&runtime_base).unwrap();
            fs::create_dir(&cache_base).unwrap();
            fs::set_permissions(&runtime_base, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&cache_base, fs::Permissions::from_mode(0o755)).unwrap();

            let security_context = CurrentSecurityContextSnapshot::current().unwrap();
            let expected_uid = security_context.effective_uid();
            let (runtime_path, runtime) =
                discover_linux_runtime(Some(runtime_base.into_os_string()), expected_uid).unwrap();
            let (cache_path, cache) =
                discover_linux_cache(Some(cache_base.into_os_string()), None, expected_uid)
                    .unwrap();

            assert_eq!(
                runtime_path.file_name(),
                Some(OsStr::new(PRODUCT_DIRECTORY))
            );
            assert_eq!(cache_path.file_name(), Some(OsStr::new(PRODUCT_DIRECTORY)));
            assert_eq!(
                fs::metadata(&runtime_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            runtime
                .revalidate(&runtime_path, &security_context)
                .unwrap();
            cache.revalidate(&cache_path, &security_context).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn invalid_explicit_xdg_runtime_never_falls_back() {
            let temporary = tempfile::tempdir().unwrap();
            let runtime_base = temporary.path().join("runtime");
            fs::create_dir(&runtime_base).unwrap();
            fs::set_permissions(&runtime_base, fs::Permissions::from_mode(0o755)).unwrap();

            assert!(
                discover_linux_runtime(
                    Some(runtime_base.clone().into_os_string()),
                    CurrentSecurityContextSnapshot::current()
                        .unwrap()
                        .effective_uid(),
                )
                .is_err()
            );
            assert!(!runtime_base.join(PRODUCT_DIRECTORY).exists());
        }
    }
}

#[cfg(windows)]
#[path = "roots_windows.rs"]
mod platform;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use std::io;
    use std::path::Path;

    use super::{DiscoveredRoots, PrivateRootsError};
    use crate::security_context::CurrentSecurityContextSnapshot;

    pub(super) struct PrivateDirectory;

    impl PrivateDirectory {
        pub(super) fn revalidate(
            &self,
            _path: &Path,
            _security_context: &CurrentSecurityContextSnapshot,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private local roots are unsupported on this platform",
            ))
        }
    }

    pub(super) fn discover(
        _security_context: &CurrentSecurityContextSnapshot,
    ) -> Result<DiscoveredRoots, PrivateRootsError> {
        Err(PrivateRootsError::UnsupportedPlatform)
    }
}
