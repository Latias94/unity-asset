//! Validated SerializedFile format capabilities.
//!
//! This module is the single source of truth for format-version transitions. Parsers and writers
//! consume semantic layouts from [`SerializedFileFormat`] instead of repeating numeric thresholds.

use crate::error::{BinaryError, Result};
use std::ops::Range;

/// Physical header layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderLayout {
    /// The 16-byte header used before format 9; endian metadata lives at the file tail.
    Legacy16,
    /// The 20-byte header with endian and three reserved bytes.
    Standard20,
    /// The 48-byte large-file header with 64-bit file and data offsets.
    LargeFiles48,
}

/// Location of the metadata block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataPlacement {
    /// Metadata is at the end of the file and starts with the endian byte.
    TailWithEndianPrefix,
    /// Metadata immediately follows the header and uses the endian declared there.
    AfterHeader,
}

/// How TypeTree enablement is represented on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeTreeEnablement {
    /// No flag is stored; TypeTrees are unconditionally enabled.
    ImplicitEnabled,
    /// Metadata stores an explicit boolean flag.
    ExplicitFlag,
}

impl TypeTreeEnablement {
    /// Resolves the semantic value from an optional wire flag.
    ///
    /// An explicit format returns `None` when the caller has not read its required flag.
    pub const fn resolve(self, explicit_flag: Option<bool>) -> Option<bool> {
        match self {
            Self::ImplicitEnabled => Some(true),
            Self::ExplicitFlag => explicit_flag,
        }
    }
}

/// TypeTree wire encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeTreeEncoding {
    /// Recursive stringful layout with the format-2 variable-count field.
    LegacyV2,
    /// Recursive stringful layout with the format-3 omissions.
    LegacyV3,
    /// Recursive stringful layout used by other legacy formats.
    LegacyStandard,
    /// Flat blob nodes without a reference-type hash.
    Blob,
    /// Flat blob nodes with the format-19 reference-type hash.
    BlobWithRefTypeHash,
}

/// Object and script-reference path-ID encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathIdEncoding {
    /// A signed 32-bit path ID.
    I32,
    /// The metadata `bigIdEnabled` value selects a 32-bit or 64-bit path ID.
    BigIdFlag,
    /// A four-byte-aligned signed 64-bit path ID.
    AlignedI64,
}

/// Object data-offset encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectOffsetEncoding {
    /// An unsigned 32-bit offset relative to the file data offset.
    U32,
    /// A validated non-negative signed 64-bit relative offset.
    I64,
}

/// Meaning of the object table's raw 32-bit type field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectTypeEncoding {
    /// A type ID followed by a 16-bit class ID.
    Legacy,
    /// Format 16's raw type-table index, retained separately from later indexed encodings.
    TransitionalV16,
    /// An index into the SerializedType table.
    Indexed,
}

/// Version-specific fields after an object's type information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectTailEncoding {
    /// A 16-bit destroyed value.
    Destroyed,
    /// A 16-bit script-type index.
    ScriptTypeIndex,
    /// A 16-bit script-type index followed by an 8-bit stripped value.
    ScriptTypeIndexAndStripped,
    /// No object-level tail fields.
    None,
}

/// External-file identifier encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternalEncoding {
    /// Only the final path string is stored.
    PathOnly,
    /// GUID, type, and final path are stored.
    GuidAndType,
    /// Asset path, GUID, type, and final path are stored.
    AssetPathGuidAndType,
}

/// Optional metadata fields whose presence changes by format version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MetadataField {
    /// Null-terminated Unity version string.
    UnityVersion = 0,
    /// Raw target-platform value.
    TargetPlatform = 1,
    /// Explicit `enableTypeTree` boolean.
    EnableTypeTree = 2,
    /// Legacy `bigIdEnabled` value.
    BigIdEnabled = 3,
    /// Script-type table.
    ScriptTypes = 4,
    /// Reference-type table.
    RefTypes = 5,
    /// Null-terminated user-information string.
    UserInformation = 6,
}

impl MetadataField {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

const TYPE_HASHES: u8 = 1 << 0;
const TYPE_STRIPPED: u8 = 1 << 1;
const TYPE_SCRIPT_INDEX: u8 = 1 << 2;
const TYPE_DEPENDENCIES: u8 = 1 << 3;

const META_VERSION: u8 = MetadataField::UnityVersion.bit();
const META_PLATFORM: u8 = MetadataField::TargetPlatform.bit();
const META_ENABLE_TREE: u8 = MetadataField::EnableTypeTree.bit();
const META_BIG_ID: u8 = MetadataField::BigIdEnabled.bit();
const META_SCRIPTS: u8 = MetadataField::ScriptTypes.bit();
const META_REF_TYPES: u8 = MetadataField::RefTypes.bit();
const META_USER: u8 = MetadataField::UserInformation.bit();

/// A validated SerializedFile format and its complete wire capabilities.
///
/// Values can only be created for known formats: 2, 3, and every format from 5 through 22.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SerializedFileFormat {
    version: u8,
    header_layout: HeaderLayout,
    metadata_placement: MetadataPlacement,
    type_tree_enablement: TypeTreeEnablement,
    type_tree_encoding: TypeTreeEncoding,
    path_id_encoding: PathIdEncoding,
    object_offset_encoding: ObjectOffsetEncoding,
    object_type_encoding: ObjectTypeEncoding,
    object_tail_encoding: ObjectTailEncoding,
    external_encoding: ExternalEncoding,
    metadata_fields: u8,
    serialized_type_fields: u8,
}

/// Checked physical regions for one SerializedFile image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializedFileRegions {
    pub file: Range<u64>,
    pub header: Range<u64>,
    pub metadata: Range<u64>,
    pub metadata_body: Range<u64>,
    pub data: Range<u64>,
}

/// Checked output dimensions shared by SerializedFile encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerializedFileLayout {
    pub header_size: u64,
    pub metadata_size: u32,
    pub data_offset: u64,
    pub file_size: u64,
}

impl SerializedFileFormat {
    /// Every format version understood by this module, in ascending order.
    pub const SUPPORTED: [Self; 20] = [
        profile(
            2,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyV2,
            PathIdEncoding::I32,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::PathOnly,
            0,
            0,
        ),
        profile(
            3,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyV3,
            PathIdEncoding::I32,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::PathOnly,
            0,
            0,
        ),
        profile(
            5,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::I32,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::GuidAndType,
            META_USER,
            0,
        ),
        profile(
            6,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::I32,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::AssetPathGuidAndType,
            META_USER,
            0,
        ),
        profile(
            7,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_BIG_ID | META_USER,
            0,
        ),
        profile(
            8,
            HeaderLayout::Legacy16,
            MetadataPlacement::TailWithEndianPrefix,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_BIG_ID | META_USER,
            0,
        ),
        profile(
            9,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_BIG_ID | META_USER,
            0,
        ),
        profile(
            10,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::Blob,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::Destroyed,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_BIG_ID | META_USER,
            0,
        ),
        profile(
            11,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::LegacyStandard,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::ScriptTypeIndex,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_BIG_ID | META_SCRIPTS | META_USER,
            0,
        ),
        profile(
            12,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ImplicitEnabled,
            TypeTreeEncoding::Blob,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::ScriptTypeIndex,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_BIG_ID | META_SCRIPTS | META_USER,
            0,
        ),
        profile(
            13,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::BigIdFlag,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::ScriptTypeIndex,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION
                | META_PLATFORM
                | META_ENABLE_TREE
                | META_BIG_ID
                | META_SCRIPTS
                | META_USER,
            TYPE_HASHES,
        ),
        profile(
            14,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::ScriptTypeIndex,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES,
        ),
        profile(
            15,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Legacy,
            ObjectTailEncoding::ScriptTypeIndexAndStripped,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES,
        ),
        profile(
            16,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::TransitionalV16,
            ObjectTailEncoding::ScriptTypeIndexAndStripped,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES | TYPE_STRIPPED,
        ),
        profile(
            17,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX,
        ),
        profile(
            18,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::Blob,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX,
        ),
        profile(
            19,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::BlobWithRefTypeHash,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION | META_PLATFORM | META_ENABLE_TREE | META_SCRIPTS | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX,
        ),
        profile(
            20,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::BlobWithRefTypeHash,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION
                | META_PLATFORM
                | META_ENABLE_TREE
                | META_SCRIPTS
                | META_REF_TYPES
                | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX,
        ),
        profile(
            21,
            HeaderLayout::Standard20,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::BlobWithRefTypeHash,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::U32,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION
                | META_PLATFORM
                | META_ENABLE_TREE
                | META_SCRIPTS
                | META_REF_TYPES
                | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX | TYPE_DEPENDENCIES,
        ),
        profile(
            22,
            HeaderLayout::LargeFiles48,
            MetadataPlacement::AfterHeader,
            TypeTreeEnablement::ExplicitFlag,
            TypeTreeEncoding::BlobWithRefTypeHash,
            PathIdEncoding::AlignedI64,
            ObjectOffsetEncoding::I64,
            ObjectTypeEncoding::Indexed,
            ObjectTailEncoding::None,
            ExternalEncoding::AssetPathGuidAndType,
            META_VERSION
                | META_PLATFORM
                | META_ENABLE_TREE
                | META_SCRIPTS
                | META_REF_TYPES
                | META_USER,
            TYPE_HASHES | TYPE_STRIPPED | TYPE_SCRIPT_INDEX | TYPE_DEPENDENCIES,
        ),
    ];

    /// Validates and constructs a known format version.
    pub fn new(version: u32) -> Result<Self> {
        let index = match version {
            2 => 0,
            3 => 1,
            5..=22 => (version - 3) as usize,
            _ => {
                return Err(BinaryError::unsupported_version(format!(
                    "SerializedFile format {version}; expected 2, 3, or 5 through 22"
                )));
            }
        };
        Ok(Self::SUPPORTED[index])
    }

    /// Returns whether the version is known without constructing an error.
    pub const fn is_supported(version: u32) -> bool {
        matches!(version, 2 | 3 | 5..=22)
    }

    /// Returns supported numeric versions in ascending order.
    pub fn supported_versions() -> impl ExactSizeIterator<Item = u32> + DoubleEndedIterator {
        Self::SUPPORTED.iter().map(|format| format.version())
    }

    /// Numeric SerializedFile format version.
    pub const fn version(self) -> u32 {
        self.version as u32
    }

    /// Physical header layout.
    pub const fn header_layout(self) -> HeaderLayout {
        self.header_layout
    }

    /// Header size in bytes.
    pub const fn header_size(self) -> u64 {
        match self.header_layout {
            HeaderLayout::Legacy16 => 16,
            HeaderLayout::Standard20 => 20,
            HeaderLayout::LargeFiles48 => 48,
        }
    }

    /// Metadata location and endian-prefix policy.
    pub const fn metadata_placement(self) -> MetadataPlacement {
        self.metadata_placement
    }

    /// TypeTree enablement policy.
    pub const fn type_tree_enablement(self) -> TypeTreeEnablement {
        self.type_tree_enablement
    }

    /// TypeTree node encoding.
    pub const fn type_tree_encoding(self) -> TypeTreeEncoding {
        self.type_tree_encoding
    }

    /// Object and script-reference path-ID encoding.
    pub const fn path_id_encoding(self) -> PathIdEncoding {
        self.path_id_encoding
    }

    /// Object data-offset encoding.
    pub const fn object_offset_encoding(self) -> ObjectOffsetEncoding {
        self.object_offset_encoding
    }

    /// Object type-field semantics.
    pub const fn object_type_encoding(self) -> ObjectTypeEncoding {
        self.object_type_encoding
    }

    /// Object-level tail-field encoding.
    pub const fn object_tail_encoding(self) -> ObjectTailEncoding {
        self.object_tail_encoding
    }

    /// External-file identifier encoding.
    pub const fn external_encoding(self) -> ExternalEncoding {
        self.external_encoding
    }

    /// Returns whether an optional metadata field is present.
    pub const fn has_metadata_field(self, field: MetadataField) -> bool {
        self.metadata_fields & field.bit() != 0
    }

    /// Returns whether SerializedType records carry type hashes.
    pub const fn serialized_types_have_hashes(self) -> bool {
        self.serialized_type_fields & TYPE_HASHES != 0
    }

    /// Returns whether SerializedType records carry the stripped flag.
    pub const fn serialized_types_have_stripped_flag(self) -> bool {
        self.serialized_type_fields & TYPE_STRIPPED != 0
    }

    /// Returns whether SerializedType records carry a script-type index.
    pub const fn serialized_types_have_script_type_index(self) -> bool {
        self.serialized_type_fields & TYPE_SCRIPT_INDEX != 0
    }

    /// Returns whether this SerializedType record carries a 16-byte script ID.
    pub const fn serialized_type_has_script_id(
        self,
        class_id: i32,
        script_type_index: i16,
        is_ref_type: bool,
    ) -> bool {
        self.serialized_types_have_hashes()
            && ((is_ref_type && script_type_index >= 0)
                || (!self.serialized_types_have_stripped_flag() && class_id < 0)
                || (self.serialized_types_have_stripped_flag()
                    && class_id == unity_asset_core::class_ids::MONO_BEHAVIOUR))
    }

    /// Returns whether ordinary TypeTrees are followed by type dependencies.
    pub const fn has_type_dependencies(self) -> bool {
        self.serialized_type_fields & TYPE_DEPENDENCIES != 0
    }

    /// Returns whether reference TypeTrees are followed by class, namespace, and assembly names.
    pub const fn has_ref_type_names(self) -> bool {
        self.has_type_dependencies()
    }

    /// Validates header dimensions and returns bounded logical regions.
    pub fn decode_regions(
        self,
        metadata_size: u32,
        file_size: u64,
        data_offset: u64,
        source_len: u64,
    ) -> Result<SerializedFileRegions> {
        let header_size = self.header_size();
        if file_size > source_len {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} declares file size {file_size} beyond source length {source_len}",
                self.version()
            )));
        }
        if file_size < header_size {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} file size {file_size} is smaller than its {header_size}-byte header",
                self.version()
            )));
        }
        if data_offset < header_size || data_offset > file_size {
            return Err(BinaryError::invalid_data(format!(
                "SerializedFile v{} data offset {data_offset} is outside {header_size}..={file_size}",
                self.version()
            )));
        }

        let (metadata, metadata_body, data) = match self.metadata_placement {
            MetadataPlacement::TailWithEndianPrefix => {
                if metadata_size < 1 {
                    return Err(BinaryError::invalid_data(
                        "Legacy SerializedFile metadata must contain its endian prefix",
                    ));
                }
                let metadata_start =
                    file_size
                        .checked_sub(u64::from(metadata_size))
                        .ok_or_else(|| {
                            BinaryError::invalid_data(
                                "SerializedFile metadata size exceeds declared file size",
                            )
                        })?;
                if metadata_start < data_offset {
                    return Err(BinaryError::invalid_data(format!(
                        "SerializedFile v{} metadata overlaps its data offset",
                        self.version()
                    )));
                }
                let body_start = metadata_start.checked_add(1).ok_or_else(|| {
                    BinaryError::invalid_data("SerializedFile metadata body offset overflow")
                })?;
                (
                    metadata_start..file_size,
                    body_start..file_size,
                    data_offset..metadata_start,
                )
            }
            MetadataPlacement::AfterHeader => {
                if metadata_size == 0 {
                    return Err(BinaryError::invalid_data(
                        "SerializedFile metadata size cannot be zero",
                    ));
                }
                let metadata_end = header_size
                    .checked_add(u64::from(metadata_size))
                    .ok_or_else(|| {
                        BinaryError::invalid_data("SerializedFile metadata end overflow")
                    })?;
                if metadata_end > data_offset {
                    return Err(BinaryError::invalid_data(format!(
                        "SerializedFile v{} metadata end {metadata_end} exceeds data offset {data_offset}",
                        self.version()
                    )));
                }
                (
                    header_size..metadata_end,
                    header_size..metadata_end,
                    data_offset..file_size,
                )
            }
        };

        Ok(SerializedFileRegions {
            file: 0..file_size,
            header: 0..header_size,
            metadata,
            metadata_body,
            data,
        })
    }

    /// Computes checked output dimensions from metadata-body and data sizes.
    pub fn plan_layout(
        self,
        metadata_body_size: u64,
        data_size: u64,
        legacy_data_offset_hint: Option<u64>,
    ) -> Result<SerializedFileLayout> {
        let header_size = self.header_size();
        let metadata_size = match self.metadata_placement {
            MetadataPlacement::TailWithEndianPrefix => metadata_body_size
                .checked_add(1)
                .ok_or_else(|| BinaryError::invalid_data("metadata size overflow"))?,
            MetadataPlacement::AfterHeader => metadata_body_size,
        };
        if metadata_size == 0 {
            return Err(BinaryError::invalid_data(
                "SerializedFile metadata size cannot be zero",
            ));
        }
        let metadata_size_u32 = u32::try_from(metadata_size).map_err(|_| {
            BinaryError::invalid_data(format!(
                "SerializedFile metadata size {metadata_size} does not fit u32"
            ))
        })?;

        let data_offset = match self.metadata_placement {
            MetadataPlacement::TailWithEndianPrefix => legacy_data_offset_hint
                .unwrap_or(32)
                .max(32)
                .max(header_size),
            MetadataPlacement::AfterHeader => {
                let metadata_end = header_size
                    .checked_add(metadata_size)
                    .ok_or_else(|| BinaryError::invalid_data("metadata end overflow"))?;
                align_up(metadata_end, 16)?
            }
        };
        let file_size = match self.metadata_placement {
            MetadataPlacement::TailWithEndianPrefix => data_offset
                .checked_add(data_size)
                .and_then(|value| value.checked_add(metadata_size))
                .ok_or_else(|| BinaryError::invalid_data("file size overflow"))?,
            MetadataPlacement::AfterHeader => data_offset
                .checked_add(data_size)
                .ok_or_else(|| BinaryError::invalid_data("file size overflow"))?,
        };

        match self.header_layout {
            HeaderLayout::Legacy16 | HeaderLayout::Standard20 => {
                u32::try_from(file_size).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "SerializedFile v{} file size {file_size} does not fit u32",
                        self.version()
                    ))
                })?;
                u32::try_from(data_offset).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "SerializedFile v{} data offset {data_offset} does not fit u32",
                        self.version()
                    ))
                })?;
            }
            HeaderLayout::LargeFiles48 => {
                i64::try_from(file_size).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "SerializedFile v{} file size {file_size} does not fit i64",
                        self.version()
                    ))
                })?;
                i64::try_from(data_offset).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "SerializedFile v{} data offset {data_offset} does not fit i64",
                        self.version()
                    ))
                })?;
            }
        }

        Ok(SerializedFileLayout {
            header_size,
            metadata_size: metadata_size_u32,
            data_offset,
            file_size,
        })
    }
}

fn align_up(value: u64, alignment: u64) -> Result<u64> {
    debug_assert!(alignment.is_power_of_two());
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
        .ok_or_else(|| BinaryError::invalid_data("alignment overflow"))
}

impl TryFrom<u32> for SerializedFileFormat {
    type Error = BinaryError;

    fn try_from(version: u32) -> Result<Self> {
        Self::new(version)
    }
}

impl From<SerializedFileFormat> for u32 {
    fn from(format: SerializedFileFormat) -> Self {
        format.version()
    }
}

#[allow(clippy::too_many_arguments)]
const fn profile(
    version: u8,
    header_layout: HeaderLayout,
    metadata_placement: MetadataPlacement,
    type_tree_enablement: TypeTreeEnablement,
    type_tree_encoding: TypeTreeEncoding,
    path_id_encoding: PathIdEncoding,
    object_offset_encoding: ObjectOffsetEncoding,
    object_type_encoding: ObjectTypeEncoding,
    object_tail_encoding: ObjectTailEncoding,
    external_encoding: ExternalEncoding,
    metadata_fields: u8,
    serialized_type_fields: u8,
) -> SerializedFileFormat {
    SerializedFileFormat {
        version,
        header_layout,
        metadata_placement,
        type_tree_enablement,
        type_tree_encoding,
        path_id_encoding,
        object_offset_encoding,
        object_type_encoding,
        object_tail_encoding,
        external_encoding,
        metadata_fields,
        serialized_type_fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy)]
    struct Expected {
        version: u32,
        header: HeaderLayout,
        placement: MetadataPlacement,
        tree_enablement: TypeTreeEnablement,
        tree_encoding: TypeTreeEncoding,
        path_id: PathIdEncoding,
        object_offset: ObjectOffsetEncoding,
        object_type: ObjectTypeEncoding,
        object_tail: ObjectTailEncoding,
        external: ExternalEncoding,
    }

    #[test]
    fn freezes_every_supported_wire_transition() {
        let expected = [
            row(
                2,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyV2,
                PathIdEncoding::I32,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::PathOnly,
            ),
            row(
                3,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyV3,
                PathIdEncoding::I32,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::PathOnly,
            ),
            row(
                5,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::I32,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::GuidAndType,
            ),
            row(
                6,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::I32,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                7,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                8,
                HeaderLayout::Legacy16,
                MetadataPlacement::TailWithEndianPrefix,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                9,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                10,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::Blob,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::Destroyed,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                11,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::LegacyStandard,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::ScriptTypeIndex,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                12,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ImplicitEnabled,
                TypeTreeEncoding::Blob,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::ScriptTypeIndex,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                13,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::BigIdFlag,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::ScriptTypeIndex,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                14,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::ScriptTypeIndex,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                15,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Legacy,
                ObjectTailEncoding::ScriptTypeIndexAndStripped,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                16,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::TransitionalV16,
                ObjectTailEncoding::ScriptTypeIndexAndStripped,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                17,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                18,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::Blob,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                19,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::BlobWithRefTypeHash,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                20,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::BlobWithRefTypeHash,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                21,
                HeaderLayout::Standard20,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::BlobWithRefTypeHash,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::U32,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
            row(
                22,
                HeaderLayout::LargeFiles48,
                MetadataPlacement::AfterHeader,
                TypeTreeEnablement::ExplicitFlag,
                TypeTreeEncoding::BlobWithRefTypeHash,
                PathIdEncoding::AlignedI64,
                ObjectOffsetEncoding::I64,
                ObjectTypeEncoding::Indexed,
                ObjectTailEncoding::None,
                ExternalEncoding::AssetPathGuidAndType,
            ),
        ];

        assert_eq!(expected.len(), SerializedFileFormat::SUPPORTED.len());
        for expected in expected {
            let actual = SerializedFileFormat::new(expected.version).unwrap();
            assert_eq!(actual.version(), expected.version);
            assert_eq!(actual.header_layout(), expected.header);
            assert_eq!(actual.metadata_placement(), expected.placement);
            assert_eq!(actual.type_tree_enablement(), expected.tree_enablement);
            assert_eq!(actual.type_tree_encoding(), expected.tree_encoding);
            assert_eq!(actual.path_id_encoding(), expected.path_id);
            assert_eq!(actual.object_offset_encoding(), expected.object_offset);
            assert_eq!(actual.object_type_encoding(), expected.object_type);
            assert_eq!(actual.object_tail_encoding(), expected.object_tail);
            assert_eq!(actual.external_encoding(), expected.external);
        }
    }

    #[test]
    fn freezes_metadata_and_serialized_type_capabilities() {
        for version in SerializedFileFormat::supported_versions() {
            let format = SerializedFileFormat::new(version).unwrap();
            assert_eq!(
                format.has_metadata_field(MetadataField::UnityVersion),
                version >= 7
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::TargetPlatform),
                version >= 8
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::EnableTypeTree),
                version >= 13
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::BigIdEnabled),
                (7..14).contains(&version)
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::ScriptTypes),
                version >= 11
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::RefTypes),
                version >= 20
            );
            assert_eq!(
                format.has_metadata_field(MetadataField::UserInformation),
                version >= 5
            );
            assert_eq!(format.serialized_types_have_hashes(), version >= 13);
            assert_eq!(format.serialized_types_have_stripped_flag(), version >= 16);
            assert_eq!(
                format.serialized_types_have_script_type_index(),
                version >= 17
            );
            assert_eq!(format.has_type_dependencies(), version >= 21);
            assert_eq!(format.has_ref_type_names(), version >= 21);
        }
    }

    #[test]
    fn type_tree_enablement_is_implicit_before_format_13() {
        for version in [2, 3, 5, 6, 7, 8, 9, 10, 11, 12] {
            let format = SerializedFileFormat::new(version).unwrap();
            assert_eq!(format.type_tree_enablement().resolve(None), Some(true));
            assert_eq!(
                format.type_tree_enablement().resolve(Some(false)),
                Some(true)
            );
        }

        let format_13 = SerializedFileFormat::new(13).unwrap();
        assert_eq!(format_13.type_tree_enablement().resolve(None), None);
        assert_eq!(
            format_13.type_tree_enablement().resolve(Some(false)),
            Some(false)
        );
    }

    #[test]
    fn rejects_unknown_format_versions() {
        for version in [0, 1, 4, 23, 24, u32::MAX] {
            assert!(!SerializedFileFormat::is_supported(version));
            assert!(matches!(
                SerializedFileFormat::new(version),
                Err(BinaryError::UnsupportedVersion(_))
            ));
        }
    }

    #[test]
    fn supported_versions_are_complete_and_sorted() {
        assert_eq!(
            SerializedFileFormat::supported_versions().collect::<Vec<_>>(),
            vec![
                2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22
            ]
        );
    }

    #[test]
    fn decodes_legacy_and_modern_regions_without_crossing_boundaries() {
        let legacy = SerializedFileFormat::new(8)
            .unwrap()
            .decode_regions(20, 200, 64, 200)
            .unwrap();
        assert_eq!(legacy.header, 0..16);
        assert_eq!(legacy.data, 64..180);
        assert_eq!(legacy.metadata, 180..200);
        assert_eq!(legacy.metadata_body, 181..200);

        let modern = SerializedFileFormat::new(22)
            .unwrap()
            .decode_regions(60, 256, 112, 300)
            .unwrap();
        assert_eq!(modern.header, 0..48);
        assert_eq!(modern.metadata, 48..108);
        assert_eq!(modern.data, 112..256);
    }

    #[test]
    fn rejects_overlapping_or_out_of_source_regions() {
        let legacy = SerializedFileFormat::new(8).unwrap();
        assert!(legacy.decode_regions(0, 100, 32, 100).is_err());
        assert!(legacy.decode_regions(80, 100, 32, 100).is_err());

        let modern = SerializedFileFormat::new(22).unwrap();
        assert!(modern.decode_regions(80, 100, 64, 100).is_err());
        assert!(modern.decode_regions(10, 101, 64, 100).is_err());
    }

    #[test]
    fn plans_checked_legacy_standard_and_large_file_layouts() {
        let legacy = SerializedFileFormat::new(8)
            .unwrap()
            .plan_layout(19, 4, Some(16))
            .unwrap();
        assert_eq!(legacy.header_size, 16);
        assert_eq!(legacy.metadata_size, 20);
        assert_eq!(legacy.data_offset, 32);
        assert_eq!(legacy.file_size, 56);

        let standard = SerializedFileFormat::new(19)
            .unwrap()
            .plan_layout(25, 7, None)
            .unwrap();
        assert_eq!(standard.header_size, 20);
        assert_eq!(standard.metadata_size, 25);
        assert_eq!(standard.data_offset, 48);
        assert_eq!(standard.file_size, 55);

        let large = SerializedFileFormat::new(22)
            .unwrap()
            .plan_layout(17, 4, None)
            .unwrap();
        assert_eq!(large.header_size, 48);
        assert_eq!(large.data_offset, 80);
        assert_eq!(large.file_size, 84);
    }

    #[test]
    fn layout_planning_rejects_zero_metadata_and_wire_narrowing() {
        assert!(
            SerializedFileFormat::new(19)
                .unwrap()
                .plan_layout(0, 1, None)
                .is_err()
        );
        assert!(
            SerializedFileFormat::new(19)
                .unwrap()
                .plan_layout(1, u64::from(u32::MAX), None)
                .is_err()
        );
        assert!(
            SerializedFileFormat::new(22)
                .unwrap()
                .plan_layout(1, u64::MAX, None)
                .is_err()
        );
    }

    #[allow(clippy::too_many_arguments)]
    const fn row(
        version: u32,
        header: HeaderLayout,
        placement: MetadataPlacement,
        tree_enablement: TypeTreeEnablement,
        tree_encoding: TypeTreeEncoding,
        path_id: PathIdEncoding,
        object_offset: ObjectOffsetEncoding,
        object_type: ObjectTypeEncoding,
        object_tail: ObjectTailEncoding,
        external: ExternalEncoding,
    ) -> Expected {
        Expected {
            version,
            header,
            placement,
            tree_enablement,
            tree_encoding,
            path_id,
            object_offset,
            object_type,
            object_tail,
            external,
        }
    }
}
