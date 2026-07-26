//! Security primitives for the local search daemon.

use std::error::Error;
use std::fmt;
#[cfg(any(test, windows))]
use std::fs;
#[cfg(windows)]
use std::fs::OpenOptions;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::mem::{MaybeUninit, size_of};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;

use fs2::FileExt;
use rand::TryRngCore;
use subtle::ConstantTimeEq;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_TOKEN, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, AddAccessAllowedAceEx, DACL_SECURITY_INFORMATION,
    EqualSid, GetKernelObjectSecurity, GetLengthSid, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl,
    InitializeSecurityDescriptor, IsValidAcl, IsValidSecurityDescriptor, IsValidSid,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    SECURITY_DESCRIPTOR, SID, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
    SetSecurityDescriptorOwner, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, READ_CONTROL,
};
#[cfg(windows)]
use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

const TOKEN_BYTES: usize = 32;
const ENCODED_TOKEN_BYTES: usize = TOKEN_BYTES * 2;
const TOKEN_FILE_NAME: &str = "daemon.token";
const DAEMON_LEASE_FILE_NAME: &str = ".daemon-instance.lock";
const WRITER_LOCK_FILE_NAME: &str = ".daemon-token.lock";
const TEMP_FILE_PREFIX: &str = ".daemon-token-";
const TEMP_FILE_SUFFIX: &str = ".tmp";
const TEMP_FILE_ATTEMPTS: usize = 16;
#[cfg(windows)]
const MAX_WINDOWS_SECURITY_DESCRIPTOR_BYTES: usize = 64 * 1024;
#[cfg(windows)]
const MAX_WINDOWS_TOKEN_INFORMATION_BYTES: usize = 64 * 1024;

/// Rejects daemon listeners that are reachable beyond the local machine.
pub fn validate_listen_addr(address: SocketAddr) -> Result<(), SecurityError> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(SecurityError::NonLoopbackListenAddress { address })
    }
}

/// A randomly generated 256-bit daemon credential.
///
/// The token intentionally implements neither [`fmt::Display`] nor serialization. Callers must
/// opt in to exposing it when writing the credential file or constructing an authorization value.
#[derive(Clone)]
pub struct DaemonToken {
    encoded: String,
}

impl DaemonToken {
    /// Generates a 256-bit token from the operating system random number generator.
    pub fn generate() -> Result<Self, SecurityError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        fill_random(&mut bytes)?;
        Ok(Self {
            encoded: hex::encode(bytes),
        })
    }

    /// Exposes the token to a protocol boundary.
    ///
    /// The returned value must not be logged, formatted into errors, or retained beyond the
    /// request or persistence operation that needs it.
    pub fn expose_secret(&self) -> &str {
        &self.encoded
    }

    fn from_persisted(bytes: &[u8], path: &Path) -> Result<Self, SecurityError> {
        if bytes.len() != ENCODED_TOKEN_BYTES {
            return Err(SecurityError::InvalidTokenFile {
                path: path.to_path_buf(),
                reason: InvalidTokenReason::Length {
                    actual: bytes.len() as u64,
                },
            });
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(SecurityError::InvalidTokenFile {
                path: path.to_path_buf(),
                reason: InvalidTokenReason::Encoding,
            });
        }
        let encoded = std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| {
            SecurityError::InvalidTokenFile {
                path: path.to_path_buf(),
                reason: InvalidTokenReason::Encoding,
            }
        })?;
        Ok(Self { encoded })
    }

    fn constant_time_matches(&self, candidate: &[u8]) -> bool {
        constant_time_equal(self.encoded.as_bytes(), candidate)
    }
}

impl fmt::Debug for DaemonToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DaemonToken([REDACTED])")
    }
}

/// A committed token update and any post-commit warning.
///
/// Once this value exists, callers must install [`Self::token`] as the active in-memory
/// credential even when [`Self::warning`] is present.
#[must_use = "the committed token and any post-commit warning must be handled"]
#[derive(Debug)]
pub struct TokenRotation {
    token: DaemonToken,
    warning: Option<RotationWarning>,
}

impl TokenRotation {
    fn clean(token: DaemonToken) -> Self {
        Self {
            token,
            warning: None,
        }
    }

    /// Returns the committed token.
    pub fn token(&self) -> &DaemonToken {
        &self.token
    }

    /// Returns post-commit diagnostics that do not contain credential or path material.
    pub fn warning(&self) -> Option<&RotationWarning> {
        self.warning.as_ref()
    }

    /// Separates the committed credential from its post-commit diagnostics.
    pub fn into_parts(self) -> (DaemonToken, Option<RotationWarning>) {
        (self.token, self.warning)
    }

    /// Consumes the outcome and returns the committed credential.
    ///
    /// Callers should inspect [`Self::warning`] before using this convenience method.
    pub fn into_token(self) -> DaemonToken {
        self.token
    }
}

/// Non-fatal diagnostics observed after the token publication commit point.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RotationWarning {
    durability_not_confirmed: bool,
    verification_not_confirmed: bool,
}

impl RotationWarning {
    /// Whether syncing the parent directory could not be confirmed.
    pub fn durability_not_confirmed(&self) -> bool {
        self.durability_not_confirmed
    }

    /// Whether reopening and verifying the published token could not be confirmed.
    pub fn verification_not_confirmed(&self) -> bool {
        self.verification_not_confirmed
    }

    fn is_empty(&self) -> bool {
        !self.durability_not_confirmed && !self.verification_not_confirmed
    }
}

impl fmt::Display for RotationWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (
            self.durability_not_confirmed,
            self.verification_not_confirmed,
        ) {
            (true, true) => formatter.write_str(
                "token committed; directory durability and post-commit verification are unconfirmed",
            ),
            (true, false) => {
                formatter.write_str("token committed; directory durability is unconfirmed")
            }
            (false, true) => {
                formatter.write_str("token committed; post-commit verification is unconfirmed")
            }
            (false, false) => formatter.write_str("token committed without warnings"),
        }
    }
}

/// Verifies an HTTP `Authorization: Bearer ...` value without a secret-dependent comparison.
///
/// Header shape and length are public protocol information and are rejected before comparing the
/// fixed-size credential. The credential comparison itself always examines all 64 encoded bytes.
pub fn verify_bearer_token(header: Option<&str>, expected: &DaemonToken) -> bool {
    let Some(header) = header else {
        return false;
    };
    let bytes = header.as_bytes();
    if bytes.len() != b"Bearer ".len() + ENCODED_TOKEN_BYTES {
        return false;
    }
    let (scheme, candidate) = bytes.split_at(b"Bearer ".len());
    if !scheme.eq_ignore_ascii_case(b"Bearer ") {
        return false;
    }
    expected.constant_time_matches(candidate)
}

fn constant_time_equal(expected: &[u8], candidate: &[u8]) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    bool::from(expected.ct_eq(candidate))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionCheck {
    Enforce,
    #[cfg(windows)]
    Repair,
}

fn rotation_permission_check() -> PermissionCheck {
    #[cfg(windows)]
    {
        PermissionCheck::Repair
    }
    #[cfg(not(windows))]
    {
        PermissionCheck::Enforce
    }
}

/// Owns the single credential file below one canonical index root.
#[derive(Debug, Clone)]
pub struct TokenStore {
    index_root: PathBuf,
    token_path: PathBuf,
    #[cfg(unix)]
    root_directory: Arc<File>,
}

impl TokenStore {
    /// Opens an existing, ordinary index root without following a root symlink or reparse point.
    pub fn open(index_root: impl AsRef<Path>) -> Result<Self, SecurityError> {
        let supplied_root = index_root.as_ref();

        #[cfg(unix)]
        {
            let (index_root, root_directory) = open_unix_root(supplied_root)?;
            let token_path = index_root.join(TOKEN_FILE_NAME);
            return Ok(Self {
                index_root,
                token_path,
                root_directory: Arc::new(root_directory),
            });
        }

        #[cfg(windows)]
        {
            let metadata = metadata_or_io("inspect index root", supplied_root)?;
            validate_ordinary_directory(supplied_root, &metadata)?;

            let canonical_root =
                fs::canonicalize(supplied_root).map_err(|source| SecurityError::Io {
                    operation: "canonicalize index root",
                    path: supplied_root.to_path_buf(),
                    source,
                })?;
            let canonical_metadata =
                metadata_or_io("inspect canonical index root", &canonical_root)?;
            validate_ordinary_directory(&canonical_root, &canonical_metadata)?;

            Ok(Self {
                token_path: canonical_root.join(TOKEN_FILE_NAME),
                index_root: canonical_root,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = supplied_root;
            Err(SecurityError::UnsupportedPlatform)
        }
    }

    /// Returns the fixed credential path.
    pub fn token_path(&self) -> &Path {
        &self.token_path
    }

    /// Creates a new token file and refuses to replace any existing filesystem entry.
    pub fn create(&self) -> Result<DaemonToken, SecurityError> {
        self.revalidate_root()?;
        let _lease = self.acquire_writer_lease()?;
        self.create_locked()
    }

    fn create_locked(&self) -> Result<DaemonToken, SecurityError> {
        let token = DaemonToken::generate()?;
        self.write_new_file(TOKEN_FILE_NAME, &token)?;
        self.sync_root()?;
        Ok(token)
    }

    /// Loads and validates the current token without following a token symlink or reparse point.
    pub fn load(&self) -> Result<DaemonToken, SecurityError> {
        self.read_token(PermissionCheck::Enforce)
    }

    fn read_token(&self, permission_check: PermissionCheck) -> Result<DaemonToken, SecurityError> {
        self.revalidate_root()?;
        let mut file = self.open_existing_token(permission_check)?;
        let mut bytes = [0_u8; ENCODED_TOKEN_BYTES];
        file.read_exact(&mut bytes)
            .map_err(|source| SecurityError::Io {
                operation: "read daemon token",
                path: self.token_path.clone(),
                source,
            })?;
        let mut trailing = [0_u8; 1];
        if file
            .read(&mut trailing)
            .map_err(|source| SecurityError::Io {
                operation: "verify daemon token length",
                path: self.token_path.clone(),
                source,
            })?
            != 0
        {
            return Err(SecurityError::InvalidTokenFile {
                path: self.token_path.clone(),
                reason: InvalidTokenReason::Length {
                    actual: (ENCODED_TOKEN_BYTES + 1) as u64,
                },
            });
        }
        DaemonToken::from_persisted(&bytes, &self.token_path)
    }

    /// Atomically rotates the credential only when `expected` is still current.
    ///
    /// The cross-process writer lease serializes publication, while this comparison prevents a
    /// daemon that waited for the lease from overwriting a credential published by another
    /// process. Errors are returned only before the atomic replacement commit point. Once
    /// publication commits, the returned [`TokenRotation`] always carries the credential callers
    /// must install; directory sync or post-commit verification failures become
    /// [`RotationWarning`].
    pub fn rotate_if_current(
        &self,
        expected: &DaemonToken,
    ) -> Result<TokenRotation, SecurityError> {
        self.revalidate_root()?;
        let _lease = self.acquire_writer_lease()?;
        self.rotate_locked(RotationCheckpoint::Normal, Some(expected))
    }

    fn rotate_locked(
        &self,
        checkpoint: RotationCheckpoint,
        expected: Option<&DaemonToken>,
    ) -> Result<TokenRotation, SecurityError> {
        // On Windows, a valid token whose ACL was accidentally widened must remain repairable.
        // The opened handle is still checked for file type, reparse points, length, and encoding.
        let current = self.read_token(rotation_permission_check())?;
        if expected
            .is_some_and(|expected| !current.constant_time_matches(expected.encoded.as_bytes()))
        {
            return Err(SecurityError::TokenRotationConflict {
                path: self.token_path.clone(),
            });
        }
        let token = DaemonToken::generate()?;
        let temporary = self.create_temporary_file(&token)?;

        #[cfg(test)]
        if checkpoint == RotationCheckpoint::BeforeCommit {
            return Err(SecurityError::Io {
                operation: "inject token rotation pre-commit failure",
                path: self.token_path.clone(),
                source: io::Error::other("injected pre-commit token rotation failure"),
            });
        }

        temporary.commit()?;

        let durability_not_confirmed = {
            #[cfg(test)]
            if checkpoint == RotationCheckpoint::DirectorySync {
                true
            } else {
                self.sync_root().is_err()
            }
            #[cfg(not(test))]
            {
                let _ = checkpoint;
                self.sync_root().is_err()
            }
        };
        let verification_not_confirmed = {
            #[cfg(test)]
            if checkpoint == RotationCheckpoint::Verification {
                true
            } else {
                self.published_token_differs(&token)
            }
            #[cfg(not(test))]
            {
                self.published_token_differs(&token)
            }
        };
        let warning = RotationWarning {
            durability_not_confirmed,
            verification_not_confirmed,
        };
        Ok(TokenRotation {
            token,
            warning: (!warning.is_empty()).then_some(warning),
        })
    }

    fn published_token_differs(&self, expected: &DaemonToken) -> bool {
        match self.read_token(PermissionCheck::Enforce) {
            Ok(persisted) => !persisted.constant_time_matches(expected.encoded.as_bytes()),
            Err(_) => true,
        }
    }

    /// Creates the initial credential or unconditionally rotates an existing valid credential.
    ///
    /// This is intended for process startup, before a credential has been installed in memory.
    /// Running daemons must use [`Self::rotate_if_current`] to avoid overwriting a concurrent
    /// process's committed token.
    pub fn create_or_rotate(&self) -> Result<TokenRotation, SecurityError> {
        self.revalidate_root()?;
        let _lease = self.acquire_writer_lease()?;
        match self.create_locked() {
            Ok(token) => Ok(TokenRotation::clean(token)),
            Err(SecurityError::TokenAlreadyExists { .. }) => {
                self.rotate_locked(RotationCheckpoint::Normal, None)
            }
            Err(error) => Err(error),
        }
    }

    /// Loads the existing credential or creates it under one cross-process writer lease.
    pub fn load_or_create(&self) -> Result<DaemonToken, SecurityError> {
        self.revalidate_root()?;
        let _lease = self.acquire_writer_lease()?;
        match self.read_token(PermissionCheck::Enforce) {
            Ok(token) => Ok(token),
            Err(SecurityError::TokenMissing { .. }) => self.create_locked(),
            Err(error) => Err(error),
        }
    }

    /// Acquires the index-root lease that a daemon must retain for its full lifetime.
    ///
    /// This prevents another daemon from rotating the persisted credential while the running
    /// process still authenticates against its in-memory copy.
    pub fn acquire_daemon_lease(&self) -> Result<DaemonLease, SecurityError> {
        self.revalidate_root()?;
        let path = self.index_root.join(DAEMON_LEASE_FILE_NAME);
        let file =
            match self.create_private_file(DAEMON_LEASE_FILE_NAME, PrivateFileSharing::Shared) {
                Ok(file) => file,
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => self
                    .open_existing_private_file(
                        DAEMON_LEASE_FILE_NAME,
                        0,
                        PermissionCheck::Enforce,
                        false,
                        true,
                    )?,
                Err(source) => {
                    return Err(SecurityError::Io {
                        operation: "create daemon instance lease",
                        path,
                        source,
                    });
                }
            };
        FileExt::try_lock_exclusive(&file).map_err(|source| {
            SecurityError::DaemonAlreadyRunning {
                path: self.index_root.join(DAEMON_LEASE_FILE_NAME),
                source,
            }
        })?;
        Ok(DaemonLease { file })
    }

    fn revalidate_root(&self) -> Result<(), SecurityError> {
        #[cfg(unix)]
        {
            validate_unix_root(&self.index_root, &self.root_directory)
        }
        #[cfg(windows)]
        {
            let metadata = metadata_or_io("revalidate index root", &self.index_root)?;
            validate_ordinary_directory(&self.index_root, &metadata)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SecurityError::UnsupportedPlatform)
        }
    }

    fn open_existing_token(
        &self,
        permission_check: PermissionCheck,
    ) -> Result<File, SecurityError> {
        self.open_existing_private_file(
            TOKEN_FILE_NAME,
            ENCODED_TOKEN_BYTES as u64,
            permission_check,
            true,
            false,
        )
    }

    fn acquire_writer_lease(&self) -> Result<WriterLease, SecurityError> {
        let path = self.index_root.join(WRITER_LOCK_FILE_NAME);
        let file = match self.create_private_file(WRITER_LOCK_FILE_NAME, PrivateFileSharing::Shared)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => self
                .open_existing_private_file(
                    WRITER_LOCK_FILE_NAME,
                    0,
                    PermissionCheck::Enforce,
                    false,
                    true,
                )?,
            Err(source) => {
                return Err(SecurityError::Io {
                    operation: "create daemon token writer lease",
                    path,
                    source,
                });
            }
        };
        FileExt::try_lock_exclusive(&file).map_err(|source| {
            SecurityError::WriterLeaseUnavailable {
                path: self.index_root.join(WRITER_LOCK_FILE_NAME),
                source,
            }
        })?;
        let lease = WriterLease { file };
        self.cleanup_stale_temporary_files()?;
        Ok(lease)
    }

    fn open_existing_private_file(
        &self,
        name: &str,
        expected_len: u64,
        permission_check: PermissionCheck,
        token_missing: bool,
        writable: bool,
    ) -> Result<File, SecurityError> {
        let path = self.index_root.join(name);
        #[cfg(unix)]
        {
            open_unix_private_file(
                &self.root_directory,
                name,
                &path,
                expected_len,
                permission_check,
                token_missing,
                writable,
            )
        }
        #[cfg(windows)]
        {
            open_windows_private_file(
                &path,
                expected_len,
                permission_check,
                token_missing,
                writable,
            )
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (
                path,
                expected_len,
                permission_check,
                token_missing,
                writable,
            );
            Err(SecurityError::UnsupportedPlatform)
        }
    }

    fn write_new_file(&self, name: &str, token: &DaemonToken) -> Result<(), SecurityError> {
        let path = self.index_root.join(name);
        let mut file = match self.create_private_file(name, PrivateFileSharing::Exclusive) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(SecurityError::TokenAlreadyExists { path });
            }
            Err(source) => {
                return Err(SecurityError::Io {
                    operation: "create daemon token",
                    path,
                    source,
                });
            }
        };
        let write_result = file
            .write_all(token.encoded.as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(source) = write_result {
            drop(file);
            let _ = self.remove_named_file(name);
            return Err(SecurityError::Io {
                operation: "persist daemon token",
                path,
                source,
            });
        }
        Ok(())
    }

    fn create_private_file(&self, name: &str, sharing: PrivateFileSharing) -> io::Result<File> {
        #[cfg(unix)]
        {
            let _ = sharing;
            create_unix_private_file(&self.root_directory, name)
        }
        #[cfg(windows)]
        {
            create_windows_private_file(&self.index_root.join(name), sharing)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (name, sharing);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure credential persistence is unsupported on this platform",
            ))
        }
    }

    fn create_temporary_file(
        &self,
        token: &DaemonToken,
    ) -> Result<TemporaryToken<'_>, SecurityError> {
        for _ in 0..TEMP_FILE_ATTEMPTS {
            let mut nonce = [0_u8; 16];
            fill_random(&mut nonce)?;
            let name = format!("{TEMP_FILE_PREFIX}{}{TEMP_FILE_SUFFIX}", hex::encode(nonce));
            match self.write_new_file(&name, token) {
                Ok(()) => {
                    return Ok(TemporaryToken {
                        store: self,
                        name,
                        committed: false,
                    });
                }
                Err(SecurityError::TokenAlreadyExists { .. }) => continue,
                Err(error) => return Err(error),
            }
        }
        Err(SecurityError::TemporaryNameExhausted {
            root: self.index_root.clone(),
        })
    }

    fn replace_named_file(&self, source_name: &str, destination_name: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::renameat(
                self.root_directory.as_ref(),
                source_name,
                self.root_directory.as_ref(),
                destination_name,
            )
            .map_err(io::Error::from)
        }
        #[cfg(windows)]
        {
            atomic_replace(
                &self.index_root.join(source_name),
                &self.index_root.join(destination_name),
            )
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (source_name, destination_name);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "atomic credential replacement is unsupported on this platform",
            ))
        }
    }

    fn remove_named_file(&self, name: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            rustix::fs::unlinkat(
                self.root_directory.as_ref(),
                name,
                rustix::fs::AtFlags::empty(),
            )
            .map_err(io::Error::from)
        }
        #[cfg(windows)]
        {
            fs::remove_file(self.index_root.join(name))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = name;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "credential cleanup is unsupported on this platform",
            ))
        }
    }

    fn cleanup_stale_temporary_files(&self) -> Result<(), SecurityError> {
        #[cfg(unix)]
        {
            let directory =
                rustix::fs::Dir::read_from(self.root_directory.as_ref()).map_err(|source| {
                    SecurityError::Io {
                        operation: "enumerate daemon token staging files",
                        path: self.index_root.clone(),
                        source: source.into(),
                    }
                })?;
            for entry in directory {
                let entry = entry.map_err(|source| SecurityError::Io {
                    operation: "read daemon token staging entry",
                    path: self.index_root.clone(),
                    source: source.into(),
                })?;
                let Ok(name) = entry.file_name().to_str() else {
                    continue;
                };
                self.remove_stale_temporary_file(name)?;
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            let directory = fs::read_dir(&self.index_root).map_err(|source| SecurityError::Io {
                operation: "enumerate daemon token staging files",
                path: self.index_root.clone(),
                source,
            })?;
            for entry in directory {
                let entry = entry.map_err(|source| SecurityError::Io {
                    operation: "read daemon token staging entry",
                    path: self.index_root.clone(),
                    source,
                })?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                self.remove_stale_temporary_file(name)?;
            }
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SecurityError::UnsupportedPlatform)
        }
    }

    fn remove_stale_temporary_file(&self, name: &str) -> Result<(), SecurityError> {
        if !is_temporary_file_name(name) {
            return Ok(());
        }
        match self.remove_named_file(name) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SecurityError::Io {
                operation: "remove stale daemon token staging file",
                path: self.index_root.join(name),
                source,
            }),
        }
    }

    fn sync_root(&self) -> Result<(), SecurityError> {
        #[cfg(unix)]
        {
            return self
                .root_directory
                .sync_all()
                .map_err(|source| SecurityError::Io {
                    operation: "sync index root",
                    path: self.index_root.clone(),
                    source,
                });
        }
        #[cfg(windows)]
        {
            // MoveFileExW uses MOVEFILE_WRITE_THROUGH for publication.
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(SecurityError::UnsupportedPlatform)
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PrivateFileSharing {
    Exclusive,
    Shared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RotationCheckpoint {
    Normal,
    #[cfg(test)]
    BeforeCommit,
    #[cfg(test)]
    DirectorySync,
    #[cfg(test)]
    Verification,
}

struct WriterLease {
    file: File,
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// An exclusive index-root lease retained by one running daemon.
#[derive(Debug)]
#[must_use = "the lease must remain alive for the daemon's full lifetime"]
pub struct DaemonLease {
    file: File,
}

impl Drop for DaemonLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

struct TemporaryToken<'a> {
    store: &'a TokenStore,
    name: String,
    committed: bool,
}

impl TemporaryToken<'_> {
    fn commit(mut self) -> Result<(), SecurityError> {
        self.store
            .replace_named_file(&self.name, TOKEN_FILE_NAME)
            .map_err(|source| SecurityError::Io {
                operation: "atomically rotate daemon token",
                path: self.store.token_path.clone(),
                source,
            })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TemporaryToken<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.store.remove_named_file(&self.name);
        }
    }
}

fn fill_random(bytes: &mut [u8]) -> Result<(), SecurityError> {
    let mut random = rand::rngs::OsRng;
    random
        .try_fill_bytes(bytes)
        .map_err(|source| SecurityError::EntropyUnavailable {
            message: source.to_string(),
        })
}

fn is_temporary_file_name(name: &str) -> bool {
    let Some(nonce) = name
        .strip_prefix(TEMP_FILE_PREFIX)
        .and_then(|name| name.strip_suffix(TEMP_FILE_SUFFIX))
    else {
        return false;
    };
    nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(windows)]
fn metadata_or_io(operation: &'static str, path: &Path) -> Result<Metadata, SecurityError> {
    fs::symlink_metadata(path).map_err(|source| SecurityError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(windows)]
fn validate_ordinary_directory(path: &Path, metadata: &Metadata) -> Result<(), SecurityError> {
    if is_link_or_reparse_point(metadata) {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::LinkOrReparsePoint,
        });
    }
    if !metadata.is_dir() {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::NotDirectory,
        });
    }
    Ok(())
}

fn validate_ordinary_private_file(
    path: &Path,
    metadata: &Metadata,
    expected_len: u64,
) -> Result<(), SecurityError> {
    if is_link_or_reparse_point(metadata) {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::LinkOrReparsePoint,
        });
    }
    if !metadata.is_file() {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::NotRegularFile,
        });
    }
    if metadata.len() != expected_len {
        if expected_len == ENCODED_TOKEN_BYTES as u64 {
            return Err(SecurityError::InvalidTokenFile {
                path: path.to_path_buf(),
                reason: InvalidTokenReason::Length {
                    actual: metadata.len(),
                },
            });
        }
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::UnexpectedLength {
                expected: expected_len,
                actual: metadata.len(),
            },
        });
    }
    Ok(())
}

#[cfg(unix)]
fn validate_owner_only_permissions(
    path: &Path,
    _file: &File,
    metadata: &Metadata,
) -> Result<(), SecurityError> {
    use std::os::unix::fs::MetadataExt;

    let expected_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::WrongOwner {
                expected: expected_uid,
                actual: metadata.uid(),
            },
        });
    }
    let mode = metadata.mode() & 0o7777;
    if mode != 0o600 {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::InsecurePermissions { mode },
        });
    }
    Ok(())
}

#[cfg(windows)]
fn validate_owner_only_permissions(
    path: &Path,
    file: &File,
    _metadata: &Metadata,
) -> Result<(), SecurityError> {
    let descriptor = OwnerOnlySecurityDescriptor::new().map_err(|source| SecurityError::Io {
        operation: "construct daemon token access control",
        path: path.to_path_buf(),
        source,
    })?;
    let matches = descriptor
        .matches(file)
        .map_err(|source| SecurityError::Io {
            operation: "inspect daemon token access control",
            path: path.to_path_buf(),
            source,
        })?;
    if matches {
        Ok(())
    } else {
        Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::InsecureAccessControl,
        })
    }
}

#[cfg(not(any(unix, windows)))]
fn validate_owner_only_permissions(
    path: &Path,
    _file: &File,
    _metadata: &Metadata,
) -> Result<(), SecurityError> {
    Err(SecurityError::Io {
        operation: "validate daemon token permissions",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "secure credential validation is unsupported on this platform",
        ),
    })
}

#[cfg(unix)]
fn open_unix_root(path: &Path) -> Result<(PathBuf, File), SecurityError> {
    use std::path::Component;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, openat, statat};

    const DIRECTORY_FLAGS: OFlags =
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| SecurityError::Io {
                operation: "resolve current directory",
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let mut normalized = PathBuf::from("/");
    let mut directory = openat(
        rustix::fs::CWD,
        Path::new("/"),
        DIRECTORY_FLAGS,
        Mode::empty(),
    )
    .map_err(|source| SecurityError::Io {
        operation: "open filesystem root",
        path: PathBuf::from("/"),
        source: source.into(),
    })?;

    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                normalized.push(name);
                let before = statat(&directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|source| map_unix_root_open_error(&normalized, source))?;
                let file_type = FileType::from_raw_mode(before.st_mode);
                if file_type.is_symlink() {
                    return Err(SecurityError::UnsafePath {
                        path: normalized,
                        reason: UnsafePathReason::LinkOrReparsePoint,
                    });
                }
                if !file_type.is_dir() {
                    return Err(SecurityError::UnsafePath {
                        path: normalized,
                        reason: UnsafePathReason::NotDirectory,
                    });
                }
                let opened = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|source| map_unix_root_open_error(&normalized, source))?;
                let after = fstat(&opened).map_err(|source| SecurityError::Io {
                    operation: "identify opened index root component",
                    path: normalized.clone(),
                    source: source.into(),
                })?;
                if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
                    return Err(SecurityError::UnsafePath {
                        path: normalized,
                        reason: UnsafePathReason::ChangedDuringOpen,
                    });
                }
                directory = opened;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(SecurityError::UnsafePath {
                    path: absolute,
                    reason: UnsafePathReason::EscapingComponent,
                });
            }
        }
    }

    let directory = File::from(directory);
    validate_unix_root(&normalized, &directory)?;
    Ok((normalized, directory))
}

#[cfg(unix)]
fn map_unix_root_open_error(path: &Path, source: rustix::io::Errno) -> SecurityError {
    if source == rustix::io::Errno::LOOP {
        SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::LinkOrReparsePoint,
        }
    } else if source == rustix::io::Errno::NOTDIR {
        SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::NotDirectory,
        }
    } else {
        SecurityError::Io {
            operation: "open index root without following links",
            path: path.to_path_buf(),
            source: source.into(),
        }
    }
}

#[cfg(unix)]
fn validate_unix_root(path: &Path, directory: &File) -> Result<(), SecurityError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory.metadata().map_err(|source| SecurityError::Io {
        operation: "inspect opened index root",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::NotDirectory,
        });
    }
    let expected_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::WrongOwner {
                expected: expected_uid,
                actual: metadata.uid(),
            },
        });
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::InsecureDirectoryPermissions { mode },
        });
    }
    Ok(())
}

#[cfg(unix)]
fn create_unix_private_file(root: &File, name: &str) -> io::Result<File> {
    use rustix::fs::{AtFlags, Mode, OFlags, fchmod, openat, unlinkat};

    let descriptor = openat(
        root,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(io::Error::from)?;
    let file = File::from(descriptor);
    let validation = fchmod(&file, Mode::from_raw_mode(0o600))
        .map_err(io::Error::from)
        .and_then(|()| validate_created_unix_private_file(&file));
    if let Err(source) = validation {
        drop(file);
        let _ = unlinkat(root, name, AtFlags::empty());
        return Err(source);
    }
    Ok(file)
}

#[cfg(unix)]
fn validate_created_unix_private_file(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    let expected_uid = rustix::process::geteuid().as_raw();
    if !metadata.is_file()
        || metadata.len() != 0
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "created Unix credential file did not retain owner-only permissions",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_unix_private_file(
    root: &File,
    name: &str,
    path: &Path,
    expected_len: u64,
    permission_check: PermissionCheck,
    token_missing: bool,
    writable: bool,
) -> Result<File, SecurityError> {
    use std::os::unix::fs::MetadataExt;

    use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};

    let before = statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|source| {
        map_private_open_error(path, source, token_missing, "inspect daemon credential")
    })?;
    if FileType::from_raw_mode(before.st_mode).is_symlink() {
        return Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::LinkOrReparsePoint,
        });
    }
    #[allow(clippy::useless_conversion)]
    let before = UnixFileIdentity {
        device: before.st_dev.into(),
        inode: before.st_ino.into(),
    };
    let access = if writable {
        OFlags::RDWR
    } else {
        OFlags::RDONLY
    };
    let descriptor = openat(
        root,
        name,
        access | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|source| {
        map_private_open_error(path, source, token_missing, "open daemon credential")
    })?;
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(|source| SecurityError::Io {
        operation: "inspect opened daemon credential",
        path: path.to_path_buf(),
        source,
    })?;
    validate_ordinary_private_file(path, &metadata, expected_len)?;
    let after = UnixFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    validate_same_file(path, &before, &after)?;
    if permission_check == PermissionCheck::Enforce {
        validate_owner_only_permissions(path, &file, &metadata)?;
    }
    Ok(file)
}

#[cfg(unix)]
fn map_private_open_error(
    path: &Path,
    source: rustix::io::Errno,
    token_missing: bool,
    operation: &'static str,
) -> SecurityError {
    if source == rustix::io::Errno::NOENT && token_missing {
        SecurityError::TokenMissing {
            path: path.to_path_buf(),
        }
    } else if source == rustix::io::Errno::LOOP {
        SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::LinkOrReparsePoint,
        }
    } else {
        SecurityError::Io {
            operation,
            path: path.to_path_buf(),
            source: source.into(),
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(windows)]
fn create_windows_private_file(path: &Path, sharing: PrivateFileSharing) -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;

    let security = OwnerOnlySecurityDescriptor::new()?;
    let attributes = security.security_attributes()?;
    let path_wide = windows_path(path);
    let share_mode = match sharing {
        PrivateFileSharing::Exclusive => 0,
        PrivateFileSharing::Shared => FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    };
    // SAFETY: `path_wide` is null terminated, `attributes` points into `security`, and both
    // remain alive for the call. CREATE_NEW makes descriptor installation part of creation.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
            share_mode,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a newly owned valid file handle.
    let file = unsafe { File::from_raw_handle(handle) };
    let validation = (|| {
        let metadata = file.metadata()?;
        if is_link_or_reparse_point(&metadata) || !metadata.is_file() || metadata.len() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "created Windows credential is not an empty ordinary file",
            ));
        }
        if !security.matches(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "created Windows credential did not retain its atomic owner-only descriptor",
            ));
        }
        Ok(())
    })();
    if let Err(source) = validation {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(source);
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_private_file(
    path: &Path,
    expected_len: u64,
    permission_check: PermissionCheck,
    token_missing: bool,
    writable: bool,
) -> Result<File, SecurityError> {
    let preflight = fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound && token_missing {
            SecurityError::TokenMissing {
                path: path.to_path_buf(),
            }
        } else {
            SecurityError::Io {
                operation: "inspect daemon credential without following reparse points",
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    validate_ordinary_private_file(path, &preflight, expected_len)?;

    let file = open_windows_existing_file(path, writable).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound && token_missing {
            SecurityError::TokenMissing {
                path: path.to_path_buf(),
            }
        } else {
            SecurityError::Io {
                operation: "open daemon credential without following reparse points",
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let metadata = file.metadata().map_err(|source| SecurityError::Io {
        operation: "inspect opened daemon credential",
        path: path.to_path_buf(),
        source,
    })?;
    validate_ordinary_private_file(path, &metadata, expected_len)?;
    let opened_identity = windows_file_identity(&file).map_err(|source| SecurityError::Io {
        operation: "identify opened daemon credential",
        path: path.to_path_buf(),
        source,
    })?;

    let current = open_windows_existing_file(path, false).map_err(|source| SecurityError::Io {
        operation: "reopen daemon credential for identity validation",
        path: path.to_path_buf(),
        source,
    })?;
    let current_identity = windows_file_identity(&current).map_err(|source| SecurityError::Io {
        operation: "identify current daemon credential path",
        path: path.to_path_buf(),
        source,
    })?;
    validate_same_file(path, &opened_identity, &current_identity)?;

    if permission_check == PermissionCheck::Enforce {
        validate_owner_only_permissions(path, &file, &metadata)?;
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_existing_file(path: &Path, writable: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    let access = if writable {
        GENERIC_READ | GENERIC_WRITE | READ_CONTROL
    } else {
        GENERIC_READ | READ_CONTROL
    };
    options
        .read(true)
        .write(writable)
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

#[cfg(windows)]
fn windows_path(path: &Path) -> Vec<u16> {
    use std::iter;
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowsFileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
fn windows_file_identity(file: &File) -> io::Result<WindowsFileIdentity> {
    let mut information = FILE_ID_INFO::default();
    let information_size = u32::try_from(size_of::<FILE_ID_INFO>())
        .map_err(|_| io::Error::other("FILE_ID_INFO size does not fit u32"))?;
    // SAFETY: the File owns a valid handle and `information` is writable for its declared size.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            windows_file_handle(file),
            FileIdInfo,
            (&raw mut information).cast(),
            information_size,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WindowsFileIdentity {
        volume_serial_number: information.VolumeSerialNumber,
        file_id: information.FileId.Identifier,
    })
}

#[cfg(windows)]
struct OwnerOnlySecurityDescriptor {
    _token_information: Vec<MaybeUninit<usize>>,
    acl: Vec<MaybeUninit<usize>>,
    descriptor: SECURITY_DESCRIPTOR,
    owner: PSID,
}

#[cfg(windows)]
impl OwnerOnlySecurityDescriptor {
    fn new() -> io::Result<Self> {
        let token = open_effective_token()?;
        let mut required_bytes = 0_u32;
        // SAFETY: the token handle is owned and valid. A null buffer with length zero is the
        // documented size-query form, and `required_bytes` is writable for the call.
        let first = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut required_bytes,
            )
        };
        if first != 0 || last_error_code() != Some(ERROR_INSUFFICIENT_BUFFER) {
            return Err(io::Error::last_os_error());
        }
        let required_len = bounded_windows_size(
            required_bytes,
            MAX_WINDOWS_TOKEN_INFORMATION_BYTES,
            "Windows token information",
        )?;
        if required_len < size_of::<TOKEN_USER>() {
            return Err(io::Error::other(
                "Windows token information is smaller than TOKEN_USER",
            ));
        }
        let mut token_information =
            aligned_windows_storage(required_len, "Windows token information")?;
        let token_capacity_bytes = windows_storage_bytes(&token_information)?;
        let mut returned_bytes = token_capacity_bytes;
        // SAFETY: the aligned allocation is writable for `token_capacity_bytes` bytes and remains
        // alive while every pointer returned inside TOKEN_USER is inspected.
        let succeeded = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                token_information.as_mut_ptr().cast(),
                token_capacity_bytes,
                &raw mut returned_bytes,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let returned_len = usize::try_from(returned_bytes)
            .map_err(|_| io::Error::other("Windows token information size does not fit usize"))?;
        let token_capacity_len = usize::try_from(token_capacity_bytes)
            .map_err(|_| io::Error::other("Windows token capacity does not fit usize"))?;
        if returned_len < size_of::<TOKEN_USER>() || returned_len > token_capacity_len {
            return Err(io::Error::other(
                "Windows returned an invalid TOKEN_USER buffer length",
            ));
        }

        let token_user = token_information.as_ptr().cast::<TOKEN_USER>();
        // SAFETY: the returned byte count was checked to contain a complete TOKEN_USER.
        let owner = unsafe { (*token_user).User.Sid };
        let storage_start = token_information.as_ptr() as usize;
        let storage_end = storage_start
            .checked_add(returned_len)
            .ok_or_else(|| io::Error::other("Windows token information address overflow"))?;
        let owner_start = owner as usize;
        let owner_header_end = owner_start
            .checked_add(size_of::<SID>())
            .ok_or_else(|| io::Error::other("Windows user SID address overflow"))?;
        if owner.is_null()
            || owner_start < storage_start
            || owner_header_end > storage_end
            // SAFETY: the SID header lies completely inside the API-populated allocation.
            || unsafe { IsValidSid(owner) } == 0
        {
            return Err(io::Error::other(
                "effective Windows token returned an invalid user SID",
            ));
        }
        // SAFETY: `IsValidSid` succeeded and the SID header is inside the retained allocation.
        let sid_length = unsafe { GetLengthSid(owner) };
        if sid_length == 0 {
            return Err(io::Error::last_os_error());
        }
        let owner_end = owner_start
            .checked_add(sid_length as usize)
            .ok_or_else(|| io::Error::other("Windows user SID length overflow"))?;
        if owner_end > storage_end {
            return Err(io::Error::other(
                "effective Windows token returned an out-of-bounds user SID",
            ));
        }

        let ace_bytes = size_of::<ACCESS_ALLOWED_ACE>()
            .checked_sub(size_of::<u32>())
            .and_then(|value| value.checked_add(usize::try_from(sid_length).ok()?))
            .ok_or_else(|| io::Error::other("owner-only Windows ACL size overflow"))?;
        let acl_bytes = size_of::<ACL>()
            .checked_add(ace_bytes)
            .ok_or_else(|| io::Error::other("owner-only Windows ACL size overflow"))?;
        let mut acl = aligned_windows_storage(acl_bytes, "owner-only Windows ACL")?;
        let acl_length = u32::try_from(acl_bytes)
            .map_err(|_| io::Error::other("owner-only Windows ACL is too large"))?;
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        // SAFETY: `acl_ptr` is aligned, writable, and backed by exactly `acl_length` bytes.
        if unsafe { InitializeAcl(acl_ptr, acl_length, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the ACL has enough reserved space for one full-access ACE and the validated SID.
        if unsafe { AddAccessAllowedAceEx(acl_ptr, ACL_REVISION, 0, FILE_ALL_ACCESS, owner) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        // SAFETY: `descriptor_ptr` points to an aligned, writable SECURITY_DESCRIPTOR.
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the initialized ACL and its allocation outlive the security descriptor.
        if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the validated owner SID remains backed by `token_information`.
        if unsafe { SetSecurityDescriptorOwner(descriptor_ptr, owner, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor_ptr` still points to the initialized writable descriptor.
        if unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: all embedded pointers reference retained, initialized backing allocations.
        if unsafe { IsValidSecurityDescriptor(descriptor_ptr) } == 0 {
            return Err(io::Error::other(
                "constructed owner-only Windows security descriptor is invalid",
            ));
        }

        Ok(Self {
            _token_information: token_information,
            acl,
            descriptor,
            owner,
        })
    }

    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        (&raw const self.descriptor).cast_mut().cast()
    }

    fn security_attributes(&self) -> io::Result<SECURITY_ATTRIBUTES> {
        let length = u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| io::Error::other("SECURITY_ATTRIBUTES size does not fit u32"))?;
        Ok(SECURITY_ATTRIBUTES {
            nLength: length,
            lpSecurityDescriptor: self.as_ptr(),
            bInheritHandle: 0,
        })
    }

    fn matches(&self, file: &File) -> io::Result<bool> {
        self.matches_handle(windows_file_handle(file))
    }

    fn matches_handle(&self, handle: HANDLE) -> io::Result<bool> {
        let snapshot = WindowsSecuritySnapshot::capture(handle)?;
        let Some(view) = snapshot.view()? else {
            return Ok(false);
        };
        // SAFETY: both SIDs were validated and their backing allocations remain alive.
        Ok(unsafe { EqualSid(view.owner, self.owner) } != 0
            && view.dacl_protected
            && acl_equal(view.dacl, self.acl.as_ptr().cast::<ACL>())?)
    }
}

#[cfg(windows)]
struct WindowsSecuritySnapshot {
    storage: Vec<MaybeUninit<usize>>,
}

#[cfg(windows)]
impl WindowsSecuritySnapshot {
    fn capture(handle: HANDLE) -> io::Result<Self> {
        let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut required_bytes = 0_u32;
        // SAFETY: the file handle is borrowed from a live File. The null-buffer invocation is the
        // documented size query, and `required_bytes` is writable.
        let first = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                std::ptr::null_mut(),
                0,
                &raw mut required_bytes,
            )
        };
        if first != 0 || last_error_code() != Some(ERROR_INSUFFICIENT_BUFFER) {
            return Err(io::Error::last_os_error());
        }
        let required_len = bounded_windows_size(
            required_bytes,
            MAX_WINDOWS_SECURITY_DESCRIPTOR_BYTES,
            "Windows security descriptor",
        )?;
        let mut storage =
            aligned_windows_storage(required_len, "Windows security descriptor snapshot")?;
        let capacity_bytes = windows_storage_bytes(&storage)?;
        let mut returned_bytes = capacity_bytes;
        // SAFETY: `storage` is aligned, writable for `capacity_bytes` bytes, and retained by the
        // result.
        let succeeded = unsafe {
            GetKernelObjectSecurity(
                handle,
                information,
                storage.as_mut_ptr().cast(),
                capacity_bytes,
                &raw mut returned_bytes,
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        let returned_len = usize::try_from(returned_bytes)
            .map_err(|_| io::Error::other("Windows descriptor size does not fit usize"))?;
        let capacity_len = usize::try_from(capacity_bytes)
            .map_err(|_| io::Error::other("Windows descriptor capacity does not fit usize"))?;
        if returned_len == 0 || returned_len > capacity_len {
            return Err(io::Error::other(
                "Windows returned an invalid security descriptor buffer length",
            ));
        }
        let snapshot = Self { storage };
        // SAFETY: the API populated the retained aligned buffer as a security descriptor.
        if unsafe { IsValidSecurityDescriptor(snapshot.as_ptr()) } == 0 {
            return Err(io::Error::other(
                "Windows returned an invalid security descriptor",
            ));
        }
        Ok(snapshot)
    }

    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.storage.as_ptr().cast_mut().cast()
    }

    fn view(&self) -> io::Result<Option<WindowsSecurityView>> {
        let mut owner = std::ptr::null_mut();
        let mut owner_defaulted = 0;
        // SAFETY: the snapshot is a valid descriptor and remains alive with all returned pointers.
        if unsafe {
            GetSecurityDescriptorOwner(self.as_ptr(), &raw mut owner, &raw mut owner_defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetSecurityDescriptorOwner returned this pointer from a validated descriptor.
        if owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
            return Ok(None);
        }

        let mut dacl_present = 0;
        let mut dacl = std::ptr::null_mut();
        let mut dacl_defaulted = 0;
        // SAFETY: the snapshot is a valid descriptor and remains alive with all returned pointers.
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
        // SAFETY: GetSecurityDescriptorDacl returned this pointer from a validated descriptor.
        if dacl_present == 0 || dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
            return Ok(None);
        }

        let mut control = 0;
        let mut revision = 0;
        // SAFETY: the snapshot is a valid descriptor and both outputs are writable.
        if unsafe {
            GetSecurityDescriptorControl(self.as_ptr(), &raw mut control, &raw mut revision)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(WindowsSecurityView {
            owner,
            dacl,
            dacl_protected: control & SE_DACL_PROTECTED != 0,
        }))
    }
}

#[cfg(windows)]
struct WindowsSecurityView {
    owner: PSID,
    dacl: *mut ACL,
    dacl_protected: bool,
}

#[cfg(windows)]
fn open_effective_token() -> io::Result<OwnedWindowsHandle> {
    let mut handle = std::ptr::null_mut();
    // SAFETY: the pseudo thread handle is valid, the output pointer is writable, and a successful
    // call returns a real token handle transferred into OwnedWindowsHandle.
    let thread_opened =
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut handle) };
    if thread_opened != 0 {
        return Ok(OwnedWindowsHandle(handle));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_NO_TOKEN as i32) {
        return Err(error);
    }

    // SAFETY: the pseudo process handle is valid and the output pointer is writable.
    let process_opened =
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut handle) };
    if process_opened == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedWindowsHandle(handle))
    }
}

#[cfg(windows)]
struct OwnedWindowsHandle(HANDLE);

#[cfg(windows)]
impl OwnedWindowsHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this type uniquely owns every non-null token handle it stores.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
fn windows_file_handle(file: &File) -> HANDLE {
    use std::os::windows::io::AsRawHandle;

    file.as_raw_handle()
}

#[cfg(windows)]
fn bounded_windows_size(size: u32, maximum: usize, description: &'static str) -> io::Result<usize> {
    let size = usize::try_from(size)
        .map_err(|_| io::Error::other(format!("{description} size does not fit usize")))?;
    if size == 0 {
        return Err(io::Error::other(format!(
            "{description} reported a zero size"
        )));
    }
    if size > maximum {
        return Err(io::Error::other(format!(
            "{description} exceeds the supported {maximum}-byte limit"
        )));
    }
    Ok(size)
}

#[cfg(windows)]
fn aligned_windows_storage(
    byte_len: usize,
    description: &'static str,
) -> io::Result<Vec<MaybeUninit<usize>>> {
    let units = byte_len.div_ceil(size_of::<usize>());
    let mut storage = Vec::new();
    storage.try_reserve_exact(units).map_err(|source| {
        io::Error::other(format!("could not allocate {description}: {source}"))
    })?;
    storage.resize_with(units, MaybeUninit::zeroed);
    Ok(storage)
}

#[cfg(windows)]
fn windows_storage_bytes(storage: &[MaybeUninit<usize>]) -> io::Result<u32> {
    storage
        .len()
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("Windows security buffer size overflow"))
}

#[cfg(windows)]
fn last_error_code() -> Option<u32> {
    io::Error::last_os_error()
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
}

#[cfg(windows)]
fn acl_equal(left: *const ACL, right: *const ACL) -> io::Result<bool> {
    let left_length = acl_length(left)?;
    let right_length = acl_length(right)?;
    if left_length != right_length {
        return Ok(false);
    }
    // SAFETY: `acl_length` validated both ACL pointers and bounds from each ACL header.
    let left = unsafe { std::slice::from_raw_parts(left.cast::<u8>(), left_length) };
    // SAFETY: same invariant as `left`; both backing security allocations remain alive.
    let right = unsafe { std::slice::from_raw_parts(right.cast::<u8>(), right_length) };
    Ok(left == right)
}

#[cfg(windows)]
fn acl_length(acl: *const ACL) -> io::Result<usize> {
    // SAFETY: callers obtain ACL pointers from validated descriptors or initialized local ACLs.
    if acl.is_null() || unsafe { IsValidAcl(acl) } == 0 {
        return Ok(0);
    }
    // SAFETY: IsValidAcl succeeded, so the ACL header is readable.
    let length = usize::from(unsafe { (*acl).AclSize });
    if length < size_of::<ACL>() {
        return Err(io::Error::other("Windows DACL is truncated"));
    }
    Ok(length)
}

#[cfg(unix)]
fn validate_same_file(
    path: &Path,
    before: &UnixFileIdentity,
    after: &UnixFileIdentity,
) -> Result<(), SecurityError> {
    if before == after {
        Ok(())
    } else {
        Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::ChangedDuringOpen,
        })
    }
}

#[cfg(windows)]
fn validate_same_file(
    path: &Path,
    before: &WindowsFileIdentity,
    after: &WindowsFileIdentity,
) -> Result<(), SecurityError> {
    if before == after {
        Ok(())
    } else {
        Err(SecurityError::UnsafePath {
            path: path.to_path_buf(),
            reason: UnsafePathReason::ChangedDuringOpen,
        })
    }
}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source_wide = windows_path(source);
    let destination_wide = windows_path(destination);
    // SAFETY: both pointers refer to null-terminated buffers that live for the duration of the
    // call. The paths identify files in the same validated index directory.
    let replaced = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Reasons a filesystem entry is unsafe for credential storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafePathReason {
    LinkOrReparsePoint,
    NotDirectory,
    NotRegularFile,
    EscapingComponent,
    WrongOwner { expected: u32, actual: u32 },
    InsecureDirectoryPermissions { mode: u32 },
    InsecurePermissions { mode: u32 },
    InsecureAccessControl,
    UnexpectedLength { expected: u64, actual: u64 },
    ChangedDuringOpen,
}

impl fmt::Display for UnsafePathReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LinkOrReparsePoint => formatter.write_str("path is a link or reparse point"),
            Self::NotDirectory => formatter.write_str("path is not a directory"),
            Self::NotRegularFile => formatter.write_str("path is not a regular file"),
            Self::EscapingComponent => formatter.write_str("path contains an escaping component"),
            Self::WrongOwner { expected, actual } => {
                write!(
                    formatter,
                    "file owner {actual} does not match effective user {expected}"
                )
            }
            Self::InsecureDirectoryPermissions { mode } => write!(
                formatter,
                "directory mode {mode:#o} permits group or other writes"
            ),
            Self::InsecurePermissions { mode } => {
                write!(formatter, "file mode {mode:#o} is not owner-only 0o600")
            }
            Self::InsecureAccessControl => {
                formatter.write_str("file access control is not owner-only")
            }
            Self::UnexpectedLength { expected, actual } => {
                write!(
                    formatter,
                    "file length {actual} does not match expected {expected}"
                )
            }
            Self::ChangedDuringOpen => formatter.write_str("path changed while it was opened"),
        }
    }
}

/// Reasons persisted token bytes are invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidTokenReason {
    Length { actual: u64 },
    Encoding,
}

impl fmt::Display for InvalidTokenReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { actual } => write!(
                formatter,
                "expected {ENCODED_TOKEN_BYTES} lowercase hexadecimal bytes, found {actual}"
            ),
            Self::Encoding => formatter.write_str("token is not lowercase hexadecimal"),
        }
    }
}

/// Failures from listener validation and credential persistence.
#[derive(Debug)]
#[non_exhaustive]
pub enum SecurityError {
    NonLoopbackListenAddress {
        address: SocketAddr,
    },
    EntropyUnavailable {
        message: String,
    },
    TokenMissing {
        path: PathBuf,
    },
    TokenAlreadyExists {
        path: PathBuf,
    },
    TokenRotationConflict {
        path: PathBuf,
    },
    DaemonAlreadyRunning {
        path: PathBuf,
        source: io::Error,
    },
    WriterLeaseUnavailable {
        path: PathBuf,
        source: io::Error,
    },
    TemporaryNameExhausted {
        root: PathBuf,
    },
    UnsafePath {
        path: PathBuf,
        reason: UnsafePathReason,
    },
    InvalidTokenFile {
        path: PathBuf,
        reason: InvalidTokenReason,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedPlatform,
}

impl fmt::Display for SecurityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopbackListenAddress { address } => {
                write!(formatter, "daemon listen address {address} is not loopback")
            }
            Self::EntropyUnavailable { message } => {
                write!(
                    formatter,
                    "operating system random generator failed: {message}"
                )
            }
            Self::TokenMissing { path } => {
                write!(
                    formatter,
                    "daemon token does not exist at {}",
                    path.display()
                )
            }
            Self::TokenAlreadyExists { path } => {
                write!(
                    formatter,
                    "daemon token already exists at {}",
                    path.display()
                )
            }
            Self::TokenRotationConflict { path } => write!(
                formatter,
                "daemon token changed before rotation at {}",
                path.display()
            ),
            Self::DaemonAlreadyRunning { path, source } => write!(
                formatter,
                "another daemon holds the index lease at {}: {source}",
                path.display()
            ),
            Self::WriterLeaseUnavailable { path, source } => write!(
                formatter,
                "daemon token writer lease is unavailable at {}: {source}",
                path.display()
            ),
            Self::TemporaryNameExhausted { root } => write!(
                formatter,
                "could not allocate a unique token staging file in {}",
                root.display()
            ),
            Self::UnsafePath { path, reason } => {
                write!(
                    formatter,
                    "unsafe credential path {}: {reason}",
                    path.display()
                )
            }
            Self::InvalidTokenFile { path, reason } => {
                write!(
                    formatter,
                    "invalid daemon token at {}: {reason}",
                    path.display()
                )
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} at {}: {source}", path.display()),
            Self::UnsupportedPlatform => formatter
                .write_str("secure daemon credential persistence is unsupported on this platform"),
        }
    }
}

impl Error for SecurityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DaemonAlreadyRunning { source, .. }
            | Self::WriterLeaseUnavailable { source, .. }
            | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn store_with_token() -> (TempDir, TokenStore, DaemonToken) {
        let directory = tempfile::tempdir().expect("the test directory must be creatable");
        let store = TokenStore::open(directory.path()).expect("the token store must open");
        let token = store.create().expect("the initial token must be created");
        (directory, store, token)
    }

    fn assert_token_matches(expected: &DaemonToken, actual: &DaemonToken) {
        assert!(actual.constant_time_matches(expected.expose_secret().as_bytes()));
    }

    fn staging_file_names(directory: &Path) -> Vec<String> {
        fs::read_dir(directory)
            .expect("the test directory must be readable")
            .map(|entry| {
                entry
                    .expect("the test entry must be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(TEMP_FILE_PREFIX) && name.ends_with(TEMP_FILE_SUFFIX))
            .collect()
    }

    #[test]
    fn writer_lease_cleans_only_canonical_staging_names() {
        let (directory, store, _current) = store_with_token();
        let stale_name = format!("{TEMP_FILE_PREFIX}{}{TEMP_FILE_SUFFIX}", "0".repeat(32));
        let lookalike_name = format!("{TEMP_FILE_PREFIX}not-a-nonce{TEMP_FILE_SUFFIX}");
        fs::write(directory.path().join(&stale_name), b"stale secret")
            .expect("the stale staging file must be creatable");
        fs::write(directory.path().join(&lookalike_name), b"unrelated")
            .expect("the lookalike file must be creatable");

        store
            .load_or_create()
            .expect("acquiring the writer lease must clean stale staging files");

        assert!(!directory.path().join(stale_name).exists());
        assert!(directory.path().join(lookalike_name).exists());
    }

    #[test]
    fn temporary_file_name_validation_is_exact() {
        assert!(is_temporary_file_name(
            ".daemon-token-0123456789abcdef0123456789abcdef.tmp"
        ));
        for invalid in [
            ".daemon-token-0123456789ABCDEF0123456789ABCDEF.tmp",
            ".daemon-token-0123456789abcdef.tmp",
            ".daemon-token-0123456789abcdef0123456789abcdef.tmp.extra",
            "daemon-token-0123456789abcdef0123456789abcdef.tmp",
        ] {
            assert!(!is_temporary_file_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn precommit_failure_preserves_current_token_and_cleans_staging_file() {
        let (directory, store, current) = store_with_token();
        let _lease = store
            .acquire_writer_lease()
            .expect("the writer lease must be available");

        let error = store
            .rotate_locked(RotationCheckpoint::BeforeCommit, Some(&current))
            .expect_err("the injected pre-commit failure must abort rotation");

        assert!(matches!(error, SecurityError::Io { .. }));
        assert_token_matches(
            &current,
            &store
                .load()
                .expect("the original token must remain readable"),
        );
        assert!(staging_file_names(directory.path()).is_empty());
    }

    #[test]
    fn directory_sync_failure_returns_the_committed_token_with_a_warning() {
        let (_directory, store, current) = store_with_token();
        let _lease = store
            .acquire_writer_lease()
            .expect("the writer lease must be available");

        let rotation = store
            .rotate_locked(RotationCheckpoint::DirectorySync, Some(&current))
            .expect("post-commit sync failure must not become an error");
        let (committed, warning) = rotation.into_parts();
        let warning = warning.expect("the sync failure must be reported");

        assert!(warning.durability_not_confirmed());
        assert!(!warning.verification_not_confirmed());
        assert_token_matches(
            &committed,
            &store.load().expect("the committed token must be readable"),
        );
    }

    #[test]
    fn verification_failure_returns_the_committed_token_with_a_warning() {
        let (_directory, store, current) = store_with_token();
        let _lease = store
            .acquire_writer_lease()
            .expect("the writer lease must be available");

        let rotation = store
            .rotate_locked(RotationCheckpoint::Verification, Some(&current))
            .expect("post-commit verification failure must not become an error");
        let (committed, warning) = rotation.into_parts();
        let warning = warning.expect("the verification failure must be reported");

        assert!(!warning.durability_not_confirmed());
        assert!(warning.verification_not_confirmed());
        assert_token_matches(
            &committed,
            &store.load().expect("the committed token must be readable"),
        );
    }

    #[test]
    fn concurrent_writer_lease_is_rejected_without_publishing() {
        let (_directory, store, current) = store_with_token();
        let _lease = store
            .acquire_writer_lease()
            .expect("the first writer lease must be available");

        let error = store
            .rotate_if_current(&current)
            .expect_err("a second writer must not enter the publication transaction");

        assert!(matches!(
            error,
            SecurityError::WriterLeaseUnavailable { .. }
        ));
        assert_token_matches(
            &current,
            &store
                .load()
                .expect("the current token must remain readable"),
        );
    }

    #[test]
    fn daemon_lease_is_exclusive_until_the_owner_drops_it() {
        let (_directory, store, _current) = store_with_token();
        let lease = store
            .acquire_daemon_lease()
            .expect("the first daemon lease must be available");

        let error = store
            .acquire_daemon_lease()
            .expect_err("a second daemon must not share the index root");
        assert!(matches!(error, SecurityError::DaemonAlreadyRunning { .. }));

        drop(lease);
        let _replacement_lease = store
            .acquire_daemon_lease()
            .expect("dropping the owner must release the daemon lease");
    }

    #[test]
    fn stale_compare_and_swap_is_rejected_without_overwriting_the_winner() {
        let (_directory, store, stale) = store_with_token();
        let winner = store
            .rotate_if_current(&stale)
            .expect("the first compare-and-swap must succeed")
            .into_token();

        let error = store
            .rotate_if_current(&stale)
            .expect_err("the stale compare-and-swap must fail");

        assert!(matches!(error, SecurityError::TokenRotationConflict { .. }));
        assert_token_matches(
            &winner,
            &store
                .load()
                .expect("the winning token must remain readable"),
        );
    }

    #[test]
    fn rotation_warning_formatting_contains_no_token_or_path_material() {
        let (_directory, store, token) = store_with_token();
        let warning = RotationWarning {
            durability_not_confirmed: true,
            verification_not_confirmed: true,
        };
        let secret = token.expose_secret();
        let path = store.token_path().display().to_string();

        for formatted in [format!("{warning}"), format!("{warning:?}")] {
            assert!(!formatted.contains(secret));
            assert!(!formatted.contains(&path));
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use windows_sys::Win32::Security::SetKernelObjectSecurity;
    use windows_sys::Win32::Storage::FileSystem::{WRITE_DAC, WRITE_OWNER};

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn create() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock must follow the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "unity-asset-search-daemon-windows-acl-{}-{timestamp}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("the unique test directory must be creatable");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn created_and_rotated_tokens_have_owner_only_protected_dacls() {
        let directory = TestDirectory::create();
        let store = TokenStore::open(&directory.path).expect("the token store must open");

        let current = store.create().expect("the token must be created");
        assert_owner_only_acl(store.token_path());

        let rotation = store
            .rotate_if_current(&current)
            .expect("the token must rotate");
        assert!(rotation.warning().is_none());
        assert_owner_only_acl(store.token_path());
    }

    #[test]
    fn owner_only_descriptor_is_installed_before_any_secret_is_written() {
        let directory = TestDirectory::create();
        let path = directory.path.join("prewrite.token");

        let file = create_windows_private_file(&path, PrivateFileSharing::Exclusive)
            .expect("the protected file must be created atomically");
        let expected =
            OwnerOnlySecurityDescriptor::new().expect("the current user SID must be available");

        assert_eq!(
            file.metadata()
                .expect("the protected file metadata must be readable")
                .len(),
            0
        );
        assert!(
            expected
                .matches(&file)
                .expect("the pre-write descriptor must be readable"),
            "the owner-only descriptor must already be present before writing the secret"
        );
    }

    #[test]
    fn load_rejects_a_null_dacl_and_rotation_repairs_it() {
        let directory = TestDirectory::create();
        let store = TokenStore::open(&directory.path).expect("the token store must open");
        let current = store.create().expect("the token must be created");

        install_null_dacl(store.token_path());
        assert!(matches!(
            store.load(),
            Err(SecurityError::UnsafePath {
                reason: UnsafePathReason::InsecureAccessControl,
                ..
            })
        ));

        let rotation = store
            .rotate_if_current(&current)
            .expect("rotation must replace a valid token whose DACL was widened");
        assert!(rotation.warning().is_none());
        store
            .load()
            .expect("the repaired owner-only token must load");
        assert_owner_only_acl(store.token_path());
    }

    fn assert_owner_only_acl(path: &Path) {
        let file = open_for_acl_update(path);
        let expected =
            OwnerOnlySecurityDescriptor::new().expect("the current user SID must be available");
        assert!(
            expected
                .matches(&file)
                .expect("the token DACL must be readable"),
            "the token DACL must contain only one current-user full-access ACE"
        );
    }

    fn install_null_dacl(path: &Path) {
        let file = open_for_acl_update(path);
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor_ptr = (&raw mut descriptor).cast();
        // SAFETY: the pointer refers to a writable local SECURITY_DESCRIPTOR.
        assert_ne!(
            unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) },
            0,
            "the test security descriptor must initialize"
        );
        // SAFETY: the descriptor is initialized; a present null DACL intentionally grants access
        // to every caller so the production validator must reject it.
        assert_ne!(
            unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, std::ptr::null(), 0) },
            0,
            "the test null DACL must initialize"
        );
        // SAFETY: the live test File owns the handle and was opened with WRITE_DAC.
        assert_ne!(
            unsafe {
                SetKernelObjectSecurity(
                    windows_file_handle(&file),
                    DACL_SECURITY_INFORMATION,
                    descriptor_ptr,
                )
            },
            0,
            "the test must be able to install a deliberately permissive null DACL"
        );
    }

    fn open_for_acl_update(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;

        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options
            .open(path)
            .expect("the test token must open without following a reparse point")
    }
}
