use std::collections::TryReserveError;
use std::io::{self, Cursor, Read};
use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, ContractError, SourceMemberId,
    arc_slice_allocation_bytes,
};
use zip::ZipArchive;
use zip::result::ZipError;

const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP_CENTRAL_HEADER_SIGNATURE: u32 = 0x0201_4b50;
const ZIP_CENTRAL_DIGITAL_SIGNATURE: u32 = 0x0505_4b50;
const ZIP_LOCAL_HEADER_SIGNATURE: u32 = 0x0403_4b50;
const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;
const ZIP_AES_EXTRA_FIELD_ID: u16 = 0x9901;
const ZIP_ENCRYPTED_FLAG: u16 = 1;
const ZIP_UTF8_FLAG: u16 = 1 << 11;
const ZIP_EOCD_FIXED_LEN: usize = 22;
const ZIP_EOCD_SEARCH_LEN: usize = ZIP_EOCD_FIXED_LEN + u16::MAX as usize;
const ZIP64_LOCATOR_LEN: usize = 20;
const ZIP64_EOCD_MIN_LEN: usize = 56;
const ZIP64_RECORD_SEARCH_LEN: usize = ZIP_EOCD_SEARCH_LEN;
const ZIP_PREFLIGHT_TAIL_LEN: usize =
    ZIP_EOCD_SEARCH_LEN + ZIP64_LOCATOR_LEN + ZIP64_RECORD_SEARCH_LEN;
const ZIP_CENTRAL_HEADER_LEN: usize = 46;
const ZIP_LOCAL_HEADER_LEN: usize = 30;
const MAX_MEMBER_PATH_BYTES: usize = 16 * 1024;
const ZIP_PARSER_BYTES_PER_MEMBER: u64 = 512;
// zip 0.6 retains the raw central comment while CP437 decoding may grow its String capacity to
// four times the encoded length: one raw buffer plus four decoded-capacity buffers at peak.
const ZIP_PARSER_DIRECTORY_MULTIPLIER: u64 = 5;
const ZIP_PARSER_FIXED_BYTES: u64 = 4 * 1024;
// zip 0.6 creates opaque streaming decoders. These dependency-bound conservative charges cover
// the accepted window/state sizes before the decoder can allocate them.
const ZIP_DEFLATE_DECODER_SCRATCH_BYTES: u64 = 1024 * 1024;
const ZIP_BZIP2_DECODER_SCRATCH_BYTES: u64 = 16 * 1024 * 1024;
const ZIP_ZSTD_DECODER_SCRATCH_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OCCURRENCE_MEMBER_COUNT: u64 = u32::MAX as u64 + 1;

/// One materialized regular-file member in central-directory wire order.
#[derive(Debug, Clone)]
pub(crate) struct ArchiveMemberRecord {
    pub(crate) wire_ordinal: u64,
    pub(crate) member_id: SourceMemberId,
    pub(crate) bytes: BudgetedSourceBytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ArchiveMemberNameError {
    #[error("the name is empty")]
    Empty,
    #[error("the name exceeds the portable member path limit")]
    TooLong,
    #[error("the name is not stable UTF-8")]
    UnstableEncoding,
    #[error("the name is absolute")]
    Absolute,
    #[error("the name contains a backslash")]
    Backslash,
    #[error("the name contains a NUL or control character")]
    ControlCharacter,
    #[error("the name contains an empty, current-directory, or parent-directory component")]
    TraversalComponent,
}

#[derive(Debug, Error)]
pub(crate) enum ArchiveLoadError {
    #[error("invalid ZIP structure")]
    InvalidStructure {
        #[source]
        source: io::Error,
    },
    #[error("ZIP load budget rejected {operation}")]
    Budget {
        operation: &'static str,
        #[source]
        source: BudgetError,
    },
    #[error("failed to allocate {resource} ({requested} bytes)")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error("ZIP arithmetic overflow while computing {resource}")]
    ArithmeticOverflow { resource: &'static str },
    #[error("ZIP member {wire_ordinal} has an invalid stable name: {reason}")]
    InvalidMemberName {
        wire_ordinal: u64,
        reason: ArchiveMemberNameError,
    },
    #[error("failed to construct the identity of ZIP member {wire_ordinal}")]
    MemberIdentity {
        wire_ordinal: u64,
        #[source]
        source: ContractError,
    },
    #[error("failed to parse the ZIP archive")]
    OpenArchive {
        #[source]
        source: ZipError,
    },
    #[error("failed to open ZIP member {wire_ordinal}")]
    OpenMember {
        wire_ordinal: u64,
        #[source]
        source: ZipError,
    },
    #[error("failed to decompress ZIP member {wire_ordinal}")]
    ReadMember {
        wire_ordinal: u64,
        #[source]
        source: io::Error,
    },
    #[error("ZIP metadata changed between preflight and decode: {detail}")]
    InconsistentMetadata { detail: &'static str },
    #[error("same-name occurrence does not fit in u32 for ZIP member {wire_ordinal}")]
    OccurrenceOverflow { wire_ordinal: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ZipDirectoryPreflight {
    member_count: u64,
    directory_start: u64,
    directory_size: u64,
    archive_offset: u64,
    eocd_start: u64,
    eocd_comment_size: u64,
    zip64_nominal_record: Option<u64>,
    zip64_record_start: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ArchiveTotals {
    file_count: u64,
    compressed_bytes: u64,
    decompressed_bytes: u64,
    payload_bytes: u64,
    retained_arc_bytes: u64,
    name_bytes: u64,
    codec_scratch_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ZipArchiveLoadPlan {
    directory: ZipDirectoryPreflight,
    totals: ArchiveTotals,
    parsed_member_count: usize,
    file_count: usize,
    planned_bytes: u64,
}

impl ZipArchiveLoadPlan {
    #[must_use]
    pub(crate) const fn has_file_members(self) -> bool {
        self.file_count != 0
    }
}

#[derive(Debug)]
struct PendingArchiveMember {
    wire_ordinal: u64,
    name: String,
    same_name_occurrence: u32,
    compressed_size: u64,
    decompressed_size: u64,
}

#[derive(Debug, Clone, Copy)]
struct CentralEntry<'a> {
    wire_ordinal: u64,
    flags: u16,
    method: u16,
    name: &'a [u8],
    compressed_size: u64,
    decompressed_size: u64,
    is_directory: bool,
}

/// Loads regular-file payloads from an owned ZIP image.
///
/// The result preserves central-directory order. Directory records consume the member and
/// decompression ledgers but do not produce payload records.
#[cfg(test)]
pub(crate) fn load_zip_archive(
    archive_bytes: Arc<[u8]>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ArchiveMemberRecord>, ArchiveLoadError> {
    let plan = preflight_zip_archive(archive_bytes.as_ref(), budget)?;
    load_preflighted_zip_archive(archive_bytes, plan, budget)
}

pub(crate) fn preflight_zip_archive(
    archive_bytes: &[u8],
    budget: &AssetLoadBudget,
) -> Result<ZipArchiveLoadPlan, ArchiveLoadError> {
    let directory = preflight_zip_directory(archive_bytes).map_err(invalid_structure)?;
    budget
        .check_members(directory.member_count)
        .map_err(|source| budget_error("member traversal", source))?;
    let totals = preflight_members(archive_bytes, directory, budget)?;
    let parsed_member_count = usize_from_u64(directory.member_count, "member count")?;
    let file_count = usize_from_u64(totals.file_count, "regular member count")?;
    validate_occurrence_capacity(totals.file_count)?;
    let planned_bytes = planned_allocation_bytes(directory, totals)?;

    budget
        .check_decompression(totals.compressed_bytes, totals.decompressed_bytes)
        .map_err(|source| budget_error("aggregate decompression", source))?;
    budget
        .check_bytes(planned_bytes)
        .map_err(|source| budget_error("archive adapter allocations", source))?;

    Ok(ZipArchiveLoadPlan {
        directory,
        totals,
        parsed_member_count,
        file_count,
        planned_bytes,
    })
}

pub(crate) fn load_preflighted_zip_archive(
    archive_bytes: Arc<[u8]>,
    plan: ZipArchiveLoadPlan,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ArchiveMemberRecord>, ArchiveLoadError> {
    let ZipArchiveLoadPlan {
        directory,
        totals,
        parsed_member_count,
        file_count,
        planned_bytes,
    } = plan;

    budget
        .check_members(directory.member_count)
        .map_err(|source| budget_error("member traversal", source))?;
    budget
        .check_decompression(totals.compressed_bytes, totals.decompressed_bytes)
        .map_err(|source| budget_error("aggregate decompression", source))?;
    budget
        .check_bytes(planned_bytes)
        .map_err(|source| budget_error("archive adapter allocations", source))?;

    budget
        .consume_members(directory.member_count)
        .map_err(|source| budget_error("member traversal", source))?;
    budget
        .begin_decompression()
        .consume(totals.compressed_bytes, totals.decompressed_bytes)
        .map_err(|source| budget_error("aggregate decompression", source))?;
    budget
        .consume_bytes(planned_bytes.checked_sub(totals.retained_arc_bytes).ok_or(
            ArchiveLoadError::ArithmeticOverflow {
                resource: "archive upfront allocation total",
            },
        )?)
        .map_err(|source| budget_error("archive adapter allocations", source))?;

    let mut zip = ZipArchive::new(Cursor::new(Arc::clone(&archive_bytes)))
        .map_err(|source| ArchiveLoadError::OpenArchive { source })?;
    if zip.len() != parsed_member_count {
        return Err(ArchiveLoadError::InconsistentMetadata {
            detail: "central-directory member count differs from ZipArchive",
        });
    }

    let mut pending = try_vec_with_capacity(file_count, "pending archive members")?;
    walk_central_directory(&archive_bytes, directory, |entry| {
        if entry.is_directory {
            return Ok(());
        }
        let name = stable_member_name(entry.name, entry.flags, false).map_err(|reason| {
            ArchiveLoadError::InvalidMemberName {
                wire_ordinal: entry.wire_ordinal,
                reason,
            }
        })?;
        let mut owned_name = try_string_with_capacity(name.len(), "archive member identity")?;
        owned_name.push_str(name);
        pending.push(PendingArchiveMember {
            wire_ordinal: entry.wire_ordinal,
            name: owned_name,
            same_name_occurrence: 0,
            compressed_size: entry.compressed_size,
            decompressed_size: entry.decompressed_size,
        });
        Ok(())
    })?;
    if pending.len() != file_count {
        return Err(ArchiveLoadError::InconsistentMetadata {
            detail: "regular member count changed after preflight",
        });
    }
    assign_same_name_occurrences(&mut pending)?;

    let mut output = try_vec_with_capacity(file_count, "archive member output")?;
    for member in pending {
        let index = usize_from_u64(member.wire_ordinal, "member wire ordinal")?;
        let mut entry = zip
            .by_index(index)
            .map_err(|source| ArchiveLoadError::OpenMember {
                wire_ordinal: member.wire_ordinal,
                source,
            })?;
        if entry.is_dir()
            || entry.name_raw() != member.name.as_bytes()
            || entry.compressed_size() != member.compressed_size
            || entry.size() != member.decompressed_size
        {
            return Err(ArchiveLoadError::InconsistentMetadata {
                detail: "member identity or declared size differs from preflight",
            });
        }

        let payload_len = usize_from_u64(member.decompressed_size, "member payload length")?;
        let payload = read_exact_payload(&mut entry, payload_len, member.wire_ordinal)?;
        let bytes = BudgetedSourceBytes::from_vec(payload, budget)
            .map_err(|source| budget_error("archive member source allocation", source))?;
        let member_id = SourceMemberId::with_occurrence(member.name, member.same_name_occurrence)
            .map_err(|source| ArchiveLoadError::MemberIdentity {
            wire_ordinal: member.wire_ordinal,
            source,
        })?;
        output.push(ArchiveMemberRecord {
            wire_ordinal: member.wire_ordinal,
            member_id,
            bytes,
        });
    }
    Ok(output)
}

fn preflight_members(
    bytes: &[u8],
    directory: ZipDirectoryPreflight,
    budget: &AssetLoadBudget,
) -> Result<ArchiveTotals, ArchiveLoadError> {
    let mut totals = ArchiveTotals::default();
    walk_central_directory(bytes, directory, |entry| {
        let name =
            stable_member_name(entry.name, entry.flags, entry.is_directory).map_err(|reason| {
                ArchiveLoadError::InvalidMemberName {
                    wire_ordinal: entry.wire_ordinal,
                    reason,
                }
            })?;
        budget
            .check_decompression(entry.compressed_size, entry.decompressed_size)
            .map_err(|source| budget_error("member decompression", source))?;
        totals.compressed_bytes = checked_add(
            totals.compressed_bytes,
            entry.compressed_size,
            "compressed member total",
        )?;
        totals.decompressed_bytes = checked_add(
            totals.decompressed_bytes,
            entry.decompressed_size,
            "decompressed member total",
        )?;
        totals.codec_scratch_bytes = checked_add(
            totals.codec_scratch_bytes,
            codec_scratch_bytes(entry.method),
            "ZIP decoder scratch total",
        )?;
        if !entry.is_directory {
            let payload_len = usize_from_u64(entry.decompressed_size, "member payload length")?;
            totals.file_count = checked_add(totals.file_count, 1, "regular member count")?;
            totals.payload_bytes = checked_add(
                totals.payload_bytes,
                entry.decompressed_size,
                "regular member payload total",
            )?;
            totals.retained_arc_bytes = checked_add(
                totals.retained_arc_bytes,
                arc_slice_allocation_bytes::<u8>(payload_len).map_err(|_| {
                    ArchiveLoadError::ArithmeticOverflow {
                        resource: "archive payload Arc allocation",
                    }
                })?,
                "archive payload Arc allocation total",
            )?;
            totals.name_bytes = checked_add(
                totals.name_bytes,
                u64_from_usize(name.len(), "member name length")?,
                "member name total",
            )?;
        }
        Ok(())
    })?;
    Ok(totals)
}

fn planned_allocation_bytes(
    directory: ZipDirectoryPreflight,
    totals: ArchiveTotals,
) -> Result<u64, ArchiveLoadError> {
    let parser_directory = checked_mul(
        directory.directory_size,
        ZIP_PARSER_DIRECTORY_MULTIPLIER,
        "ZipArchive central-directory workspace",
    )?;
    let parser_members = checked_mul(
        directory.member_count,
        ZIP_PARSER_BYTES_PER_MEMBER,
        "ZipArchive member workspace",
    )?;
    let pending = checked_sized_allocation::<PendingArchiveMember>(
        totals.file_count,
        "pending archive members",
    )?;
    let occurrence_order =
        checked_sized_allocation::<usize>(totals.file_count, "occurrence sort order")?;
    let output = checked_sized_allocation::<ArchiveMemberRecord>(
        totals.file_count,
        "archive member output",
    )?;
    let payload_working_copy = totals.payload_bytes;

    [
        parser_directory,
        parser_members,
        ZIP_PARSER_FIXED_BYTES,
        pending,
        occurrence_order,
        output,
        directory.eocd_comment_size,
        totals.name_bytes,
        totals.codec_scratch_bytes,
        payload_working_copy,
        totals.retained_arc_bytes,
    ]
    .into_iter()
    .try_fold(0_u64, |total, amount| {
        checked_add(total, amount, "archive adapter allocation total")
    })
}

fn assign_same_name_occurrences(
    pending: &mut [PendingArchiveMember],
) -> Result<(), ArchiveLoadError> {
    let mut order = try_vec_with_capacity(pending.len(), "archive occurrence sort order")?;
    order.extend(0..pending.len());
    order.sort_unstable_by(|left, right| {
        pending[*left]
            .name
            .cmp(&pending[*right].name)
            .then_with(|| {
                pending[*left]
                    .wire_ordinal
                    .cmp(&pending[*right].wire_ordinal)
            })
    });

    let mut previous: Option<usize> = None;
    let mut occurrence = 0_u32;
    for index in order {
        occurrence = match previous {
            Some(previous_index) if pending[previous_index].name == pending[index].name => {
                occurrence
                    .checked_add(1)
                    .ok_or(ArchiveLoadError::OccurrenceOverflow {
                        wire_ordinal: pending[index].wire_ordinal,
                    })?
            }
            _ => 0,
        };
        pending[index].same_name_occurrence = occurrence;
        previous = Some(index);
    }
    Ok(())
}

fn validate_occurrence_capacity(file_count: u64) -> Result<(), ArchiveLoadError> {
    if file_count > MAX_OCCURRENCE_MEMBER_COUNT {
        return Err(ArchiveLoadError::OccurrenceOverflow {
            wire_ordinal: file_count - 1,
        });
    }
    Ok(())
}

fn read_exact_payload(
    reader: &mut impl Read,
    expected_len: usize,
    wire_ordinal: u64,
) -> Result<Vec<u8>, ArchiveLoadError> {
    let mut payload = try_vec_with_capacity(expected_len, "archive member payload")?;
    let mut chunk = [0_u8; 64 * 1024];
    while payload.len() < expected_len {
        let remaining = expected_len - payload.len();
        let read_limit = remaining.min(chunk.len());
        let read = reader.read(&mut chunk[..read_limit]).map_err(|source| {
            ArchiveLoadError::ReadMember {
                wire_ordinal,
                source,
            }
        })?;
        if read == 0 {
            return Err(ArchiveLoadError::ReadMember {
                wire_ordinal,
                source: invalid_zip("member ended before its declared decompressed size"),
            });
        }
        payload.extend_from_slice(&chunk[..read]);
    }

    let mut trailing = [0_u8; 1];
    let trailing_len =
        reader
            .read(&mut trailing)
            .map_err(|source| ArchiveLoadError::ReadMember {
                wire_ordinal,
                source,
            })?;
    if trailing_len != 0 {
        return Err(ArchiveLoadError::ReadMember {
            wire_ordinal,
            source: invalid_zip("member exceeds its declared decompressed size"),
        });
    }
    Ok(payload)
}

fn stable_member_name(
    raw_name: &[u8],
    flags: u16,
    is_directory: bool,
) -> Result<&str, ArchiveMemberNameError> {
    if raw_name.is_empty() {
        return Err(ArchiveMemberNameError::Empty);
    }
    if raw_name.len() > MAX_MEMBER_PATH_BYTES + usize::from(is_directory) {
        return Err(ArchiveMemberNameError::TooLong);
    }
    if raw_name.iter().any(|byte| !byte.is_ascii()) && flags & ZIP_UTF8_FLAG == 0 {
        return Err(ArchiveMemberNameError::UnstableEncoding);
    }
    let name =
        std::str::from_utf8(raw_name).map_err(|_| ArchiveMemberNameError::UnstableEncoding)?;
    if name.contains('\\') {
        return Err(ArchiveMemberNameError::Backslash);
    }
    let identity = if is_directory {
        name.strip_suffix('/')
            .ok_or(ArchiveMemberNameError::TraversalComponent)?
    } else {
        name
    };
    if identity.is_empty() {
        return Err(ArchiveMemberNameError::Empty);
    }
    if identity.len() > MAX_MEMBER_PATH_BYTES {
        return Err(ArchiveMemberNameError::TooLong);
    }
    let bytes = identity.as_bytes();
    let has_drive_prefix =
        bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic);
    if identity.starts_with('/') || has_drive_prefix {
        return Err(ArchiveMemberNameError::Absolute);
    }
    if identity.chars().any(char::is_control) {
        return Err(ArchiveMemberNameError::ControlCharacter);
    }
    if identity
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(ArchiveMemberNameError::TraversalComponent);
    }
    Ok(identity)
}

fn preflight_zip_directory(bytes: &[u8]) -> io::Result<ZipDirectoryPreflight> {
    let file_len = u64::try_from(bytes.len())
        .map_err(|_| invalid_zip("archive length does not fit in u64"))?;
    if bytes.len() < ZIP_EOCD_FIXED_LEN {
        return Err(invalid_zip("archive is shorter than the ZIP end record"));
    }

    let tail_len = bytes.len().min(ZIP_PREFLIGHT_TAIL_LEN);
    let tail_start_usize = bytes
        .len()
        .checked_sub(tail_len)
        .ok_or_else(|| invalid_zip("ZIP tail range underflow"))?;
    let tail_start = u64::try_from(tail_start_usize)
        .map_err(|_| invalid_zip("ZIP tail start does not fit in u64"))?;
    let tail = &bytes[tail_start_usize..];
    let search_start = tail_len.saturating_sub(ZIP_EOCD_SEARCH_LEN);
    let last_candidate = tail_len
        .checked_sub(ZIP_EOCD_FIXED_LEN)
        .ok_or_else(|| invalid_zip("archive is shorter than the ZIP end record"))?;

    let mut last_error = None;
    let mut valid_candidate = None;
    for candidate in (search_start..=last_candidate).rev() {
        if zip_u32(tail, candidate) != Some(ZIP_EOCD_SIGNATURE) {
            continue;
        }
        let Some(comment_len) = zip_u16(tail, candidate + 20).map(usize::from) else {
            continue;
        };
        let Some(candidate_end) = candidate
            .checked_add(ZIP_EOCD_FIXED_LEN)
            .and_then(|offset| offset.checked_add(comment_len))
        else {
            continue;
        };
        if candidate_end != tail_len {
            continue;
        }

        match validate_zip_end_candidate(file_len, tail, tail_start, candidate) {
            Ok(preflight) => {
                if valid_candidate.is_some() {
                    return Err(invalid_zip("archive has ambiguous valid ZIP end records"));
                }
                valid_candidate = Some(preflight);
            }
            Err(error) => last_error = Some(error),
        }
    }
    let preflight = valid_candidate.ok_or_else(|| {
        last_error.unwrap_or_else(|| invalid_zip("could not find a valid ZIP end record"))
    })?;
    validate_zip_decoder_selection(bytes, preflight)?;
    Ok(preflight)
}

fn validate_zip_end_candidate(
    file_len: u64,
    tail: &[u8],
    tail_start: u64,
    eocd: usize,
) -> io::Result<ZipDirectoryPreflight> {
    let eocd_position = tail_start
        .checked_add(u64::try_from(eocd).map_err(|_| invalid_zip("ZIP end offset overflow"))?)
        .ok_or_else(|| invalid_zip("ZIP end position overflow"))?;
    let disk_number = required_zip_u16(tail, eocd + 4, "ZIP disk number")?;
    let directory_disk = required_zip_u16(tail, eocd + 6, "ZIP directory disk")?;
    let entries_on_disk = required_zip_u16(tail, eocd + 8, "ZIP disk entry count")?;
    let entries = required_zip_u16(tail, eocd + 10, "ZIP entry count")?;
    let directory_size = required_zip_u32(tail, eocd + 12, "ZIP directory size")?;
    let directory_offset = required_zip_u32(tail, eocd + 16, "ZIP directory offset")?;
    let comment_size = required_zip_u16(tail, eocd + 20, "ZIP comment length")?;

    let locator = eocd
        .checked_sub(ZIP64_LOCATOR_LEN)
        .filter(|offset| zip_u32(tail, *offset) == Some(ZIP64_LOCATOR_SIGNATURE));
    if let Some(locator) = locator {
        return validate_zip64_directory(
            file_len,
            tail,
            tail_start,
            locator,
            eocd_position,
            comment_size,
        );
    }

    let needs_zip64 = disk_number == u16::MAX
        || directory_disk == u16::MAX
        || entries_on_disk == u16::MAX
        || entries == u16::MAX
        || directory_size == u32::MAX
        || directory_offset == u32::MAX;
    if needs_zip64 {
        return Err(invalid_zip("ZIP64 end locator is missing"));
    }
    if disk_number != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(invalid_zip("multi-disk ZIP archives are not supported"));
    }

    let directory_size = u64::from(directory_size);
    let nominal_directory = u64::from(directory_offset);
    let directory_start = eocd_position
        .checked_sub(directory_size)
        .ok_or_else(|| invalid_zip("ZIP central directory starts before the archive"))?;
    if nominal_directory > directory_start {
        return Err(invalid_zip(
            "ZIP central directory offset is outside the archive",
        ));
    }
    let directory_end = directory_start
        .checked_add(directory_size)
        .ok_or_else(|| invalid_zip("ZIP central directory range overflow"))?;
    if directory_end != eocd_position || directory_end > file_len {
        return Err(invalid_zip(
            "ZIP central directory extends beyond the archive",
        ));
    }
    Ok(ZipDirectoryPreflight {
        member_count: u64::from(entries),
        directory_start,
        directory_size,
        archive_offset: directory_start - nominal_directory,
        eocd_start: eocd_position,
        eocd_comment_size: u64::from(comment_size),
        zip64_nominal_record: None,
        zip64_record_start: None,
    })
}

fn validate_zip64_directory(
    file_len: u64,
    tail: &[u8],
    tail_start: u64,
    locator: usize,
    eocd_position: u64,
    comment_size: u16,
) -> io::Result<ZipDirectoryPreflight> {
    let locator_position = tail_start
        .checked_add(
            u64::try_from(locator).map_err(|_| invalid_zip("ZIP64 locator offset overflow"))?,
        )
        .ok_or_else(|| invalid_zip("ZIP64 locator position overflow"))?;
    if locator_position.checked_add(ZIP64_LOCATOR_LEN as u64) != Some(eocd_position) {
        return Err(invalid_zip(
            "ZIP64 locator is not adjacent to the ZIP end record",
        ));
    }
    let locator_disk = required_zip_u32(tail, locator + 4, "ZIP64 locator disk")?;
    let nominal_record = required_zip_u64(tail, locator + 8, "ZIP64 record offset")?;
    let disk_count = required_zip_u32(tail, locator + 16, "ZIP64 disk count")?;
    if locator_disk != 0 || disk_count != 1 {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }

    let record_search_start = locator.saturating_sub(ZIP64_RECORD_SEARCH_LEN);
    let last_record = locator
        .checked_sub(ZIP64_EOCD_MIN_LEN)
        .ok_or_else(|| invalid_zip("ZIP64 end record is truncated"))?;
    let mut record = None;
    for candidate in (record_search_start..=last_record).rev() {
        if zip_u32(tail, candidate) != Some(ZIP64_EOCD_SIGNATURE) {
            continue;
        }
        let Some(record_size) = zip_u64(tail, candidate + 4) else {
            continue;
        };
        if record_size < 44 {
            continue;
        }
        let Some(record_len) = record_size.checked_add(12) else {
            continue;
        };
        let Ok(record_len) = usize::try_from(record_len) else {
            continue;
        };
        if candidate.checked_add(record_len) == Some(locator) {
            record = Some(candidate);
            break;
        }
    }
    let record = record.ok_or_else(|| {
        invalid_zip("ZIP64 end record is invalid or exceeds the bounded preflight window")
    })?;
    let record_position = tail_start
        .checked_add(
            u64::try_from(record).map_err(|_| invalid_zip("ZIP64 record offset overflow"))?,
        )
        .ok_or_else(|| invalid_zip("ZIP64 record position overflow"))?;
    if nominal_record > record_position {
        return Err(invalid_zip(
            "ZIP64 nominal record offset is outside the archive",
        ));
    }
    let archive_offset = record_position - nominal_record;

    let disk_number = required_zip_u32(tail, record + 16, "ZIP64 disk number")?;
    let directory_disk = required_zip_u32(tail, record + 20, "ZIP64 directory disk")?;
    let entries_on_disk = required_zip_u64(tail, record + 24, "ZIP64 disk entry count")?;
    let entries = required_zip_u64(tail, record + 32, "ZIP64 entry count")?;
    let directory_size = required_zip_u64(tail, record + 40, "ZIP64 directory size")?;
    let nominal_directory = required_zip_u64(tail, record + 48, "ZIP64 directory offset")?;
    if disk_number != 0 || directory_disk != 0 || entries_on_disk != entries {
        return Err(invalid_zip("multi-disk ZIP64 archives are not supported"));
    }
    let directory_start = nominal_directory
        .checked_add(archive_offset)
        .ok_or_else(|| invalid_zip("ZIP64 central directory position overflow"))?;
    let directory_end = directory_start
        .checked_add(directory_size)
        .ok_or_else(|| invalid_zip("ZIP64 central directory range overflow"))?;
    if directory_end > record_position {
        return Err(invalid_zip(
            "ZIP64 central directory overlaps its end record",
        ));
    }
    if directory_end > file_len {
        return Err(invalid_zip(
            "ZIP64 central directory extends beyond the archive",
        ));
    }
    Ok(ZipDirectoryPreflight {
        member_count: entries,
        directory_start,
        directory_size,
        archive_offset,
        eocd_start: eocd_position,
        eocd_comment_size: u64::from(comment_size),
        zip64_nominal_record: Some(nominal_record),
        zip64_record_start: Some(record_position),
    })
}

fn validate_zip_decoder_selection(
    bytes: &[u8],
    preflight: ZipDirectoryPreflight,
) -> io::Result<()> {
    let eocd_start = usize::try_from(preflight.eocd_start)
        .map_err(|_| invalid_zip("ZIP end position does not fit in usize"))?;
    let decoder_last_candidate = bytes
        .len()
        .checked_sub(ZIP_EOCD_FIXED_LEN)
        .ok_or_else(|| invalid_zip("archive is shorter than the ZIP end record"))?;
    let trailing_start = eocd_start
        .checked_add(1)
        .ok_or_else(|| invalid_zip("ZIP end search range overflow"))?;
    if contains_zip_signature(
        bytes,
        trailing_start,
        decoder_last_candidate,
        ZIP_EOCD_SIGNATURE,
    ) {
        return Err(invalid_zip(
            "ZIP end record is ambiguous to the downstream ZIP decoder",
        ));
    }

    if let (Some(nominal), Some(actual)) =
        (preflight.zip64_nominal_record, preflight.zip64_record_start)
    {
        let nominal = usize::try_from(nominal)
            .map_err(|_| invalid_zip("ZIP64 nominal record does not fit in usize"))?;
        let actual = usize::try_from(actual)
            .map_err(|_| invalid_zip("ZIP64 record position does not fit in usize"))?;
        if nominal < actual
            && contains_zip_signature(bytes, nominal, actual - 1, ZIP64_EOCD_SIGNATURE)
        {
            return Err(invalid_zip(
                "ZIP64 end record is ambiguous to the downstream ZIP decoder",
            ));
        }
    }
    Ok(())
}

fn contains_zip_signature(bytes: &[u8], start: usize, end: usize, signature: u32) -> bool {
    if start > end {
        return false;
    }
    (start..=end).any(|position| zip_u32(bytes, position) == Some(signature))
}

fn walk_central_directory(
    bytes: &[u8],
    directory: ZipDirectoryPreflight,
    mut visit: impl FnMut(CentralEntry<'_>) -> Result<(), ArchiveLoadError>,
) -> Result<(), ArchiveLoadError> {
    let directory_start = usize_from_u64(directory.directory_start, "directory start")?;
    let directory_size = usize_from_u64(directory.directory_size, "directory size")?;
    let directory_end = directory_start.checked_add(directory_size).ok_or(
        ArchiveLoadError::ArithmeticOverflow {
            resource: "central-directory range",
        },
    )?;
    if directory_end > bytes.len() {
        return Err(invalid_structure(invalid_zip(
            "ZIP central directory extends beyond the archive",
        )));
    }
    let minimum_size = directory
        .member_count
        .checked_mul(ZIP_CENTRAL_HEADER_LEN as u64)
        .ok_or(ArchiveLoadError::ArithmeticOverflow {
            resource: "minimum central-directory size",
        })?;
    if minimum_size > directory.directory_size {
        return Err(invalid_structure(invalid_zip(
            "ZIP central directory is too small for its entry count",
        )));
    }

    let mut position = directory_start;
    for wire_ordinal in 0..directory.member_count {
        let fixed_end = position.checked_add(ZIP_CENTRAL_HEADER_LEN).ok_or(
            ArchiveLoadError::ArithmeticOverflow {
                resource: "central-directory header range",
            },
        )?;
        let header = bytes.get(position..fixed_end).ok_or_else(|| {
            invalid_structure(invalid_zip("ZIP central directory entry is truncated"))
        })?;
        if zip_u32(header, 0) != Some(ZIP_CENTRAL_HEADER_SIGNATURE) {
            return Err(invalid_structure(invalid_zip(
                "invalid ZIP central directory entry signature",
            )));
        }
        let flags = required_zip_u16(header, 8, "ZIP entry flags").map_err(invalid_structure)?;
        let method =
            required_zip_u16(header, 10, "ZIP compression method").map_err(invalid_structure)?;
        if flags & ZIP_ENCRYPTED_FLAG != 0 || method == 99 {
            return Err(invalid_structure(invalid_zip(
                "encrypted ZIP members are not supported by the workspace adapter",
            )));
        }
        let compressed32 =
            required_zip_u32(header, 20, "ZIP compressed size").map_err(invalid_structure)?;
        let decompressed32 =
            required_zip_u32(header, 24, "ZIP decompressed size").map_err(invalid_structure)?;
        let name_len = usize::from(
            required_zip_u16(header, 28, "ZIP entry name length").map_err(invalid_structure)?,
        );
        let extra_len = usize::from(
            required_zip_u16(header, 30, "ZIP extra field length").map_err(invalid_structure)?,
        );
        let comment_len = usize::from(
            required_zip_u16(header, 32, "ZIP entry comment length").map_err(invalid_structure)?,
        );
        let disk_start =
            required_zip_u16(header, 34, "ZIP entry disk start").map_err(invalid_structure)?;
        let local_offset32 =
            required_zip_u32(header, 42, "ZIP local header offset").map_err(invalid_structure)?;
        let name_start = fixed_end;
        let name_end =
            name_start
                .checked_add(name_len)
                .ok_or(ArchiveLoadError::ArithmeticOverflow {
                    resource: "central-directory name range",
                })?;
        let extra_end =
            name_end
                .checked_add(extra_len)
                .ok_or(ArchiveLoadError::ArithmeticOverflow {
                    resource: "central-directory extra range",
                })?;
        let entry_end =
            extra_end
                .checked_add(comment_len)
                .ok_or(ArchiveLoadError::ArithmeticOverflow {
                    resource: "central-directory comment range",
                })?;
        if entry_end > directory_end {
            return Err(invalid_structure(invalid_zip(
                "ZIP central directory entry is truncated",
            )));
        }
        let name = &bytes[name_start..name_end];
        let extra = &bytes[name_end..extra_end];
        let resolved = resolve_zip64_entry(
            compressed32,
            decompressed32,
            local_offset32,
            disk_start,
            extra,
        )
        .map_err(invalid_structure)?;
        if resolved.disk_start != 0 {
            return Err(invalid_structure(invalid_zip(
                "multi-disk ZIP entries are not supported",
            )));
        }
        validate_local_entry(
            bytes,
            directory,
            flags,
            method,
            name,
            resolved.local_header_offset,
            resolved.compressed_size,
        )
        .map_err(invalid_structure)?;
        visit(CentralEntry {
            wire_ordinal,
            flags,
            method,
            name,
            compressed_size: resolved.compressed_size,
            decompressed_size: resolved.decompressed_size,
            is_directory: name.ends_with(b"/"),
        })?;
        position = entry_end;
    }

    if position == directory_end {
        return Ok(());
    }
    let remaining = directory_end - position;
    if remaining < 6 {
        return Err(invalid_structure(invalid_zip(
            "ZIP central directory has trailing bytes",
        )));
    }
    let signature_header = &bytes[position..position + 6];
    if zip_u32(signature_header, 0) != Some(ZIP_CENTRAL_DIGITAL_SIGNATURE) {
        return Err(invalid_structure(invalid_zip(
            "ZIP central directory contains uncounted entries",
        )));
    }
    let signature_len = usize::from(
        required_zip_u16(
            signature_header,
            4,
            "ZIP central directory signature length",
        )
        .map_err(invalid_structure)?,
    );
    if signature_len.checked_add(6) != Some(remaining) {
        return Err(invalid_structure(invalid_zip(
            "ZIP central directory digital signature is truncated",
        )));
    }
    Ok(())
}

const fn codec_scratch_bytes(method: u16) -> u64 {
    match method {
        8 => ZIP_DEFLATE_DECODER_SCRATCH_BYTES,
        12 => ZIP_BZIP2_DECODER_SCRATCH_BYTES,
        93 => ZIP_ZSTD_DECODER_SCRATCH_BYTES,
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy)]
struct ResolvedCentralEntry {
    compressed_size: u64,
    decompressed_size: u64,
    local_header_offset: u64,
    disk_start: u32,
}

fn resolve_zip64_entry(
    compressed32: u32,
    decompressed32: u32,
    local_offset32: u32,
    disk_start16: u16,
    extra: &[u8],
) -> io::Result<ResolvedCentralEntry> {
    let needs_decompressed = decompressed32 == u32::MAX;
    let needs_compressed = compressed32 == u32::MAX;
    let needs_local_offset = local_offset32 == u32::MAX;
    let needs_disk_start = disk_start16 == u16::MAX;
    let needs_zip64 =
        needs_decompressed || needs_compressed || needs_local_offset || needs_disk_start;
    let zip64 = find_zip64_extra(extra)?;
    if needs_zip64 && zip64.is_none() {
        return Err(invalid_zip("ZIP64 entry extra field is missing"));
    }
    let mut position = 0_usize;
    let zip64 = zip64.unwrap_or_default();

    let decompressed_size = if needs_decompressed {
        take_zip64_u64(zip64, &mut position, "ZIP64 decompressed size")?
    } else {
        u64::from(decompressed32)
    };
    let compressed_size = if needs_compressed {
        take_zip64_u64(zip64, &mut position, "ZIP64 compressed size")?
    } else {
        u64::from(compressed32)
    };
    let local_header_offset = if needs_local_offset {
        take_zip64_u64(zip64, &mut position, "ZIP64 local header offset")?
    } else {
        u64::from(local_offset32)
    };
    let disk_start = if needs_disk_start {
        take_zip64_u32(zip64, &mut position, "ZIP64 disk start")?
    } else {
        u32::from(disk_start16)
    };
    Ok(ResolvedCentralEntry {
        compressed_size,
        decompressed_size,
        local_header_offset,
        disk_start,
    })
}

fn find_zip64_extra(extra: &[u8]) -> io::Result<Option<&[u8]>> {
    let mut position = 0_usize;
    let mut zip64 = None;
    while position < extra.len() {
        let header_end = position
            .checked_add(4)
            .ok_or_else(|| invalid_zip("ZIP extra field header range overflow"))?;
        if header_end > extra.len() {
            return Err(invalid_zip("ZIP extra field header is truncated"));
        }
        let field_id = required_zip_u16(extra, position, "ZIP extra field id")?;
        let field_len = usize::from(required_zip_u16(
            extra,
            position + 2,
            "ZIP extra field length",
        )?);
        let field_end = header_end
            .checked_add(field_len)
            .ok_or_else(|| invalid_zip("ZIP extra field range overflow"))?;
        let field = extra
            .get(header_end..field_end)
            .ok_or_else(|| invalid_zip("ZIP extra field is truncated"))?;
        if field_id == ZIP_AES_EXTRA_FIELD_ID {
            return Err(invalid_zip(
                "AES ZIP members are not supported by the workspace adapter",
            ));
        }
        if field_id == ZIP64_EXTRA_FIELD_ID && zip64.replace(field).is_some() {
            return Err(invalid_zip("ZIP64 entry extra field is duplicated"));
        }
        position = field_end;
    }
    Ok(zip64)
}

fn take_zip64_u64(bytes: &[u8], position: &mut usize, field: &str) -> io::Result<u64> {
    let value = required_zip_u64(bytes, *position, field)?;
    *position = position
        .checked_add(8)
        .ok_or_else(|| invalid_zip(format!("{field} position overflow")))?;
    Ok(value)
}

fn take_zip64_u32(bytes: &[u8], position: &mut usize, field: &str) -> io::Result<u32> {
    let value = required_zip_u32(bytes, *position, field)?;
    *position = position
        .checked_add(4)
        .ok_or_else(|| invalid_zip(format!("{field} position overflow")))?;
    Ok(value)
}

fn validate_local_entry(
    bytes: &[u8],
    directory: ZipDirectoryPreflight,
    flags: u16,
    method: u16,
    central_name: &[u8],
    nominal_local_offset: u64,
    compressed_size: u64,
) -> io::Result<()> {
    let local_offset = nominal_local_offset
        .checked_add(directory.archive_offset)
        .ok_or_else(|| invalid_zip("ZIP local header position overflow"))?;
    let fixed_end = local_offset
        .checked_add(ZIP_LOCAL_HEADER_LEN as u64)
        .ok_or_else(|| invalid_zip("ZIP local header range overflow"))?;
    if fixed_end > directory.directory_start {
        return Err(invalid_zip(
            "ZIP local header overlaps the central directory",
        ));
    }
    let local_offset = usize::try_from(local_offset)
        .map_err(|_| invalid_zip("ZIP local header position does not fit in usize"))?;
    let fixed_end = usize::try_from(fixed_end)
        .map_err(|_| invalid_zip("ZIP local header end does not fit in usize"))?;
    let header = bytes
        .get(local_offset..fixed_end)
        .ok_or_else(|| invalid_zip("ZIP local header is truncated"))?;
    if zip_u32(header, 0) != Some(ZIP_LOCAL_HEADER_SIGNATURE) {
        return Err(invalid_zip("invalid ZIP local header signature"));
    }
    let local_flags = required_zip_u16(header, 6, "ZIP local flags")?;
    let local_method = required_zip_u16(header, 8, "ZIP local compression method")?;
    if local_flags != flags || local_method != method {
        return Err(invalid_zip(
            "ZIP local header flags or compression method differ from the central directory",
        ));
    }
    let local_name_len = usize::from(required_zip_u16(header, 26, "ZIP local name length")?);
    let local_extra_len = usize::from(required_zip_u16(header, 28, "ZIP local extra length")?);
    let name_end = fixed_end
        .checked_add(local_name_len)
        .ok_or_else(|| invalid_zip("ZIP local name range overflow"))?;
    let data_start = name_end
        .checked_add(local_extra_len)
        .ok_or_else(|| invalid_zip("ZIP local extra range overflow"))?;
    let data_end = u64::try_from(data_start)
        .map_err(|_| invalid_zip("ZIP member data start does not fit in u64"))?
        .checked_add(compressed_size)
        .ok_or_else(|| invalid_zip("ZIP compressed member range overflow"))?;
    if data_end > directory.directory_start || data_start > bytes.len() {
        return Err(invalid_zip(
            "ZIP compressed member extends into the central directory",
        ));
    }
    let local_name = bytes
        .get(fixed_end..name_end)
        .ok_or_else(|| invalid_zip("ZIP local member name is truncated"))?;
    if local_name != central_name {
        return Err(invalid_zip(
            "ZIP local member name differs from the central directory",
        ));
    }
    Ok(())
}

fn checked_sized_allocation<T>(
    count: u64,
    resource: &'static str,
) -> Result<u64, ArchiveLoadError> {
    let element_size = u64_from_usize(size_of::<T>(), resource)?;
    checked_mul(count, element_size, resource)
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ArchiveLoadError> {
    left.checked_add(right)
        .ok_or(ArchiveLoadError::ArithmeticOverflow { resource })
}

fn checked_mul(left: u64, right: u64, resource: &'static str) -> Result<u64, ArchiveLoadError> {
    left.checked_mul(right)
        .ok_or(ArchiveLoadError::ArithmeticOverflow { resource })
}

fn usize_from_u64(value: u64, resource: &'static str) -> Result<usize, ArchiveLoadError> {
    usize::try_from(value).map_err(|_| ArchiveLoadError::ArithmeticOverflow { resource })
}

fn u64_from_usize(value: usize, resource: &'static str) -> Result<u64, ArchiveLoadError> {
    u64::try_from(value).map_err(|_| ArchiveLoadError::ArithmeticOverflow { resource })
}

fn try_vec_with_capacity<T>(
    capacity: usize,
    resource: &'static str,
) -> Result<Vec<T>, ArchiveLoadError> {
    let requested = capacity
        .checked_mul(size_of::<T>())
        .ok_or(ArchiveLoadError::ArithmeticOverflow { resource })?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| ArchiveLoadError::Allocation {
            resource,
            requested,
            source,
        })?;
    Ok(values)
}

fn try_string_with_capacity(
    capacity: usize,
    resource: &'static str,
) -> Result<String, ArchiveLoadError> {
    let mut value = String::new();
    value
        .try_reserve_exact(capacity)
        .map_err(|source| ArchiveLoadError::Allocation {
            resource,
            requested: capacity,
            source,
        })?;
    Ok(value)
}

fn budget_error(operation: &'static str, source: BudgetError) -> ArchiveLoadError {
    ArchiveLoadError::Budget { operation, source }
}

fn invalid_structure(source: io::Error) -> ArchiveLoadError {
    ArchiveLoadError::InvalidStructure { source }
}

fn zip_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value: [u8; 2] = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
    Some(u16::from_le_bytes(value))
}

fn zip_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value: [u8; 4] = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

fn zip_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value: [u8; 8] = bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?;
    Some(u64::from_le_bytes(value))
}

fn required_zip_u16(bytes: &[u8], offset: usize, field: &str) -> io::Result<u16> {
    zip_u16(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn required_zip_u32(bytes: &[u8], offset: usize, field: &str) -> io::Result<u32> {
    zip_u32(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn required_zip_u64(bytes: &[u8], offset: usize, field: &str) -> io::Result<u64> {
    zip_u64(bytes, offset).ok_or_else(|| invalid_zip(format!("{field} is truncated")))
}

fn invalid_zip(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use unity_asset_core::AssetLoadLimits;
    use zip::CompressionMethod;
    use zip::write::FileOptions;

    use super::*;

    fn zip_bytes(entries: &[(&str, Option<&[u8]>)]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, payload) in entries {
            match payload {
                Some(payload) => {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(payload).unwrap();
                }
                None => writer.add_directory(*name, options).unwrap(),
            }
        }
        writer.finish().unwrap().into_inner()
    }

    fn with_central_comment(mut bytes: Vec<u8>, comment: &[u8]) -> Vec<u8> {
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let name_len = usize::from(zip_u16(&bytes, central + 28).unwrap());
        let extra_len = usize::from(zip_u16(&bytes, central + 30).unwrap());
        let insert_at = central + ZIP_CENTRAL_HEADER_LEN + name_len + extra_len;
        bytes[central + 32..central + 34]
            .copy_from_slice(&u16::try_from(comment.len()).unwrap().to_le_bytes());
        bytes.splice(insert_at..insert_at, comment.iter().copied());
        let eocd = bytes
            .windows(4)
            .rposition(|window| window == ZIP_EOCD_SIGNATURE.to_le_bytes())
            .unwrap();
        let directory_size = zip_u32(&bytes, eocd + 12).unwrap();
        let comment_len = u32::try_from(comment.len()).unwrap();
        bytes[eocd + 12..eocd + 16].copy_from_slice(
            &directory_size
                .checked_add(comment_len)
                .unwrap()
                .to_le_bytes(),
        );
        bytes
    }

    fn with_central_extra(mut bytes: Vec<u8>, extra: &[u8]) -> Vec<u8> {
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let name_len = usize::from(zip_u16(&bytes, central + 28).unwrap());
        let extra_len = usize::from(zip_u16(&bytes, central + 30).unwrap());
        let insert_at = central + ZIP_CENTRAL_HEADER_LEN + name_len + extra_len;
        let new_extra_len = extra_len.checked_add(extra.len()).unwrap();
        bytes[central + 30..central + 32]
            .copy_from_slice(&u16::try_from(new_extra_len).unwrap().to_le_bytes());
        bytes.splice(insert_at..insert_at, extra.iter().copied());
        let eocd = bytes
            .windows(4)
            .rposition(|window| window == ZIP_EOCD_SIGNATURE.to_le_bytes())
            .unwrap();
        let directory_size = zip_u32(&bytes, eocd + 12).unwrap();
        bytes[eocd + 12..eocd + 16].copy_from_slice(
            &directory_size
                .checked_add(u32::try_from(extra.len()).unwrap())
                .unwrap()
                .to_le_bytes(),
        );
        bytes
    }

    fn budget_with(mut limits: AssetLoadLimits) -> AssetLoadBudget {
        limits.max_bytes = limits.max_bytes.max(1);
        AssetLoadBudget::new(limits).unwrap()
    }

    fn empty_zip64() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ZIP64_EOCD_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&44_u64.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&45_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&ZIP64_LOCATOR_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&ZIP_EOCD_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes
    }

    #[test]
    fn preserves_wire_order_and_assigns_duplicate_occurrences() {
        let bytes = zip_bytes(&[
            ("folder/", None),
            ("same.assets", Some(b"first")),
            ("other.assets", Some(b"middle")),
            ("same.assets", Some(b"second")),
        ]);
        let mut budget = AssetLoadBudget::default();

        let members = load_zip_archive(Arc::from(bytes), &mut budget).unwrap();

        assert_eq!(members.len(), 3);
        assert_eq!(members[0].wire_ordinal, 1);
        assert_eq!(members[0].member_id.name(), "same.assets");
        assert_eq!(members[0].member_id.same_name_occurrence(), 0);
        assert_eq!(members[0].bytes.as_ref(), b"first");
        assert_eq!(members[1].wire_ordinal, 2);
        assert_eq!(members[1].member_id.name(), "other.assets");
        assert_eq!(members[2].wire_ordinal, 3);
        assert_eq!(members[2].member_id.same_name_occurrence(), 1);
        assert_eq!(members[2].bytes.as_ref(), b"second");
        assert!(
            members
                .iter()
                .all(|member| member.bytes.validate_budget(&budget).is_ok())
        );
        assert_eq!(budget.usage().members, 4);
        assert_eq!(budget.usage().compressed_bytes, 17);
        assert_eq!(budget.usage().decompressed_bytes, 17);
    }

    #[test]
    fn rejects_member_flood_before_publishing_payloads() {
        let bytes = zip_bytes(&[
            ("directory/", None),
            ("first.assets", Some(b"one")),
            ("second.assets", Some(b"two")),
        ]);
        let limits = AssetLoadLimits {
            max_members: 2,
            ..AssetLoadLimits::default()
        };
        let mut budget = budget_with(limits);

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(matches!(
            error,
            ArchiveLoadError::Budget {
                operation: "member traversal",
                ..
            }
        ));
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn rejects_traversal_backslash_absolute_and_nul_names() {
        for name in ["../escape.assets", "dir\\file.assets", "/absolute.assets"] {
            let bytes = zip_bytes(&[(name, Some(b"payload"))]);
            let mut budget = AssetLoadBudget::default();
            assert!(matches!(
                load_zip_archive(Arc::from(bytes), &mut budget),
                Err(ArchiveLoadError::InvalidMemberName { .. })
            ));
        }

        let mut bytes = zip_bytes(&[("nulx.assets", Some(b"payload"))]);
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let local = bytes
            .windows(4)
            .position(|window| window == ZIP_LOCAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        bytes[local + ZIP_LOCAL_HEADER_LEN + 3] = 0;
        bytes[central + ZIP_CENTRAL_HEADER_LEN + 3] = 0;
        let mut budget = AssetLoadBudget::default();
        assert!(matches!(
            load_zip_archive(Arc::from(bytes), &mut budget),
            Err(ArchiveLoadError::InvalidMemberName {
                reason: ArchiveMemberNameError::ControlCharacter,
                ..
            })
        ));
    }

    #[test]
    fn rejects_ambiguous_non_ascii_names_without_utf8_flag() {
        let mut bytes = zip_bytes(&[("café.assets", Some(b"payload"))]);
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let local = bytes
            .windows(4)
            .position(|window| window == ZIP_LOCAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let central_flags = zip_u16(&bytes, central + 8).unwrap() & !ZIP_UTF8_FLAG;
        let local_flags = zip_u16(&bytes, local + 6).unwrap() & !ZIP_UTF8_FLAG;
        bytes[central + 8..central + 10].copy_from_slice(&central_flags.to_le_bytes());
        bytes[local + 6..local + 8].copy_from_slice(&local_flags.to_le_bytes());
        let mut budget = AssetLoadBudget::default();

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(matches!(
            error,
            ArchiveLoadError::InvalidMemberName {
                reason: ArchiveMemberNameError::UnstableEncoding,
                ..
            }
        ));
    }

    #[test]
    fn rejects_aes_extra_without_panicking_even_when_encryption_flag_is_clear() {
        let baseline = zip_bytes(&[("plain.assets", Some(b"payload"))]);
        let aes_extra = [
            0x01, 0x99, // field ID 0x9901
            0x07, 0x00, // payload length
            0x02, 0x00, b'A', b'E', 0x01, 0x00, 0x00,
        ];
        let bytes = with_central_extra(baseline, &aes_extra);

        let result = std::panic::catch_unwind(|| {
            load_zip_archive(Arc::from(bytes), &mut AssetLoadBudget::default())
        });

        assert!(
            result.is_ok(),
            "AES metadata must not reach zip's panic path"
        );
        assert!(matches!(
            result.unwrap(),
            Err(ArchiveLoadError::InvalidStructure { .. })
        ));
    }

    #[test]
    fn rejects_per_member_expansion_before_ziparchive_decode() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file("compressed.assets", options).unwrap();
        writer.write_all(&vec![0_u8; 64 * 1024]).unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let limits = AssetLoadLimits {
            max_expansion_ratio: 1,
            ..AssetLoadLimits::default()
        };
        let mut budget = budget_with(limits);

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(matches!(
            error,
            ArchiveLoadError::Budget {
                operation: "member decompression",
                ..
            }
        ));
        assert_eq!(budget.usage().members, 0);
        assert_eq!(budget.usage().decompressed_bytes, 0);
    }

    #[test]
    fn zstd_decoder_scratch_is_charged_before_decoder_creation() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = FileOptions::default().compression_method(CompressionMethod::Zstd);
        writer.start_file("compressed.assets", options).unwrap();
        writer.write_all(b"tiny payload").unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        let directory = preflight_zip_directory(&bytes).unwrap();
        let totals = preflight_members(&bytes, directory, &AssetLoadBudget::default()).unwrap();
        assert_eq!(totals.decompressed_bytes, 12);
        assert_eq!(totals.codec_scratch_bytes, ZIP_ZSTD_DECODER_SCRATCH_BYTES);
        let limits = AssetLoadLimits {
            max_bytes: 1024 * 1024,
            ..AssetLoadLimits::default()
        };
        let mut budget = budget_with(limits);

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(matches!(
            error,
            ArchiveLoadError::Budget {
                operation: "archive adapter allocations",
                ..
            }
        ));
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn occurrence_width_is_checked_before_allocation() {
        validate_occurrence_capacity(MAX_OCCURRENCE_MEMBER_COUNT).unwrap();
        assert!(matches!(
            validate_occurrence_capacity(MAX_OCCURRENCE_MEMBER_COUNT + 1),
            Err(ArchiveLoadError::OccurrenceOverflow { wire_ordinal })
                if wire_ordinal == MAX_OCCURRENCE_MEMBER_COUNT
        ));
    }

    #[test]
    fn output_allocation_budget_has_an_exact_preflight_boundary() {
        let bytes = zip_bytes(&[("file.assets", Some(b"payload"))]);
        let directory = preflight_zip_directory(&bytes).unwrap();
        let totals = preflight_members(&bytes, directory, &AssetLoadBudget::default()).unwrap();
        let required = planned_allocation_bytes(directory, totals).unwrap();

        let insufficient_limits = AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        };
        let mut insufficient = budget_with(insufficient_limits);
        let error = load_zip_archive(Arc::from(bytes.clone()), &mut insufficient).unwrap_err();
        assert!(matches!(
            error,
            ArchiveLoadError::Budget {
                operation: "archive adapter allocations",
                ..
            }
        ));
        assert_eq!(insufficient.usage().members, 0);
        assert_eq!(insufficient.usage().bytes, 0);
        assert_eq!(insufficient.usage().decompressed_bytes, 0);

        let exact_limits = AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        };
        let mut exact = budget_with(exact_limits);
        let members = load_zip_archive(Arc::from(bytes), &mut exact).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(exact.usage().bytes, required);
    }

    #[test]
    fn eocd_comment_is_budgeted_and_cannot_redirect_ziparchive() {
        let baseline = zip_bytes(&[]);
        assert_eq!(zip_u32(&baseline, 0), Some(ZIP_EOCD_SIGNATURE));
        let baseline_directory = preflight_zip_directory(&baseline).unwrap();
        let baseline_totals =
            preflight_members(&baseline, baseline_directory, &AssetLoadBudget::default()).unwrap();
        let baseline_bytes = planned_allocation_bytes(baseline_directory, baseline_totals).unwrap();

        let comment = b"stable archive comment";
        let mut commented = baseline.clone();
        commented[20..22].copy_from_slice(&(comment.len() as u16).to_le_bytes());
        commented.extend_from_slice(comment);
        let commented_directory = preflight_zip_directory(&commented).unwrap();
        let commented_totals =
            preflight_members(&commented, commented_directory, &AssetLoadBudget::default())
                .unwrap();
        let commented_bytes =
            planned_allocation_bytes(commented_directory, commented_totals).unwrap();
        assert_eq!(commented_bytes - baseline_bytes, comment.len() as u64);

        // zip 0.6 scans backward for the first signature without validating that its declared
        // comment reaches EOF. This invalid trailing record would otherwise make it reserve for
        // 65,000 members even though the strict record declares an empty archive.
        let padding_len = 65_000_usize;
        let mut ambiguous = baseline;
        let fake_eocd_start = ambiguous.len() + padding_len;
        let comment_len = padding_len + ZIP_EOCD_FIXED_LEN;
        ambiguous[20..22].copy_from_slice(&(comment_len as u16).to_le_bytes());
        ambiguous.resize(fake_eocd_start, 0);
        ambiguous.extend_from_slice(&ZIP_EOCD_SIGNATURE.to_le_bytes());
        ambiguous.extend_from_slice(&0_u16.to_le_bytes());
        ambiguous.extend_from_slice(&0_u16.to_le_bytes());
        ambiguous.extend_from_slice(&65_000_u16.to_le_bytes());
        ambiguous.extend_from_slice(&0_u16.to_le_bytes());
        ambiguous.extend_from_slice(&0_u32.to_le_bytes());
        ambiguous.extend_from_slice(&(fake_eocd_start as u32).to_le_bytes());
        ambiguous.extend_from_slice(&0_u16.to_le_bytes());
        let mut budget = AssetLoadBudget::default();

        let error = load_zip_archive(Arc::from(ambiguous), &mut budget).unwrap_err();

        let ArchiveLoadError::InvalidStructure { source } = error else {
            panic!("expected a structured ZIP error");
        };
        assert!(source.to_string().contains("ambiguous"));
        assert_eq!(budget.usage(), Default::default());
    }

    #[test]
    fn non_ascii_central_comment_expansion_is_budgeted() {
        let baseline = zip_bytes(&[("file.assets", Some(b"payload"))]);
        let baseline_directory = preflight_zip_directory(&baseline).unwrap();
        let baseline_totals =
            preflight_members(&baseline, baseline_directory, &AssetLoadBudget::default()).unwrap();
        let baseline_bytes = planned_allocation_bytes(baseline_directory, baseline_totals).unwrap();

        let comment = vec![0xdb_u8; 4 * 1024];
        let commented = with_central_comment(baseline, &comment);
        let directory = preflight_zip_directory(&commented).unwrap();
        let totals = preflight_members(&commented, directory, &AssetLoadBudget::default()).unwrap();
        let required = planned_allocation_bytes(directory, totals).unwrap();
        assert_eq!(
            required - baseline_bytes,
            u64::try_from(comment.len()).unwrap() * ZIP_PARSER_DIRECTORY_MULTIPLIER
        );

        let insufficient_limits = AssetLoadLimits {
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        };
        let mut insufficient = budget_with(insufficient_limits);
        assert!(matches!(
            load_zip_archive(Arc::from(commented.clone()), &mut insufficient),
            Err(ArchiveLoadError::Budget {
                operation: "archive adapter allocations",
                ..
            })
        ));
        assert_eq!(insufficient.usage(), Default::default());

        let exact_limits = AssetLoadLimits {
            max_bytes: required,
            ..AssetLoadLimits::default()
        };
        let mut exact = budget_with(exact_limits);
        let members = load_zip_archive(Arc::from(commented), &mut exact).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(exact.usage().bytes, required);
    }

    #[test]
    fn zip64_preflight_validates_locator_and_directory_ranges() {
        let valid = empty_zip64();
        assert_eq!(preflight_zip_directory(&valid).unwrap().member_count, 0);

        let mut invalid_locator = valid.clone();
        invalid_locator[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        let locator_error = preflight_zip_directory(&invalid_locator).unwrap_err();
        assert!(locator_error.to_string().contains("offset"));

        let mut overflowing_directory = valid;
        overflowing_directory[40..48].copy_from_slice(&u64::MAX.to_le_bytes());
        overflowing_directory[48..56].copy_from_slice(&u64::MAX.to_le_bytes());
        preflight_zip_directory(&overflowing_directory)
            .expect_err("overflowing ZIP64 central-directory ranges must be rejected");

        let mut ambiguous_record = vec![0_u8; 64];
        ambiguous_record[..4].copy_from_slice(&ZIP64_EOCD_SIGNATURE.to_le_bytes());
        ambiguous_record.extend_from_slice(&empty_zip64());
        let error = preflight_zip_directory(&ambiguous_record)
            .expect_err("the downstream decoder must select the preflighted ZIP64 record");
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn unsupported_compression_preserves_zip_source() {
        let mut bytes = zip_bytes(&[("file.assets", Some(b"payload"))]);
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        let local = bytes
            .windows(4)
            .position(|window| window == ZIP_LOCAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        bytes[central + 10..central + 12].copy_from_slice(&255_u16.to_le_bytes());
        bytes[local + 8..local + 10].copy_from_slice(&255_u16.to_le_bytes());
        let mut budget = AssetLoadBudget::default();

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(
            matches!(
                error,
                ArchiveLoadError::OpenMember {
                    source: ZipError::UnsupportedArchive(_),
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_central_directory_preserves_io_source() {
        let mut bytes = zip_bytes(&[("file.assets", Some(b"payload"))]);
        let central = bytes
            .windows(4)
            .position(|window| window == ZIP_CENTRAL_HEADER_SIGNATURE.to_le_bytes())
            .unwrap();
        bytes[central..central + 4].fill(0);
        let mut budget = AssetLoadBudget::default();

        let error = load_zip_archive(Arc::from(bytes), &mut budget).unwrap_err();

        assert!(matches!(error, ArchiveLoadError::InvalidStructure { .. }));
        assert!(std::error::Error::source(&error).is_some());
    }
}
