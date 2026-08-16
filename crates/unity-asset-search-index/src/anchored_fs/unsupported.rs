use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::path::Path;

use super::{AnchoredFsError, DirectoryEntryHint, OpenPolicy};

pub(super) struct ReadDirectory;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryObjectIdentity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity;

impl FileIdentity {
    pub(super) const fn length(self) -> u64 {
        0
    }
}

pub(super) struct DirectoryEntries<'directory> {
    _authority: PhantomData<&'directory ReadDirectory>,
}

pub(super) struct DirectoryNames<'directory> {
    _authority: PhantomData<&'directory ReadDirectory>,
}

impl Iterator for DirectoryNames<'_> {
    type Item = Result<OsString, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

impl Iterator for DirectoryEntries<'_> {
    type Item = Result<DirectoryEntryHint, AnchoredFsError>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

pub(super) fn open_directory(_: &Path, _: OpenPolicy) -> Result<ReadDirectory, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn open_directory_at(
    _: &ReadDirectory,
    _: &OsStr,
    _: OpenPolicy,
) -> Result<ReadDirectory, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn open_regular_at(
    _: &ReadDirectory,
    _: &OsStr,
    _: OpenPolicy,
) -> Result<(File, FileIdentity), AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn opened_file_identity(
    _: &File,
    _: OpenPolicy,
) -> Result<FileIdentity, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn opened_directory_identity(
    _: &ReadDirectory,
) -> Result<DirectoryIdentity, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn opened_directory_object_identity(
    _: &ReadDirectory,
) -> Result<DirectoryObjectIdentity, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn read_directory(
    _: &ReadDirectory,
    _: OpenPolicy,
) -> Result<DirectoryEntries<'_>, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn read_directory_names(
    _: &ReadDirectory,
    _: OpenPolicy,
) -> Result<DirectoryNames<'_>, AnchoredFsError> {
    Err(AnchoredFsError::UnsupportedPlatform)
}

pub(super) fn read_at(_: &File, _: &mut [u8], _: u64) -> io::Result<usize> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "positional anchored reads are unsupported on this platform",
    ))
}
