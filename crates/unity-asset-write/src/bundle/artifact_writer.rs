//! Prepared-artifact encoding for Unity bundle containers.
//!
//! This module deliberately accepts an ordered list of member artifacts.  A bundle directory is
//! an ordered wire structure (and may contain duplicate names), so a name-keyed edit map cannot
//! be the authority for a new proof image.

use std::io::{self, BufRead, Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use thiserror::Error;
use unity_asset_binary::bundle::header::LegacyWebRawHeader;
use unity_asset_binary::bundle::{AssetBundle, BundleHeader, BundleLayoutKind};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::unity_version::UnityVersion;
use unity_asset_core::UnityAssetError;

use crate::PackingPolicy;
use crate::artifact::{ArtifactBatch, ArtifactBuildError, ArtifactHandle};

use super::writer::BundleWriter;

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
    #[error("bundle member {member} has an embedded NUL in its name")]
    EmbeddedNul { member: usize },
    #[error("bundle member count {count} does not fit the signed wire count")]
    MemberCountOverflow { count: usize },
    #[error("bundle member {member} length {length} does not fit the file-stream offset domain")]
    FileStreamLengthOverflow { member: usize, length: u64 },
    #[error("bundle member {member} length {length} does not fit the legacy u32 domain")]
    LegacyLengthOverflow { member: usize, length: u64 },
    #[error("bundle member {member} range overflows the concatenated data stream")]
    DataLengthOverflow { member: usize },
    #[error(
        "file-stream bundle contains no data blocks; at least one non-empty member is required"
    )]
    EmptyFileStreamData,
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
    #[error(
        "bundle member {member} could not be read completely: expected {expected}, got {actual}"
    )]
    MemberReadMismatch {
        member: usize,
        expected: u64,
        actual: u64,
    },
    #[error("bundle block compression failed: {message}")]
    Compression { message: String },
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

impl BundleWriter {
    /// Build one exact, independently reparsed bundle artifact in `batch`.
    ///
    /// `members` are consumed in the supplied order.  This is intentionally separate from the
    /// legacy `BundleEdits` name map so duplicate directory entries remain representable.
    pub fn prepare_artifact(
        batch: &mut ArtifactBatch<'_, '_>,
        bundle: &AssetBundle,
        members: &[BundleArtifactMember<'_>],
        policy: PackingPolicy,
    ) -> Result<ArtifactHandle, BundleArtifactError> {
        let layout = bundle.header.layout_kind()?;
        validate_members(layout, members)?;
        for member in members {
            batch.artifact_len(member.artifact)?;
        }
        preflight_bundle(bundle, members, policy)?;
        match layout {
            BundleLayoutKind::FileStream => {
                prepare_file_stream(batch, &bundle.header, &bundle.blocks, members, policy)
            }
            BundleLayoutKind::Legacy => {
                let legacy = bundle.header.legacy_web_raw.as_ref().ok_or(
                    BundleArtifactError::MissingLegacyHeader {
                        version: bundle.header.version,
                    },
                )?;
                prepare_legacy(batch, &bundle.header, legacy, members, policy)
            }
        }
    }
}

fn preflight_bundle(
    bundle: &AssetBundle,
    members: &[BundleArtifactMember<'_>],
    policy: PackingPolicy,
) -> Result<(), BundleArtifactError> {
    match bundle.header.layout_kind()? {
        BundleLayoutKind::FileStream => {
            let (header_flags, block_flags) =
                resolve_file_stream_flags(&bundle.header, &bundle.blocks, policy)?;
            if header_flags & 0x40 == 0 {
                return Err(BundleArtifactError::Unity(UnityAssetError::format(
                    "file-stream bundle writer requires DirectoryInfo (flags must include 0x40)",
                )));
            }
            reject_file_stream_encryption(&bundle.header, &bundle.blocks)?;
            if bundle.header.signature != "UnityFS"
                && bundle.header.file_stream_header_byte.is_none()
            {
                return Err(BundleArtifactError::MissingFileStreamHeaderByte {
                    signature: bundle.header.signature.clone(),
                });
            }
            let total = members.iter().try_fold(0_u64, |total, member| {
                total
                    .checked_add(member.length())
                    .ok_or(BundleArtifactError::ArithmeticOverflow {
                        resource: "file-stream member length",
                    })
            })?;
            if total == 0 {
                return Err(BundleArtifactError::EmptyFileStreamData);
            }
            let _ = plan_block_ranges(total, block_flags & 0x3f)?;
            validate_compression_switch(header_flags & 0x3f, "blocks-info")?;
            let _ =
                file_stream_header_length(&bundle.header, uses_block_alignment(&bundle.header))?;
            Ok(())
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
            match (bundle.header.signature.as_str(), policy) {
                ("UnityRaw", PackingPolicy::Preserve | PackingPolicy::Uncompressed)
                | ("UnityWeb", PackingPolicy::Preserve | PackingPolicy::Lzma) => {}
                _ => {
                    return Err(BundleArtifactError::UnsupportedPackingPolicy {
                        signature: bundle.header.signature.clone(),
                        policy,
                    });
                }
            }
            if bundle.header.version >= 4
                && (legacy.hash.as_ref().is_none_or(|hash| hash.len() != 16)
                    || legacy.crc.is_none())
            {
                return Err(BundleArtifactError::MissingLegacyIntegrity {
                    version: bundle.header.version,
                });
            }
            let directory_len = legacy_directory_length(members)?;
            let member_len = members.iter().try_fold(0_u64, |total, member| {
                total
                    .checked_add(member.length())
                    .ok_or(BundleArtifactError::ArithmeticOverflow {
                        resource: "legacy member length",
                    })
            })?;
            u32_value(
                directory_len.checked_add(member_len).ok_or(
                    BundleArtifactError::ArithmeticOverflow {
                        resource: "legacy content length",
                    },
                )?,
                "legacy content length",
            )?;
            let _ = legacy_header_length(&bundle.header)?;
            Ok(())
        }
    }
}

fn validate_members(
    layout: BundleLayoutKind,
    members: &[BundleArtifactMember<'_>],
) -> Result<(), BundleArtifactError> {
    i32::try_from(members.len()).map_err(|_| BundleArtifactError::MemberCountOverflow {
        count: members.len(),
    })?;
    let mut total = 0_u64;
    for (member, input) in members.iter().copied().enumerate() {
        if input.name.as_bytes().contains(&0) {
            return Err(BundleArtifactError::EmbeddedNul { member });
        }
        let length = input.length();
        match layout {
            BundleLayoutKind::FileStream => {
                if length > i64::MAX as u64 {
                    return Err(BundleArtifactError::FileStreamLengthOverflow { member, length });
                }
            }
            BundleLayoutKind::Legacy => {
                if length > u64::from(u32::MAX) {
                    return Err(BundleArtifactError::LegacyLengthOverflow { member, length });
                }
            }
        }
        total = total
            .checked_add(length)
            .ok_or(BundleArtifactError::DataLengthOverflow { member })?;
        if layout == BundleLayoutKind::Legacy && total > u64::from(u32::MAX) {
            return Err(BundleArtifactError::DataLengthOverflow { member });
        }
    }
    Ok(())
}

fn prepare_file_stream(
    batch: &mut ArtifactBatch<'_, '_>,
    header: &BundleHeader,
    original_blocks: &[CompressionBlock],
    members: &[BundleArtifactMember<'_>],
    policy: PackingPolicy,
) -> Result<ArtifactHandle, BundleArtifactError> {
    let (header_flags, block_flags) = resolve_file_stream_flags(header, original_blocks, policy)?;
    if header_flags & 0x40 == 0 {
        return Err(BundleArtifactError::Unity(UnityAssetError::format(
            "file-stream bundle writer requires DirectoryInfo (flags must include 0x40)",
        )));
    }
    reject_file_stream_encryption(header, original_blocks)?;

    let total_data_len = total_member_length(members)?;
    if total_data_len == 0 {
        return Err(BundleArtifactError::EmptyFileStreamData);
    }
    let block_switch = block_flags & 0x3f;
    let block_info_switch = header_flags & 0x3f;
    validate_compression_switch(block_info_switch, "blocks-info")?;
    let ranges = plan_block_ranges(total_data_len, block_switch)?;
    let mut data_chunks = Vec::new();
    let mut blocks = Vec::new();

    for range in ranges {
        let uncompressed = range.end - range.start;
        if block_switch == 0 {
            blocks.push(FileStreamBlock {
                uncompressed_size: u32_value(uncompressed, "block uncompressed size")?,
                compressed_size: u32_value(uncompressed, "block compressed size")?,
                flags: u16_value(block_flags, "block flags")?,
            });
            continue;
        }

        let mut compressed_len = 0_u64;
        let mut encoded_flags = 0_u16;
        let chunk = batch.derive_generated_chunk(|derived| {
            let mut raw = derived.generated_chunk_writer()?;
            visit_member_range(members, range.clone(), |member, local_start, length| {
                let mut reader = derived.dependency_reader(member.artifact)?;
                reader.seek(SeekFrom::Start(local_start))?;
                let mut limited = reader.take(length);
                let copied = io::copy(&mut limited, &mut raw)?;
                if copied != length {
                    return Err(ArtifactBuildError::DependencyIo(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "bundle member {} supplied {} bytes, expected {}",
                            member.name, copied, length
                        ),
                    )));
                }
                Ok(())
            })?;

            let raw_len = raw.len();
            let compressed = match block_switch {
                1 => {
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
                    let encoded_len = encoded.len();
                    if encoded_len > raw_len {
                        encoded_flags = u16_value(block_flags & !0x3f, "block flags")?;
                        compressed_len = u64::try_from(raw_len).map_err(|_| {
                            ArtifactBuildError::InternalInvariant {
                                message: "raw block length does not fit u64",
                            }
                        })?;
                        drop(encoded);
                        raw
                    } else {
                        encoded_flags = u16_value(block_flags, "block flags")?;
                        compressed_len = u64::try_from(encoded_len).map_err(|_| {
                            ArtifactBuildError::InternalInvariant {
                                message: "compressed block length does not fit u64",
                            }
                        })?;
                        drop(raw);
                        encoded
                    }
                }
                2 | 3 => {
                    let max_len = lz4_flex::block::get_maximum_output_size(raw_len);
                    let mut encoded = derived.generated_chunk_writer()?;
                    encoded.resize_zero(max_len)?;
                    let encoded_len =
                        lz4_flex::block::compress_into(raw.as_slice(), encoded.as_mut_slice()?)
                            .map_err(|error| {
                                io::Error::other(format!("LZ4 block compression: {error}"))
                            })?;
                    encoded.resize_zero(encoded_len)?;
                    if encoded_len > raw_len {
                        encoded_flags = u16_value(block_flags & !0x3f, "block flags")?;
                        compressed_len = u64::try_from(raw_len).map_err(|_| {
                            ArtifactBuildError::InternalInvariant {
                                message: "raw block length does not fit u64",
                            }
                        })?;
                        drop(encoded);
                        raw
                    } else {
                        encoded_flags = u16_value(block_flags, "block flags")?;
                        compressed_len = u64::try_from(encoded_len).map_err(|_| {
                            ArtifactBuildError::InternalInvariant {
                                message: "compressed block length does not fit u64",
                            }
                        })?;
                        drop(raw);
                        encoded
                    }
                }
                other => {
                    return Err(ArtifactBuildError::DependencyIo(io::Error::other(format!(
                        "unsupported file-stream compression switch: {other}"
                    ))));
                }
            };
            derived.finish_generated_chunk(compressed)
        })?;
        blocks.push(FileStreamBlock {
            uncompressed_size: u32_value(uncompressed, "block uncompressed size")?,
            compressed_size: u32_value(compressed_len, "block compressed size")?,
            flags: encoded_flags,
        });
        data_chunks.push(chunk);
    }

    // Block information is itself an exact generated chunk.  Empty member artifacts are attached
    // here so a zero-byte directory entry remains reachable without adding wire bytes.
    let mut block_info_uncompressed_len = 0_u64;
    let mut block_info_compressed_len = 0_u64;
    let block_info = batch.derive_generated_chunk(|derived| {
        for member in members.iter().filter(|member| member.length == 0) {
            derived.record_empty_dependency(member.artifact)?;
        }

        let mut raw = derived.generated_chunk_writer()?;
        raw.write_all(&[0_u8; 16])?;
        raw.write_all(&i32_value(blocks.len(), "block count")?.to_be_bytes())?;
        for block in &blocks {
            raw.write_all(&block.uncompressed_size.to_be_bytes())?;
            raw.write_all(&block.compressed_size.to_be_bytes())?;
            raw.write_all(&block.flags.to_be_bytes())?;
        }
        raw.write_all(&i32_value(members.len(), "directory count")?.to_be_bytes())?;
        let mut offset = 0_i64;
        for member in members {
            raw.write_all(&offset.to_be_bytes())?;
            let length = i64_value(member.length, "directory length")?;
            raw.write_all(&length.to_be_bytes())?;
            offset = offset
                .checked_add(length)
                .ok_or(ArtifactBuildError::InternalInvariant {
                    message: "file-stream directory offset overflow",
                })?;
            raw.write_all(&member.flags.to_be_bytes())?;
            write_cstring(&mut raw, member.name)?;
        }
        block_info_uncompressed_len =
            u64::try_from(raw.len()).map_err(|_| ArtifactBuildError::InternalInvariant {
                message: "block-info length does not fit u64",
            })?;

        let encoded = match block_info_switch {
            0 => raw,
            1 => {
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
            2 | 3 => {
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
            other => {
                return Err(ArtifactBuildError::DependencyIo(io::Error::other(format!(
                    "unsupported block-info compression switch: {other}"
                ))));
            }
        };
        block_info_compressed_len =
            u64::try_from(encoded.len()).map_err(|_| ArtifactBuildError::InternalInvariant {
                message: "compressed block-info length does not fit u64",
            })?;
        derived.finish_generated_chunk(encoded)
    })?;

    let uses_alignment = uses_block_alignment(header);
    let header_len = file_stream_header_length(header, uses_alignment)?;
    let at_end = (header_flags & 0x80) != 0;
    let pad_position = if at_end {
        header_len
    } else {
        header_len.checked_add(block_info_compressed_len).ok_or(
            BundleArtifactError::ArithmeticOverflow {
                resource: "file-stream padding position",
            },
        )?
    };
    let padding_len = if header_flags & 0x200 != 0 {
        align_up(pad_position, 16)? - pad_position
    } else {
        0
    };
    let data_len = if block_switch == 0 {
        total_data_len
    } else {
        data_chunks.iter().try_fold(0_u64, |sum, chunk| {
            sum.checked_add(chunk.len())
                .ok_or(BundleArtifactError::ArithmeticOverflow {
                    resource: "file-stream compressed data length",
                })
        })?
    };
    let total_len = header_len
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
        writer.write_all(&header_flags.to_be_bytes())?;
        if header.signature != "UnityFS" {
            let byte = header.file_stream_header_byte.ok_or_else(|| {
                ArtifactBuildError::DependencyIo(io::Error::other(
                    BundleArtifactError::MissingFileStreamHeaderByte {
                        signature: header.signature.clone(),
                    },
                ))
            })?;
            writer.write_all(&[byte])?;
        }
        writer.resize_zero(usize_value(header_len, "file-stream header length")?)?;
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
            if at_end {
                if let Some(padding) = padding {
                    encoder.push_derived_generated_chunk(padding)?;
                }
                if block_switch == 0 {
                    for member in members.iter().filter(|member| member.length != 0) {
                        encoder.append_dependency(member.artifact)?;
                    }
                } else {
                    for chunk in data_chunks {
                        encoder.push_derived_generated_chunk(chunk)?;
                    }
                }
                encoder.push_derived_generated_chunk(block_info)?;
            } else {
                encoder.push_derived_generated_chunk(block_info)?;
                if let Some(padding) = padding {
                    encoder.push_derived_generated_chunk(padding)?;
                }
                if block_switch == 0 {
                    for member in members.iter().filter(|member| member.length != 0) {
                        encoder.append_dependency(member.artifact)?;
                    }
                } else {
                    for chunk in data_chunks {
                        encoder.push_derived_generated_chunk(chunk)?;
                    }
                }
            }
            Ok(())
        })
        .map_err(Into::into)
}

fn prepare_legacy(
    batch: &mut ArtifactBatch<'_, '_>,
    header: &BundleHeader,
    legacy: &LegacyWebRawHeader,
    members: &[BundleArtifactMember<'_>],
    policy: PackingPolicy,
) -> Result<ArtifactHandle, BundleArtifactError> {
    if !(3..=5).contains(&header.version) {
        return Err(BundleArtifactError::UnsupportedLegacyVersion {
            version: header.version,
        });
    }
    if legacy.level_count != 1 {
        return Err(BundleArtifactError::UnsupportedLegacyLevelCount {
            count: legacy.level_count,
        });
    }
    let compress = match (header.signature.as_str(), policy) {
        ("UnityRaw", PackingPolicy::Preserve | PackingPolicy::Uncompressed) => false,
        ("UnityWeb", PackingPolicy::Preserve | PackingPolicy::Lzma) => true,
        _ => {
            return Err(BundleArtifactError::UnsupportedPackingPolicy {
                signature: header.signature.clone(),
                policy,
            });
        }
    };

    let directory_len = legacy_directory_length(members)?;
    let member_bytes = total_member_length(members)?;
    let uncompressed_len =
        directory_len
            .checked_add(member_bytes)
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "legacy uncompressed content length",
            })?;
    u32_value(uncompressed_len, "legacy uncompressed content size")?;

    let mut compressed_len = 0_u64;
    let content = if compress {
        let content = batch.derive_generated_chunk(|derived| {
            for member in members.iter().filter(|member| member.length == 0) {
                derived.record_empty_dependency(member.artifact)?;
            }
            let mut raw = derived.generated_chunk_writer()?;
            write_legacy_directory(&mut raw, members, directory_len)?;
            visit_member_range(members, 0..member_bytes, |member, local_start, length| {
                let mut reader = derived.dependency_reader(member.artifact)?;
                reader.seek(SeekFrom::Start(local_start))?;
                let mut limited = reader.take(length);
                let copied = io::copy(&mut limited, &mut raw)?;
                if copied != length {
                    return Err(ArtifactBuildError::DependencyIo(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "legacy bundle member {} supplied {} bytes, expected {}",
                            member.name, copied, length
                        ),
                    )));
                }
                Ok(())
            })?;
            let mut encoded = derived.generated_chunk_writer()?;
            let mut input = Cursor::new(raw.as_slice());
            compress_lzma(&mut input, &mut encoded, Some(uncompressed_len))?;
            compressed_len = u64::try_from(encoded.len()).map_err(|_| {
                ArtifactBuildError::InternalInvariant {
                    message: "legacy compressed length does not fit u64",
                }
            })?;
            drop(raw);
            derived.finish_generated_chunk(encoded)
        })?;
        Some(content)
    } else {
        compressed_len = uncompressed_len;
        None
    };

    let directory = if compress {
        None
    } else {
        Some(batch.derive_generated_chunk(|derived| {
            for member in members.iter().filter(|member| member.length == 0) {
                derived.record_empty_dependency(member.artifact)?;
            }
            let mut writer = derived.generated_chunk_writer()?;
            write_legacy_directory(&mut writer, members, directory_len)?;
            derived.finish_generated_chunk(writer)
        })?)
    };

    let header_len = legacy_header_length(header)?;
    let total_len =
        header_len
            .checked_add(compressed_len)
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "legacy bundle length",
            })?;
    u32_value(total_len, "legacy complete file size")?;
    let header_chunk = batch.derive_generated_chunk(|derived| {
        let mut writer = derived.generated_chunk_writer()?;
        write_cstring(&mut writer, &header.signature)?;
        writer.write_all(&header.version.to_be_bytes())?;
        write_cstring(&mut writer, &header.unity_version)?;
        write_cstring(&mut writer, &header.unity_revision)?;
        if header.version >= 4 {
            let hash = legacy
                .hash
                .as_deref()
                .filter(|hash| hash.len() == 16)
                .ok_or_else(|| {
                    ArtifactBuildError::DependencyIo(io::Error::other(
                        BundleArtifactError::MissingLegacyIntegrity {
                            version: header.version,
                        },
                    ))
                })?;
            let crc = legacy.crc.ok_or_else(|| {
                ArtifactBuildError::DependencyIo(io::Error::other(
                    BundleArtifactError::MissingLegacyIntegrity {
                        version: header.version,
                    },
                ))
            })?;
            writer.write_all(hash)?;
            writer.write_all(&crc.to_be_bytes())?;
        }
        let total_u32 = u32_value(total_len, "legacy complete file size")?;
        writer.write_all(&total_u32.to_be_bytes())?;
        writer.write_all(&u32_value(header_len, "legacy header size")?.to_be_bytes())?;
        writer.write_all(&1_u32.to_be_bytes())?;
        writer.write_all(&1_i32.to_be_bytes())?;
        writer.write_all(&u32_value(compressed_len, "legacy compressed size")?.to_be_bytes())?;
        writer
            .write_all(&u32_value(uncompressed_len, "legacy uncompressed size")?.to_be_bytes())?;
        writer.write_all(&total_u32.to_be_bytes())?;
        writer.write_all(&u32_value(directory_len, "legacy directory size")?.to_be_bytes())?;
        writer.resize_zero(usize_value(header_len, "legacy header length")?)?;
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
                for member in members.iter().filter(|member| member.length != 0) {
                    encoder.append_dependency(member.artifact)?;
                }
            }
            Ok(())
        })
        .map_err(Into::into)
}

fn resolve_file_stream_flags(
    header: &BundleHeader,
    original_blocks: &[CompressionBlock],
    policy: PackingPolicy,
) -> Result<(u32, u32), BundleArtifactError> {
    let original_block = original_blocks
        .first()
        .map_or(0x40, |block| u32::from(block.flags));
    let (header_switch, block_switch) = match policy {
        PackingPolicy::Preserve => return Ok((header.flags, original_block)),
        PackingPolicy::Uncompressed => (0, 0),
        PackingPolicy::Lz4 => (2, 2),
        PackingPolicy::Lzma => (1, 1),
    };
    Ok((
        (header.flags & !0x3f) | header_switch,
        (original_block & !0x3f) | block_switch,
    ))
}

fn reject_file_stream_encryption(
    header: &BundleHeader,
    blocks: &[CompressionBlock],
) -> Result<(), BundleArtifactError> {
    let uses_old_flags = uses_old_archive_flags(header).unwrap_or(false);
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

fn uses_old_archive_flags(header: &BundleHeader) -> Option<bool> {
    let parsed = UnityVersion::parse_version(&header.unity_revision)
        .or_else(|_| UnityVersion::parse_version(&header.unity_version))
        .ok()?;
    let (major, minor, build) = (parsed.major, parsed.minor, parsed.build);
    Some(if major < 2020 {
        true
    } else if major == 2020 {
        minor < 3 || (minor == 3 && build < 34)
    } else if major == 2021 {
        minor < 3 || (minor == 3 && build < 2)
    } else if major == 2022 {
        minor < 1 || (minor == 1 && build < 1)
    } else {
        false
    })
}

fn uses_block_alignment(header: &BundleHeader) -> bool {
    if header.version >= 7 {
        return true;
    }
    let parsed = UnityVersion::parse_version(&header.unity_revision)
        .or_else(|_| UnityVersion::parse_version(&header.unity_version));
    let Ok(parsed) = parsed else {
        return false;
    };
    parsed.major > 2019 || (parsed.major == 2019 && parsed.minor >= 4)
}

fn plan_block_ranges(total: u64, switch: u32) -> Result<Vec<Range<u64>>, BundleArtifactError> {
    let chunk_size = match switch {
        0 => {
            u32_value(total, "uncompressed file-stream block")?;
            let mut ranges = Vec::new();
            ranges
                .try_reserve_exact(1)
                .map_err(|source| BundleArtifactError::Compression {
                    message: format!("failed to reserve file-stream block range: {source}"),
                })?;
            ranges.push(0..total);
            return Ok(ranges);
        }
        1 => u64::from(u32::MAX),
        2 | 3 => 0x0002_0000,
        other => {
            return Err(BundleArtifactError::Compression {
                message: format!("unsupported file-stream compression switch: {other}"),
            });
        }
    };
    let mut ranges = Vec::new();
    let block_count = total.div_ceil(chunk_size);
    ranges
        .try_reserve_exact(usize_value(block_count, "file-stream block count")?)
        .map_err(|source| BundleArtifactError::Compression {
            message: format!("failed to reserve file-stream block ranges: {source}"),
        })?;
    let mut start = 0_u64;
    while start < total {
        let end = start.saturating_add(chunk_size).min(total);
        ranges.push(start..end);
        start = end;
    }
    Ok(ranges)
}

fn validate_compression_switch(
    switch: u32,
    resource: &'static str,
) -> Result<(), BundleArtifactError> {
    if switch > 3 {
        return Err(BundleArtifactError::Compression {
            message: format!("unsupported {resource} compression switch: {switch}"),
        });
    }
    Ok(())
}

fn visit_member_range(
    members: &[BundleArtifactMember<'_>],
    range: Range<u64>,
    mut visit: impl FnMut(&BundleArtifactMember<'_>, u64, u64) -> Result<(), ArtifactBuildError>,
) -> Result<(), ArtifactBuildError> {
    let mut member_start = 0_u64;
    for member in members {
        let member_end = member_start.checked_add(member.length).ok_or(
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

fn total_member_length(members: &[BundleArtifactMember<'_>]) -> Result<u64, BundleArtifactError> {
    members.iter().try_fold(0_u64, |total, member| {
        total
            .checked_add(member.length)
            .ok_or(BundleArtifactError::ArithmeticOverflow {
                resource: "bundle member data length",
            })
    })
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
    members: &[BundleArtifactMember<'_>],
) -> Result<u64, BundleArtifactError> {
    let length = members.iter().try_fold(4_u64, |length, member| {
        let name = u64::try_from(member.name.len()).map_err(|_| {
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
    members: &[BundleArtifactMember<'_>],
    directory_len: u64,
) -> io::Result<()> {
    writer.write_all(&i32_value(members.len(), "legacy directory count")?.to_be_bytes())?;
    let mut offset = directory_len;
    for member in members {
        write_cstring(writer, member.name)?;
        writer.write_all(&u32_value(offset, "legacy member offset")?.to_be_bytes())?;
        writer.write_all(&u32_value(member.length, "legacy member length")?.to_be_bytes())?;
        offset = offset
            .checked_add(member.length)
            .ok_or_else(|| io::Error::other("legacy member content offset overflow"))?;
    }
    let written = members.iter().try_fold(4_u64, |length, member| {
        length
            .checked_add(
                u64::try_from(member.name.len())
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
        ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload,
        LogicalArtifactName, StreamedResourceExtentInspection,
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
        bundle.blocks.push(CompressionBlock::new(1, 1, 2));
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
        let members = [
            BundleArtifactMember::new(&batch, "duplicate.assets", 4, first_handle).unwrap(),
            BundleArtifactMember::new(&batch, "duplicate.assets", 4, second_handle).unwrap(),
        ];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &members, PackingPolicy::Preserve)
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
        let members =
            [BundleArtifactMember::new(&batch, "payload.assets", 4, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &members, PackingPolicy::Preserve)
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
        let members =
            [BundleArtifactMember::new(&batch, "mixed.assets", 4, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &members, PackingPolicy::Preserve)
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
    fn file_stream_rejects_an_empty_data_stream() {
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
        let members = [BundleArtifactMember::new(&batch, "empty.assets", 4, empty_handle).unwrap()];

        assert!(matches!(
            BundleWriter::prepare_artifact(&mut batch, &bundle, &members, PackingPolicy::Preserve,),
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
        let members = [
            BundleArtifactMember::new(&batch, "first", 4, first_handle).unwrap(),
            BundleArtifactMember::new(&batch, "empty", 4, empty_handle).unwrap(),
        ];
        let root = BundleWriter::prepare_artifact(
            &mut batch,
            &bundle,
            &members,
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
        let members = [BundleArtifactMember::new(&batch, "payload", 4, payload_handle).unwrap()];
        let root =
            BundleWriter::prepare_artifact(&mut batch, &bundle, &members, PackingPolicy::Preserve)
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
