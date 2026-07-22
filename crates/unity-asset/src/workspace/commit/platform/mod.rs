//! Platform publication primitives.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use unity_asset_core::DigestV1;

use super::super::source_catalog::PhysicalFileIdentity;
use super::journal::RECOVERY_DIRECTORY;

const COMMIT_LOCK_FILE: &str = ".commit.lock";
const LEGACY_COMMIT_LOCK_DIRECTORY: &str = "v1";

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

pub(crate) struct CommitGuard {
    _legacy_file: File,
    _stable_file: File,
    lock_path: PathBuf,
}

impl CommitGuard {
    pub(crate) fn acquire(root: &Path) -> io::Result<Self> {
        let recovery_directory = root.join(RECOVERY_DIRECTORY);
        let legacy_directory = recovery_directory.join(LEGACY_COMMIT_LOCK_DIRECTORY);
        platform::create_private_directory(&recovery_directory)?;
        platform::create_private_directory(&legacy_directory)?;

        // Journal v1 placed its only writer lock under v1/. Newer protocols
        // retain that lock and also take a version-independent lock so old and
        // future binaries cannot publish concurrently into the same root.
        let legacy_lock_path = legacy_directory.join(COMMIT_LOCK_FILE);
        let legacy_file = platform::acquire_lock(&legacy_lock_path)?;
        let lock_path = recovery_directory.join(COMMIT_LOCK_FILE);
        let stable_file = platform::acquire_lock(&lock_path)?;
        Ok(Self {
            _legacy_file: legacy_file,
            _stable_file: stable_file,
            lock_path,
        })
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.lock_path
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn commit_guard_holds_stable_and_v1_compatibility_locks() {
        let root = tempdir().expect("temporary publication root");
        let guard = CommitGuard::acquire(root.path()).expect("commit guard");
        let recovery = root.path().join(RECOVERY_DIRECTORY);
        let legacy = recovery
            .join(LEGACY_COMMIT_LOCK_DIRECTORY)
            .join(COMMIT_LOCK_FILE);

        assert_eq!(guard.path(), recovery.join(COMMIT_LOCK_FILE));
        assert!(platform::acquire_lock(&legacy).is_err());
        assert!(platform::acquire_lock(guard.path()).is_err());

        drop(guard);
        CommitGuard::acquire(root.path()).expect("locks released with guard");
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
}

pub(crate) fn create_private_directory(path: &Path) -> io::Result<()> {
    platform::create_private_directory(path)
}

pub(crate) fn create_private_directory_exclusive_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<DirectoryIdentity> {
    platform::create_private_directory_exclusive_in_parent(path, &expected_parent.0)
        .map(DirectoryIdentity)
}

pub(crate) fn create_private_file_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    platform::create_private_file_in_parent(path, &expected_parent.0)
}

pub(crate) fn remove_owned_file_in_parent(
    path: &Path,
    expected_file: &FileIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::remove_owned_file_in_parent(path, &expected_file.0, &expected_parent.0)
}

pub(crate) fn remove_owned_empty_directory_in_parent(
    path: &Path,
    expected_directory: &DirectoryIdentity,
    expected_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::remove_owned_empty_directory_in_parent(
        path,
        &expected_directory.0,
        &expected_parent.0,
    )
}

/// Opens an existing regular file without following the final path component.
pub(crate) fn open_readonly_regular(path: &Path) -> io::Result<File> {
    platform::open_readonly_regular(path)
}

pub(crate) fn open_readonly_regular_in_parent(
    path: &Path,
    expected_parent: &DirectoryIdentity,
) -> io::Result<File> {
    platform::open_readonly_regular_in_parent(path, &expected_parent.0)
}

#[cfg(test)]
pub(crate) fn observe_file_identity(path: &Path) -> io::Result<FileIdentity> {
    platform::observe_file_identity(path).map(FileIdentity)
}

pub(crate) fn opened_file_identity(file: &File) -> io::Result<FileIdentity> {
    platform::opened_file_identity(file).map(FileIdentity)
}

pub(crate) fn observe_directory_identity(path: &Path) -> io::Result<DirectoryIdentity> {
    platform::observe_directory_identity(path).map(DirectoryIdentity)
}

pub(crate) fn ensure_same_filesystem(first: &Path, second: &Path) -> io::Result<()> {
    platform::ensure_same_filesystem(first, second)
}

pub(crate) fn ensure_single_hardlink(path: &Path) -> io::Result<()> {
    platform::ensure_single_hardlink(path)
}

pub(crate) fn copy_security_metadata(
    source: &Path,
    destination: &Path,
    expected_source: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination: &FileIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::copy_security_metadata(
        source,
        destination,
        &expected_source.0,
        &expected_source_parent.0,
        &expected_destination.0,
        &expected_destination_parent.0,
    )
}

pub(crate) fn capture_existing_in_parent(
    source: &Path,
    destination: &Path,
    expected: &FileIdentity,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::capture_existing(
        source,
        destination,
        &expected.0,
        None,
        &expected_source_parent.0,
        &expected_destination_parent.0,
    )
}

pub(crate) fn capture_matching_digest_in_parent(
    source: &Path,
    destination: &Path,
    expected: &FileIdentity,
    expected_digest: DigestV1,
    expected_source_parent: &DirectoryIdentity,
    expected_destination_parent: &DirectoryIdentity,
) -> io::Result<()> {
    platform::capture_existing(
        source,
        destination,
        &expected.0,
        Some(expected_digest),
        &expected_source_parent.0,
        &expected_destination_parent.0,
    )
}

#[cfg(test)]
pub(crate) fn capture_existing(
    source: &Path,
    destination: &Path,
    expected: &FileIdentity,
) -> io::Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent"))?;
    let expected_parent = observe_directory_identity(parent)?;
    let source_parent = source
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let expected_source_parent = observe_directory_identity(source_parent)?;
    capture_existing_in_parent(
        source,
        destination,
        expected,
        &expected_source_parent,
        &expected_parent,
    )
}

/// Flushes a directory containing a journal or publication entry.
pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    platform::sync_directory(path)
}

/// Atomically moves `source` while preserving whether an error occurred after the move point.
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

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;
    use std::path::Path;

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct FileIdentity;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct DirectoryIdentity;

    impl FileIdentity {
        pub(super) const fn length(&self) -> u64 {
            u64::MAX
        }
    }

    pub(super) fn sync_directory(_: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory sync is unsupported on this platform",
        ))
    }

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

    pub(super) fn create_private_directory(_: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private publication directories are unsupported on this platform",
        ))
    }

    pub(super) fn create_private_directory_exclusive_in_parent(
        _: &Path,
        _: &DirectoryIdentity,
    ) -> io::Result<DirectoryIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound private publication directories are unsupported on this platform",
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

    pub(super) fn remove_owned_file_in_parent(
        _: &Path,
        _: &FileIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound private publication file deletion is unsupported on this platform",
        ))
    }

    pub(super) fn remove_owned_empty_directory_in_parent(
        _: &Path,
        _: &DirectoryIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound private publication directory deletion is unsupported on this platform",
        ))
    }

    pub(super) fn open_readonly_regular(_: &Path) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no-follow file opening is unsupported on this platform",
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

    #[cfg(test)]
    pub(super) fn observe_file_identity(_: &Path) -> io::Result<FileIdentity> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "file identity is unsupported",
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

    pub(super) fn ensure_same_filesystem(_: &Path, _: &Path) -> io::Result<()> {
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

    pub(super) fn copy_security_metadata(
        _: &Path,
        _: &Path,
        _: &FileIdentity,
        _: &DirectoryIdentity,
        _: &FileIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "security metadata is unsupported",
        ))
    }

    pub(super) fn capture_existing(
        _: &Path,
        _: &Path,
        _: &FileIdentity,
        _: Option<unity_asset_core::DigestV1>,
        _: &DirectoryIdentity,
        _: &DirectoryIdentity,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "identity-bound capture is unsupported",
        ))
    }

    pub(super) fn acquire_lock(_: &Path) -> io::Result<std::fs::File> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "publication locking is unsupported on this platform",
        ))
    }
}
