//! Platform publication primitives.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError, DigestV1};

use super::super::source_catalog::PhysicalFileIdentity;
use super::journal::RECOVERY_DIRECTORY;

pub(crate) const COMMIT_LOCK_FILE: &str = ".commit.lock";
pub(crate) const LEGACY_COMMIT_LOCK_DIRECTORY: &str = "v1";

/// One directory-entry name borrowed from an identity-bound platform iterator.
///
/// Recovery discovery only accepts short ASCII protocol names. Keeping the
/// native spelling borrowed avoids allocating for attacker-controlled names
/// before the caller has charged its budget.
#[derive(Clone, Copy)]
pub(crate) enum DirectoryEntryName<'a> {
    #[cfg(unix)]
    Unix(&'a OsStr),
    #[cfg(windows)]
    Windows(&'a [u16]),
    #[cfg(not(any(unix, windows)))]
    Unsupported(std::marker::PhantomData<&'a ()>),
}

impl DirectoryEntryName<'_> {
    /// Copies a canonical ASCII name into caller-owned fixed storage.
    ///
    /// `None` means the platform name is non-ASCII or too long for the
    /// supplied buffer. Both states are intentionally rejected by discovery.
    pub(crate) fn copy_ascii_into(self, output: &mut [u8]) -> Option<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            let Self::Unix(value) = self;
            let bytes = value.as_bytes();
            if bytes.len() > output.len() || !bytes.is_ascii() {
                return None;
            }
            output[..bytes.len()].copy_from_slice(bytes);
            Some(bytes.len())
        }
        #[cfg(windows)]
        {
            let Self::Windows(value) = self;
            if value.len() > output.len() {
                return None;
            }
            for (index, unit) in value.iter().copied().enumerate() {
                let byte = u8::try_from(unit).ok()?;
                if !byte.is_ascii() {
                    return None;
                }
                output[index] = byte;
            }
            Some(value.len())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (self, output);
            None
        }
    }
}

/// Separates a platform enumeration failure from a visitor's typed rejection.
#[derive(Debug)]
pub(crate) enum DirectoryVisitError<E> {
    Io(io::Error),
    Visitor(E),
}

/// Failure while preserving platform security metadata for a published file.
#[derive(Debug, Error)]
pub(crate) enum SecurityMetadataError {
    #[error("security metadata handling exceeded its caller-owned budget: {0}")]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One bounded allocation ledger reserved before a durable journal can be
/// installed for a later security-metadata copy.
///
/// Windows must read the source descriptor and then read the destination back
/// to verify it. The caller is charged for the platform maximum up front, while
/// the private ledger still enforces that maximum once publication has crossed
/// the durable boundary.
#[derive(Debug)]
pub(crate) struct SecurityMetadataCopyReservation {
    budget: AssetLoadBudget,
}

impl SecurityMetadataCopyReservation {
    pub(crate) fn budget_mut(&mut self) -> &mut AssetLoadBudget {
        &mut self.budget
    }
}

/// Reserves the maximum bounded work of one journal-to-journal metadata copy.
///
/// This makes a caller-owned budget exhaustion happen before the canonical
/// manifest is visible. Platforms that perform this copy without a counted
/// allocation reserve zero caller bytes but keep a valid private ledger for a
/// shared call site.
pub(crate) fn reserve_security_metadata_copy(
    budget: &mut AssetLoadBudget,
) -> Result<SecurityMetadataCopyReservation, BudgetError> {
    budget.consume_bytes(SECURITY_METADATA_COPY_RESERVATION_BYTES)?;
    let private_limits = AssetLoadLimits {
        max_bytes: SECURITY_METADATA_COPY_RESERVATION_BYTES.max(1),
        ..AssetLoadLimits::default()
    };
    let private_budget = AssetLoadBudget::new(private_limits)
        .expect("security metadata reservation limits must remain valid");
    Ok(SecurityMetadataCopyReservation {
        budget: private_budget,
    })
}

/// Conservative caller-owned reservation required for one later
/// journal-to-journal security-metadata copy.
pub(crate) const SECURITY_METADATA_COPY_RESERVATION_BYTES: u64 =
    platform::SECURITY_METADATA_COPY_RESERVATION_BYTES;

/// Conservative caller-owned reservation required before opening an
/// identity-bound directory iterator.
///
/// Linux, Android, and Windows enumerate into caller-owned stack buffers.
/// Apple and BSD targets reserve for the opaque `fdopendir` iterator state;
/// unsupported platforms reject identity-bound enumeration before allocating.
pub(crate) const DIRECTORY_VISIT_SETUP_BYTES: u64 = platform::DIRECTORY_VISIT_SETUP_BYTES;

/// Conservative caller-owned reservation required before advancing one
/// identity-bound directory iterator entry.
///
/// Native implementations expose each entry through borrowed storage, so this
/// remains zero on every supported platform.
pub(crate) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = platform::DIRECTORY_VISIT_ENTRY_BYTES;

#[derive(Debug)]
pub(crate) enum AtomicMoveError {
    NotMoved(io::Error),
    #[cfg(any(unix, windows))]
    MovedOrUnknown(io::Error),
}

impl AtomicMoveError {
    pub(super) fn not_moved(source: io::Error) -> Self {
        Self::NotMoved(source)
    }

    #[cfg(any(unix, windows))]
    pub(super) fn moved_or_unknown(source: io::Error) -> Self {
        Self::MovedOrUnknown(source)
    }

    #[must_use]
    pub(crate) const fn moved_or_unknown_state(&self) -> bool {
        #[cfg(any(unix, windows))]
        {
            matches!(self, Self::MovedOrUnknown(_))
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    #[must_use]
    pub(crate) const fn error(&self) -> &io::Error {
        match self {
            Self::NotMoved(source) => source,
            #[cfg(any(unix, windows))]
            Self::MovedOrUnknown(source) => source,
        }
    }

    pub(crate) fn into_error(self) -> io::Error {
        match self {
            Self::NotMoved(source) => source,
            #[cfg(any(unix, windows))]
            Self::MovedOrUnknown(source) => source,
        }
    }
}

pub(super) mod hex_u64 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub(super) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&format_args!("{value:016x}"))
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 16
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(D::Error::custom(
                "file identity component must be 16 lowercase hexadecimal digits",
            ));
        }
        u64::from_str_radix(&encoded, 16).map_err(D::Error::custom)
    }
}

#[cfg(windows)]
pub(super) mod hex_16_bytes {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    const HEX: &[u8; 16] = b"0123456789abcdef";

    pub(super) fn serialize<S>(value: &[u8; 16], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut encoded = String::with_capacity(32);
        for byte in value {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        serializer.serialize_str(&encoded)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 16], D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 32
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(D::Error::custom(
                "Windows file identity must be 32 lowercase hexadecimal digits",
            ));
        }
        let mut decoded = [0; 16];
        for (index, byte) in decoded.iter_mut().enumerate() {
            let offset = index * 2;
            *byte =
                u8::from_str_radix(&encoded[offset..offset + 2], 16).map_err(D::Error::custom)?;
        }
        Ok(decoded)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct FileIdentity(platform::FileIdentity);

impl FileIdentity {
    pub(crate) fn from_physical(identity: &PhysicalFileIdentity) -> Self {
        #[cfg(unix)]
        {
            let (device, inode) = identity.unix_parts();
            Self(platform::FileIdentity::new(
                device,
                inode,
                identity.length(),
            ))
        }
        #[cfg(windows)]
        {
            let (volume_serial_number, file_id) = identity.windows_parts();
            Self(platform::FileIdentity::new(
                volume_serial_number,
                file_id,
                identity.length(),
            ))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = identity;
            Self(platform::FileIdentity)
        }
    }

    #[must_use]
    pub(crate) const fn length(&self) -> u64 {
        self.0.length()
    }

    /// Returns whether two observations name the same filesystem object.
    ///
    /// File length deliberately does not participate here: a newly created
    /// staging file grows while its writer is still owned by this process.
    /// Callers that cross an irreversible boundary retain the final complete
    /// identity, including its length, for exact verification.
    #[must_use]
    pub(crate) fn same_object(&self, other: &Self) -> bool {
        self.0.same_object(&other.0)
    }

    pub(crate) fn invalid_sentinel() -> Self {
        #[cfg(unix)]
        {
            Self(platform::FileIdentity::new(0, 0, u64::MAX))
        }
        #[cfg(windows)]
        {
            Self(platform::FileIdentity::new(0, [0; 16], u64::MAX))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self(platform::FileIdentity)
        }
    }

    #[cfg(test)]
    pub(crate) fn test_identity(seed: u64, length: u64) -> Self {
        #[cfg(unix)]
        {
            Self(platform::FileIdentity::new(
                seed,
                seed.wrapping_add(1),
                length,
            ))
        }
        #[cfg(windows)]
        {
            let mut file_id = [0; 16];
            file_id[..8].copy_from_slice(&seed.to_le_bytes());
            file_id[8..].copy_from_slice(&seed.wrapping_add(1).to_le_bytes());
            Self(platform::FileIdentity::new(seed.max(1), file_id, length))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (seed, length);
            Self(platform::FileIdentity)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct DirectoryIdentity(platform::DirectoryIdentity);

impl DirectoryIdentity {
    pub(crate) fn from_physical(identity: &PhysicalFileIdentity) -> Self {
        #[cfg(unix)]
        {
            let (device, inode) = identity.unix_parts();
            Self(platform::DirectoryIdentity::new(device, inode))
        }
        #[cfg(windows)]
        {
            let (volume_serial_number, file_id) = identity.windows_parts();
            Self(platform::DirectoryIdentity::new(
                volume_serial_number,
                file_id,
            ))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = identity;
            Self(platform::DirectoryIdentity)
        }
    }
}

/// An opened, identity-verified publication root used for writer-side
/// operations. Keeping this handle alive makes initial recovery namespace
/// creation independent of later pathname replacement.
pub(crate) struct CommitRoot(platform::CommitRoot);

pub(crate) fn open_commit_root(
    root: &Path,
    expected: &DirectoryIdentity,
) -> io::Result<CommitRoot> {
    platform::open_commit_root(root, &expected.0).map(CommitRoot)
}

/// Opened recovery and protocol-version directories rooted in a verified
/// publication root. The opaque handles prevent namespace setup from being
/// redirected through a replacement pathname.
pub(crate) struct JournalNamespace(platform::JournalNamespace);

pub(crate) fn open_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    platform::open_journal_namespace(&root.0).map(JournalNamespace)
}

/// Opens only an already-existing recovery v2 namespace below an identity-bound
/// publication root. Unlike [`open_journal_namespace`], this never creates
/// recovery evidence while inspecting or resuming a prior transaction.
pub(crate) fn open_existing_journal_namespace(root: &CommitRoot) -> io::Result<JournalNamespace> {
    platform::open_existing_journal_namespace(&root.0).map(JournalNamespace)
}

/// Capability for journal operations rooted in an already-opened publication
/// root and recovery v2 namespace. Journal paths remain useful diagnostics and
/// wire data, but never select a filesystem parent through this boundary.
pub(crate) struct JournalAccess<'a> {
    root: &'a CommitRoot,
    namespace: &'a JournalNamespace,
}

pub(crate) fn journal_access<'a>(
    root: &'a CommitRoot,
    namespace: &'a JournalNamespace,
) -> JournalAccess<'a> {
    JournalAccess { root, namespace }
}

/// An existing directory opened relative to a [`JournalAccess`] namespace or
/// another opened journal directory.
pub(crate) struct JournalDirectory(platform::JournalDirectory);

impl std::fmt::Debug for JournalDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JournalDirectory(..)")
    }
}

pub(crate) fn open_journal_directory(
    access: &JournalAccess<'_>,
    path: &Path,
) -> io::Result<JournalDirectory> {
    platform::open_journal_directory(&access.namespace.0, journal_leaf(path)?).map(JournalDirectory)
}

pub(crate) fn open_journal_directory_in_directory(
    parent: &JournalDirectory,
    path: &Path,
) -> io::Result<JournalDirectory> {
    platform::open_journal_directory_in_directory(&parent.0, journal_leaf(path)?)
        .map(JournalDirectory)
}

/// Creates a new owner-only directory directly below the opened journal v2
/// namespace. The name is interpreted only relative to that namespace.
pub(crate) fn create_journal_directory(
    access: &JournalAccess<'_>,
    path: &Path,
) -> io::Result<JournalDirectory> {
    platform::create_journal_directory(&access.namespace.0, journal_leaf(path)?)
        .map(JournalDirectory)
}

/// Creates a new owner-only directory directly below an opened private
/// journal directory.
pub(crate) fn create_journal_directory_in_directory(
    parent: &JournalDirectory,
    path: &Path,
) -> io::Result<JournalDirectory> {
    platform::create_journal_directory_in_directory(&parent.0, journal_leaf(path)?)
        .map(JournalDirectory)
}

pub(crate) fn journal_directory_identity(
    directory: &JournalDirectory,
) -> io::Result<DirectoryIdentity> {
    platform::journal_directory_identity(&directory.0).map(DirectoryIdentity)
}

pub(crate) fn open_journal_regular(access: &JournalAccess<'_>, path: &Path) -> io::Result<File> {
    platform::open_journal_regular(&access.namespace.0, journal_leaf(path)?)
}

pub(crate) fn open_journal_regular_in_directory(
    parent: &JournalDirectory,
    path: &Path,
) -> io::Result<File> {
    platform::open_journal_regular_in_directory(&parent.0, journal_leaf(path)?)
}

pub(crate) fn create_journal_regular(access: &JournalAccess<'_>, path: &Path) -> io::Result<File> {
    platform::create_journal_regular(&access.namespace.0, journal_leaf(path)?)
}

pub(crate) fn create_journal_regular_in_directory(
    parent: &JournalDirectory,
    path: &Path,
) -> io::Result<File> {
    platform::create_journal_regular_in_directory(&parent.0, journal_leaf(path)?)
}

pub(crate) fn remove_journal_regular(
    access: &JournalAccess<'_>,
    path: &Path,
    expected: &FileIdentity,
) -> io::Result<()> {
    platform::remove_journal_regular(&access.namespace.0, journal_leaf(path)?, &expected.0)
}

pub(crate) fn remove_journal_regular_in_directory(
    parent: &JournalDirectory,
    path: &Path,
    expected: &FileIdentity,
) -> io::Result<()> {
    platform::remove_journal_regular_in_directory(&parent.0, journal_leaf(path)?, &expected.0)
}

pub(crate) fn remove_journal_directory(
    access: &JournalAccess<'_>,
    path: &Path,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    platform::remove_journal_directory(&access.namespace.0, journal_leaf(path)?, &expected.0)
}

pub(crate) fn remove_journal_directory_in_directory(
    parent: &JournalDirectory,
    path: &Path,
    expected: &DirectoryIdentity,
) -> io::Result<()> {
    platform::remove_journal_directory_in_directory(&parent.0, journal_leaf(path)?, &expected.0)
}

pub(crate) fn atomic_replace_journal_regular(
    access: &JournalAccess<'_>,
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> Result<(), AtomicMoveError> {
    platform::atomic_replace_journal_regular(
        &access.namespace.0,
        journal_leaf(source).map_err(AtomicMoveError::not_moved)?,
        journal_leaf(destination).map_err(AtomicMoveError::not_moved)?,
        replace_existing,
    )
}

pub(crate) fn atomic_replace_journal_regular_in_directory(
    parent: &JournalDirectory,
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> Result<(), AtomicMoveError> {
    platform::atomic_replace_journal_regular_in_directory(
        &parent.0,
        journal_leaf(source).map_err(AtomicMoveError::not_moved)?,
        journal_leaf(destination).map_err(AtomicMoveError::not_moved)?,
        replace_existing,
    )
}

pub(crate) fn sync_journal_access(access: &JournalAccess<'_>) -> io::Result<()> {
    platform::sync_journal_namespace(&access.root.0, &access.namespace.0)
}

pub(crate) fn sync_journal_directory(directory: &JournalDirectory) -> io::Result<()> {
    platform::sync_journal_directory(&directory.0)
}

pub(crate) fn visit_journal_directory_entries<S, E>(
    directory: &JournalDirectory,
    state: &mut S,
    before_entry: impl FnMut(&mut S) -> Result<(), E>,
    visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    platform::visit_journal_directory_entries(&directory.0, state, before_entry, visitor)
}

/// Copies one identity-bound journal file into a new private v2 sibling.
///
/// Both endpoints are opened below the same already-opened namespace, so a
/// replacement of the publication root cannot redirect a rollback receipt.
pub(crate) fn capture_journal_regular(
    access: &JournalAccess<'_>,
    source: &Path,
    destination: &Path,
    expected: &FileIdentity,
) -> io::Result<()> {
    let mut source_file = open_journal_regular(access, source)?;
    if opened_file_identity(&source_file)? != *expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "journal capture source no longer matches its captured identity",
        ));
    }
    let mut destination_file = create_journal_regular(access, destination)?;
    io::copy(&mut source_file, &mut destination_file)?;
    destination_file.sync_all()?;
    if opened_file_identity(&source_file)? != *expected {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "journal capture source changed while rollback receipt was written",
        ));
    }
    sync_journal_access(access)
}

fn journal_leaf(path: &Path) -> io::Result<&OsStr> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal operation path has no leaf name",
        )
    })?;
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal operation path has an invalid leaf name",
        ));
    }
    Ok(name)
}

#[cfg(test)]
pub(crate) fn journal_namespace_version_identity(
    namespace: &JournalNamespace,
) -> io::Result<DirectoryIdentity> {
    platform::journal_namespace_version_identity(&namespace.0).map(DirectoryIdentity)
}

pub(crate) fn sync_journal_namespace(
    root: &CommitRoot,
    namespace: &JournalNamespace,
) -> io::Result<()> {
    platform::sync_journal_namespace(&root.0, &namespace.0)
}

/// Prebuilt paths used to acquire publication locks.
///
/// Callers own and account for these allocations before handing them to the
/// platform boundary, which remains allocation-free.
#[derive(Debug)]
pub(crate) struct CommitLockPaths {
    recovery_directory: PathBuf,
    legacy_directory: PathBuf,
    legacy_lock_path: PathBuf,
    lock_path: PathBuf,
}

impl CommitLockPaths {
    fn new(
        recovery_directory: PathBuf,
        legacy_directory: PathBuf,
        legacy_lock_path: PathBuf,
        lock_path: PathBuf,
    ) -> Self {
        Self {
            recovery_directory,
            legacy_directory,
            legacy_lock_path,
            lock_path,
        }
    }

    pub(crate) fn new_budgeted(
        root: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, CommitLockPathError> {
        let recovery_directory = budgeted_commit_lock_child_path(
            root,
            RECOVERY_DIRECTORY,
            "commit lock recovery directory path",
            budget,
        )?;
        let legacy_directory = budgeted_commit_lock_child_path(
            &recovery_directory,
            LEGACY_COMMIT_LOCK_DIRECTORY,
            "commit lock legacy directory path",
            budget,
        )?;
        let legacy_lock_path = budgeted_commit_lock_child_path(
            &legacy_directory,
            COMMIT_LOCK_FILE,
            "commit lock legacy file path",
            budget,
        )?;
        let lock_path = budgeted_commit_lock_child_path(
            &recovery_directory,
            COMMIT_LOCK_FILE,
            "commit lock file path",
            budget,
        )?;
        Ok(Self::new(
            recovery_directory,
            legacy_directory,
            legacy_lock_path,
            lock_path,
        ))
    }
}

#[derive(Debug, Error)]
pub(crate) enum CommitLockPathError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} bytes for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        message: String,
    },
}

fn budgeted_commit_lock_child_path(
    parent: &Path,
    child: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, CommitLockPathError> {
    let requested = parent
        .as_os_str()
        .len()
        .checked_add(child.len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    let requested_u64 =
        u64::try_from(requested).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(requested_u64)?;

    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|error| CommitLockPathError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    path.push(parent);
    path.push(child);
    let actual =
        u64::try_from(path.capacity()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(path)
}

pub(crate) struct CommitGuard {
    _legacy_file: File,
    _stable_file: File,
}

impl CommitGuard {
    pub(crate) fn acquire_with_root(root: &CommitRoot) -> io::Result<Self> {
        let (legacy_file, stable_file) = platform::acquire_commit_locks(&root.0)?;
        Ok(Self {
            _legacy_file: legacy_file,
            _stable_file: stable_file,
        })
    }

    #[cfg(test)]
    pub(crate) fn acquire(root: &Path) -> io::Result<Self> {
        let identity = observe_directory_identity(root)?;
        let root = open_commit_root(root, &identity)?;
        Self::acquire_with_root(&root)
    }

    /// Acquires the two publication locks without creating any namespace or
    /// lock-file evidence.
    ///
    /// Discovery calls this before enumerating persisted recovery state. A
    /// missing recovery directory is represented as `Ok(None)` so a clean
    /// publication target remains a strictly read-only operation.
    pub(crate) fn acquire_existing(
        root: &Path,
        root_identity: &DirectoryIdentity,
        paths: CommitLockPaths,
    ) -> io::Result<Option<Self>> {
        if &observe_directory_identity(root)? != root_identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "publication root changed before recovery lock inspection",
            ));
        }

        let CommitLockPaths {
            recovery_directory,
            legacy_directory,
            legacy_lock_path,
            lock_path,
        } = paths;
        let recovery_identity = match observe_directory_identity(&recovery_directory) {
            Ok(identity) => identity,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if &observe_directory_identity(root)? != root_identity {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "publication root changed during clean recovery inspection",
                    ));
                }
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let legacy_identity = observe_directory_identity(&legacy_directory)?;

        let legacy_file = platform::acquire_existing_lock(&legacy_lock_path)?;
        let stable_file = platform::acquire_existing_lock(&lock_path)?;

        if &observe_directory_identity(root)? != root_identity
            || observe_directory_identity(&recovery_directory)? != recovery_identity
            || observe_directory_identity(&legacy_directory)? != legacy_identity
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "recovery lock namespace changed during acquisition",
            ));
        }

        Ok(Some(Self {
            _legacy_file: legacy_file,
            _stable_file: stable_file,
        }))
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use tempfile::tempdir;

    use super::super::journal::RECOVERY_VERSION_DIRECTORY;
    use super::*;

    #[test]
    fn file_identity_separates_object_identity_from_length() {
        let empty = FileIdentity::test_identity(7, 0);
        let written = FileIdentity::test_identity(7, 9);
        let replacement = FileIdentity::test_identity(8, 9);

        assert_ne!(empty, written);
        assert!(empty.same_object(&written));
        assert!(!empty.same_object(&replacement));
    }

    fn existing_lock_paths(root: &Path) -> CommitLockPaths {
        let recovery = root.join(RECOVERY_DIRECTORY);
        let legacy = recovery.join(LEGACY_COMMIT_LOCK_DIRECTORY);
        CommitLockPaths::new(
            recovery.clone(),
            legacy.clone(),
            legacy.join(COMMIT_LOCK_FILE),
            recovery.join(COMMIT_LOCK_FILE),
        )
    }

    #[test]
    fn commit_guard_holds_stable_and_v1_compatibility_locks() {
        let root = tempdir().expect("temporary publication root");
        let guard = CommitGuard::acquire(root.path()).expect("commit guard");
        let recovery = root.path().join(RECOVERY_DIRECTORY);
        let legacy = recovery
            .join(LEGACY_COMMIT_LOCK_DIRECTORY)
            .join(COMMIT_LOCK_FILE);
        let stable = recovery.join(COMMIT_LOCK_FILE);

        assert!(platform::acquire_lock(&legacy).is_err());
        assert!(platform::acquire_lock(&stable).is_err());

        drop(guard);
        CommitGuard::acquire(root.path()).expect("locks released with guard");
    }

    #[test]
    fn existing_commit_guard_is_zero_write_on_a_clean_root_and_reports_busy() {
        let root = tempdir().expect("temporary publication root");
        let recovery = root.path().join(RECOVERY_DIRECTORY);
        let root_identity = observe_directory_identity(root.path()).expect("root identity");

        assert!(
            CommitGuard::acquire_existing(
                root.path(),
                &root_identity,
                existing_lock_paths(root.path()),
            )
            .expect("clean root inspection")
            .is_none()
        );
        assert!(!recovery.exists());

        let guard = CommitGuard::acquire(root.path()).expect("commit guard");
        let busy = match CommitGuard::acquire_existing(
            root.path(),
            &root_identity,
            existing_lock_paths(root.path()),
        ) {
            Ok(_) => panic!("existing guard must observe the held compatibility lock"),
            Err(error) => error,
        };
        assert_eq!(busy.kind(), std::io::ErrorKind::WouldBlock);

        drop(guard);
        assert!(
            CommitGuard::acquire_existing(
                root.path(),
                &root_identity,
                existing_lock_paths(root.path()),
            )
            .expect("released existing guard")
            .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_guard_tightens_an_existing_recovery_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempdir().expect("temporary publication root");
        let recovery = root.path().join(RECOVERY_DIRECTORY);
        std::fs::create_dir(&recovery).expect("broad recovery root");
        std::fs::set_permissions(&recovery, std::fs::Permissions::from_mode(0o777))
            .expect("broad recovery permissions");

        let _guard = CommitGuard::acquire(root.path()).expect("commit guard");
        let mode = std::fs::metadata(recovery)
            .expect("recovery metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn commit_guard_creates_locks_under_the_opened_root_after_path_replacement() {
        let parent = tempdir().expect("temporary publication parent");
        let root = parent.path().join("target");
        let original = parent.path().join("original-target");
        std::fs::create_dir(&root).expect("publication root");
        let identity = observe_directory_identity(&root).expect("publication root identity");
        let anchored = open_commit_root(&root, &identity).expect("opened publication root");

        std::fs::rename(&root, &original).expect("move original root");
        std::fs::create_dir(&root).expect("replacement root");

        let _guard = CommitGuard::acquire_with_root(&anchored).expect("anchored commit guard");
        assert!(original.join(RECOVERY_DIRECTORY).exists());
        assert!(!root.join(RECOVERY_DIRECTORY).exists());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn journal_namespace_uses_the_current_recovery_version_directory() {
        let root = tempdir().expect("temporary publication root");
        let identity = observe_directory_identity(root.path()).expect("publication root identity");
        let anchored = open_commit_root(root.path(), &identity).expect("opened publication root");

        let _guard = CommitGuard::acquire_with_root(&anchored).expect("commit guard");
        let namespace = open_journal_namespace(&anchored).expect("journal namespace");
        let version_identity =
            journal_namespace_version_identity(&namespace).expect("journal version identity");

        let version = root
            .path()
            .join(RECOVERY_DIRECTORY)
            .join(RECOVERY_VERSION_DIRECTORY);
        assert_eq!(
            version_identity,
            observe_directory_identity(&version).expect("journal version directory identity")
        );
    }

    #[test]
    fn security_metadata_copy_reservation_is_charged_before_publication() {
        let mut caller_budget = AssetLoadBudget::default();
        let mut reservation =
            reserve_security_metadata_copy(&mut caller_budget).expect("metadata reservation");

        assert_eq!(
            caller_budget.usage().bytes,
            SECURITY_METADATA_COPY_RESERVATION_BYTES
        );
        assert_eq!(
            reservation.budget_mut().limits().max_bytes,
            SECURITY_METADATA_COPY_RESERVATION_BYTES.max(1)
        );
    }
}

pub(crate) fn create_private_file_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    platform::create_private_file_in_parent(path, &expected_parent.0)
}

pub(crate) fn open_readonly_regular_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    platform::open_readonly_regular_in_parent(path, &expected_parent.0)
}

/// Acquires a private, non-following exclusive lock in an identity-bound directory.
pub(crate) fn acquire_private_lock_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    platform::acquire_lock_in_parent(path, &expected_parent.0)
}

pub(crate) fn observe_file_identity(path: &Path) -> io::Result<FileIdentity> {
    platform::observe_file_identity(path).map(FileIdentity)
}

pub(crate) fn opened_file_identity(file: &File) -> io::Result<FileIdentity> {
    platform::opened_file_identity(file).map(FileIdentity)
}

pub(crate) fn observe_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    platform::observe_directory_identity(path).map(DirectoryIdentity)
}

/// Creates missing directory components without following links or changing existing metadata.
pub(crate) fn ensure_directory_no_follow(path: &Path) -> io::Result<DirectoryIdentity> {
    platform::ensure_directory_no_follow(path).map(DirectoryIdentity)
}

/// Visits names in an existing, identity-bound directory without following a
/// replacement directory path while enumeration is in progress.
pub(crate) fn visit_existing_directory_entries<S, E>(
    path: &Path,
    expected: &DirectoryIdentity,
    state: &mut S,
    before_entry: impl FnMut(&mut S) -> Result<(), E>,
    visitor: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
) -> Result<(), DirectoryVisitError<E>> {
    platform::visit_existing_directory_entries(path, &expected.0, state, before_entry, visitor)
}

/// Verifies that an already-opened private journal directory and an external
/// filesystem anchor are on the same filesystem without re-resolving the
/// journal directory through a pathname.
pub(crate) fn ensure_journal_directory_same_filesystem(
    directory: &JournalDirectory,
    anchor: &Path,
) -> io::Result<()> {
    platform::ensure_journal_directory_same_filesystem(&directory.0, anchor)
}

pub(crate) fn ensure_single_hardlink(path: &Path) -> io::Result<()> {
    platform::ensure_single_hardlink(path)
}

/// Copies security metadata between two entries addressed through already-open
/// private journal directory handles.
pub(crate) fn copy_security_metadata_between_journal_directories(
    source_directory: &JournalDirectory,
    source_path: &Path,
    destination_directory: &JournalDirectory,
    destination_path: &Path,
    expected_source: &FileIdentity,
    expected_destination: &FileIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    platform::copy_security_metadata_between_journal_directories(
        &source_directory.0,
        journal_leaf(source_path)?,
        &destination_directory.0,
        journal_leaf(destination_path)?,
        &expected_source.0,
        &expected_destination.0,
        budget,
    )
}

/// Copies security metadata from an external, identity-bound file to an entry
/// addressed through an already-opened private journal directory.
pub(crate) fn copy_security_metadata_external_to_journal_directory(
    source: &Path,
    destination_directory: &JournalDirectory,
    destination_path: &Path,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination: &FileIdentity,
    budget: &mut AssetLoadBudget,
) -> Result<(), SecurityMetadataError> {
    platform::copy_security_metadata_external_to_journal_directory(
        source,
        &destination_directory.0,
        journal_leaf(destination_path)?,
        &expected_source.0,
        &expected_source_parent.0,
        &expected_destination.0,
        budget,
    )
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn test_tamper_security_metadata(path: &Path) -> io::Result<()> {
    platform::test_tamper_security_metadata(path)
}

#[cfg(all(test, any(unix, windows)))]
pub(crate) fn test_security_metadata_matches(left: &Path, right: &Path) -> io::Result<bool> {
    platform::test_security_metadata_matches(left, right)
}

/// Moves an external, identity-bound regular file into an already-opened
/// private journal directory without re-resolving that directory by path.
pub(crate) fn capture_external_regular_in_journal_directory(
    source: &Path,
    destination: &JournalDirectory,
    destination_path: &Path,
    expected_source: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_source_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::capture_external_regular_in_journal_directory(
        source,
        &destination.0,
        journal_leaf(destination_path)?,
        &expected_source.0,
        expected_digest,
        &expected_source_parent.0,
    )
}

/// Moves an entry from an already-opened private journal directory to an
/// external, identity-bound destination parent without re-resolving the
/// journal directory by path.
pub(crate) fn promote_journal_regular_to_external(
    source: &JournalDirectory,
    source_path: &Path,
    destination: &Path,
    expected_source: &FileIdentity,
    expected_digest: Option<DigestV1>,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::promote_journal_regular_to_external(
        &source.0,
        journal_leaf(source_path)?,
        destination,
        &expected_source.0,
        expected_digest,
        &expected_destination_parent.0,
    )
}

#[cfg(test)]
pub(crate) fn capture_existing(
    source: &Path,
    destination: &Path,
    expected: &FileIdentity,
) -> io::Result<()> {
    let destination_parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    let source_parent = source
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let expected_destination_parent = observe_directory_identity(destination_parent)?;
    let expected_source_parent = observe_directory_identity(source_parent)?;
    platform::capture_existing(
        source,
        destination,
        &expected.0,
        None,
        &expected_source_parent.0,
        &expected_destination_parent.0,
    )
}

/// Atomically moves `source` while preserving whether an error occurred after the move point.
#[cfg(test)]
pub(crate) fn atomic_replace_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), AtomicMoveError> {
    platform::atomic_replace_tracked(
        source,
        destination,
        replace_existing,
        &expected_source_parent.0,
        &expected_destination_parent.0,
    )
}

/// Atomically publishes a captured temporary file after revalidating its identity and digest.
pub(crate) fn atomic_replace_verified_tracked(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    expected_source: &FileIdentity,
    expected_digest: DigestV1,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> Result<(), AtomicMoveError> {
    platform::atomic_replace_captured_tracked(
        source,
        destination,
        replace_existing,
        &expected_source.0,
        expected_digest,
        &expected_source_parent.0,
        &expected_destination_parent.0,
    )
}

/// Removes an unpublished temporary file only while its captured identities still match.
pub(crate) fn remove_owned_file_in_parent(
    path: &Path,
    expected_file: &FileIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::remove_owned_file_in_parent(path, &expected_file.0, &expected_parent.0)
}

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::io;
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    use super::DigestV1;
    use super::{DirectoryEntryName, DirectoryVisitError};

    pub(super) const DIRECTORY_VISIT_SETUP_BYTES: u64 = 0;
    pub(super) const DIRECTORY_VISIT_ENTRY_BYTES: u64 = 0;
    pub(super) const SECURITY_METADATA_COPY_RESERVATION_BYTES: u64 = 0;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct FileIdentity;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct DirectoryIdentity;

    pub(super) struct CommitRoot;

    pub(super) struct JournalNamespace;

    pub(super) struct JournalDirectory;

    impl FileIdentity {
        pub(super) const fn length(&self) -> u64 {
            u64::MAX
        }

        pub(super) fn same_object(&self, _: &Self) -> bool {
            true
        }
    }

    #[cfg(test)]
    pub(super) fn atomic_replace_tracked(
        _: &Path,
        _: &Path,
        _: bool,
        _: &DirectoryIdentity,
        _: &DirectoryIdentity,
    ) -> Result<(), super::AtomicMoveError> {
        Err(super::AtomicMoveError::not_moved(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic replacement is unsupported on this platform",
        )))
    }

    pub(super) fn atomic_replace_captured_tracked(
        _: &Path,
        _: &Path,
        _: bool,
        _: &FileIdentity,
        _: DigestV1,
        _: &DirectoryIdentity,
        _: &DirectoryIdentity,
    ) -> Result<(), super::AtomicMoveError> {
        Err(super::AtomicMoveError::not_moved(io::Error::new(
            io::ErrorKind::Unsupported,
            "verified atomic replacement is unsupported on this platform",
        )))
    }

    pub(super) fn open_commit_root(_: &Path, _: &DirectoryIdentity) -> io::Result<CommitRoot> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound publication roots are unsupported on this platform",
        ))
    }

    pub(super) fn acquire_commit_locks(
        _: &CommitRoot,
    ) -> io::Result<(std::fs::File, std::fs::File)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication locking is unsupported on this platform",
        ))
    }

    pub(super) fn open_journal_namespace(_: &CommitRoot) -> io::Result<JournalNamespace> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication journals are unsupported on this platform",
        ))
    }

    pub(super) fn open_existing_journal_namespace(_: &CommitRoot) -> io::Result<JournalNamespace> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication journals are unsupported on this platform",
        ))
    }

    pub(super) fn open_journal_directory(
        _: &JournalNamespace,
        _: &OsStr,
    ) -> io::Result<JournalDirectory> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn open_journal_directory_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
    ) -> io::Result<JournalDirectory> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn create_journal_directory(
        _: &JournalNamespace,
        _: &OsStr,
    ) -> io::Result<JournalDirectory> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn create_journal_directory_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
    ) -> io::Result<JournalDirectory> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn journal_directory_identity(
        _: &JournalDirectory,
    ) -> io::Result<DirectoryIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn open_journal_regular(
        _: &JournalNamespace,
        _: &OsStr,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal files are unsupported on this platform",
        ))
    }

    pub(super) fn open_journal_regular_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal files are unsupported on this platform",
        ))
    }

    pub(super) fn create_journal_regular(
        _: &JournalNamespace,
        _: &OsStr,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal files are unsupported on this platform",
        ))
    }

    pub(super) fn create_journal_regular_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal files are unsupported on this platform",
        ))
    }

    pub(super) fn remove_journal_regular(
        _: &JournalNamespace,
        _: &OsStr,
        _: &FileIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal cleanup is unsupported on this platform",
        ))
    }

    pub(super) fn remove_journal_regular_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
        _: &FileIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal cleanup is unsupported on this platform",
        ))
    }

    pub(super) fn remove_journal_directory(
        _: &JournalNamespace,
        _: &OsStr,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal cleanup is unsupported on this platform",
        ))
    }

    pub(super) fn remove_journal_directory_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal cleanup is unsupported on this platform",
        ))
    }

    pub(super) fn atomic_replace_journal_regular(
        _: &JournalNamespace,
        _: &OsStr,
        _: &OsStr,
        _: bool,
    ) -> Result<(), super::AtomicMoveError> {
        Err(super::AtomicMoveError::not_moved(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal replacement is unsupported on this platform",
        )))
    }

    pub(super) fn atomic_replace_journal_regular_in_directory(
        _: &JournalDirectory,
        _: &OsStr,
        _: &OsStr,
        _: bool,
    ) -> Result<(), super::AtomicMoveError> {
        Err(super::AtomicMoveError::not_moved(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal replacement is unsupported on this platform",
        )))
    }

    pub(super) fn sync_journal_directory(_: &JournalDirectory) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal directories are unsupported on this platform",
        ))
    }

    pub(super) fn visit_journal_directory_entries<S, E>(
        _: &JournalDirectory,
        _: &mut S,
        _: impl FnMut(&mut S) -> Result<(), E>,
        _: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
    ) -> Result<(), DirectoryVisitError<E>> {
        Err(DirectoryVisitError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound journal enumeration is unsupported on this platform",
        )))
    }

    #[cfg(test)]
    pub(super) fn journal_namespace_version_identity(
        _: &JournalNamespace,
    ) -> io::Result<DirectoryIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication journals are unsupported on this platform",
        ))
    }

    pub(super) fn sync_journal_namespace(_: &CommitRoot, _: &JournalNamespace) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication journals are unsupported on this platform",
        ))
    }

    pub(super) fn create_private_file_in_parent(
        _: &Path,
        _: &DirectoryIdentity,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound private publication files are unsupported on this platform",
        ))
    }

    pub(super) fn open_readonly_regular_in_parent(
        _: &Path,
        _: &DirectoryIdentity,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound no-follow file opening is unsupported on this platform",
        ))
    }

    pub(super) fn observe_file_identity(_: &Path) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is unsupported",
        ))
    }

    #[cfg(test)]
    pub(super) fn capture_existing(
        _: &Path,
        _: &Path,
        _: &FileIdentity,
        _: Option<DigestV1>,
        _: &DirectoryIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound publication capture is unsupported on this platform",
        ))
    }

    pub(super) fn opened_file_identity(_: &std::fs::File) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is unsupported",
        ))
    }

    pub(super) fn observe_directory_identity(_: &Path) -> io::Result<DirectoryIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory identity is unsupported",
        ))
    }

    pub(super) fn ensure_directory_no_follow(_: &Path) -> io::Result<DirectoryIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no-follow directory creation is unsupported on this platform",
        ))
    }

    pub(super) fn remove_owned_file_in_parent(
        _: &Path,
        _: &FileIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound temporary cleanup is unsupported on this platform",
        ))
    }

    #[cfg(test)]
    pub(super) fn ensure_same_filesystem(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem identity is unsupported",
        ))
    }

    pub(super) fn ensure_journal_directory_same_filesystem(
        _: &JournalDirectory,
        _: &Path,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem identity is unsupported",
        ))
    }

    pub(super) fn ensure_single_hardlink(_: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hard-link validation is unsupported",
        ))
    }

    pub(super) fn copy_security_metadata_between_journal_directories(
        _: &JournalDirectory,
        _: &OsStr,
        _: &JournalDirectory,
        _: &OsStr,
        _: &FileIdentity,
        _: &FileIdentity,
        _: &mut AssetLoadBudget,
    ) -> Result<(), SecurityMetadataError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "journal security metadata is unsupported",
        )
        .into())
    }

    pub(super) fn copy_security_metadata_external_to_journal_directory(
        _: &Path,
        _: &JournalDirectory,
        _: &OsStr,
        _: &FileIdentity,
        _: &DirectoryIdentity,
        _: &FileIdentity,
        _: &mut AssetLoadBudget,
    ) -> Result<(), SecurityMetadataError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "external-to-journal security metadata is unsupported",
        )
        .into())
    }

    pub(super) fn capture_external_regular_in_journal_directory(
        _: &Path,
        _: &JournalDirectory,
        _: &OsStr,
        _: &FileIdentity,
        _: Option<unity_asset_core::DigestV1>,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-rooted journal capture is unsupported on this platform",
        ))
    }

    pub(super) fn promote_journal_regular_to_external(
        _: &JournalDirectory,
        _: &OsStr,
        _: &Path,
        _: &FileIdentity,
        _: Option<unity_asset_core::DigestV1>,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-rooted journal publication is unsupported on this platform",
        ))
    }

    pub(super) fn acquire_lock(_: &Path) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication locking is unsupported on this platform",
        ))
    }

    pub(super) fn acquire_lock_in_parent(
        _: &Path,
        _: &DirectoryIdentity,
    ) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound publication locking is unsupported on this platform",
        ))
    }

    pub(super) fn acquire_existing_lock(_: &Path) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "existing publication locking is unsupported on this platform",
        ))
    }

    pub(super) fn visit_existing_directory_entries<S, E>(
        _: &Path,
        _: &DirectoryIdentity,
        _: &mut S,
        _: impl FnMut(&mut S) -> Result<(), E>,
        _: impl FnMut(&mut S, DirectoryEntryName<'_>) -> Result<(), E>,
    ) -> Result<(), DirectoryVisitError<E>> {
        Err(DirectoryVisitError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound directory enumeration is unsupported on this platform",
        )))
    }
}
