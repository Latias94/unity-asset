//! Prepared-artifact encoding for Unity bundle containers.
//!
//! This module deliberately accepts an ordered list of directory entries. A bundle directory is
//! an ordered wire structure (and may contain duplicate names), so a name-keyed edit map cannot
//! be the authority for a new proof image.

use std::collections::TryReserveError;
use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use flate2::Crc;
use thiserror::Error;
use unity_asset_binary::bundle::{AssetBundle, BundleHeader, BundleLayoutKind, DirectoryNode};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_core::{DigestV1, DigestV1Builder, UnityAssetError};

use crate::PackingPolicy;
use crate::artifact::{
    ArtifactBatch, ArtifactBuildError, ArtifactBuildFailurePhase, ArtifactHandle,
};

/// Namespace for canonical prepared Bundle encoding.
#[derive(Debug, Clone, Copy, Default)]
pub struct BundleWriter;

/// One ordered member supplied to a prepared bundle encoder.
///
/// The caller supplies a handle already prepared in the same artifact batch.  The constructor
/// snapshots its length from that batch, so the wire directory cannot drift away from the
/// artifact image later used as the dependency.
#[derive(Debug, Clone, Copy)]
pub struct BundleArtifactMember<'a> {
    name: &'a str,
    flags: u32,
    artifact: ArtifactHandle,
    length: u64,
}

impl<'a> BundleArtifactMember<'a> {
    /// Use a previously prepared artifact as the member payload.
    pub fn new(
        batch: &ArtifactBatch<'_, '_>,
        name: &'a str,
        flags: u32,
        artifact: ArtifactHandle,
    ) -> Result<Self, BundleArtifactError> {
        let length = batch.artifact_len(artifact)?;
        Ok(Self {
            name,
            flags,
            artifact,
            length,
        })
    }

    /// Return the prepared member's source artifact handle.
    #[must_use]
    pub const fn artifact(self) -> ArtifactHandle {
        self.artifact
    }

    /// Return the length captured from the batch at construction time.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.length
    }

    #[must_use]
    pub const fn name(self) -> &'a str {
        self.name
    }

    #[must_use]
    pub const fn flags(self) -> u32 {
        self.flags
    }
}

/// One ordered directory entry supplied to a prepared bundle encoder.
///
/// Files retain an artifact dependency, including zero-byte files. Empty directories retain only
/// their wire metadata and never create an artifact dependency or consume data-stream bytes.
#[derive(Debug, Clone, Copy)]
pub enum BundleArtifactEntry<'a> {
    /// A file backed by an artifact prepared in the same batch.
    File(BundleArtifactMember<'a>),
    /// A directory with no payload range.
    EmptyDirectory { name: &'a str, flags: u32 },
    /// A deleted/tombstone directory record with no payload dependency.
    Deleted { name: &'a str, flags: u32 },
}

impl<'a> BundleArtifactEntry<'a> {
    /// Use a previously prepared artifact as a file entry.
    pub fn file(
        batch: &ArtifactBatch<'_, '_>,
        name: &'a str,
        flags: u32,
        artifact: ArtifactHandle,
    ) -> Result<Self, BundleArtifactError> {
        BundleArtifactMember::new(batch, name, flags, artifact).map(Self::File)
    }

    /// Preserve one empty non-file node from a parsed UnityFS directory.
    ///
    /// Non-file nodes with a payload range cannot be represented as empty directories. Rejecting
    /// them here prevents a caller from silently discarding bytes while adapting parsed nodes.
    pub fn empty_directory_from_node(node: &'a DirectoryNode) -> Result<Self, BundleArtifactError> {
        if !node.is_directory() || node.is_deleted() {
            return Err(BundleArtifactError::ExpectedLiveDirectoryNode { flags: node.flags });
        }
        if node.size != 0 {
            return Err(BundleArtifactError::UnsupportedNonFileNodeRange {
                offset: node.offset,
                size: node.size,
            });
        }
        Ok(Self::EmptyDirectory {
            name: &node.name,
            flags: node.flags,
        })
    }

    /// Preserve one zero-length deleted/tombstone node from a parsed UnityFS directory.
    pub fn deleted_from_node(node: &'a DirectoryNode) -> Result<Self, BundleArtifactError> {
        if !node.is_deleted() {
            return Err(BundleArtifactError::ExpectedDeletedNode { flags: node.flags });
        }
        if node.size != 0 {
            return Err(BundleArtifactError::UnsupportedDeletedNodeRange {
                offset: node.offset,
                size: node.size,
            });
        }
        Ok(Self::Deleted {
            name: &node.name,
            flags: node.flags,
        })
    }

    #[must_use]
    pub const fn name(self) -> &'a str {
        match self {
            Self::File(member) => member.name(),
            Self::EmptyDirectory { name, .. } | Self::Deleted { name, .. } => name,
        }
    }

    #[must_use]
    pub const fn flags(self) -> u32 {
        match self {
            Self::File(member) => member.flags(),
            Self::EmptyDirectory { flags, .. } | Self::Deleted { flags, .. } => flags,
        }
    }

    #[must_use]
    pub const fn data_len(self) -> u64 {
        match self {
            Self::File(member) => member.length(),
            Self::EmptyDirectory { .. } | Self::Deleted { .. } => 0,
        }
    }

    #[must_use]
    pub const fn file_member(&self) -> Option<&BundleArtifactMember<'a>> {
        match self {
            Self::File(member) => Some(member),
            Self::EmptyDirectory { .. } | Self::Deleted { .. } => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum BundleArtifactError {
    #[error(transparent)]
    Artifact(Box<ArtifactBuildError>),
    #[error(transparent)]
    Unity(#[from] UnityAssetError),
    #[error(transparent)]
    Binary(#[from] unity_asset_binary::BinaryError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("bundle entry {entry} has an embedded NUL in its name")]
    EmbeddedNul { entry: usize },
    #[error("bundle entry count {count} does not fit the signed wire count")]
    EntryCountOverflow { count: usize },
    #[error("bundle block count {count} does not fit the signed wire count")]
    BlockCountOverflow { count: usize },
    #[error("bundle file entry {entry} has the directory flag: {flags:#x}")]
    FileEntryHasDirectoryFlag { entry: usize, flags: u32 },
    #[error("bundle file entry {entry} has the deleted flag: {flags:#x}")]
    FileEntryHasDeletedFlag { entry: usize, flags: u32 },
    #[error("bundle empty-directory entry {entry} is missing the directory flag: {flags:#x}")]
    EmptyDirectoryMissingDirectoryFlag { entry: usize, flags: u32 },
    #[error("bundle empty-directory entry {entry} has the deleted flag: {flags:#x}")]
    EmptyDirectoryHasDeletedFlag { entry: usize, flags: u32 },
    #[error("parsed node with flags {flags:#x} is not a live directory")]
    ExpectedLiveDirectoryNode { flags: u32 },
    #[error("parsed node with flags {flags:#x} is not deleted")]
    ExpectedDeletedNode { flags: u32 },
    #[error("non-file bundle node has unsupported payload range at {offset} with size {size}")]
    UnsupportedNonFileNodeRange { offset: u64, size: u64 },
    #[error("deleted bundle node has unsupported payload range at {offset} with size {size}")]
    UnsupportedDeletedNodeRange { offset: u64, size: u64 },
    #[error("legacy bundle layout cannot preserve empty-directory entry {entry}")]
    UnsupportedLegacyEmptyDirectory { entry: usize },
    #[error("legacy bundle layout cannot preserve deleted entry {entry}")]
    UnsupportedLegacyDeletedEntry { entry: usize },
    #[error(
        "legacy bundle file entry {entry} has flags {flags:#x}, but legacy directories encode no flags"
    )]
    UnsupportedLegacyFileFlags { entry: usize, flags: u32 },
    #[error("bundle deleted entry {entry} is missing the deleted flag: {flags:#x}")]
    DeletedEntryMissingDeletedFlag { entry: usize, flags: u32 },
    #[error("bundle file entry {entry} length {length} does not fit the file-stream offset domain")]
    FileStreamLengthOverflow { entry: usize, length: u64 },
    #[error("bundle file entry {entry} length {length} does not fit the legacy u32 domain")]
    LegacyLengthOverflow { entry: usize, length: u64 },
    #[error("bundle file entry {entry} range overflows the concatenated data stream")]
    DataLengthOverflow { entry: usize },
    #[error("file-stream bundle contains no data blocks; at least one non-empty file is required")]
    EmptyFileStreamData,
    #[error("preserve packing requires at least one original file-stream block")]
    MissingFileStreamBlocks,
    #[error("original file-stream block {block} has zero uncompressed bytes")]
    ZeroLengthFileStreamBlock { block: usize },
    #[error("bundle signature {signature:?} does not support packing policy {policy}")]
    UnsupportedPackingPolicy {
        signature: String,
        policy: PackingPolicy,
    },
    #[error(
        "legacy bundle version {version} cannot be independently inspected by the artifact proof"
    )]
    UnsupportedLegacyVersion { version: u32 },
    #[error("legacy bundle level count must be one, got {count}")]
    UnsupportedLegacyLevelCount { count: i32 },
    #[error("legacy bundle version {version} has no parsed header fields")]
    MissingLegacyHeader { version: u32 },
    #[error("legacy bundle version {version} requires a 16-byte hash and CRC")]
    MissingLegacyIntegrity { version: u32 },
    #[error("file-stream bundle {signature:?} is missing its v6 header byte")]
    MissingFileStreamHeaderByte { signature: String },
    #[error("bundle metadata arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("bundle metadata value {value} does not fit u32 for {resource}")]
    U32Overflow { value: u64, resource: &'static str },
    #[error("bundle metadata value {value} does not fit i64 for {resource}")]
    I64Overflow { value: u64, resource: &'static str },
    #[error("bundle block compression failed: {message}")]
    Compression { message: String },
    #[error("failed to reserve {requested} entries for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
}

impl BundleArtifactError {
    /// Reports the artifact-build stage in which this bundle preparation failed.
    #[must_use]
    pub const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::Artifact(error) => error.failure_phase(),
            _ => ArtifactBuildFailurePhase::Encoding,
        }
    }
}

impl From<ArtifactBuildError> for BundleArtifactError {
    fn from(error: ArtifactBuildError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

#[derive(Debug, Clone, Copy)]
struct FileStreamBlock {
    uncompressed_size: u32,
    compressed_size: u32,
    flags: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStreamCompression {
    None,
    Lzma,
    Lz4,
}

impl FileStreamCompression {
    fn from_switch(
        compression_switch: u32,
        resource: &'static str,
    ) -> Result<Self, BundleArtifactError> {
        match compression_switch {
            0 => Ok(Self::None),
            1 => Ok(Self::Lzma),
            2 | 3 => Ok(Self::Lz4),
            other => Err(BundleArtifactError::Compression {
                message: format!("unsupported {resource} compression switch: {other}"),
            }),
        }
    }

    fn chunk_size(self) -> u64 {
        match self {
            Self::None | Self::Lzma => u64::from(u32::MAX),
            Self::Lz4 => 0x0002_0000,
        }
    }

    fn from_validated_flags(flags: u32) -> Self {
        match flags & 0x3f {
            0 => Self::None,
            1 => Self::Lzma,
            2 | 3 => Self::Lz4,
            _ => unreachable!("file-stream block flags were validated by the layout plan"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FileStreamBlockLayoutSource<'a> {
    Uniform {
        flags: u32,
        compression: FileStreamCompression,
    },
    Preserve {
        blocks: &'a [CompressionBlock],
        tail_flags: u32,
        tail_compression: FileStreamCompression,
    },
}

#[derive(Debug, Clone, Copy)]
struct FileStreamBlockLayout<'a> {
    total: u64,
    count: usize,
    compressed_count: usize,
    source: FileStreamBlockLayoutSource<'a>,
}

impl<'a> FileStreamBlockLayout<'a> {
    fn uniform(
        total: u64,
        flags: u32,
        compression: FileStreamCompression,
    ) -> Result<Self, BundleArtifactError> {
        let count = usize_value(
            total.div_ceil(compression.chunk_size()),
            "file-stream block count",
        )?;
        Ok(Self {
            total,
            count,
            compressed_count: if compression == FileStreamCompression::None {
                0
            } else {
                count
            },
            source: FileStreamBlockLayoutSource::Uniform { flags, compression },
        })
    }

    fn preserve(total: u64, blocks: &'a [CompressionBlock]) -> Result<Self, BundleArtifactError> {
        let last = blocks
            .last()
            .ok_or(BundleArtifactError::MissingFileStreamBlocks)?;
        let tail_flags = u32::from(last.flags);
        let tail_compression =
            FileStreamCompression::from_switch(tail_flags & 0x3f, "file-stream block")?;
        let mut covered = 0_u64;
        let mut count = 0_usize;
        let mut compressed_count = 0_usize;
        for (block, original) in blocks.iter().enumerate() {
            if original.uncompressed_size == 0 {
                return Err(BundleArtifactError::ZeroLengthFileStreamBlock { block });
            }
            let flags = u32::from(original.flags);
            let compression =
                FileStreamCompression::from_switch(flags & 0x3f, "file-stream block")?;
            if covered >= total {
                continue;
            }
            covered = covered
                .checked_add(u64::from(original.uncompressed_size))
                .ok_or(BundleArtifactError::ArithmeticOverflow {
                    resource: "original file-stream block coverage",
                })?
                .min(total);
            count = count
                .checked_add(1)
                .ok_or(BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream block count",
                })?;
            if compression != FileStreamCompression::None {
                compressed_count = compressed_count.checked_add(1).ok_or(
                    BundleArtifactError::ArithmeticOverflow {
                        resource: "compressed file-stream block count",
                    },
                )?;
            }
        }
        if covered < total {
            let tail_count = usize_value(
                (total - covered).div_ceil(tail_compression.chunk_size()),
                "file-stream tail block count",
            )?;
            count =
                count
                    .checked_add(tail_count)
                    .ok_or(BundleArtifactError::ArithmeticOverflow {
                        resource: "file-stream block count",
                    })?;
            if tail_compression != FileStreamCompression::None {
                compressed_count = compressed_count.checked_add(tail_count).ok_or(
                    BundleArtifactError::ArithmeticOverflow {
                        resource: "compressed file-stream block count",
                    },
                )?;
            }
        }
        Ok(Self {
            total,
            count,
            compressed_count,
            source: FileStreamBlockLayoutSource::Preserve {
                blocks,
                tail_flags,
                tail_compression,
            },
        })
    }

    const fn len(self) -> usize {
        self.count
    }

    const fn compressed_len(self) -> usize {
        self.compressed_count
    }

    fn iter(self) -> FileStreamBlockLayoutIter<'a> {
        FileStreamBlockLayoutIter {
            layout: self,
            start: 0,
            original_index: 0,
            remaining: self.count,
        }
    }
}

struct PlannedFileStreamBlock {
    range: Range<u64>,
    flags: u32,
    compression: FileStreamCompression,
}

struct FileStreamBlockLayoutIter<'a> {
    layout: FileStreamBlockLayout<'a>,
    start: u64,
    original_index: usize,
    remaining: usize,
}

impl Iterator for FileStreamBlockLayoutIter<'_> {
    type Item = PlannedFileStreamBlock;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let (length, flags, compression) = match self.layout.source {
            FileStreamBlockLayoutSource::Uniform { flags, compression } => {
                (compression.chunk_size(), flags, compression)
            }
            FileStreamBlockLayoutSource::Preserve {
                blocks,
                tail_flags,
                tail_compression,
            } => {
                if let Some(block) = blocks.get(self.original_index) {
                    self.original_index += 1;
                    let flags = u32::from(block.flags);
                    (
                        u64::from(block.uncompressed_size),
                        flags,
                        FileStreamCompression::from_validated_flags(flags),
                    )
                } else {
                    (tail_compression.chunk_size(), tail_flags, tail_compression)
                }
            }
        };
        let start = self.start;
        let end = start.saturating_add(length).min(self.layout.total);
        self.start = end;
        self.remaining -= 1;
        Some(PlannedFileStreamBlock {
            range: start..end,
            flags,
            compression,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for FileStreamBlockLayoutIter<'_> {}

#[derive(Debug, Clone, Copy)]
struct FileStreamPlan<'a> {
    header_flags: u32,
    block_info_compression: FileStreamCompression,
    block_layout: FileStreamBlockLayout<'a>,
    block_info_uncompressed_len: u64,
    total_data_len: u64,
    header_len: u64,
    at_end: bool,
    signature_byte: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct LegacyIntegrity {
    hash: [u8; 16],
    crc: u32,
    content_digest: DigestV1,
}

#[derive(Debug, Clone, Copy)]
struct LegacyPlan {
    compress: bool,
    levels_before_streaming: u32,
    directory_len: u64,
    member_bytes: u64,
    uncompressed_len: u64,
    header_len: u64,
    integrity: Option<LegacyIntegrity>,
}

#[derive(Debug, Clone, Copy)]
enum BundleEncodingPlan<'a> {
    FileStream(FileStreamPlan<'a>),
    Legacy(LegacyPlan),
}

impl<'a> BundleEncodingPlan<'a> {
    fn new(
        bundle: &'a AssetBundle,
        entries: &[BundleArtifactEntry<'_>],
        policy: PackingPolicy,
    ) -> Result<Self, BundleArtifactError> {
        let layout = bundle.header.layout_kind()?;
        validate_entries(layout, entries)?;
        match layout {
            BundleLayoutKind::FileStream => {
                let header_flags = resolve_file_stream_header_flags(&bundle.header, policy);
                if header_flags & 0x40 == 0 {
                    return Err(BundleArtifactError::Unity(UnityAssetError::format(
                        "file-stream bundle writer requires DirectoryInfo (flags must include 0x40)",
                    )));
                }
                let unity_version = parse_bundle_unity_version(&bundle.header)?;
                reject_file_stream_encryption(&bundle.header, &bundle.blocks, &unity_version)?;
                let signature_byte = if bundle.header.signature == "UnityFS" {
                    None
                } else {
                    Some(bundle.header.file_stream_header_byte.ok_or_else(|| {
                        BundleArtifactError::MissingFileStreamHeaderByte {
                            signature: bundle.header.signature.clone(),
                        }
                    })?)
                };

                let total_data_len = total_entry_data_length(entries)?;
                if total_data_len == 0 {
                    return Err(BundleArtifactError::EmptyFileStreamData);
                }
                let block_info_compression =
                    FileStreamCompression::from_switch(header_flags & 0x3f, "blocks-info")?;
                let block_layout = match policy {
                    PackingPolicy::Preserve => {
                        FileStreamBlockLayout::preserve(total_data_len, &bundle.blocks)?
                    }
                    PackingPolicy::Uncompressed | PackingPolicy::Lz4 | PackingPolicy::Lzma => {
                        let compression_switch = match policy {
                            PackingPolicy::Uncompressed => 0,
                            PackingPolicy::Lz4 => 2,
                            PackingPolicy::Lzma => 1,
                            PackingPolicy::Preserve => unreachable!(),
                        };
                        let flags = bundle
                            .blocks
                            .first()
                            .map_or(0, |block| u32::from(block.flags) & !0x3f)
                            | compression_switch;
                        let compression = FileStreamCompression::from_switch(
                            compression_switch,
                            "file-stream block",
                        )?;
                        FileStreamBlockLayout::uniform(total_data_len, flags, compression)?
                    }
                };
                i32::try_from(block_layout.len()).map_err(|_| {
                    BundleArtifactError::BlockCountOverflow {
                        count: block_layout.len(),
                    }
                })?;
                let block_info_uncompressed_len =
                    file_stream_block_info_length(block_layout.len(), entries)?;
                u32_value(block_info_uncompressed_len, "uncompressed block-info size")?;
                let uses_alignment = uses_block_alignment(&bundle.header, &unity_version);
                let header_len = file_stream_header_length(&bundle.header, uses_alignment)?;

                Ok(Self::FileStream(FileStreamPlan {
                    header_flags,
                    block_info_compression,
                    block_layout,
                    block_info_uncompressed_len,
                    total_data_len,
                    header_len,
                    at_end: header_flags & 0x80 != 0,
                    signature_byte,
                }))
            }
            BundleLayoutKind::Legacy => {
                let legacy = bundle.header.legacy_web_raw.as_ref().ok_or(
                    BundleArtifactError::MissingLegacyHeader {
                        version: bundle.header.version,
                    },
                )?;
                if !(3..=5).contains(&bundle.header.version) {
                    return Err(BundleArtifactError::UnsupportedLegacyVersion {
                        version: bundle.header.version,
                    });
                }
                if legacy.level_count != 1 {
                    return Err(BundleArtifactError::UnsupportedLegacyLevelCount {
                        count: legacy.level_count,
                    });
                }
                let compress = match (bundle.header.signature.as_str(), policy) {
                    ("UnityRaw", PackingPolicy::Preserve | PackingPolicy::Uncompressed) => false,
                    ("UnityWeb", PackingPolicy::Preserve | PackingPolicy::Lzma) => true,
                    _ => {
                        return Err(BundleArtifactError::UnsupportedPackingPolicy {
                            signature: bundle.header.signature.clone(),
                            policy,
                        });
                    }
                };
                let integrity = if bundle.header.version >= 4 {
                    let hash = legacy
                        .hash
                        .as_deref()
                        .and_then(|hash| <[u8; 16]>::try_from(hash).ok())
                        .ok_or(BundleArtifactError::MissingLegacyIntegrity {
                            version: bundle.header.version,
                        })?;
                    let crc = legacy
                        .crc
                        .ok_or(BundleArtifactError::MissingLegacyIntegrity {
                            version: bundle.header.version,
                        })?;
                    Some(LegacyIntegrity {
                        hash,
                        crc,
                        content_digest: DigestV1::hash_bytes(bundle.data_checked()?),
                    })
                } else {
                    None
                };

                let directory_len = legacy_directory_length(entries)?;
                let member_bytes = total_entry_data_length(entries)?;
                let uncompressed_len = directory_len.checked_add(member_bytes).ok_or(
                    BundleArtifactError::ArithmeticOverflow {
                        resource: "legacy uncompressed content length",
                    },
                )?;
                u32_value(uncompressed_len, "legacy uncompressed content size")?;
                let header_len = legacy_header_length(&bundle.header)?;

                Ok(Self::Legacy(LegacyPlan {
                    compress,
                    levels_before_streaming: legacy.number_of_levels_to_download_before_streaming,
                    directory_len,
                    member_bytes,
                    uncompressed_len,
                    header_len,
                    integrity,
                }))
            }
        }
    }
}

impl BundleWriter {
    /// Build one exact, independently reparsed bundle artifact in `batch`.
    ///
    /// `entries` are consumed in the supplied order so duplicate directory entries remain
    /// representable.
    ///
    /// Legacy v4/v5 integrity fields are preserved when the canonical uncompressed content is
    /// unchanged. If it changes, the writer recomputes the content CRC and writes an all-zero
    /// Unity build hash as an explicit invalidation marker because that build-system identity
    /// cannot be reconstructed from archive bytes alone.
    pub fn prepare_artifact(
        batch: &mut ArtifactBatch<'_, '_>,
        bundle: &AssetBundle,
        entries: &[BundleArtifactEntry<'_>],
        policy: PackingPolicy,
    ) -> Result<ArtifactHandle, BundleArtifactError> {
        let plan = BundleEncodingPlan::new(bundle, entries, policy)?;
        for member in entries.iter().filter_map(|entry| entry.file_member()) {
            batch.artifact_len(member.artifact())?;
        }
        match plan {
            BundleEncodingPlan::FileStream(plan) => batch
                .run_fail_stop(|batch| prepare_file_stream(batch, &bundle.header, entries, plan)),
            BundleEncodingPlan::Legacy(plan) => {
                batch.run_fail_stop(|batch| prepare_legacy(batch, &bundle.header, entries, plan))
            }
        }
    }
}

fn validate_entries(
    layout: BundleLayoutKind,
    entries: &[BundleArtifactEntry<'_>],
) -> Result<(), BundleArtifactError> {
    i32::try_from(entries.len()).map_err(|_| BundleArtifactError::EntryCountOverflow {
        count: entries.len(),
    })?;
    let mut total = 0_u64;
    for (entry, input) in entries.iter().copied().enumerate() {
        if input.name().as_bytes().contains(&0) {
            return Err(BundleArtifactError::EmbeddedNul { entry });
        }
        match input {
            BundleArtifactEntry::File(member)
                if member.flags() & DirectoryNode::DIRECTORY_FLAG != 0 =>
            {
                return Err(BundleArtifactError::FileEntryHasDirectoryFlag {
                    entry,
                    flags: member.flags(),
                });
            }
            BundleArtifactEntry::File(member)
                if member.flags() & DirectoryNode::DELETED_FLAG != 0 =>
            {
                return Err(BundleArtifactError::FileEntryHasDeletedFlag {
                    entry,
                    flags: member.flags(),
                });
            }
            BundleArtifactEntry::EmptyDirectory { flags, .. }
                if flags & DirectoryNode::DIRECTORY_FLAG == 0 =>
            {
                return Err(BundleArtifactError::EmptyDirectoryMissingDirectoryFlag {
                    entry,
                    flags,
                });
            }
            BundleArtifactEntry::EmptyDirectory { flags, .. }
                if flags & DirectoryNode::DELETED_FLAG != 0 =>
            {
                return Err(BundleArtifactError::EmptyDirectoryHasDeletedFlag { entry, flags });
            }
            BundleArtifactEntry::Deleted { flags, .. }
                if flags & DirectoryNode::DELETED_FLAG == 0 =>
            {
                return Err(BundleArtifactError::DeletedEntryMissingDeletedFlag { entry, flags });
            }
            BundleArtifactEntry::EmptyDirectory { .. } if layout == BundleLayoutKind::Legacy => {
                return Err(BundleArtifactError::UnsupportedLegacyEmptyDirectory { entry });
            }
            BundleArtifactEntry::Deleted { .. } if layout == BundleLayoutKind::Legacy => {
                return Err(BundleArtifactError::UnsupportedLegacyDeletedEntry { entry });
            }
            BundleArtifactEntry::File(member)
                if layout == BundleLayoutKind::Legacy && member.flags() != 0 =>
            {
                return Err(BundleArtifactError::UnsupportedLegacyFileFlags {
                    entry,
                    flags: member.flags(),
                });
            }
            BundleArtifactEntry::File(_)
            | BundleArtifactEntry::EmptyDirectory { .. }
            | BundleArtifactEntry::Deleted { .. } => {}
        }
        let length = input.data_len();
        match layout {
            BundleLayoutKind::FileStream => {
                if length > i64::MAX as u64 {
                    return Err(BundleArtifactError::FileStreamLengthOverflow { entry, length });
                }
            }
            BundleLayoutKind::Legacy => {
                if length > u64::from(u32::MAX) {
                    return Err(BundleArtifactError::LegacyLengthOverflow { entry, length });
                }
            }
        }
        total = total
            .checked_add(length)
            .ok_or(BundleArtifactError::DataLengthOverflow { entry })?;
        if layout == BundleLayoutKind::FileStream && total > i64::MAX as u64 {
            return Err(BundleArtifactError::DataLengthOverflow { entry });
        }
        if layout == BundleLayoutKind::Legacy && total > u64::from(u32::MAX) {
            return Err(BundleArtifactError::DataLengthOverflow { entry });
        }
    }
    Ok(())
}

fn prepare_file_stream(
    batch: &mut ArtifactBatch<'_, '_>,
    header: &BundleHeader,
    entries: &[BundleArtifactEntry<'_>],
    plan: FileStreamPlan<'_>,
) -> Result<ArtifactHandle, BundleArtifactError> {
    let mut data_chunks = Vec::new();
    let mut blocks = Vec::new();
    let mut data_len = 0_u64;
    blocks
        .try_reserve_exact(plan.block_layout.len())
        .map_err(|source| BundleArtifactError::Allocation {
            resource: "file-stream blocks",
            requested: plan.block_layout.len(),
            source,
        })?;
    if plan.block_layout.compressed_len() != 0 {
        data_chunks
            .try_reserve_exact(plan.block_layout.compressed_len())
            .map_err(|source| BundleArtifactError::Allocation {
                resource: "file-stream data chunks",
                requested: plan.block_layout.compressed_len(),
                source,
            })?;
    }

    for planned_block in plan.block_layout.iter() {
        let range = planned_block.range;
        let uncompressed = range.end - range.start;
        if planned_block.compression == FileStreamCompression::None {
            data_len = data_len.checked_add(uncompressed).ok_or(
                BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream data length",
                },
            )?;
            blocks.push(FileStreamBlock {
                uncompressed_size: u32_value(uncompressed, "block uncompressed size")?,
                compressed_size: u32_value(uncompressed, "block compressed size")?,
                flags: u16_value(planned_block.flags, "block flags")?,
            });
            continue;
        }

        let block_flags = planned_block.flags;
        let block_compression = planned_block.compression;
        let encoded_flags = u16_value(block_flags, "block flags")?;
        let chunk = batch.derive_generated_chunk(|derived| {
            let mut raw = derived.generated_chunk_writer()?;
            visit_file_range(entries, range.clone(), |member, local_start, length| {
                let mut reader = derived.dependency_reader(member.artifact())?;
                reader.seek(SeekFrom::Start(local_start))?;
                let mut limited = reader.take(length);
                let copied = io::copy(&mut limited, &mut raw)?;
                if copied != length {
                    return Err(ArtifactBuildError::DependencyIo(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "bundle member {} supplied {} bytes, expected {}",
                            member.name(),
                            copied,
                            length
                        ),
                    )));
                }
                Ok(())
            })?;

            let raw_len = raw.len();
            let compressed = match block_compression {
                FileStreamCompression::None => {
                    return Err(ArtifactBuildError::InternalInvariant {
                        message: "uncompressed block entered generated compression path",
                    });
                }
                FileStreamCompression::Lzma => {
                    let mut encoded = derived.generated_chunk_writer()?;
                    {
                        let mut sink = SkipBytesWriter {
                            writer: &mut encoded,
                            position: 0,
                            skip: 5..13,
                        };
                        let mut input = Cursor::new(raw.as_slice());
                        compress_lzma(&mut input, &mut sink, None)?;
                    }
                    drop(raw);
                    encoded
                }
                FileStreamCompression::Lz4 => {
                    let max_len = lz4_flex::block::get_maximum_output_size(raw_len);
                    let mut encoded = derived.generated_chunk_writer()?;
                    encoded.resize_zero(max_len)?;
                    let encoded_len =
                        lz4_flex::block::compress_into(raw.as_slice(), encoded.as_mut_slice()?)
                            .map_err(|error| {
                                io::Error::other(format!("LZ4 block compression: {error}"))
                            })?;
                    encoded.resize_zero(encoded_len)?;
                    drop(raw);
                    encoded
                }
            };
            derived.finish_generated_chunk(compressed)
        })?;
        let compressed_len = chunk.len();
        blocks.push(FileStreamBlock {
            uncompressed_size: u32_value(uncompressed, "block uncompressed size")?,
            compressed_size: u32_value(compressed_len, "block compressed size")?,
            flags: encoded_flags,
        });
        data_len =
            data_len
                .checked_add(chunk.len())
                .ok_or(BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream compressed data length",
                })?;
        data_chunks.push(chunk);
    }

    // Block information is itself an exact generated chunk. Empty file artifacts are attached
    // here so a zero-byte file entry remains reachable without adding wire bytes. Empty
    // directories deliberately have no dependency.
    let block_info_uncompressed_len = plan.block_info_uncompressed_len;
    let block_info = batch.derive_generated_chunk(|derived| {
        for member in entries
            .iter()
            .filter_map(|entry| entry.file_member())
            .filter(|member| member.length() == 0)
        {
            derived.record_empty_dependency(member.artifact())?;
        }

        let mut raw = derived.generated_chunk_writer()?;
        raw.write_all(&[0_u8; 16])?;
        raw.write_all(&i32_value(blocks.len(), "block count")?.to_be_bytes())?;
        for block in &blocks {
            raw.write_all(&block.uncompressed_size.to_be_bytes())?;
            raw.write_all(&block.compressed_size.to_be_bytes())?;
            raw.write_all(&block.flags.to_be_bytes())?;
        }
        raw.write_all(&i32_value(entries.len(), "directory count")?.to_be_bytes())?;
        let mut offset = 0_i64;
        for entry in entries.iter().copied() {
            raw.write_all(&offset.to_be_bytes())?;
            let length = i64_value(entry.data_len(), "directory length")?;
            raw.write_all(&length.to_be_bytes())?;
            offset = offset
                .checked_add(length)
                .ok_or(ArtifactBuildError::InternalInvariant {
                    message: "file-stream directory offset overflow",
                })?;
            raw.write_all(&entry.flags().to_be_bytes())?;
            write_cstring(&mut raw, entry.name())?;
        }
        debug_assert_eq!(
            u64::try_from(raw.len()).ok(),
            Some(block_info_uncompressed_len)
        );

        let encoded = match plan.block_info_compression {
            FileStreamCompression::None => raw,
            FileStreamCompression::Lzma => {
                let mut encoded = derived.generated_chunk_writer()?;
                {
                    let mut sink = SkipBytesWriter {
                        writer: &mut encoded,
                        position: 0,
                        skip: 5..13,
                    };
                    let mut input = Cursor::new(raw.as_slice());
                    compress_lzma(&mut input, &mut sink, None)?;
                }
                drop(raw);
                encoded
            }
            FileStreamCompression::Lz4 => {
                let max_len = lz4_flex::block::get_maximum_output_size(raw.len());
                let mut encoded = derived.generated_chunk_writer()?;
                encoded.resize_zero(max_len)?;
                let encoded_len =
                    lz4_flex::block::compress_into(raw.as_slice(), encoded.as_mut_slice()?)
                        .map_err(|error| {
                            io::Error::other(format!("LZ4 block-info compression: {error}"))
                        })?;
                encoded.resize_zero(encoded_len)?;
                drop(raw);
                encoded
            }
        };
        derived.finish_generated_chunk(encoded)
    })?;
    let block_info_compressed_len = block_info.len();

    let pad_position = if plan.at_end {
        plan.header_len
    } else {
        plan.header_len
            .checked_add(block_info_compressed_len)
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "file-stream padding position",
            })?
    };
    let padding_len = if plan.header_flags & 0x200 != 0 {
        align_up(pad_position, 16)? - pad_position
    } else {
        0
    };
    debug_assert_eq!(
        blocks
            .iter()
            .map(|block| u64::from(block.uncompressed_size))
            .sum::<u64>(),
        plan.total_data_len
    );
    let total_len = plan
        .header_len
        .checked_add(padding_len)
        .and_then(|value| value.checked_add(block_info_compressed_len))
        .and_then(|value| value.checked_add(data_len))
        .ok_or(BundleArtifactError::ArithmeticOverflow {
            resource: "file-stream bundle length",
        })?;
    let header_chunk = batch.derive_generated_chunk(|derived| {
        let mut writer = derived.generated_chunk_writer()?;
        write_cstring(&mut writer, &header.signature)?;
        writer.write_all(&header.version.to_be_bytes())?;
        write_cstring(&mut writer, &header.unity_version)?;
        write_cstring(&mut writer, &header.unity_revision)?;
        writer.write_all(&i64_value(total_len, "file-stream bundle size")?.to_be_bytes())?;
        writer.write_all(
            &u32_value(block_info_compressed_len, "compressed block-info size")?.to_be_bytes(),
        )?;
        writer.write_all(
            &u32_value(block_info_uncompressed_len, "uncompressed block-info size")?.to_be_bytes(),
        )?;
        writer.write_all(&plan.header_flags.to_be_bytes())?;
        if let Some(byte) = plan.signature_byte {
            writer.write_all(&[byte])?;
        }
        writer.resize_zero(usize_value(plan.header_len, "file-stream header length")?)?;
        derived.finish_generated_chunk(writer)
    })?;
    let padding = if padding_len == 0 {
        None
    } else {
        Some(batch.derive_generated_chunk(|derived| {
            let mut writer = derived.generated_chunk_writer()?;
            writer.resize_zero(usize_value(padding_len, "file-stream padding")?)?;
            derived.finish_generated_chunk(writer)
        })?)
    };

    batch
        .prepare_asset_bundle(total_len, |encoder| {
            encoder.push_derived_generated_chunk(header_chunk)?;
            let mut block_info = Some(block_info);
            if !plan.at_end {
                encoder.push_derived_generated_chunk(block_info.take().ok_or(
                    ArtifactBuildError::InternalInvariant {
                        message: "file-stream block info was already emitted",
                    },
                )?)?;
            }
            if let Some(padding) = padding {
                encoder.push_derived_generated_chunk(padding)?;
            }

            let mut compressed_chunks = data_chunks.into_iter();
            for planned_block in plan.block_layout.iter() {
                if planned_block.compression == FileStreamCompression::None {
                    visit_file_range(
                        entries,
                        planned_block.range,
                        |member, local_start, length| {
                            let local_end = local_start.checked_add(length).ok_or(
                                ArtifactBuildError::InternalInvariant {
                                    message: "bundle dependency range overflow",
                                },
                            )?;
                            encoder
                                .append_dependency_range(member.artifact(), local_start..local_end)
                        },
                    )?;
                } else {
                    let chunk =
                        compressed_chunks
                            .next()
                            .ok_or(ArtifactBuildError::InternalInvariant {
                                message: "missing compressed bundle block chunk",
                            })?;
                    encoder.push_derived_generated_chunk(chunk)?;
                }
            }
            if compressed_chunks.next().is_some() {
                return Err(ArtifactBuildError::InternalInvariant {
                    message: "unused compressed bundle block chunk",
                });
            }
            if plan.at_end {
                encoder.push_derived_generated_chunk(block_info.take().ok_or(
                    ArtifactBuildError::InternalInvariant {
                        message: "file-stream block info was already emitted",
                    },
                )?)?;
            }
            Ok(())
        })
        .map_err(Into::into)
}

fn prepare_legacy(
    batch: &mut ArtifactBatch<'_, '_>,
    header: &BundleHeader,
    entries: &[BundleArtifactEntry<'_>],
    plan: LegacyPlan,
) -> Result<ArtifactHandle, BundleArtifactError> {
    let (content, compressed_len) = if plan.compress {
        let content = batch.derive_generated_chunk(|derived| {
            for member in entries
                .iter()
                .filter_map(|entry| entry.file_member())
                .filter(|member| member.length() == 0)
            {
                derived.record_empty_dependency(member.artifact())?;
            }
            let mut raw = derived.generated_chunk_writer()?;
            write_legacy_directory(&mut raw, entries, plan.directory_len)?;
            visit_file_range(
                entries,
                0..plan.member_bytes,
                |member, local_start, length| {
                    let mut reader = derived.dependency_reader(member.artifact())?;
                    reader.seek(SeekFrom::Start(local_start))?;
                    let mut limited = reader.take(length);
                    let copied = io::copy(&mut limited, &mut raw)?;
                    if copied != length {
                        return Err(ArtifactBuildError::DependencyIo(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "legacy bundle member {} supplied {} bytes, expected {}",
                                member.name(),
                                copied,
                                length
                            ),
                        )));
                    }
                    Ok(())
                },
            )?;
            let mut encoded = derived.generated_chunk_writer()?;
            let mut input = Cursor::new(raw.as_slice());
            compress_lzma(&mut input, &mut encoded, Some(plan.uncompressed_len))?;
            drop(raw);
            derived.finish_generated_chunk(encoded)
        })?;
        let compressed_len = content.len();
        (Some(content), compressed_len)
    } else {
        (None, plan.uncompressed_len)
    };

    let directory = if plan.compress {
        None
    } else {
        Some(batch.derive_generated_chunk(|derived| {
            for member in entries
                .iter()
                .filter_map(|entry| entry.file_member())
                .filter(|member| member.length() == 0)
            {
                derived.record_empty_dependency(member.artifact())?;
            }
            let mut writer = derived.generated_chunk_writer()?;
            write_legacy_directory(&mut writer, entries, plan.directory_len)?;
            derived.finish_generated_chunk(writer)
        })?)
    };

    let total_len = plan.header_len.checked_add(compressed_len).ok_or(
        BundleArtifactError::ArithmeticOverflow {
            resource: "legacy bundle length",
        },
    )?;
    u32_value(total_len, "legacy complete file size")?;
    let header_chunk = batch.derive_generated_chunk(|derived| {
        let encoded_integrity = if let Some(integrity) = plan.integrity {
            let mut checksum = LegacyIntegrityWriter::new(plan.uncompressed_len);
            write_legacy_directory(&mut checksum, entries, plan.directory_len)?;
            for member in entries.iter().filter_map(|entry| entry.file_member()) {
                if member.length() == 0 {
                    derived.record_empty_dependency(member.artifact())?;
                    continue;
                }
                let mut reader = derived.dependency_reader(member.artifact())?;
                let mut limited = reader.by_ref().take(member.length());
                let copied = io::copy(&mut limited, &mut checksum)?;
                if copied != member.length() {
                    return Err(ArtifactBuildError::DependencyIo(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "legacy bundle member {} supplied {} bytes, expected {} while computing integrity",
                            member.name(),
                            copied,
                            member.length()
                        ),
                    )));
                }
            }
            let (content_digest, computed_crc) = checksum.finish()?;
            if content_digest == integrity.content_digest {
                Some((integrity.hash, integrity.crc))
            } else {
                Some(([0_u8; 16], computed_crc))
            }
        } else {
            None
        };

        let mut writer = derived.generated_chunk_writer()?;
        write_cstring(&mut writer, &header.signature)?;
        writer.write_all(&header.version.to_be_bytes())?;
        write_cstring(&mut writer, &header.unity_version)?;
        write_cstring(&mut writer, &header.unity_revision)?;
        if let Some((hash, crc)) = encoded_integrity {
            writer.write_all(&hash)?;
            writer.write_all(&crc.to_be_bytes())?;
        }
        let total_u32 = u32_value(total_len, "legacy complete file size")?;
        writer.write_all(&total_u32.to_be_bytes())?;
        writer.write_all(&u32_value(plan.header_len, "legacy header size")?.to_be_bytes())?;
        writer.write_all(&plan.levels_before_streaming.to_be_bytes())?;
        writer.write_all(&1_i32.to_be_bytes())?;
        writer.write_all(&u32_value(compressed_len, "legacy compressed size")?.to_be_bytes())?;
        writer
            .write_all(&u32_value(plan.uncompressed_len, "legacy uncompressed size")?.to_be_bytes())?;
        writer.write_all(&total_u32.to_be_bytes())?;
        writer.write_all(&u32_value(plan.directory_len, "legacy directory size")?.to_be_bytes())?;
        writer.resize_zero(usize_value(plan.header_len, "legacy header length")?)?;
        derived.finish_generated_chunk(writer)
    })?;

    batch
        .prepare_asset_bundle(total_len, |encoder| {
            encoder.push_derived_generated_chunk(header_chunk)?;
            if let Some(content) = content {
                encoder.push_derived_generated_chunk(content)?;
            } else {
                encoder.push_derived_generated_chunk(directory.ok_or(
                    ArtifactBuildError::InternalInvariant {
                        message: "UnityRaw directory chunk was not prepared",
                    },
                )?)?;
                for member in entries
                    .iter()
                    .filter_map(|entry| entry.file_member())
                    .filter(|member| member.length() != 0)
                {
                    encoder.append_dependency(member.artifact())?;
                }
            }
            Ok(())
        })
        .map_err(Into::into)
}

fn resolve_file_stream_header_flags(header: &BundleHeader, policy: PackingPolicy) -> u32 {
    let compression_switch = match policy {
        PackingPolicy::Preserve => return header.flags,
        PackingPolicy::Uncompressed => 0,
        PackingPolicy::Lz4 => 2,
        PackingPolicy::Lzma => 1,
    };
    (header.flags & !0x3f) | compression_switch
}

fn reject_file_stream_encryption(
    header: &BundleHeader,
    blocks: &[CompressionBlock],
    unity_version: &UnityVersion,
) -> Result<(), BundleArtifactError> {
    let uses_old_flags = uses_old_archive_flags(unity_version);
    let encryption_mask = if uses_old_flags { 0x200 } else { 0x1400 };
    let encrypted_header = header.flags & encryption_mask;
    let encrypted_block = blocks
        .iter()
        .any(|block| u32::from(block.flags) & encryption_mask != 0);
    if encrypted_header != 0 || encrypted_block {
        return Err(BundleArtifactError::Unity(UnityAssetError::format(
            format!(
                "file-stream encryption flags cannot be preserved while re-encoding: header_flags={:#x}",
                header.flags
            ),
        )));
    }
    Ok(())
}

fn parse_bundle_unity_version(header: &BundleHeader) -> Result<UnityVersion, BundleArtifactError> {
    UnityVersion::parse_version(&header.unity_revision)
        .or_else(|_| UnityVersion::parse_version(&header.unity_version))
        .map_err(Into::into)
}

fn uses_old_archive_flags(version: &UnityVersion) -> bool {
    let (major, minor, build) = (version.major, version.minor, version.build);
    if major < 2020 {
        true
    } else if major == 2020 {
        minor < 3 || (minor == 3 && build < 34)
    } else if major == 2021 {
        minor < 3 || (minor == 3 && build < 2)
    } else if major == 2022 {
        minor < 1 || (minor == 1 && build < 1)
    } else {
        false
    }
}

fn uses_block_alignment(header: &BundleHeader, unity_version: &UnityVersion) -> bool {
    if header.version >= 7 {
        return true;
    }
    unity_version.major > 2019 || (unity_version.major == 2019 && unity_version.minor >= 4)
}

fn visit_file_range(
    entries: &[BundleArtifactEntry<'_>],
    range: Range<u64>,
    mut visit: impl FnMut(&BundleArtifactMember<'_>, u64, u64) -> Result<(), ArtifactBuildError>,
) -> Result<(), ArtifactBuildError> {
    let mut member_start = 0_u64;
    for member in entries.iter().filter_map(|entry| entry.file_member()) {
        let member_end = member_start.checked_add(member.length()).ok_or(
            ArtifactBuildError::InternalInvariant {
                message: "bundle member range overflow",
            },
        )?;
        let overlap_start = member_start.max(range.start);
        let overlap_end = member_end.min(range.end);
        if overlap_start < overlap_end {
            visit(
                member,
                overlap_start - member_start,
                overlap_end - overlap_start,
            )?;
        }
        member_start = member_end;
        if member_start >= range.end {
            break;
        }
    }
    Ok(())
}

fn total_entry_data_length(
    entries: &[BundleArtifactEntry<'_>],
) -> Result<u64, BundleArtifactError> {
    entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.data_len())
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "bundle entry data length",
            })
    })
}

fn file_stream_block_info_length(
    block_count: usize,
    entries: &[BundleArtifactEntry<'_>],
) -> Result<u64, BundleArtifactError> {
    let block_count =
        u64::try_from(block_count).map_err(|_| BundleArtifactError::ArithmeticOverflow {
            resource: "file-stream block-info length",
        })?;
    let blocks = block_count
        .checked_mul(10)
        .ok_or(BundleArtifactError::ArithmeticOverflow {
            resource: "file-stream block-info length",
        })?;
    entries.iter().try_fold(
        16_u64
            .checked_add(4)
            .and_then(|length| length.checked_add(blocks))
            .and_then(|length| length.checked_add(4))
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "file-stream block-info length",
            })?,
        |length, entry| {
            let name = u64::try_from(entry.name().len()).map_err(|_| {
                BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream block-info length",
                }
            })?;
            length
                .checked_add(8 + 8 + 4)
                .and_then(|length| length.checked_add(name))
                .and_then(|length| length.checked_add(1))
                .ok_or(BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream block-info length",
                })
        },
    )
}

fn file_stream_header_length(
    header: &BundleHeader,
    uses_alignment: bool,
) -> Result<u64, BundleArtifactError> {
    let variable = header
        .signature
        .len()
        .checked_add(1 + 4)
        .and_then(|value| value.checked_add(header.unity_version.len() + 1))
        .and_then(|value| value.checked_add(header.unity_revision.len() + 1))
        .and_then(|value| value.checked_add(8 + 4 + 4 + 4))
        .and_then(|value| value.checked_add(usize::from(header.signature != "UnityFS")))
        .ok_or(BundleArtifactError::ArithmeticOverflow {
            resource: "file-stream header length",
        })?;
    let length = u64::try_from(variable).map_err(|_| BundleArtifactError::ArithmeticOverflow {
        resource: "file-stream header length",
    })?;
    if uses_alignment {
        align_up(length, 16)
    } else {
        Ok(length)
    }
}

fn legacy_header_length(header: &BundleHeader) -> Result<u64, BundleArtifactError> {
    let common = header
        .signature
        .len()
        .checked_add(1 + 4)
        .and_then(|value| value.checked_add(header.unity_version.len() + 1))
        .and_then(|value| value.checked_add(header.unity_revision.len() + 1))
        .ok_or(BundleArtifactError::ArithmeticOverflow {
            resource: "legacy header length",
        })?;
    let mut length =
        u64::try_from(common).map_err(|_| BundleArtifactError::ArithmeticOverflow {
            resource: "legacy header length",
        })?;
    if header.version >= 4 {
        length = length
            .checked_add(20)
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "legacy header length",
            })?;
    }
    length = length
        .checked_add(24)
        .and_then(|value| value.checked_add(u64::from(header.version >= 2) * 4))
        .and_then(|value| value.checked_add(u64::from(header.version >= 3) * 4))
        .ok_or(BundleArtifactError::ArithmeticOverflow {
            resource: "legacy header length",
        })?;
    align_up(length, 4)
}

fn legacy_directory_length(
    entries: &[BundleArtifactEntry<'_>],
) -> Result<u64, BundleArtifactError> {
    let length = entries.iter().try_fold(4_u64, |length, entry| {
        let name = u64::try_from(entry.name().len()).map_err(|_| {
            BundleArtifactError::ArithmeticOverflow {
                resource: "legacy directory length",
            }
        })?;
        length
            .checked_add(name)
            .and_then(|value| value.checked_add(1 + 4 + 4))
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "legacy directory length",
            })
    })?;
    let length = align_up(length, 4)?;
    u32_value(length, "legacy directory length")?;
    Ok(length)
}

fn write_legacy_directory(
    writer: &mut impl Write,
    entries: &[BundleArtifactEntry<'_>],
    directory_len: u64,
) -> io::Result<()> {
    writer.write_all(&i32_value(entries.len(), "legacy directory count")?.to_be_bytes())?;
    let mut offset = directory_len;
    for entry in entries.iter().copied() {
        write_cstring(writer, entry.name())?;
        writer.write_all(&u32_value(offset, "legacy member offset")?.to_be_bytes())?;
        writer.write_all(&u32_value(entry.data_len(), "legacy member length")?.to_be_bytes())?;
        offset = offset
            .checked_add(entry.data_len())
            .ok_or_else(|| io::Error::other("legacy member content offset overflow"))?;
    }
    let written = entries.iter().try_fold(4_u64, |length, entry| {
        length
            .checked_add(
                u64::try_from(entry.name().len())
                    .map_err(|_| io::Error::other("legacy member name length does not fit u64"))?,
            )
            .and_then(|value| value.checked_add(1 + 4 + 4))
            .ok_or_else(|| io::Error::other("legacy directory length overflow"))
    })?;
    write_zeroes(writer, directory_len - written)
}

fn write_cstring(writer: &mut impl Write, value: &str) -> io::Result<()> {
    writer.write_all(value.as_bytes())?;
    writer.write_all(&[0])
}

fn write_zeroes(writer: &mut impl Write, mut count: u64) -> io::Result<()> {
    const ZEROES: [u8; 64] = [0; 64];
    while count != 0 {
        let chunk = usize::try_from(count.min(ZEROES.len() as u64))
            .map_err(|_| io::Error::other("zero padding length does not fit usize"))?;
        writer.write_all(&ZEROES[..chunk])?;
        count -= chunk as u64;
    }
    Ok(())
}

fn compress_lzma(
    input: &mut impl BufRead,
    output: &mut impl Write,
    unpacked_size: Option<u64>,
) -> io::Result<()> {
    let options = lzma_rs::compress::Options {
        unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(unpacked_size),
    };
    lzma_rs::lzma_compress_with_options(input, output, &options)
}

struct LegacyIntegrityWriter {
    digest: DigestV1Builder,
    crc: Crc,
}

impl LegacyIntegrityWriter {
    fn new(content_len: u64) -> Self {
        Self {
            digest: DigestV1Builder::new(content_len),
            crc: Crc::new(),
        }
    }

    fn finish(self) -> io::Result<(DigestV1, u32)> {
        let digest = self.digest.finalize().map_err(io::Error::other)?;
        Ok((digest, self.crc.sum()))
    }
}

impl Write for LegacyIntegrityWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.digest.update(bytes).map_err(io::Error::other)?;
        self.crc.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SkipBytesWriter<'writer, W> {
    writer: &'writer mut W,
    position: u64,
    skip: Range<u64>,
}

impl<W: Write> Write for SkipBytesWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = self.position;
        let end = start
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| io::Error::other("compressed write length does not fit u64"))?,
            )
            .ok_or_else(|| io::Error::other("compressed write position overflow"))?;
        if start < self.skip.start {
            let before_end = end.min(self.skip.start);
            let length = usize::try_from(before_end - start)
                .map_err(|_| io::Error::other("compressed prefix length does not fit usize"))?;
            self.writer.write_all(&bytes[..length])?;
        }
        if end > self.skip.end {
            let after_start = start.max(self.skip.end);
            let offset = usize::try_from(after_start - start)
                .map_err(|_| io::Error::other("compressed suffix offset does not fit usize"))?;
            self.writer.write_all(&bytes[offset..])?;
        }
        self.position = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

fn align_up(value: u64, alignment: u64) -> Result<u64, BundleArtifactError> {
    let mask = alignment - 1;
    value.checked_add(mask).map(|value| value & !mask).ok_or(
        BundleArtifactError::ArithmeticOverflow {
            resource: "bundle alignment",
        },
    )
}

fn u32_value(value: u64, resource: &'static str) -> io::Result<u32> {
    u32::try_from(value)
        .map_err(|_| io::Error::other(BundleArtifactError::U32Overflow { value, resource }))
}

fn i64_value(value: u64, resource: &'static str) -> io::Result<i64> {
    i64::try_from(value)
        .map_err(|_| io::Error::other(BundleArtifactError::I64Overflow { value, resource }))
}

fn i32_value(value: usize, resource: &'static str) -> io::Result<i32> {
    i32::try_from(value)
        .map_err(|_| io::Error::other(format!("{resource} {value} does not fit i32")))
}

fn u16_value(value: u32, resource: &'static str) -> io::Result<u16> {
    u16::try_from(value)
        .map_err(|_| io::Error::other(format!("{resource} {value} does not fit u16")))
}

fn usize_value(value: u64, resource: &'static str) -> io::Result<usize> {
    usize::try_from(value)
        .map_err(|_| io::Error::other(format!("{resource} {value} does not fit usize")))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use unity_asset_binary::bundle::header::LegacyWebRawHeader;
    use unity_asset_binary::bundle::{BundleHeader, BundleParser};
    use unity_asset_binary::compression::CompressionBlock;
    use unity_asset_core::{
        AssetLoadBudget, DigestV1, SourceId, SourceKind, VerifiedSourceImage, WorkspaceId,
    };

    use super::*;
    use crate::artifact::{
        ArtifactBatchDeclaration, ArtifactBudget, ArtifactBudgetError, ArtifactLimits,
        ArtifactPayload, LogicalArtifactName, StreamedResourceExtentInspection,
    };

    fn source_payload(local: u128, bytes: &[u8]) -> ArtifactPayload {
        let source = SourceId::new(
            WorkspaceId::from_u128(91).expect("test workspace id"),
            SourceKind::StreamedResource,
            local,
        )
        .expect("test source id");
        let image = VerifiedSourceImage::verify(SourceKind::StreamedResource, Arc::from(bytes));
        ArtifactPayload::source_backed(source, image).expect("source payload")
    }

    fn prepare_payload(
        batch: &mut ArtifactBatch<'_, '_>,
        payload: &ArtifactPayload,
    ) -> ArtifactHandle {
        let digest = payload
            .digest()
            .unwrap_or_else(|| DigestV1::hash_bytes(payload.bytes()));
        let mut layout = batch.streamed_resource_layout_builder().unwrap();
        layout
            .push(StreamedResourceExtentInspection::new(
                digest,
                0,
                payload.len(),
                1,
            ))
            .unwrap();
        batch
            .prepare_streamed_resource(layout, |encoder| encoder.push_payload_full(payload))
            .unwrap()
    }

    fn file_stream_bundle(signature: &str, version: u32, flags: u32) -> AssetBundle {
        let header = BundleHeader {
            signature: signature.to_string(),
            version,
            unity_version: "2021.3.0f1".to_string(),
            unity_revision: "2021.3.0f1".to_string(),
            size: 1,
            compressed_blocks_info_size: 1,
            uncompressed_blocks_info_size: 1,
            flags,
            actual_header_size: 0,
            legacy_web_raw: None,
            file_stream_header_byte: (signature != "UnityFS").then_some(0x5a),
        };
        let mut bundle = AssetBundle::new(header, Vec::new());
        bundle.blocks.push(CompressionBlock::new(0x0002_0000, 1, 2));
        bundle
    }

    fn legacy_bundle(signature: &str) -> AssetBundle {
        let header = BundleHeader {
            signature: signature.to_string(),
            version: 3,
            unity_version: "3.5.0f5".to_string(),
            unity_revision: "3.5.0f5".to_string(),
            size: 1,
            legacy_web_raw: Some(LegacyWebRawHeader {
                minimum_streamed_bytes: 1,
                header_size: 1,
                number_of_levels_to_download_before_streaming: 1,
                level_count: 1,
                compressed_size: 1,
                uncompressed_size: 1,
                complete_file_size: Some(1),
                file_info_header_size: Some(4),
                ..LegacyWebRawHeader::default()
            }),
            ..BundleHeader::default()
        };
        AssetBundle::new(header, Vec::new())
    }

    fn canonical_legacy_content(name: &str, bytes: &[u8]) -> Vec<u8> {
        let directory_len =
            align_up(u64::try_from(4 + name.len() + 1 + 4 + 4).unwrap(), 4).unwrap();
        let mut content = Vec::new();
        content.extend_from_slice(&1_i32.to_be_bytes());
        content.extend_from_slice(name.as_bytes());
        content.push(0);
        content.extend_from_slice(&u32::try_from(directory_len).unwrap().to_be_bytes());
        content.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
        content.resize(usize::try_from(directory_len).unwrap(), 0);
        content.extend_from_slice(bytes);
        content
    }

    fn legacy_bundle_with_version(
        signature: &str,
        version: u32,
        original_name: &str,
        original_bytes: &[u8],
    ) -> AssetBundle {
        let mut bundle = legacy_bundle(signature);
        bundle.header.version = version;
        if version >= 4 {
            let legacy = bundle
                .header
                .legacy_web_raw
                .as_mut()
                .expect("legacy fixture header");
            legacy.hash = Some(vec![0x5a; 16]);
            legacy.crc = Some(0x1234_5678);
            legacy.number_of_levels_to_download_before_streaming = version;
        }
        AssetBundle::new(
            bundle.header,
            canonical_legacy_content(original_name, original_bytes),
        )
    }

    fn build_uncompressed_directory_bundle(
        directory_name: &str,
        max_generated_chunk_bytes: u64,
    ) -> Result<Vec<u8>, BundleArtifactError> {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc0);
        let payload = source_payload(40, b"x");
        let limits =
            ArtifactLimits::default().with_max_generated_chunk_bytes(max_generated_chunk_bytes);
        let mut artifact_budget = ArtifactBudget::new(limits).expect("valid artifact limits");
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)?;
        let output = declaration.declare_output(
            LogicalArtifactName::new("bundle").expect("valid logical artifact name"),
        )?;
        let mut batch = declaration.seal_output_names()?;
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [
            BundleArtifactEntry::file(&batch, "f", 0, payload_handle)?,
            BundleArtifactEntry::EmptyDirectory {
                name: directory_name,
                flags: DirectoryNode::DIRECTORY_FLAG,
            },
        ];
        let root = match BundleWriter::prepare_artifact(
            &mut batch,
            &bundle,
            &entries,
            PackingPolicy::Uncompressed,
        ) {
            Ok(root) => root,
            Err(error) => {
                assert!(matches!(
                    batch.finish(),
                    Err(ArtifactBuildError::PoisonedBatch)
                ));
                return Err(error);
            }
        };
        batch.bind_output(output, root)?;
        let set = batch.finish()?;
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .expect("declared output")
            .artifact()
            .stream_verified_to(&mut bytes)
            .expect("verified artifact stream");
        Ok(bytes)
    }

    #[derive(Debug)]
    struct Lz4ScratchFailure {
        error: BundleArtifactError,
        poison: ArtifactBuildError,
    }

    fn build_lz4_bundle_with_scratch_limit(
        max_scratch_bytes: u64,
    ) -> Result<(Vec<u8>, u64), Box<Lz4ScratchFailure>> {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        let payload = source_payload(41, &[b'z'; 0x0002_0000]);
        let limits = ArtifactLimits::default().with_max_scratch_bytes(max_scratch_bytes);
        let mut artifact_budget = ArtifactBudget::new(limits).expect("valid artifact limits");
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)
                .expect("declaration metadata fits compression test limit");
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .expect("output metadata fits compression test limit");
        let mut batch = declaration
            .seal_output_names()
            .expect("sealed namespace fits compression test limit");
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
        let root =
            match BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Lz4)
            {
                Ok(root) => root,
                Err(error) => {
                    let poison = batch
                        .finish()
                        .expect_err("compression failure poisons the artifact batch");
                    return Err(Box::new(Lz4ScratchFailure { error, poison }));
                }
            };
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();
        let peak = artifact_budget.usage().peak_scratch_bytes();
        Ok((bytes, peak))
    }

    #[test]
    fn file_stream_artifact_preserves_duplicate_member_order_and_dependencies() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        let first = source_payload(1, &[b'a'; 100_000]);
        let second = source_payload(2, &[b'b'; 90_000]);

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let first_handle = prepare_payload(&mut batch, &first);
        let second_handle = prepare_payload(&mut batch, &second);
        let entries = [
            BundleArtifactEntry::file(&batch, "duplicate.assets", 4, first_handle).unwrap(),
            BundleArtifactEntry::file(&batch, "duplicate.assets", 4, second_handle).unwrap(),
        ];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let prepared = set.outputs().next().unwrap();
        let mut bytes = Vec::new();
        prepared.artifact().stream_verified_to(&mut bytes).unwrap();

        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.header.signature, "UnityFS");
        assert_eq!(reparsed.nodes.len(), 2);
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            first.bytes()
        );
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[1]).unwrap(),
            second.bytes()
        );
        assert_eq!(set.source_dependencies().len(), 2);
    }

    #[test]
    fn uncompressed_file_stream_reuses_member_artifact_without_generated_copy() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        let payload_bytes = vec![b'u'; 1024 * 1024];
        let payload = source_payload(42, &payload_bytes);
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
        let root = BundleWriter::prepare_artifact(
            &mut batch,
            &bundle,
            &entries,
            PackingPolicy::Uncompressed,
        )
        .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();

        let logical_source_bytes = payload.len() * 2;
        assert_eq!(set.source_dependencies().len(), 1);
        assert_eq!(
            set.source_dependencies()[0].referenced_bytes(),
            logical_source_bytes
        );
        assert_eq!(
            set.footprint().referenced_source_bytes(),
            logical_source_bytes
        );
        assert_eq!(
            set.footprint().pinned_source_bytes(),
            payload.backing().allocation_bytes().unwrap()
        );
        assert_eq!(set.build_counters().source_ranges(), 1);
        assert!(set.footprint().generated_bytes() < payload.len());

        let mut bytes = Vec::new();
        let root = set.outputs().next().unwrap().artifact();
        assert_eq!(root.build_counters().source_ranges(), 0);
        root.stream_verified_to(&mut bytes).unwrap();
        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            payload.bytes()
        );
    }

    #[test]
    fn file_stream_roundtrips_empty_directories_in_mixed_entry_order() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        let first = source_payload(10, b"abc");
        let empty_file = source_payload(11, b"");
        let second = source_payload(12, b"de");
        let original_directory = DirectoryNode::new(
            "before".to_string(),
            91,
            0,
            DirectoryNode::DIRECTORY_FLAG | 0x10,
        );
        let deleted_file = DirectoryNode::new(
            "removed-file".to_string(),
            0,
            0,
            DirectoryNode::DELETED_FLAG,
        );
        let deleted_directory = DirectoryNode::new(
            "removed-directory".to_string(),
            0,
            0,
            DirectoryNode::DIRECTORY_FLAG | DirectoryNode::DELETED_FLAG,
        );

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let first_handle = prepare_payload(&mut batch, &first);
        let empty_file_handle = prepare_payload(&mut batch, &empty_file);
        let second_handle = prepare_payload(&mut batch, &second);
        let entries = [
            BundleArtifactEntry::empty_directory_from_node(&original_directory).unwrap(),
            BundleArtifactEntry::file(&batch, "first", 0, first_handle).unwrap(),
            BundleArtifactEntry::deleted_from_node(&deleted_file).unwrap(),
            BundleArtifactEntry::file(&batch, "empty-file", 0, empty_file_handle).unwrap(),
            BundleArtifactEntry::EmptyDirectory {
                name: "middle",
                flags: DirectoryNode::DIRECTORY_FLAG | 0x20,
            },
            BundleArtifactEntry::deleted_from_node(&deleted_directory).unwrap(),
            BundleArtifactEntry::file(&batch, "second", 0x14, second_handle).unwrap(),
            BundleArtifactEntry::EmptyDirectory {
                name: "after",
                flags: DirectoryNode::DIRECTORY_FLAG | 0x40,
            },
        ];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();

        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        let actual = reparsed
            .nodes
            .iter()
            .map(|node| (node.name.as_str(), node.offset, node.size, node.flags))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            [
                ("before", 0, 0, 0x11),
                ("first", 0, 3, 0),
                ("removed-file", 3, 0, 0x2),
                ("empty-file", 3, 0, 0),
                ("middle", 3, 0, 0x21),
                ("removed-directory", 3, 0, 0x3),
                ("second", 3, 2, 0x14),
                ("after", 5, 0, 0x41),
            ]
        );
        assert!(reparsed.nodes[0].is_directory());
        assert!(reparsed.nodes[2].is_deleted());
        assert!(!reparsed.nodes[2].is_file());
        assert!(reparsed.nodes[3].is_file());
        assert!(reparsed.nodes[4].is_directory());
        assert!(reparsed.nodes[5].is_deleted());
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[1]).unwrap(),
            b"abc"
        );
        assert!(
            reparsed
                .extract_node_data(&reparsed.nodes[3])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[6]).unwrap(),
            b"de"
        );
        assert_eq!(set.source_dependencies().len(), 3);
        assert_eq!(
            set.source_dependencies()
                .iter()
                .map(|dependency| dependency.referenced_bytes())
                .sum::<u64>(),
            5
        );
    }

    #[test]
    fn parsed_non_file_node_with_data_range_is_rejected() {
        let node = DirectoryNode::new(
            "unsupported".to_string(),
            17,
            9,
            DirectoryNode::DIRECTORY_FLAG | 0x20,
        );

        assert!(matches!(
            BundleArtifactEntry::empty_directory_from_node(&node),
            Err(BundleArtifactError::UnsupportedNonFileNodeRange {
                offset: 17,
                size: 9,
            })
        ));

        let deleted = DirectoryNode::new(
            "deleted".to_string(),
            0,
            0,
            DirectoryNode::DIRECTORY_FLAG | DirectoryNode::DELETED_FLAG,
        );
        assert!(matches!(
            BundleArtifactEntry::empty_directory_from_node(&deleted),
            Err(BundleArtifactError::ExpectedLiveDirectoryNode { flags })
                if flags == DirectoryNode::DIRECTORY_FLAG | DirectoryNode::DELETED_FLAG
        ));

        let deleted_with_data = DirectoryNode::new(
            "deleted-with-data".to_string(),
            23,
            4,
            DirectoryNode::DELETED_FLAG,
        );
        assert!(matches!(
            BundleArtifactEntry::deleted_from_node(&deleted_with_data),
            Err(BundleArtifactError::UnsupportedDeletedNodeRange {
                offset: 23,
                size: 4,
            })
        ));
    }

    #[test]
    fn entry_variants_require_matching_file_flags() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc0);
        let payload = source_payload(13, b"x");
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let file_with_directory_flag = [BundleArtifactEntry::file(
            &batch,
            "file",
            DirectoryNode::DIRECTORY_FLAG,
            payload_handle,
        )
        .unwrap()];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &file_with_directory_flag,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::FileEntryHasDirectoryFlag {
                entry: 0,
                flags: DirectoryNode::DIRECTORY_FLAG,
            })
        ));

        let deleted_file = [BundleArtifactEntry::file(
            &batch,
            "deleted",
            DirectoryNode::DELETED_FLAG,
            payload_handle,
        )
        .unwrap()];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &deleted_file,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::FileEntryHasDeletedFlag {
                entry: 0,
                flags: DirectoryNode::DELETED_FLAG,
            })
        ));

        let directory_without_directory_flag = [BundleArtifactEntry::EmptyDirectory {
            name: "directory",
            flags: DirectoryNode::SERIALIZED_FILE_FLAG,
        }];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &directory_without_directory_flag,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::EmptyDirectoryMissingDirectoryFlag {
                entry: 0,
                flags: DirectoryNode::SERIALIZED_FILE_FLAG,
            })
        ));

        let deleted_directory = [BundleArtifactEntry::EmptyDirectory {
            name: "deleted-directory",
            flags: DirectoryNode::DIRECTORY_FLAG | DirectoryNode::DELETED_FLAG,
        }];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &deleted_directory,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::EmptyDirectoryHasDeletedFlag { entry: 0, flags })
                if flags == DirectoryNode::DIRECTORY_FLAG | DirectoryNode::DELETED_FLAG
        ));

        let deleted_without_flag = [BundleArtifactEntry::Deleted {
            name: "not-deleted",
            flags: 0,
        }];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &deleted_without_flag,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::DeletedEntryMissingDeletedFlag { entry: 0, flags: 0 })
        ));
    }

    #[test]
    fn file_stream_plan_rejects_old_and_new_encryption_flags() {
        let mut old = file_stream_bundle("UnityFS", 7, 0xc2 | 0x200);
        old.header.unity_revision = "2020.3.33f1".to_string();
        old.header.unity_version = "2020.3.33f1".to_string();
        old.blocks[0].flags |= 0x200;
        let old_error = BundleEncodingPlan::new(&old, &[], PackingPolicy::Preserve).unwrap_err();
        assert!(old_error.to_string().contains("encryption flags"));

        let mut new = file_stream_bundle("UnityFS", 7, 0xc2 | 0x200 | 0x1000);
        new.header.unity_revision = "2020.3.34f1".to_string();
        new.header.unity_version = "2020.3.34f1".to_string();
        new.blocks[0].flags |= 0x1000;
        let new_error = BundleEncodingPlan::new(&new, &[], PackingPolicy::Preserve).unwrap_err();
        assert!(new_error.to_string().contains("encryption flags"));
    }

    #[test]
    fn file_stream_plan_rejects_unparseable_archive_flag_version() {
        let mut bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        bundle.header.unity_revision = "not-a-version".to_string();
        bundle.header.unity_version = "also-not-a-version".to_string();

        assert!(matches!(
            BundleEncodingPlan::new(&bundle, &[], PackingPolicy::Preserve),
            Err(BundleArtifactError::Binary(_))
        ));
    }

    #[test]
    fn legacy_layout_rejects_directory_metadata_it_cannot_preserve() {
        let bundle = legacy_bundle("UnityRaw");
        let payload = source_payload(14, b"x");
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);

        let flagged_file = [BundleArtifactEntry::file(
            &batch,
            "serialized",
            DirectoryNode::SERIALIZED_FILE_FLAG,
            payload_handle,
        )
        .unwrap()];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &flagged_file,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::UnsupportedLegacyFileFlags {
                entry: 0,
                flags: DirectoryNode::SERIALIZED_FILE_FLAG,
            })
        ));

        let deleted = [BundleArtifactEntry::Deleted {
            name: "removed",
            flags: DirectoryNode::DELETED_FLAG,
        }];
        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &deleted,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::UnsupportedLegacyDeletedEntry { entry: 0 })
        ));

        let entries = [BundleArtifactEntry::EmptyDirectory {
            name: "directory",
            flags: DirectoryNode::DIRECTORY_FLAG,
        }];

        assert!(matches!(
            BundleWriter::prepare_artifact(
                &mut batch,
                &bundle,
                &entries,
                PackingPolicy::Uncompressed,
            ),
            Err(BundleArtifactError::UnsupportedLegacyEmptyDirectory { entry: 0 })
        ));
    }

    #[test]
    fn empty_directory_metadata_is_deterministic_and_obeys_exact_chunk_budget() {
        let directory_name = "d".repeat(100);
        // 16-byte hash, counts, one block, and two directory entries total 177 bytes.
        let exact_block_info_bytes = 177;

        let first = build_uncompressed_directory_bundle(&directory_name, exact_block_info_bytes)
            .expect("exact block-info budget must succeed");
        let second = build_uncompressed_directory_bundle(&directory_name, exact_block_info_bytes)
            .expect("identical input must remain encodable");
        assert_eq!(first, second);

        let error =
            build_uncompressed_directory_bundle(&directory_name, exact_block_info_bytes - 1)
                .expect_err("one byte below the block-info budget must fail");
        assert_eq!(error.failure_phase(), ArtifactBuildFailurePhase::Encoding);
        assert!(matches!(
            error,
            BundleArtifactError::Artifact(error)
                if matches!(
                    *error,
                    ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                        resource: "generated_chunk_bytes",
                        requested: 177,
                        limit: 176,
                    })
                )
        ));
    }

    #[test]
    fn lz4_block_peak_is_bounded_and_one_short_failure_poisons_batch() {
        let (bytes, peak) =
            build_lz4_bundle_with_scratch_limit(ArtifactLimits::default().max_scratch_bytes())
                .expect("default scratch budget must encode one LZ4 block");
        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        let extracted = reparsed.extract_node_data(&reparsed.nodes[0]).unwrap();
        assert_eq!(extracted.len(), 0x0002_0000);
        assert!(extracted.iter().all(|byte| *byte == b'z'));

        let raw_bytes = 0x0002_0000_u64;
        let maximum_encoded_bytes = u64::try_from(lz4_flex::block::get_maximum_output_size(
            usize::try_from(raw_bytes).unwrap(),
        ))
        .unwrap();
        assert!(peak >= raw_bytes + maximum_encoded_bytes);
        assert!(peak <= raw_bytes + maximum_encoded_bytes + 64 * 1024);

        let failure = build_lz4_bundle_with_scratch_limit(peak - 1)
            .expect_err("one byte below the observed compression peak must fail");
        let Lz4ScratchFailure { error, poison } = *failure;
        assert_eq!(error.failure_phase(), ArtifactBuildFailurePhase::Encoding);
        assert!(matches!(poison, ArtifactBuildError::PoisonedBatch));
        assert!(error.to_string().contains("scratch_bytes"));
    }

    #[test]
    fn file_stream_v6_preserves_non_unityfs_signature_byte() {
        let bundle = file_stream_bundle("UnityWeb", 6, 0x42);
        let payload = source_payload(3, b"v6-payload");

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries =
            [BundleArtifactEntry::file(&batch, "payload.assets", 4, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();
        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.header.signature, "UnityWeb");
        assert_eq!(reparsed.header.version, 6);
        assert_eq!(reparsed.header.file_stream_header_byte, Some(0x5a));
        assert_eq!(reparsed.blocks[0].flags & 0x3f, 2);
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            b"v6-payload"
        );
    }

    #[test]
    fn file_stream_preserves_independent_blocks_info_and_data_compression() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc1);
        let payload = source_payload(7, &[b'm'; 150_000]);

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries =
            [BundleArtifactEntry::file(&batch, "mixed.assets", 4, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();

        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.header.flags & 0x3f, 1);
        assert!(reparsed.blocks.iter().all(|block| block.flags & 0x3f == 2));
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            payload.bytes()
        );
    }

    #[test]
    fn file_stream_preserve_keeps_mixed_block_boundaries_and_flags() {
        let mut bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        bundle.blocks = vec![
            CompressionBlock::new(3, 3, 0x40),
            CompressionBlock::new(100_000, 1, 0x82),
        ];
        let mut payload_bytes = b"raw".to_vec();
        payload_bytes.extend(std::iter::repeat_n(b'c', 100_000));
        payload_bytes.extend(std::iter::repeat_n(b'd', 10));
        let payload = source_payload(81, &payload_bytes);
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();

        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.blocks.len(), 3);
        assert_eq!(reparsed.blocks[0].uncompressed_size, 3);
        assert_eq!(reparsed.blocks[0].flags, 0x40);
        assert_eq!(reparsed.blocks[1].uncompressed_size, 100_000);
        assert_eq!(reparsed.blocks[1].flags, 0x82);
        assert_eq!(reparsed.blocks[2].uncompressed_size, 10);
        assert_eq!(reparsed.blocks[2].flags, 0x82);
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            payload.bytes()
        );
        assert_eq!(set.source_dependencies().len(), 1);
    }

    #[test]
    fn file_stream_packing_policies_reparse_with_canonical_switches() {
        for (ordinal, policy, expected_switch) in [
            (0_u128, PackingPolicy::Preserve, 2_u32),
            (1, PackingPolicy::Uncompressed, 0),
            (2, PackingPolicy::Lz4, 2),
            (3, PackingPolicy::Lzma, 1),
        ] {
            let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
            let payload = source_payload(70 + ordinal, &[b'p'; 64 * 1024]);
            let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
            let mut load_budget = AssetLoadBudget::default();
            let mut declaration =
                ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
            let output = declaration
                .declare_output(LogicalArtifactName::new("bundle").unwrap())
                .unwrap();
            let mut batch = declaration.seal_output_names().unwrap();
            let payload_handle = prepare_payload(&mut batch, &payload);
            let entries =
                [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
            let root =
                BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, policy).unwrap();
            batch.bind_output(output, root).unwrap();
            let set = batch.finish().unwrap();
            let mut bytes = Vec::new();
            set.outputs()
                .next()
                .unwrap()
                .artifact()
                .stream_verified_to(&mut bytes)
                .unwrap();

            let reparsed = BundleParser::from_bytes(bytes).unwrap();
            assert_eq!(reparsed.header.flags & 0x3f, expected_switch);
            assert!(
                reparsed
                    .blocks
                    .iter()
                    .all(|block| u32::from(block.flags) & 0x3f == expected_switch)
            );
            assert_eq!(
                reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
                payload.bytes()
            );
        }
    }

    #[test]
    fn file_stream_preserves_new_archive_padding_flag() {
        let mut bundle = file_stream_bundle("UnityFS", 7, 0x42 | 0x200);
        bundle.header.unity_revision = "2021.3.2f1".to_string();
        bundle.header.unity_version = "2021.3.2f1".to_string();
        let payload = source_payload(80, b"padding-aware");
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();

        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_ne!(reparsed.header.flags & 0x200, 0);
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            payload.bytes()
        );
    }

    #[test]
    fn file_stream_rejects_metadata_only_directories_without_data_blocks() {
        let bundle = file_stream_bundle("UnityFS", 7, 0xc2);
        let empty = source_payload(8, b"");

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let empty_handle = prepare_payload(&mut batch, &empty);
        let entries = [
            BundleArtifactEntry::file(&batch, "empty.assets", 0, empty_handle).unwrap(),
            BundleArtifactEntry::EmptyDirectory {
                name: "empty-directory",
                flags: DirectoryNode::DIRECTORY_FLAG,
            },
            BundleArtifactEntry::Deleted {
                name: "removed",
                flags: DirectoryNode::DELETED_FLAG,
            },
        ];

        assert!(matches!(
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve,),
            Err(BundleArtifactError::EmptyFileStreamData)
        ));
    }

    #[test]
    fn legacy_raw_artifact_keeps_empty_member_reachable() {
        let bundle = legacy_bundle("UnityRaw");
        let first = source_payload(4, b"raw-data");
        let empty = source_payload(5, b"");

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let first_handle = prepare_payload(&mut batch, &first);
        let empty_handle = prepare_payload(&mut batch, &empty);
        let entries = [
            BundleArtifactEntry::file(&batch, "first", 0, first_handle).unwrap(),
            BundleArtifactEntry::file(&batch, "empty", 0, empty_handle).unwrap(),
        ];
        let root = BundleWriter::prepare_artifact(
            &mut batch,
            &bundle,
            &entries,
            PackingPolicy::Uncompressed,
        )
        .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();
        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.header.signature, "UnityRaw");
        assert_eq!(reparsed.nodes.len(), 2);
        assert!(reparsed.nodes.iter().all(|node| node.flags == 0));
        assert!(reparsed.nodes.iter().all(|node| !node.is_serialized_file()));
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            b"raw-data"
        );
        assert!(
            reparsed
                .extract_node_data(&reparsed.nodes[1])
                .unwrap()
                .is_empty()
        );
        assert_eq!(set.source_dependencies().len(), 2);
    }

    #[test]
    fn legacy_versions_four_and_five_preserve_integrity_fields() {
        for (signature, policy) in [
            ("UnityRaw", PackingPolicy::Uncompressed),
            ("UnityWeb", PackingPolicy::Lzma),
        ] {
            for version in [4, 5] {
                let original = b"legacy-versioned";
                let bundle = legacy_bundle_with_version(signature, version, "payload", original);
                let payload = source_payload(u128::from(version) + 50, original);
                let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
                let mut load_budget = AssetLoadBudget::default();
                let mut declaration =
                    ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)
                        .unwrap();
                let output = declaration
                    .declare_output(LogicalArtifactName::new("bundle").unwrap())
                    .unwrap();
                let mut batch = declaration.seal_output_names().unwrap();
                let payload_handle = prepare_payload(&mut batch, &payload);
                let entries =
                    [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
                let root =
                    BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, policy).unwrap();
                batch.bind_output(output, root).unwrap();
                let set = batch.finish().unwrap();
                let mut bytes = Vec::new();
                set.outputs()
                    .next()
                    .unwrap()
                    .artifact()
                    .stream_verified_to(&mut bytes)
                    .unwrap();

                let reparsed = BundleParser::from_bytes(bytes).unwrap();
                let legacy = reparsed.header.legacy_web_raw.as_ref().unwrap();
                assert_eq!(reparsed.header.signature, signature);
                assert_eq!(reparsed.header.version, version);
                assert_eq!(legacy.hash.as_deref(), Some([0x5a; 16].as_slice()));
                assert_eq!(legacy.crc, Some(0x1234_5678));
                assert_eq!(
                    legacy.number_of_levels_to_download_before_streaming,
                    version
                );
                assert_eq!(
                    reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
                    payload.bytes()
                );
            }
        }
    }

    #[test]
    fn legacy_versions_four_and_five_recompute_crc_and_invalidate_hash_after_edit() {
        for (signature, policy) in [
            ("UnityRaw", PackingPolicy::Uncompressed),
            ("UnityWeb", PackingPolicy::Lzma),
        ] {
            for version in [4, 5] {
                let bundle =
                    legacy_bundle_with_version(signature, version, "payload", b"before-edit");
                let payload = source_payload(u128::from(version) + 60, b"after-edit");
                let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
                let mut load_budget = AssetLoadBudget::default();
                let mut declaration =
                    ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)
                        .unwrap();
                let output = declaration
                    .declare_output(LogicalArtifactName::new("bundle").unwrap())
                    .unwrap();
                let mut batch = declaration.seal_output_names().unwrap();
                let payload_handle = prepare_payload(&mut batch, &payload);
                let entries =
                    [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
                let root =
                    BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, policy).unwrap();
                batch.bind_output(output, root).unwrap();
                let set = batch.finish().unwrap();
                let mut bytes = Vec::new();
                set.outputs()
                    .next()
                    .unwrap()
                    .artifact()
                    .stream_verified_to(&mut bytes)
                    .unwrap();

                let reparsed = BundleParser::from_bytes(bytes).unwrap();
                let legacy = reparsed.header.legacy_web_raw.as_ref().unwrap();
                let canonical_content = canonical_legacy_content("payload", b"after-edit");
                let reparsed_content = reparsed.data_checked().unwrap();
                let mut crc = Crc::new();
                crc.update(&canonical_content);
                assert_eq!(reparsed.header.signature, signature);
                assert_eq!(reparsed_content, canonical_content);
                assert_eq!(legacy.hash.as_deref(), Some([0_u8; 16].as_slice()));
                assert_eq!(legacy.crc, Some(crc.sum()));
                assert_eq!(
                    reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
                    b"after-edit"
                );
            }
        }
    }

    #[test]
    fn legacy_web_artifact_reparses_lzma_content() {
        let bundle = legacy_bundle("UnityWeb");
        let payload = source_payload(6, b"legacy-web-data");

        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("bundle").unwrap())
            .unwrap();
        let mut batch = declaration.seal_output_names().unwrap();
        let payload_handle = prepare_payload(&mut batch, &payload);
        let entries = [BundleArtifactEntry::file(&batch, "payload", 0, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &entries, PackingPolicy::Preserve)
                .unwrap();
        batch.bind_output(output, root).unwrap();
        let set = batch.finish().unwrap();
        let mut bytes = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut bytes)
            .unwrap();
        let reparsed = BundleParser::from_bytes(bytes).unwrap();
        assert_eq!(reparsed.header.signature, "UnityWeb");
        assert_eq!(
            reparsed.extract_node_data(&reparsed.nodes[0]).unwrap(),
            b"legacy-web-data"
        );
    }
}
