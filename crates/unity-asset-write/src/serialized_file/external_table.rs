use std::collections::TryReserveError;
use std::fmt;

use thiserror::Error;
use unity_asset_binary::asset::{
    ExternalEncoding, FileIdentifier, SerializedFile, SerializedFileFormat,
};
use unity_asset_core::{
    AllocationSizeError, AssetLoadBudget, BudgetError, DigestBuildError, DigestV1, DigestV1Builder,
    string_allocation_bytes, vec_allocation_bytes,
};

use super::SerializedFileEdits;

const EXTERNAL_TABLE_IDENTITY_DOMAIN: &[u8] = b"unity-asset:serialized-file:externals:v1";
const EXTERNAL_ALLOCATION_RESOURCE: &str = "serialized_file_external_table";

/// A field that cannot be represented by the current SerializedFile external encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalIdentifierField {
    AssetPath,
    Guid,
    Type,
    AssetPathNul,
    PathNul,
}

impl fmt::Display for ExternalIdentifierField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AssetPath => "asset path",
            Self::Guid => "GUID",
            Self::Type => "type",
            Self::AssetPathNul => "asset path containing NUL",
            Self::PathNul => "path containing NUL",
        };
        formatter.write_str(name)
    }
}

/// Metadata that disagrees for two identifiers with the same canonical path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalMetadataField {
    AssetPath,
    Guid,
    Type,
}

impl fmt::Display for ExternalMetadataField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AssetPath => "asset path",
            Self::Guid => "GUID",
            Self::Type => "type",
        };
        formatter.write_str(name)
    }
}

/// Typed failures produced while planning a SerializedFile external table.
#[derive(Debug, Error)]
pub enum ExternalTableError {
    #[error(
        "SerializedFile header version {header_version} disagrees with retained format {format_version}"
    )]
    FormatVersionMismatch {
        header_version: u32,
        format_version: u32,
    },
    #[error(
        "SerializedFile v{version} cannot encode external {index} field {field} with {encoding:?}"
    )]
    Unrepresentable {
        version: u32,
        encoding: ExternalEncoding,
        index: usize,
        field: ExternalIdentifierField,
    },
    #[error(
        "external {candidate_index} conflicts with external {existing_index} for canonical-path {field} metadata"
    )]
    CanonicalPathConflict {
        existing_index: usize,
        candidate_index: usize,
        field: ExternalMetadataField,
    },
    #[error("external table index {index} cannot be represented as a positive i32 file ID")]
    FileIdOverflow { index: usize },
    #[error("external additions are already attached to these SerializedFile edits")]
    EditsAlreadyContainExternals,
    #[error(
        "external edits were planned for SerializedFile v{expected_version} with {expected_count} existing entries, not v{actual_version} with {actual_count} entries"
    )]
    BaseTableShapeChanged {
        expected_version: u32,
        actual_version: u32,
        expected_count: usize,
        actual_count: usize,
    },
    #[error("the SerializedFile external table changed after its file IDs were allocated")]
    BaseTableContentChanged,
    #[error(
        "planned external file ID {planned} changed to {actual} before the edit transaction committed"
    )]
    PlannedFileIdChanged { planned: i32, actual: i32 },
    #[error("failed to reserve {requested} external table entries: {source}")]
    Allocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    AllocationSize(#[from] AllocationSizeError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Digest(#[from] DigestBuildError),
    #[error("external table arithmetic overflow for {resource}")]
    ArithmeticOverflow { resource: &'static str },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExternalTableBase {
    format: SerializedFileFormat,
    count: usize,
    digest: DigestV1,
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalTableMutation {
    base: ExternalTableBase,
    additions: Vec<FileIdentifier>,
}

impl ExternalTableMutation {
    pub(crate) fn additions(&self) -> &[FileIdentifier] {
        &self.additions
    }
}

/// Deterministically allocates positive PPtr file IDs against one immutable external table.
///
/// Existing identifiers are never copied. New identifiers remain private until [`Self::finish`]
/// or [`Self::into_edits`] atomically transfers the complete plan into `SerializedFileEdits`.
pub struct ExternalTableAllocator<'file> {
    file: &'file SerializedFile,
    base: ExternalTableBase,
    additions: Vec<FileIdentifier>,
}

impl fmt::Debug for ExternalTableAllocator<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExternalTableAllocator")
            .field("format", &self.base.format.version())
            .field("existing", &self.base.count)
            .field("additions", &self.additions)
            .finish()
    }
}

impl<'file> ExternalTableAllocator<'file> {
    /// Validates the retained format and existing table before any file ID is returned.
    pub fn new(file: &'file SerializedFile) -> Result<Self, ExternalTableError> {
        validate_header_format(file)?;
        let format = file.format();
        validate_table(format, &file.externals, &[])?;
        let base = ExternalTableBase {
            format,
            count: file.externals.len(),
            digest: external_table_digest(&file.externals)?,
        };
        Ok(Self {
            file,
            base,
            additions: Vec::new(),
        })
    }

    /// Returns the existing or newly allocated positive PPtr file ID for `identifier`.
    ///
    /// Exact identifiers win first. Canonically equivalent path spellings reuse the lowest table
    /// index when their metadata agrees and fail with a typed conflict when it does not. A new
    /// identifier is appended in first-seen order; the allocator never sorts additions.
    pub fn intern(
        &mut self,
        identifier: FileIdentifier,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        let disposition = plan_identifier(
            self.base.format,
            &self.file.externals,
            &self.additions,
            &identifier,
        )?;
        if disposition.append {
            append_budgeted(&mut self.additions, identifier, budget)?;
        }
        Ok(disposition.file_id)
    }

    /// Plans the file ID against existing edits without retaining or allocating the identifier.
    ///
    /// This supports transactions that must encode candidate object bytes before publishing either
    /// the object replacement or a new external-table entry.
    pub fn planned_file_id(
        file: &SerializedFile,
        edits: &SerializedFileEdits,
        identifier: &FileIdentifier,
    ) -> Result<i32, ExternalTableError> {
        let table = PlannedExternalTable::build(file, edits)?;
        Ok(plan_identifier(file.format(), table.existing, table.additions, identifier)?.file_id)
    }

    /// Plans a file ID when the caller knows only the dependency path.
    ///
    /// A canonical path match reuses the retained identifier without inventing GUID or type
    /// metadata. A missing path is appended with the compatibility metadata Unity uses for an
    /// unresolved dependency.
    pub fn planned_file_id_for_path(
        file: &SerializedFile,
        edits: &SerializedFileEdits,
        path: &str,
    ) -> Result<i32, ExternalTableError> {
        let table = PlannedExternalTable::build(file, edits)?;
        Ok(plan_path(file.format(), table.existing, table.additions, path)?.file_id)
    }

    /// Atomically interns one identifier into an existing edit set.
    ///
    /// On error, the external additions attached to `edits` are unchanged. This is the continuation
    /// entry point for callers that retain one edit set across multiple object operations.
    pub fn intern_into_edits(
        file: &SerializedFile,
        edits: &mut SerializedFileEdits,
        identifier: FileIdentifier,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        Self::intern_into_edits_impl(
            file,
            edits,
            ExternalCandidate::Identifier(identifier),
            None,
            budget,
        )
    }

    /// Atomically interns a dependency path whose GUID and type are unknown to the caller.
    pub fn intern_path_into_edits(
        file: &SerializedFile,
        edits: &mut SerializedFileEdits,
        path: String,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        Self::intern_into_edits_impl(file, edits, ExternalCandidate::Path(path), None, budget)
    }

    /// Commits an identifier whose file ID was obtained from [`Self::planned_file_id`].
    ///
    /// A stale planned ID is rejected before the edit set or budget changes.
    pub fn commit_planned_into_edits(
        file: &SerializedFile,
        edits: &mut SerializedFileEdits,
        identifier: FileIdentifier,
        planned_file_id: i32,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        Self::intern_into_edits_impl(
            file,
            edits,
            ExternalCandidate::Identifier(identifier),
            Some(planned_file_id),
            budget,
        )
    }

    /// Commits a path-only candidate previously planned by [`Self::planned_file_id_for_path`].
    pub fn commit_planned_path_into_edits(
        file: &SerializedFile,
        edits: &mut SerializedFileEdits,
        path: String,
        planned_file_id: i32,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        Self::intern_into_edits_impl(
            file,
            edits,
            ExternalCandidate::Path(path),
            Some(planned_file_id),
            budget,
        )
    }

    fn intern_into_edits_impl(
        file: &SerializedFile,
        edits: &mut SerializedFileEdits,
        candidate: ExternalCandidate,
        planned_file_id: Option<i32>,
        budget: &mut AssetLoadBudget,
    ) -> Result<i32, ExternalTableError> {
        let disposition = {
            let table = PlannedExternalTable::build(file, edits)?;
            candidate.plan(file.format(), table.existing, table.additions)?
        };
        if let Some(planned) = planned_file_id
            && disposition.file_id != planned
        {
            return Err(ExternalTableError::PlannedFileIdChanged {
                planned,
                actual: disposition.file_id,
            });
        }
        if !disposition.append {
            bind_edits_to_base(file, edits)?;
            return Ok(disposition.file_id);
        }

        let identifier = candidate.into_identifier();

        match &mut edits.external_table {
            Some(mutation) => append_budgeted(&mut mutation.additions, identifier, budget)?,
            None => {
                let base = external_table_base(file)?;
                let mut additions = Vec::new();
                append_budgeted(&mut additions, identifier, budget)?;
                edits.external_table = Some(ExternalTableMutation { base, additions });
            }
        }
        Ok(disposition.file_id)
    }

    /// Returns additions accumulated so far in first-seen order.
    #[must_use]
    pub fn additions(&self) -> &[FileIdentifier] {
        &self.additions
    }

    /// Produces edits containing only this external-table plan.
    #[must_use]
    pub fn finish(self) -> SerializedFileEdits {
        SerializedFileEdits {
            object_bytes: Default::default(),
            external_table: Some(ExternalTableMutation {
                base: self.base,
                additions: self.additions,
            }),
        }
    }

    /// Atomically attaches all planned additions to object edits.
    pub fn into_edits(
        self,
        mut edits: SerializedFileEdits,
    ) -> Result<SerializedFileEdits, ExternalTableError> {
        if edits.external_table.is_some() {
            return Err(ExternalTableError::EditsAlreadyContainExternals);
        }
        edits.external_table = Some(ExternalTableMutation {
            base: self.base,
            additions: self.additions,
        });
        Ok(edits)
    }
}

#[derive(Debug, Clone, Copy)]
struct InternDisposition {
    file_id: i32,
    append: bool,
}

enum ExternalCandidate {
    Identifier(FileIdentifier),
    Path(String),
}

impl ExternalCandidate {
    fn plan(
        &self,
        format: SerializedFileFormat,
        existing: &[FileIdentifier],
        additions: &[FileIdentifier],
    ) -> Result<InternDisposition, ExternalTableError> {
        match self {
            Self::Identifier(identifier) => {
                plan_identifier(format, existing, additions, identifier)
            }
            Self::Path(path) => plan_path(format, existing, additions, path),
        }
    }

    fn into_identifier(self) -> FileIdentifier {
        match self {
            Self::Identifier(identifier) => identifier,
            Self::Path(path) => FileIdentifier {
                temp_empty: String::new(),
                guid: [0; 16],
                type_: 0,
                path,
            },
        }
    }
}

pub(crate) struct PlannedExternalTable<'table> {
    existing: &'table [FileIdentifier],
    additions: &'table [FileIdentifier],
    count: usize,
}

impl<'table> PlannedExternalTable<'table> {
    pub(crate) fn build(
        file: &'table SerializedFile,
        edits: &'table SerializedFileEdits,
    ) -> Result<Self, ExternalTableError> {
        validate_header_format(file)?;
        let format = file.format();
        let additions = match &edits.external_table {
            Some(mutation) => {
                validate_base(file, mutation.base)?;
                mutation.additions.as_slice()
            }
            None => &[],
        };
        let count = validate_table(format, &file.externals, additions)?;
        Ok(Self {
            existing: &file.externals,
            additions,
            count,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.count
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &'table FileIdentifier> + '_ {
        self.existing.iter().chain(self.additions.iter())
    }
}

fn validate_header_format(file: &SerializedFile) -> Result<(), ExternalTableError> {
    let format_version = file.format().version();
    if file.header.version != format_version {
        return Err(ExternalTableError::FormatVersionMismatch {
            header_version: file.header.version,
            format_version,
        });
    }
    Ok(())
}

fn validate_base(
    file: &SerializedFile,
    expected: ExternalTableBase,
) -> Result<(), ExternalTableError> {
    let actual_format = file.format();
    if actual_format != expected.format || file.externals.len() != expected.count {
        return Err(ExternalTableError::BaseTableShapeChanged {
            expected_version: expected.format.version(),
            actual_version: actual_format.version(),
            expected_count: expected.count,
            actual_count: file.externals.len(),
        });
    }
    if external_table_digest(&file.externals)? != expected.digest {
        return Err(ExternalTableError::BaseTableContentChanged);
    }
    Ok(())
}

fn external_table_base(file: &SerializedFile) -> Result<ExternalTableBase, ExternalTableError> {
    Ok(ExternalTableBase {
        format: file.format(),
        count: file.externals.len(),
        digest: external_table_digest(&file.externals)?,
    })
}

fn bind_edits_to_base(
    file: &SerializedFile,
    edits: &mut SerializedFileEdits,
) -> Result<(), ExternalTableError> {
    if edits.external_table.is_none() {
        edits.external_table = Some(ExternalTableMutation {
            base: external_table_base(file)?,
            additions: Vec::new(),
        });
    }
    Ok(())
}

fn validate_table(
    format: SerializedFileFormat,
    existing: &[FileIdentifier],
    additions: &[FileIdentifier],
) -> Result<usize, ExternalTableError> {
    let count = existing.len().checked_add(additions.len()).ok_or(
        ExternalTableError::ArithmeticOverflow {
            resource: "external table length",
        },
    )?;
    if count != 0 {
        external_index_to_file_id(count - 1)?;
    }

    for (index, identifier) in existing.iter().enumerate() {
        validate_identifier(format, index, identifier)?;
    }
    for (addition_index, identifier) in additions.iter().enumerate() {
        let index = existing.len().checked_add(addition_index).ok_or(
            ExternalTableError::ArithmeticOverflow {
                resource: "external table length",
            },
        )?;
        validate_identifier(format, index, identifier)?;
        for (previous_index, previous) in existing
            .iter()
            .chain(additions.iter())
            .take(index)
            .enumerate()
        {
            if canonical_paths_equal(&previous.path, &identifier.path)
                && let Some(field) = conflicting_metadata(previous, identifier)
            {
                return Err(ExternalTableError::CanonicalPathConflict {
                    existing_index: previous_index,
                    candidate_index: index,
                    field,
                });
            }
        }
    }
    Ok(count)
}

fn plan_identifier(
    format: SerializedFileFormat,
    existing: &[FileIdentifier],
    additions: &[FileIdentifier],
    identifier: &FileIdentifier,
) -> Result<InternDisposition, ExternalTableError> {
    let candidate_index = existing.len().checked_add(additions.len()).ok_or(
        ExternalTableError::ArithmeticOverflow {
            resource: "external table length",
        },
    )?;
    validate_identifier(format, candidate_index, identifier)?;

    if let Some(index) = existing
        .iter()
        .chain(additions.iter())
        .position(|retained| retained == identifier)
    {
        return Ok(InternDisposition {
            file_id: external_index_to_file_id(index)?,
            append: false,
        });
    }

    for (index, retained) in existing.iter().chain(additions.iter()).enumerate() {
        if canonical_paths_equal(&retained.path, &identifier.path) {
            if let Some(field) = conflicting_metadata(retained, identifier) {
                return Err(ExternalTableError::CanonicalPathConflict {
                    existing_index: index,
                    candidate_index,
                    field,
                });
            }
            return Ok(InternDisposition {
                file_id: external_index_to_file_id(index)?,
                append: false,
            });
        }
    }

    Ok(InternDisposition {
        file_id: external_index_to_file_id(candidate_index)?,
        append: true,
    })
}

fn plan_path(
    format: SerializedFileFormat,
    existing: &[FileIdentifier],
    additions: &[FileIdentifier],
    path: &str,
) -> Result<InternDisposition, ExternalTableError> {
    let candidate_index = existing.len().checked_add(additions.len()).ok_or(
        ExternalTableError::ArithmeticOverflow {
            resource: "external table length",
        },
    )?;
    if path.contains('\0') {
        return Err(ExternalTableError::Unrepresentable {
            version: format.version(),
            encoding: format.external_encoding(),
            index: candidate_index,
            field: ExternalIdentifierField::PathNul,
        });
    }

    if let Some(index) = existing
        .iter()
        .chain(additions.iter())
        .position(|retained| canonical_paths_equal(&retained.path, path))
    {
        return Ok(InternDisposition {
            file_id: external_index_to_file_id(index)?,
            append: false,
        });
    }

    Ok(InternDisposition {
        file_id: external_index_to_file_id(candidate_index)?,
        append: true,
    })
}

fn validate_identifier(
    format: SerializedFileFormat,
    index: usize,
    identifier: &FileIdentifier,
) -> Result<(), ExternalTableError> {
    let encoding = format.external_encoding();
    let unsupported = match encoding {
        ExternalEncoding::PathOnly if !identifier.temp_empty.is_empty() => {
            Some(ExternalIdentifierField::AssetPath)
        }
        ExternalEncoding::PathOnly if identifier.guid != [0; 16] => {
            Some(ExternalIdentifierField::Guid)
        }
        ExternalEncoding::PathOnly if identifier.type_ != 0 => Some(ExternalIdentifierField::Type),
        ExternalEncoding::GuidAndType if !identifier.temp_empty.is_empty() => {
            Some(ExternalIdentifierField::AssetPath)
        }
        ExternalEncoding::AssetPathGuidAndType if identifier.temp_empty.contains('\0') => {
            Some(ExternalIdentifierField::AssetPathNul)
        }
        ExternalEncoding::PathOnly
        | ExternalEncoding::GuidAndType
        | ExternalEncoding::AssetPathGuidAndType
            if identifier.path.contains('\0') =>
        {
            Some(ExternalIdentifierField::PathNul)
        }
        _ => None,
    };
    match unsupported {
        Some(field) => Err(ExternalTableError::Unrepresentable {
            version: format.version(),
            encoding,
            index,
            field,
        }),
        None => Ok(()),
    }
}

fn conflicting_metadata(
    existing: &FileIdentifier,
    candidate: &FileIdentifier,
) -> Option<ExternalMetadataField> {
    if existing.temp_empty != candidate.temp_empty {
        Some(ExternalMetadataField::AssetPath)
    } else if existing.guid != candidate.guid {
        Some(ExternalMetadataField::Guid)
    } else if existing.type_ != candidate.type_ {
        Some(ExternalMetadataField::Type)
    } else {
        None
    }
}

fn canonical_paths_equal(left: &str, right: &str) -> bool {
    canonical_path(left)
        .iter()
        .copied()
        .map(canonical_path_byte)
        .eq(canonical_path(right)
            .iter()
            .copied()
            .map(canonical_path_byte))
}

fn canonical_path(path: &str) -> &[u8] {
    let bytes = path.as_bytes();
    let mut start = 0;
    let mut end = bytes.len();
    while start + 1 < end && bytes[start] == b'.' && is_path_separator(bytes[start + 1]) {
        start += 2;
    }
    if start + 9 <= end
        && bytes[start..start + 8].eq_ignore_ascii_case(b"archive:")
        && is_path_separator(bytes[start + 8])
    {
        start += 9;
    }
    while start + 1 < end && bytes[start] == b'.' && is_path_separator(bytes[start + 1]) {
        start += 2;
    }
    while start < end && is_path_separator(bytes[start]) {
        start += 1;
    }
    while end > start && is_path_separator(bytes[end - 1]) {
        end -= 1;
    }
    &bytes[start..end]
}

const fn canonical_path_byte(byte: u8) -> u8 {
    if is_path_separator(byte) {
        b'/'
    } else {
        byte.to_ascii_lowercase()
    }
}

const fn is_path_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

fn append_budgeted(
    additions: &mut Vec<FileIdentifier>,
    identifier: FileIdentifier,
    budget: &mut AssetLoadBudget,
) -> Result<(), ExternalTableError> {
    budget.check_entries(1)?;
    let retained_strings = string_allocation_bytes(identifier.temp_empty.capacity())?
        .checked_add(string_allocation_bytes(identifier.path.capacity())?)
        .ok_or(ExternalTableError::ArithmeticOverflow {
            resource: EXTERNAL_ALLOCATION_RESOURCE,
        })?;
    let required =
        additions
            .len()
            .checked_add(1)
            .ok_or(ExternalTableError::ArithmeticOverflow {
                resource: "external additions length",
            })?;

    if required <= additions.capacity() {
        budget.check_bytes(retained_strings)?;
        budget.consume_entries(1)?;
        budget.consume_bytes(retained_strings)?;
        additions.push(identifier);
        return Ok(());
    }

    let planned_table = vec_allocation_bytes::<FileIdentifier>(required)?;
    let planned = retained_strings.checked_add(planned_table).ok_or(
        ExternalTableError::ArithmeticOverflow {
            resource: EXTERNAL_ALLOCATION_RESOURCE,
        },
    )?;
    budget.check_bytes(planned)?;

    // Allocate a staging table so a reserve or supplemental-budget failure leaves the allocator
    // unchanged. Existing identifiers move only after every fallible check succeeds.
    let mut staged = Vec::new();
    staged
        .try_reserve_exact(required)
        .map_err(|source| ExternalTableError::Allocation {
            requested: required,
            source,
        })?;
    let actual_table = vec_allocation_bytes::<FileIdentifier>(staged.capacity())?;
    let actual = retained_strings.checked_add(actual_table).ok_or(
        ExternalTableError::ArithmeticOverflow {
            resource: EXTERNAL_ALLOCATION_RESOURCE,
        },
    )?;
    budget.check_bytes(actual)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(actual)?;

    staged.append(additions);
    staged.push(identifier);
    *additions = staged;
    Ok(())
}

fn external_index_to_file_id(index: usize) -> Result<i32, ExternalTableError> {
    index
        .checked_add(1)
        .and_then(|file_id| i32::try_from(file_id).ok())
        .ok_or(ExternalTableError::FileIdOverflow { index })
}

fn external_table_digest(externals: &[FileIdentifier]) -> Result<DigestV1, ExternalTableError> {
    let mut declared = DigestV1Builder::framed_len(EXTERNAL_TABLE_IDENTITY_DOMAIN)?
        .checked_add(8)
        .ok_or(ExternalTableError::ArithmeticOverflow {
            resource: "external table digest length",
        })?;
    for identifier in externals {
        let path_length = DigestV1Builder::framed_len(identifier.path.as_bytes())?;
        declared = declared
            .checked_add(DigestV1Builder::framed_len(
                identifier.temp_empty.as_bytes(),
            )?)
            .and_then(|length| length.checked_add(16 + 4))
            .and_then(|length| length.checked_add(path_length))
            .ok_or(ExternalTableError::ArithmeticOverflow {
                resource: "external table digest length",
            })?;
    }

    let mut digest = DigestV1Builder::new(declared);
    digest.update_framed(EXTERNAL_TABLE_IDENTITY_DOMAIN)?;
    digest.update(
        &u64::try_from(externals.len())
            .map_err(|_| ExternalTableError::ArithmeticOverflow {
                resource: "external table count",
            })?
            .to_le_bytes(),
    )?;
    for identifier in externals {
        digest.update_framed(identifier.temp_empty.as_bytes())?;
        digest.update(&identifier.guid)?;
        digest.update(&identifier.type_.to_le_bytes())?;
        digest.update_framed(identifier.path.as_bytes())?;
    }
    Ok(digest.finalize()?)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use unity_asset_binary::asset::SerializedFileParser;
    use unity_asset_core::{AssetLoadLimits, AssetLoadUsage};

    use super::*;

    const V2_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v2.assets.bin");
    const V5_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v5.assets.bin");
    const V22_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin");

    fn parse_file(bytes: &[u8]) -> SerializedFile {
        SerializedFileParser::from_bytes(bytes.to_vec()).unwrap()
    }

    fn external(path: &str, seed: u8) -> FileIdentifier {
        FileIdentifier {
            temp_empty: String::new(),
            guid: [seed; 16],
            type_: i32::from(seed),
            path: path.to_owned(),
        }
    }

    #[test]
    fn exact_and_canonical_reuse_choose_the_lowest_index() {
        let mut file = parse_file(V22_FIXTURE);
        file.externals.push(file.externals[0].clone());
        let expected = file.externals[0].clone();
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();

        assert_eq!(
            allocator
                .intern(expected.clone(), &mut AssetLoadBudget::default())
                .unwrap(),
            1
        );
        let canonical_spelling = FileIdentifier {
            path: ".\\ARCHIVE:\\FIXTURE-DEPENDENCY.ASSETS/".to_owned(),
            ..expected
        };
        assert_eq!(
            allocator
                .intern(canonical_spelling, &mut AssetLoadBudget::default())
                .unwrap(),
            1
        );
        assert!(allocator.additions().is_empty());
    }

    #[test]
    fn canonical_path_metadata_conflict_is_typed() {
        let file = parse_file(V22_FIXTURE);
        let mut candidate = file.externals[0].clone();
        candidate.path = "FIXTURE-DEPENDENCY.ASSETS".to_owned();
        candidate.guid[0] ^= 0xff;
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();

        let error = allocator
            .intern(candidate, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ExternalTableError::CanonicalPathConflict {
                existing_index: 0,
                candidate_index: 1,
                field: ExternalMetadataField::Guid,
            }
        ));
        assert!(allocator.additions().is_empty());
    }

    #[test]
    fn additions_preserve_first_seen_order_and_are_deterministic() {
        let file = parse_file(V22_FIXTURE);
        let allocate = || {
            let mut allocator = ExternalTableAllocator::new(&file).unwrap();
            let mut budget = AssetLoadBudget::default();
            let first = allocator
                .intern(external("second.assets", 2), &mut budget)
                .unwrap();
            let second = allocator
                .intern(external("first.assets", 1), &mut budget)
                .unwrap();
            let duplicate = allocator
                .intern(external("SECOND.ASSETS", 2), &mut budget)
                .unwrap();
            (first, second, duplicate, allocator.finish())
        };

        let first = allocate();
        let second = allocate();
        assert_eq!((first.0, first.1, first.2), (2, 3, 2));
        assert_eq!(first.3.external_additions(), second.3.external_additions());
        assert_eq!(
            first
                .3
                .external_additions()
                .iter()
                .map(|external| external.path.as_str())
                .collect::<Vec<_>>(),
            ["second.assets", "first.assets"]
        );
    }

    #[test]
    fn format_metadata_is_rejected_before_allocation() {
        let path_only = parse_file(V2_FIXTURE);
        let mut allocator = ExternalTableAllocator::new(&path_only).unwrap();
        let error = allocator
            .intern(
                external("dependency.assets", 1),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ExternalTableError::Unrepresentable {
                field: ExternalIdentifierField::Guid,
                ..
            }
        ));

        let guid_and_type = parse_file(V5_FIXTURE);
        let mut identifier = external("dependency.assets", 1);
        identifier.temp_empty = "asset-path".to_owned();
        let mut allocator = ExternalTableAllocator::new(&guid_and_type).unwrap();
        let error = allocator
            .intern(identifier, &mut AssetLoadBudget::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ExternalTableError::Unrepresentable {
                field: ExternalIdentifierField::AssetPath,
                ..
            }
        ));
    }

    #[test]
    fn retained_entry_allocation_has_an_exact_budget_boundary() {
        let file = parse_file(V22_FIXTURE);
        let identifier = external("unique-budget.assets", 7);
        let expected_minimum = u64::try_from(size_of::<FileIdentifier>()).unwrap()
            + u64::try_from(identifier.path.capacity()).unwrap()
            + u64::try_from(identifier.temp_empty.capacity()).unwrap();
        let mut measured = AssetLoadBudget::default();
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();
        allocator.intern(identifier.clone(), &mut measured).unwrap();
        assert_eq!(measured.usage().entries, 1);
        assert!(measured.usage().bytes >= expected_minimum);

        let exact = measured.usage().bytes;
        let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: exact,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();
        allocator
            .intern(identifier.clone(), &mut exact_budget)
            .unwrap();
        assert_eq!(exact_budget.usage().bytes, exact);

        let mut short_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: exact - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();
        let error = allocator.intern(identifier, &mut short_budget).unwrap_err();
        assert!(matches!(
            error,
            ExternalTableError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(short_budget.usage(), AssetLoadUsage::default());
        assert!(allocator.additions().is_empty());
    }

    #[test]
    fn continuing_edits_is_atomic_at_the_one_short_budget_boundary() {
        let file = parse_file(V22_FIXTURE);
        let mut edits = SerializedFileEdits::default();
        ExternalTableAllocator::intern_into_edits(
            &file,
            &mut edits,
            external("first-new.assets", 1),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let before = edits.external_additions().to_vec();

        let mut measured_edits = edits.clone();
        let mut measured = AssetLoadBudget::default();
        ExternalTableAllocator::intern_into_edits(
            &file,
            &mut measured_edits,
            external("second-new.assets", 2),
            &mut measured,
        )
        .unwrap();
        let required = measured.usage().bytes;
        assert!(required > 0);

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: required - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = ExternalTableAllocator::intern_into_edits(
            &file,
            &mut edits,
            external("second-new.assets", 2),
            &mut short,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ExternalTableError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
        assert_eq!(edits.external_additions(), before);
        assert_eq!(short.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn stale_planned_file_id_is_rejected_before_state_or_budget_changes() {
        let file = parse_file(V22_FIXTURE);
        let planned = external("planned.assets", 1);
        let mut edits = SerializedFileEdits::default();
        let planned_file_id =
            ExternalTableAllocator::planned_file_id(&file, &edits, &planned).unwrap();
        ExternalTableAllocator::intern_into_edits(
            &file,
            &mut edits,
            external("intervening.assets", 2),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let before = edits.external_additions().to_vec();
        let mut budget = AssetLoadBudget::default();

        let error = ExternalTableAllocator::commit_planned_into_edits(
            &file,
            &mut edits,
            planned,
            planned_file_id,
            &mut budget,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ExternalTableError::PlannedFileIdChanged {
                planned: 2,
                actual: 3,
            }
        ));
        assert_eq!(edits.external_additions(), before);
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn file_id_overflow_is_typed() {
        assert!(matches!(
            external_index_to_file_id(i32::MAX as usize),
            Err(ExternalTableError::FileIdOverflow { .. })
        ));
        assert!(matches!(
            external_index_to_file_id(usize::MAX),
            Err(ExternalTableError::FileIdOverflow { .. })
        ));
    }

    #[test]
    fn edits_are_bound_to_the_external_table_used_for_allocation() {
        let file = parse_file(V22_FIXTURE);
        let mut allocator = ExternalTableAllocator::new(&file).unwrap();
        allocator
            .intern(
                external("new-dependency.assets", 9),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let edits = allocator.finish();

        let mut changed = parse_file(V22_FIXTURE);
        changed.externals[0].guid[0] ^= 0xff;
        assert!(matches!(
            PlannedExternalTable::build(&changed, &edits),
            Err(ExternalTableError::BaseTableContentChanged)
        ));
    }

    #[test]
    fn retained_path_reuse_is_bound_without_inventing_metadata() {
        let file = parse_file(V22_FIXTURE);
        let path_only_spelling = ".\\ARCHIVE:\\FIXTURE-DEPENDENCY.ASSETS/".to_owned();
        let mut edits = SerializedFileEdits::default();

        let file_id = ExternalTableAllocator::intern_path_into_edits(
            &file,
            &mut edits,
            path_only_spelling,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        assert_eq!(file_id, 1);
        assert!(edits.external_additions().is_empty());
        assert!(edits.is_empty());

        let mut changed = parse_file(V22_FIXTURE);
        changed.externals[0].guid[0] ^= 0xff;
        assert!(matches!(
            PlannedExternalTable::build(&changed, &edits),
            Err(ExternalTableError::BaseTableContentChanged)
        ));
    }

    #[test]
    fn missing_path_only_candidate_appends_compatibility_metadata() {
        let file = parse_file(V22_FIXTURE);
        let mut edits = SerializedFileEdits::default();

        let file_id = ExternalTableAllocator::intern_path_into_edits(
            &file,
            &mut edits,
            "new-path-only.assets".to_owned(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();

        assert_eq!(file_id, 2);
        assert_eq!(edits.external_additions().len(), 1);
        let addition = &edits.external_additions()[0];
        assert_eq!(addition.path, "new-path-only.assets");
        assert_eq!(addition.guid, [0; 16]);
        assert_eq!(addition.type_, 0);
    }
}
