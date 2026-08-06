//! Versioned, revision-bound inspection projections over immutable workspace views.

use std::fmt;
use std::io::{self, Read};

use indexmap::IndexMap;
use serde::de::{Error as _, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
#[cfg(feature = "decode")]
use unity_asset_binary::asset::SerializedObjectContext;
use unity_asset_binary::asset::{SerializedFile, SerializedFileInspection};
use unity_asset_binary::bundle::{AssetBundle, BundleInspection, BundleLayoutKind};
use unity_asset_binary::compression::CompressionType;
use unity_asset_binary::reader::ByteOrder;
use unity_asset_binary::webfile::{WebFile, WebFileCompression, WebFileInspection};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, ContractError, ContractJsonLimits,
    ContractJsonResourceModel, Diagnostic, DiagnosticSeverity, ObjectAddress,
    RevisionedObjectHandle, SourceFingerprint, SourceId, SourceKind, SourceLocator, UnityValue,
    WorkspaceId, WorkspaceRevision, read_contract_json, string_allocation_bytes,
};
use unity_asset_write::artifact::PreparedArtifactFormat;

use super::snapshot::{budgeted_result_vec, consume_retained_bytes, consume_single_result};
use super::{
    ReferenceViewParts, ReferenceViewState, WorkspaceAllocationUnit, WorkspaceByteRange,
    WorkspaceError, WorkspaceLookup, WorkspaceObject, WorkspaceObjectValue, WorkspaceSource,
    WorkspaceView, reference_view_parts,
};

/// Current wire version for workspace source inspection projections.
pub const WORKSPACE_SOURCE_INSPECTION_VERSION: u8 = 1;
/// Current wire version for workspace object inspection projections.
pub const WORKSPACE_OBJECT_INSPECTION_VERSION: u8 = 2;
/// Current wire version for streamed-resource requests and query results.
pub const STREAMED_RESOURCE_QUERY_VERSION: u8 = 2;

const MAX_STREAM_PATH_BYTES: usize = 4096;
const STREAMED_RESOURCE_REQUEST_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.streamed_resource_request",
    64 * 1024,
    16,
    512,
    512,
    ContractJsonResourceModel::new(6, 4 * 1024, 2 * 1024, 512),
);

#[cfg(test)]
std::thread_local! {
    static STREAMED_RESOURCE_INDEX_BUILDS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

/// Stable byte order used by SerializedFile inspection output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceByteOrder {
    Little,
    Big,
}

impl From<ByteOrder> for WorkspaceByteOrder {
    fn from(value: ByteOrder) -> Self {
        match value {
            ByteOrder::Little => Self::Little,
            ByteOrder::Big => Self::Big,
        }
    }
}

/// Stable compression vocabulary shared by bundle and WebFile summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCompression {
    None,
    Lzma,
    Lz4,
    Lz4Hc,
    Lzham,
    Brotli,
    Gzip,
}

impl From<CompressionType> for WorkspaceCompression {
    fn from(value: CompressionType) -> Self {
        match value {
            CompressionType::None => Self::None,
            CompressionType::Lzma => Self::Lzma,
            CompressionType::Lz4 => Self::Lz4,
            CompressionType::Lz4Hc => Self::Lz4Hc,
            CompressionType::Lzham => Self::Lzham,
            CompressionType::Brotli => Self::Brotli,
        }
    }
}

impl From<WebFileCompression> for WorkspaceCompression {
    fn from(value: WebFileCompression) -> Self {
        match value {
            WebFileCompression::None => Self::None,
            WebFileCompression::Gzip => Self::Gzip,
            WebFileCompression::Brotli => Self::Brotli,
        }
    }
}

/// Stable physical layout family used by AssetBundle inspection output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceBundleLayout {
    FileStream,
    Legacy,
}

impl From<BundleLayoutKind> for WorkspaceBundleLayout {
    fn from(value: BundleLayoutKind) -> Self {
        match value {
            BundleLayoutKind::FileStream => Self::FileStream,
            BundleLayoutKind::Legacy => Self::Legacy,
        }
    }
}

/// Signed path-ID distribution for one validated SerializedFile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SerializedPathIdSummary {
    negative: u64,
    positive: u64,
    minimum: Option<i64>,
    maximum: Option<i64>,
}

impl SerializedPathIdSummary {
    #[must_use]
    pub const fn negative(self) -> u64 {
        self.negative
    }

    #[must_use]
    pub const fn positive(self) -> u64 {
        self.positive
    }

    #[must_use]
    pub const fn minimum(self) -> Option<i64> {
        self.minimum
    }

    #[must_use]
    pub const fn maximum(self) -> Option<i64> {
        self.maximum
    }
}

/// Format-specific metadata for one validated SerializedFile image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SerializedFileSummary {
    version: u32,
    byte_order: WorkspaceByteOrder,
    metadata_size: u32,
    data_offset: u64,
    declared_file_size: u64,
    unity_version: String,
    target_platform: i32,
    #[cfg(feature = "decode")]
    #[serde(skip)]
    object_context: Option<SerializedObjectContext>,
    type_tree_enabled: bool,
    legacy_big_id: Option<i32>,
    object_count: u64,
    type_count: u64,
    script_type_count: u64,
    external_count: u64,
    reference_type_count: u64,
    path_ids: SerializedPathIdSummary,
}

impl SerializedFileSummary {
    pub(crate) fn from_file(
        file: &SerializedFile,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        consume_scan(file.objects().len(), "serialized_path_id_scan", budget)?;
        Ok(Self {
            version: file.format().version(),
            byte_order: file.header.byte_order().into(),
            metadata_size: file.header.metadata_size,
            data_offset: file.header.data_offset,
            declared_file_size: file.header.file_size,
            unity_version: clone_text(
                &file.unity_version,
                budget,
                "serialized_file_unity_version",
            )?,
            target_platform: file.target_platform,
            #[cfg(feature = "decode")]
            object_context: Some(file.object_context()),
            type_tree_enabled: file.type_tree_enabled(),
            legacy_big_id: file.legacy_big_id(),
            object_count: usize_to_u64(file.objects().len(), "serialized_object_count")?,
            type_count: usize_to_u64(file.types().len(), "serialized_type_count")?,
            script_type_count: usize_to_u64(
                file.script_types.len(),
                "serialized_script_type_count",
            )?,
            external_count: usize_to_u64(file.externals.len(), "serialized_external_count")?,
            reference_type_count: usize_to_u64(
                file.ref_types().len(),
                "serialized_reference_type_count",
            )?,
            path_ids: path_id_summary(file.objects().iter().map(|object| object.path_id()))?,
        })
    }

    pub(crate) fn from_proof(
        proof: &SerializedFileInspection,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        consume_scan(proof.objects().len(), "serialized_path_id_scan", budget)?;
        Ok(Self {
            version: proof.version(),
            byte_order: proof.byte_order().into(),
            metadata_size: proof.metadata_size(),
            data_offset: proof.data_offset(),
            declared_file_size: proof.declared_file_size(),
            unity_version: clone_text(
                proof.unity_version(),
                budget,
                "serialized_file_unity_version",
            )?,
            target_platform: proof.target_platform(),
            #[cfg(feature = "decode")]
            object_context: Some(proof.object_context()),
            type_tree_enabled: proof.type_tree_enabled(),
            legacy_big_id: proof.legacy_big_id(),
            object_count: usize_to_u64(proof.objects().len(), "serialized_object_count")?,
            type_count: proof.type_count(),
            script_type_count: proof.script_type_count(),
            external_count: usize_to_u64(proof.externals().len(), "serialized_external_count")?,
            reference_type_count: proof.reference_type_count(),
            path_ids: path_id_summary(proof.objects().iter().map(|object| object.path_id()))?,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn byte_order(&self) -> WorkspaceByteOrder {
        self.byte_order
    }

    #[must_use]
    pub const fn metadata_size(&self) -> u32 {
        self.metadata_size
    }

    #[must_use]
    pub const fn data_offset(&self) -> u64 {
        self.data_offset
    }

    #[must_use]
    pub const fn declared_file_size(&self) -> u64 {
        self.declared_file_size
    }

    #[must_use]
    pub fn unity_version(&self) -> &str {
        &self.unity_version
    }

    #[must_use]
    pub const fn target_platform(&self) -> i32 {
        self.target_platform
    }

    #[cfg(feature = "decode")]
    pub(crate) const fn object_context(&self) -> Option<SerializedObjectContext> {
        self.object_context
    }

    #[must_use]
    pub const fn type_tree_enabled(&self) -> bool {
        self.type_tree_enabled
    }

    #[must_use]
    pub const fn legacy_big_id(&self) -> Option<i32> {
        self.legacy_big_id
    }

    #[must_use]
    pub const fn object_count(&self) -> u64 {
        self.object_count
    }

    #[must_use]
    pub const fn type_count(&self) -> u64 {
        self.type_count
    }

    #[must_use]
    pub const fn script_type_count(&self) -> u64 {
        self.script_type_count
    }

    #[must_use]
    pub const fn external_count(&self) -> u64 {
        self.external_count
    }

    #[must_use]
    pub const fn reference_type_count(&self) -> u64 {
        self.reference_type_count
    }

    #[must_use]
    pub const fn path_ids(&self) -> SerializedPathIdSummary {
        self.path_ids
    }

    fn retained_clone_bytes(&self) -> Result<usize, WorkspaceError> {
        retained_string_clone_bytes(&[&self.unity_version])
    }
}

/// Compact AssetBundle metadata frozen when a source is loaded or reparsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetBundleSummary {
    signature: String,
    version: u32,
    unity_version: String,
    unity_revision: String,
    layout: WorkspaceBundleLayout,
    declared_size: u64,
    flags: u32,
    compression: WorkspaceCompression,
    block_count: u64,
    directory_count: u64,
}

impl AssetBundleSummary {
    pub(crate) fn from_bundle(
        bundle: &AssetBundle,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        let layout = bundle
            .header
            .layout_kind()
            .map_err(|error| WorkspaceError::operation("bundle layout inspection", error))?;
        let compression = bundle
            .header
            .compression_type()
            .map_err(|error| WorkspaceError::operation("bundle compression inspection", error))?;
        Ok(Self {
            signature: clone_text(
                &bundle.header.signature,
                budget,
                "bundle_inspection_signature",
            )?,
            version: bundle.header.version,
            unity_version: clone_text(
                &bundle.header.unity_version,
                budget,
                "bundle_inspection_unity_version",
            )?,
            unity_revision: clone_text(
                &bundle.header.unity_revision,
                budget,
                "bundle_inspection_unity_revision",
            )?,
            layout: layout.into(),
            declared_size: bundle.header.size,
            flags: bundle.header.flags,
            compression: compression.into(),
            block_count: usize_to_u64(bundle.blocks.len(), "bundle_block_count")?,
            directory_count: usize_to_u64(bundle.nodes.len(), "bundle_directory_count")?,
        })
    }

    pub(crate) fn from_proof(
        proof: &BundleInspection,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self {
            signature: clone_text(proof.signature(), budget, "bundle_inspection_signature")?,
            version: proof.version(),
            unity_version: clone_text(
                proof.unity_version(),
                budget,
                "bundle_inspection_unity_version",
            )?,
            unity_revision: clone_text(
                proof.unity_revision(),
                budget,
                "bundle_inspection_unity_revision",
            )?,
            layout: proof.layout().into(),
            declared_size: proof.declared_size(),
            flags: proof.flags(),
            compression: proof.compression().into(),
            block_count: usize_to_u64(proof.blocks().len(), "bundle_block_count")?,
            directory_count: usize_to_u64(proof.directory().len(), "bundle_directory_count")?,
        })
    }

    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn unity_version(&self) -> &str {
        &self.unity_version
    }

    #[must_use]
    pub fn unity_revision(&self) -> &str {
        &self.unity_revision
    }

    #[must_use]
    pub const fn layout(&self) -> WorkspaceBundleLayout {
        self.layout
    }

    #[must_use]
    pub const fn declared_size(&self) -> u64 {
        self.declared_size
    }

    #[must_use]
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    #[must_use]
    pub const fn compression(&self) -> WorkspaceCompression {
        self.compression
    }

    #[must_use]
    pub const fn block_count(&self) -> u64 {
        self.block_count
    }

    #[must_use]
    pub const fn directory_count(&self) -> u64 {
        self.directory_count
    }

    fn retained_clone_bytes(&self) -> Result<usize, WorkspaceError> {
        retained_string_clone_bytes(&[&self.signature, &self.unity_version, &self.unity_revision])
    }
}

/// Compact WebFile metadata frozen when a source is loaded or reparsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WebFileSummary {
    signature: String,
    compression: WorkspaceCompression,
    directory_count: u64,
}

impl WebFileSummary {
    pub(crate) fn from_webfile(
        web_file: &WebFile,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self {
            signature: clone_text(&web_file.signature, budget, "webfile_inspection_signature")?,
            compression: web_file.compression.into(),
            directory_count: usize_to_u64(web_file.files().len(), "webfile_directory_count")?,
        })
    }

    pub(crate) fn from_proof(
        proof: &WebFileInspection,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self {
            signature: clone_text(proof.signature(), budget, "webfile_inspection_signature")?,
            compression: proof.compression().into(),
            directory_count: usize_to_u64(proof.directory().len(), "webfile_directory_count")?,
        })
    }

    #[must_use]
    pub fn signature(&self) -> &str {
        &self.signature
    }

    #[must_use]
    pub const fn compression(&self) -> WorkspaceCompression {
        self.compression
    }

    #[must_use]
    pub const fn directory_count(&self) -> u64 {
        self.directory_count
    }

    fn retained_clone_bytes(&self) -> Result<usize, WorkspaceError> {
        retained_string_clone_bytes(&[&self.signature])
    }
}

/// Format-specific source metadata retained without reparsing source bytes at query time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "summary", rename_all = "snake_case")]
pub enum WorkspaceSourceFormatInspection {
    Archive { member_count: u64 },
    AssetBundle(AssetBundleSummary),
    SerializedFile(SerializedFileSummary),
    StreamedResource,
    WebFile(WebFileSummary),
    Yaml { document_count: u64 },
}

impl WorkspaceSourceFormatInspection {
    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        match self {
            Self::Archive { .. } => SourceKind::Archive,
            Self::AssetBundle(_) => SourceKind::AssetBundle,
            Self::SerializedFile(_) => SourceKind::SerializedFile,
            Self::StreamedResource => SourceKind::StreamedResource,
            Self::WebFile(_) => SourceKind::WebFile,
            Self::Yaml { .. } => SourceKind::Yaml,
        }
    }

    pub(crate) fn try_clone_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        let retained = match self {
            Self::AssetBundle(summary) => summary.retained_clone_bytes()?,
            Self::SerializedFile(summary) => summary.retained_clone_bytes()?,
            Self::WebFile(summary) => summary.retained_clone_bytes()?,
            Self::Archive { .. } | Self::StreamedResource | Self::Yaml { .. } => 0,
        };
        consume_retained_bytes(retained, "source_inspection_clone", budget)?;
        Ok(self.clone())
    }

    #[cfg(test)]
    pub(crate) fn minimal_for_test(kind: SourceKind) -> Self {
        match kind {
            SourceKind::Archive => Self::Archive { member_count: 0 },
            SourceKind::AssetBundle => Self::AssetBundle(AssetBundleSummary {
                signature: String::new(),
                version: 0,
                unity_version: String::new(),
                unity_revision: String::new(),
                layout: WorkspaceBundleLayout::FileStream,
                declared_size: 0,
                flags: 0,
                compression: WorkspaceCompression::None,
                block_count: 0,
                directory_count: 0,
            }),
            SourceKind::SerializedFile => Self::SerializedFile(SerializedFileSummary {
                version: 0,
                byte_order: WorkspaceByteOrder::Little,
                metadata_size: 0,
                data_offset: 0,
                declared_file_size: 0,
                unity_version: String::new(),
                target_platform: 0,
                #[cfg(feature = "decode")]
                object_context: None,
                type_tree_enabled: false,
                legacy_big_id: None,
                object_count: 0,
                type_count: 0,
                script_type_count: 0,
                external_count: 0,
                reference_type_count: 0,
                path_ids: SerializedPathIdSummary::default(),
            }),
            SourceKind::StreamedResource => Self::StreamedResource,
            SourceKind::WebFile => Self::WebFile(WebFileSummary {
                signature: String::new(),
                compression: WorkspaceCompression::None,
                directory_count: 0,
            }),
            SourceKind::Yaml => Self::Yaml { document_count: 0 },
        }
    }
}

/// One versioned source projection bound to an exact workspace revision.
#[derive(Debug, Clone)]
pub struct WorkspaceSourceInspection {
    source: WorkspaceSource,
    revision: WorkspaceRevision,
    parent_locator: Option<SourceLocator>,
    encoded_length: u64,
    format: WorkspaceSourceFormatInspection,
}

impl WorkspaceSourceInspection {
    #[must_use]
    pub const fn version(&self) -> u8 {
        WORKSPACE_SOURCE_INSPECTION_VERSION
    }

    #[must_use]
    pub const fn source(&self) -> &WorkspaceSource {
        &self.source
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn parent_locator(&self) -> Option<&SourceLocator> {
        self.parent_locator.as_ref()
    }

    #[must_use]
    pub const fn encoded_length(&self) -> u64 {
        self.encoded_length
    }

    #[must_use]
    pub const fn format(&self) -> &WorkspaceSourceFormatInspection {
        &self.format
    }
}

impl Serialize for WorkspaceSourceInspection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("WorkspaceSourceInspection", 12)?;
        state.serialize_field("version", &WORKSPACE_SOURCE_INSPECTION_VERSION)?;
        state.serialize_field("workspace_id", &self.source.id().workspace())?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("source_id", &self.source.id())?;
        state.serialize_field("kind", &self.source.kind())?;
        state.serialize_field("locator", self.source.locator())?;
        state.serialize_field("fingerprint", &self.source.fingerprint())?;
        state.serialize_field("parent", &self.source.parent())?;
        state.serialize_field("parent_locator", &self.parent_locator)?;
        state.serialize_field("location", &self.source.location())?;
        state.serialize_field("encoded_length", &self.encoded_length)?;
        state.serialize_field("format", &self.format)?;
        state.end()
    }
}

/// Format-specific object metadata that does not require Display parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceObjectFormatInspection {
    Binary {
        path_id: i64,
        byte_start: u64,
        byte_size: u32,
        payload_bytes: u64,
        byte_order: WorkspaceByteOrder,
    },
    Yaml {
        document_index: u64,
    },
}

/// Owned, read-only object inspection projection.
///
/// This value is intentionally serializable but not deserializable. Recipe lowering
/// continues to require the opaque, revision-bound observation returned by
/// [`crate::schema::SchemaRecipePlanner`].
#[derive(Debug, Clone)]
pub struct WorkspaceObjectInspection {
    address: ObjectAddress,
    object: WorkspaceObject,
    format: WorkspaceObjectFormatInspection,
}

impl WorkspaceObjectInspection {
    #[must_use]
    pub const fn version(&self) -> u8 {
        WORKSPACE_OBJECT_INSPECTION_VERSION
    }

    #[must_use]
    pub const fn address(&self) -> &ObjectAddress {
        &self.address
    }

    #[must_use]
    pub const fn object(&self) -> &WorkspaceObject {
        &self.object
    }

    #[must_use]
    pub const fn format(&self) -> WorkspaceObjectFormatInspection {
        self.format
    }
}

#[derive(Serialize)]
struct UnityClassInspectionRef<'class> {
    class_id: i32,
    class_name: &'class str,
    anchor: &'class str,
    extra_anchor_data: &'class str,
    properties: &'class IndexMap<String, UnityValue>,
}

impl Serialize for WorkspaceObjectInspection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let class = self.object.class();
        let class = UnityClassInspectionRef {
            class_id: class.class_id(),
            class_name: class.class_name(),
            anchor: class.anchor(),
            extra_anchor_data: class.extra_anchor_data(),
            properties: class.properties(),
        };
        let mut state = serializer.serialize_struct("WorkspaceObjectInspection", 8)?;
        state.serialize_field("version", &WORKSPACE_OBJECT_INSPECTION_VERSION)?;
        state.serialize_field("workspace_id", &self.object.handle().workspace())?;
        state.serialize_field("revision", &self.object.handle().revision())?;
        state.serialize_field("address", &self.address)?;
        state.serialize_field("source_id", &self.object.handle().object().source())?;
        state.serialize_field("format", &self.format)?;
        state.serialize_field("schema", self.object.schema_provenance())?;
        state.serialize_field("class", &class)?;
        state.end()
    }
}

/// Versioned logical resource request resolved only against loaded workspace sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedResourceRequest {
    owner: SourceLocator,
    stream_path: String,
    offset: u64,
    size: u64,
}

impl StreamedResourceRequest {
    pub fn new(
        owner: SourceLocator,
        stream_path: impl Into<String>,
        offset: u64,
        size: u64,
    ) -> Result<Self, StreamedResourceRequestError> {
        let stream_path = stream_path.into();
        Self::validate_parts(&stream_path, offset, size)?;
        Ok(Self {
            owner,
            stream_path,
            offset,
            size,
        })
    }

    #[must_use]
    pub const fn owner(&self) -> &SourceLocator {
        &self.owner
    }

    #[must_use]
    pub fn stream_path(&self) -> &str {
        &self.stream_path
    }

    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn validate_parts(
        stream_path: &str,
        offset: u64,
        size: u64,
    ) -> Result<(), StreamedResourceRequestError> {
        validate_stream_path(stream_path)?;
        if size == 0 {
            return Err(StreamedResourceRequestError::ZeroSize);
        }
        offset
            .checked_add(size)
            .ok_or(StreamedResourceRequestError::RangeOverflow { offset, size })?;
        Ok(())
    }

    /// Reads an untrusted request with caller-owned input and parser-work budgets.
    pub fn read_json(
        reader: impl Read,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, BudgetedJsonError> {
        read_contract_json(reader, budget, STREAMED_RESOURCE_REQUEST_JSON_LIMITS)
    }

    fn try_clone_with_budget(&self, budget: &mut AssetLoadBudget) -> Result<Self, WorkspaceError> {
        let locator_bytes =
            self.owner
                .retained_clone_bytes()
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "streamed_resource_request_clone",
                })?;
        let bytes = locator_bytes.checked_add(self.stream_path.len()).ok_or(
            BudgetError::ArithmeticOverflow {
                resource: "streamed_resource_request_clone",
            },
        )?;
        consume_retained_bytes(bytes, "streamed_resource_request_clone", budget)?;
        Ok(self.clone())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamedResourceRequestWire {
    version: u8,
    owner: SourceLocator,
    stream_path: BoundedStreamPath,
    offset: u64,
    size: u64,
}

struct BoundedStreamPath(String);

impl<'de> Deserialize<'de> for BoundedStreamPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedStreamPathVisitor;

        impl<'de> Visitor<'de> for BoundedStreamPathVisitor {
            type Value = BoundedStreamPath;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "a valid streamed-resource path of at most {MAX_STREAM_PATH_BYTES} bytes"
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_stream_path(value).map_err(E::custom)?;
                Ok(BoundedStreamPath(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_stream_path(&value).map_err(E::custom)?;
                Ok(BoundedStreamPath(value))
            }
        }

        deserializer.deserialize_str(BoundedStreamPathVisitor)
    }
}

impl Serialize for StreamedResourceRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("StreamedResourceRequest", 5)?;
        state.serialize_field("version", &STREAMED_RESOURCE_QUERY_VERSION)?;
        state.serialize_field("owner", &self.owner)?;
        state.serialize_field("stream_path", &self.stream_path)?;
        state.serialize_field("offset", &self.offset)?;
        state.serialize_field("size", &self.size)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for StreamedResourceRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = StreamedResourceRequestWire::deserialize(deserializer)?;
        if wire.version != STREAMED_RESOURCE_QUERY_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported streamed-resource query version {}",
                wire.version
            )));
        }
        Self::new(wire.owner, wire.stream_path.0, wire.offset, wire.size).map_err(D::Error::custom)
    }
}

/// Stable candidate identity returned when a resource path is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamedResourceCandidate {
    source_id: SourceId,
    locator: SourceLocator,
    fingerprint: SourceFingerprint,
}

impl StreamedResourceCandidate {
    fn from_source(
        source: &WorkspaceSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        let retained =
            source
                .locator()
                .retained_clone_bytes()
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "streamed_resource_candidate",
                })?;
        consume_retained_bytes(retained, "streamed_resource_candidate", budget)?;
        Ok(Self {
            source_id: source.id(),
            locator: source.locator().clone(),
            fingerprint: source.fingerprint(),
        })
    }

    #[must_use]
    pub const fn source_id(&self) -> SourceId {
        self.source_id
    }

    #[must_use]
    pub const fn locator(&self) -> &SourceLocator {
        &self.locator
    }

    #[must_use]
    pub const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }
}

/// A resolved, revision-bound streamed-resource range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedStreamedResource {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    candidate: StreamedResourceCandidate,
    offset: u64,
    size: u64,
}

impl ResolvedStreamedResource {
    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn source(&self) -> &StreamedResourceCandidate {
        &self.candidate
    }

    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Opens the exact range after revalidating revision, source identity, and fingerprint.
    pub fn open(
        &self,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceByteRange, WorkspaceError> {
        if view.workspace_id() != self.workspace_id {
            return Err(ContractError::WorkspaceMismatch {
                expected: self.workspace_id,
                actual: view.workspace_id(),
            }
            .into());
        }
        if view.revision() != self.revision {
            return Err(ContractError::RevisionMismatch {
                expected: self.revision,
                actual: view.revision(),
            }
            .into());
        }
        let source = match view.source(self.candidate.source_id, budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded | WorkspaceLookup::Missing => {
                return Err(WorkspaceError::MissingSource(self.candidate.source_id));
            }
            WorkspaceLookup::Ambiguous { .. } | WorkspaceLookup::Invalid { .. } => {
                return Err(WorkspaceError::operation(
                    "streamed-resource source validation",
                    io::Error::other("resolved source identity became invalid"),
                ));
            }
        };
        if source.kind() != SourceKind::StreamedResource
            || source.locator() != self.candidate.locator()
            || source.fingerprint() != self.candidate.fingerprint()
        {
            return Err(WorkspaceError::ObservedSourceChanged {
                source_id: Box::new(self.candidate.source_id),
                expected: self.candidate.fingerprint,
                actual: source.fingerprint(),
            });
        }
        view.read_source_range(self.candidate.source_id, self.offset, self.size, budget)
    }
}

/// Terminal state of one loaded-catalog-only stream query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StreamedResourceResolution {
    Resolved {
        resource: ResolvedStreamedResource,
    },
    Missing,
    Ambiguous {
        candidates: Vec<StreamedResourceCandidate>,
    },
    OwnerUnloaded,
    OwnerMissing,
    Invalid {
        diagnostic: Diagnostic,
    },
}

/// Versioned streamed-resource query result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamedResourceQueryResult {
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request: StreamedResourceRequest,
    resolution: StreamedResourceResolution,
}

impl StreamedResourceQueryResult {
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    #[must_use]
    pub const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn request(&self) -> &StreamedResourceRequest {
        &self.request
    }

    #[must_use]
    pub const fn resolution(&self) -> &StreamedResourceResolution {
        &self.resolution
    }
}

/// Invalid logical streamed-resource request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StreamedResourceRequestError {
    #[error("stream path is empty")]
    EmptyPath,
    #[error("stream path exceeds {maximum} UTF-8 bytes: {actual}")]
    PathTooLong { actual: usize, maximum: usize },
    #[error("stream path contains a NUL or control character")]
    ControlCharacter,
    #[error("stream path has no safe final component")]
    InvalidBasename,
    #[error("stream range size must be non-zero")]
    ZeroSize,
    #[error("stream range overflows: offset={offset}, size={size}")]
    RangeOverflow { offset: u64, size: u64 },
}

struct StreamedResourceIndexEntry {
    basename_key: String,
    source_index: usize,
}

/// Caller-budgeted streamed-resource index shared by inspection and extraction.
///
/// Sources are indexed once by their normalized basename. Queries use a binary
/// search to find the matching basename bucket, then apply the canonical path
/// rank and owner-proximity rules only within that bucket.
pub(crate) struct StreamedResourceResolver<'view, 'source> {
    view: &'view dyn WorkspaceView,
    sources: &'source [WorkspaceSource],
    entries: Vec<StreamedResourceIndexEntry>,
}

impl<'view, 'source> StreamedResourceResolver<'view, 'source> {
    pub(crate) fn new(
        view: &'view dyn WorkspaceView,
        sources: &'source [WorkspaceSource],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        let count = sources
            .iter()
            .filter(|source| source.kind() == SourceKind::StreamedResource)
            .count();
        let mut entries = budgeted_result_vec::<StreamedResourceIndexEntry>(count, budget)?;
        for (source_index, source) in sources.iter().enumerate() {
            if source.kind() != SourceKind::StreamedResource {
                continue;
            }
            let mut basename_key = clone_text(
                stream_basename(source_logical_path(source))
                    .unwrap_or_else(|| source.locator().root_alias().as_str()),
                budget,
                "streamed_resource_index_key",
            )?;
            basename_key.make_ascii_lowercase();
            entries.push(StreamedResourceIndexEntry {
                basename_key,
                source_index,
            });
        }
        entries.sort_unstable_by(|left, right| {
            left.basename_key
                .cmp(&right.basename_key)
                .then_with(|| left.source_index.cmp(&right.source_index))
        });
        #[cfg(test)]
        STREAMED_RESOURCE_INDEX_BUILDS.with(|builds| builds.set(builds.get().saturating_add(1)));
        Ok(Self {
            view,
            sources,
            entries,
        })
    }

    pub(crate) fn resolve(
        &self,
        owner: &WorkspaceSource,
        stream_path: &str,
        offset: u64,
        size: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceResolution, WorkspaceError> {
        let request_basename = stream_basename(stream_path).ok_or_else(|| {
            WorkspaceError::operation(
                "streamed-resource request",
                io::Error::new(io::ErrorKind::InvalidInput, "stream path has no basename"),
            )
        })?;
        let mut lookup_key = clone_text(request_basename, budget, "streamed_resource_lookup_key")?;
        lookup_key.make_ascii_lowercase();
        let start = self
            .entries
            .partition_point(|entry| entry.basename_key.as_str() < lookup_key.as_str());
        let end = self
            .entries
            .partition_point(|entry| entry.basename_key.as_str() <= lookup_key.as_str());
        if start == end {
            return Ok(StreamedResourceResolution::Missing);
        }

        let bucket = &self.entries[start..end];
        consume_scan(bucket.len(), "streamed_resource_candidate_scan", budget)?;
        let mut best_key = None;
        let mut best_source_index = 0_usize;
        let mut matching_count = 0_usize;
        for entry in bucket {
            let candidate = &self.sources[entry.source_index];
            let Some(path_rank) =
                stream_candidate_match_rank(candidate, stream_path, request_basename)
            else {
                continue;
            };
            let key = (path_rank, stream_source_score(owner, candidate));
            match best_key {
                None => {
                    best_key = Some(key);
                    best_source_index = entry.source_index;
                    matching_count = 1;
                }
                Some(current) if key < current => {
                    best_key = Some(key);
                    best_source_index = entry.source_index;
                    matching_count = 1;
                }
                Some(current) if key == current => {
                    matching_count =
                        matching_count
                            .checked_add(1)
                            .ok_or(BudgetError::ArithmeticOverflow {
                                resource: "streamed_resource_candidate_count",
                            })?;
                }
                Some(_) => {}
            }
        }
        let Some(best_key) = best_key else {
            return Ok(StreamedResourceResolution::Missing);
        };
        if matching_count != 1 {
            let mut candidates =
                budgeted_result_vec::<StreamedResourceCandidate>(matching_count, budget)?;
            consume_scan(bucket.len(), "streamed_resource_ambiguity_scan", budget)?;
            for entry in bucket {
                let candidate = &self.sources[entry.source_index];
                let Some(path_rank) =
                    stream_candidate_match_rank(candidate, stream_path, request_basename)
                else {
                    continue;
                };
                if (path_rank, stream_source_score(owner, candidate)) == best_key {
                    candidates.push(StreamedResourceCandidate::from_source(candidate, budget)?);
                }
            }
            return Ok(StreamedResourceResolution::Ambiguous { candidates });
        }

        let source = &self.sources[best_source_index];
        let end = offset
            .checked_add(size)
            .ok_or(WorkspaceError::RangeOverflow { offset, size })?;
        let source_len = self.view.source_length(source.id())?;
        if end > source_len {
            return Err(WorkspaceError::RangeOutOfBounds {
                source_id: source.id(),
                offset,
                end,
                source_len,
            });
        }
        let candidate = StreamedResourceCandidate::from_source(source, budget)?;
        Ok(StreamedResourceResolution::Resolved {
            resource: ResolvedStreamedResource {
                workspace_id: self.view.workspace_id(),
                revision: self.view.revision(),
                candidate,
                offset,
                size,
            },
        })
    }

    pub(crate) fn resolve_request(
        &self,
        request: &StreamedResourceRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceResolution, WorkspaceError> {
        let owner = match self.view.resolve_source(request.owner(), budget)? {
            WorkspaceLookup::Resolved(source) => source,
            WorkspaceLookup::Unloaded => return Ok(StreamedResourceResolution::OwnerUnloaded),
            WorkspaceLookup::Missing => return Ok(StreamedResourceResolution::OwnerMissing),
            WorkspaceLookup::Ambiguous { .. } => {
                return invalid_stream_resolution(
                    "WORKSPACE_STREAM_OWNER_AMBIGUOUS",
                    "stream owner resolves to multiple loaded sources",
                    budget,
                );
            }
            WorkspaceLookup::Invalid { diagnostic } => {
                return Ok(StreamedResourceResolution::Invalid { diagnostic });
            }
        };
        self.resolve(
            &owner,
            request.stream_path(),
            request.offset(),
            request.size(),
            budget,
        )
    }

    #[cfg(test)]
    pub(crate) fn reset_test_build_count() {
        STREAMED_RESOURCE_INDEX_BUILDS.with(|builds| builds.set(0));
    }

    #[cfg(test)]
    pub(crate) fn test_build_count() -> usize {
        STREAMED_RESOURCE_INDEX_BUILDS.with(std::cell::Cell::get)
    }
}

/// Deep inspection interface shared by committed snapshots and prepared overlays.
pub struct WorkspaceInspector<'view> {
    view: &'view dyn WorkspaceView,
}

impl<'view> WorkspaceInspector<'view> {
    #[must_use]
    pub const fn new(view: &'view dyn WorkspaceView) -> Self {
        Self { view }
    }

    pub fn sources(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<WorkspaceSourceInspection>, WorkspaceError> {
        let sources = self.view.sources(budget)?;
        let mut output = budgeted_result_vec::<WorkspaceSourceInspection>(sources.len(), budget)?;
        for source in sources {
            output.push(self.inspect_source(source, budget)?);
        }
        Ok(output)
    }

    pub fn source(
        &self,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceSourceInspection>, WorkspaceError> {
        match self.view.source(source, budget)? {
            WorkspaceLookup::Resolved(source) => Ok(WorkspaceLookup::Resolved(
                self.inspect_source(source, budget)?,
            )),
            WorkspaceLookup::Unloaded => Ok(WorkspaceLookup::Unloaded),
            WorkspaceLookup::Missing => Ok(WorkspaceLookup::Missing),
            WorkspaceLookup::Ambiguous { candidates } => {
                let mut output =
                    budgeted_result_vec::<WorkspaceSourceInspection>(candidates.len(), budget)?;
                for source in candidates {
                    output.push(self.inspect_source(source, budget)?);
                }
                Ok(WorkspaceLookup::Ambiguous { candidates: output })
            }
            WorkspaceLookup::Invalid { diagnostic } => Ok(WorkspaceLookup::Invalid { diagnostic }),
        }
    }

    #[cfg(feature = "decode")]
    pub(crate) fn serialized_object_context(
        &self,
        source: SourceId,
    ) -> Result<SerializedObjectContext, WorkspaceError> {
        serialized_object_context(reference_view_parts(self.view), source)
    }

    pub fn object(
        &self,
        address: &ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceLookup<WorkspaceObjectInspection>, WorkspaceError> {
        match self.view.resolve_object(address, budget)? {
            WorkspaceLookup::Resolved(handle) => Ok(WorkspaceLookup::Resolved(
                self.inspect_handle(&handle, budget)?,
            )),
            WorkspaceLookup::Unloaded => Ok(WorkspaceLookup::Unloaded),
            WorkspaceLookup::Missing => Ok(WorkspaceLookup::Missing),
            WorkspaceLookup::Ambiguous { candidates } => {
                let mut output =
                    budgeted_result_vec::<WorkspaceObjectInspection>(candidates.len(), budget)?;
                for handle in candidates {
                    output.push(self.inspect_handle(&handle, budget)?);
                }
                Ok(WorkspaceLookup::Ambiguous { candidates: output })
            }
            WorkspaceLookup::Invalid { diagnostic } => Ok(WorkspaceLookup::Invalid { diagnostic }),
        }
    }

    pub fn objects(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<WorkspaceObjectInspection>, WorkspaceError> {
        let handles = self.view.objects(budget)?;
        let mut output = budgeted_result_vec::<WorkspaceObjectInspection>(handles.len(), budget)?;
        for handle in handles {
            output.push(self.inspect_handle(&handle, budget)?);
        }
        Ok(output)
    }

    pub fn resolve_streamed_resource(
        &self,
        request: &StreamedResourceRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<StreamedResourceQueryResult, WorkspaceError> {
        let request_copy = request.try_clone_with_budget(budget)?;
        let sources = self.view.sources(budget)?;
        let resolver = StreamedResourceResolver::new(self.view, &sources, budget)?;
        let resolution = resolver.resolve_request(request, budget)?;
        Ok(self.stream_result(request_copy, resolution))
    }

    fn inspect_source(
        &self,
        source: WorkspaceSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceSourceInspection, WorkspaceError> {
        let parts = reference_view_parts(self.view);
        let catalog = catalog(&parts);
        let parent_locator = source
            .parent()
            .map(|parent| clone_locator(catalog.source_locator(parent)?, budget))
            .transpose()?;
        let encoded_length = self.view.source_length(source.id())?;
        let format = format_inspection(parts, source.id(), budget)?;
        if format.kind() != source.kind() {
            return Err(WorkspaceError::operation(
                "workspace source inspection",
                io::Error::other("frozen source format does not match source identity"),
            ));
        }
        Ok(WorkspaceSourceInspection {
            source,
            revision: self.view.revision(),
            parent_locator,
            encoded_length,
            format,
        })
    }

    fn inspect_handle(
        &self,
        handle: &RevisionedObjectHandle,
        budget: &mut AssetLoadBudget,
    ) -> Result<WorkspaceObjectInspection, WorkspaceError> {
        let address = self.view.object_address(handle, budget)?;
        let object = self.view.read_object(handle, budget)?;
        let format = match object.value() {
            WorkspaceObjectValue::Binary(object) => WorkspaceObjectFormatInspection::Binary {
                path_id: object.path_id(),
                byte_start: object.byte_start(),
                byte_size: object.byte_size(),
                payload_bytes: usize_to_u64(object.payload_len(), "object_payload_bytes")?,
                byte_order: object.byte_order().into(),
            },
            WorkspaceObjectValue::Yaml(object) => WorkspaceObjectFormatInspection::Yaml {
                document_index: usize_to_u64(object.document_index(), "yaml_document_index")?,
            },
        };
        Ok(WorkspaceObjectInspection {
            address,
            object,
            format,
        })
    }

    fn stream_result(
        &self,
        request: StreamedResourceRequest,
        resolution: StreamedResourceResolution,
    ) -> StreamedResourceQueryResult {
        StreamedResourceQueryResult {
            version: STREAMED_RESOURCE_QUERY_VERSION,
            workspace_id: self.view.workspace_id(),
            revision: self.view.revision(),
            request,
            resolution,
        }
    }
}

pub(crate) fn object_address_for_view(
    view: &dyn WorkspaceView,
    handle: &RevisionedObjectHandle,
    budget: &mut AssetLoadBudget,
) -> Result<ObjectAddress, WorkspaceError> {
    handle.validate_context(view.workspace_id(), view.revision())?;
    let parts = reference_view_parts(view);
    let catalog = catalog(&parts);
    let locator = catalog.source_locator(handle.object().source())?;
    let retained = locator
        .retained_clone_bytes()
        .and_then(|bytes| bytes.checked_add(handle.object().retained_clone_bytes()))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_object_address",
        })?;
    consume_single_result(retained, "workspace_object_address", budget)?;
    catalog
        .address_for_object(handle.object())
        .map_err(WorkspaceError::from)
}

fn format_inspection(
    parts: ReferenceViewParts<'_>,
    source: SourceId,
    budget: &mut AssetLoadBudget,
) -> Result<WorkspaceSourceFormatInspection, WorkspaceError> {
    match parts.state {
        ReferenceViewState::Committed(state) => state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))?
            .format()
            .try_clone_with_budget(budget),
        ReferenceViewState::Prepared(state) => {
            let Some(binding) = state.source_binding(source) else {
                return state
                    .base()
                    .state()
                    .store()
                    .get(source)
                    .ok_or(WorkspaceError::MissingSource(source))?
                    .format()
                    .try_clone_with_budget(budget);
            };
            let artifact = state
                .artifacts()
                .artifact(binding.artifact())
                .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
            match artifact.format() {
                PreparedArtifactFormat::SerializedFile(proof) => {
                    Ok(WorkspaceSourceFormatInspection::SerializedFile(
                        SerializedFileSummary::from_proof(proof, budget)?,
                    ))
                }
                PreparedArtifactFormat::AssetBundle(proof) => {
                    Ok(WorkspaceSourceFormatInspection::AssetBundle(
                        AssetBundleSummary::from_proof(proof, budget)?,
                    ))
                }
                PreparedArtifactFormat::WebFile(proof) => {
                    Ok(WorkspaceSourceFormatInspection::WebFile(
                        WebFileSummary::from_proof(proof, budget)?,
                    ))
                }
                PreparedArtifactFormat::StreamedResource(_) => {
                    Ok(WorkspaceSourceFormatInspection::StreamedResource)
                }
                PreparedArtifactFormat::Yaml(proof) => Ok(WorkspaceSourceFormatInspection::Yaml {
                    document_count: proof.documents(),
                }),
                PreparedArtifactFormat::VerbatimSource(_) => state
                    .base()
                    .state()
                    .store()
                    .get(source)
                    .ok_or(WorkspaceError::MissingSource(source))?
                    .format()
                    .try_clone_with_budget(budget),
                _ => Err(WorkspaceError::operation(
                    "workspace source inspection",
                    io::Error::other("prepared artifact has an unsupported source format"),
                )),
            }
        }
    }
}

#[cfg(feature = "decode")]
fn serialized_object_context(
    parts: ReferenceViewParts<'_>,
    source: SourceId,
) -> Result<SerializedObjectContext, WorkspaceError> {
    match parts.state {
        ReferenceViewState::Committed(state) => state
            .store()
            .get(source)
            .ok_or(WorkspaceError::MissingSource(source))
            .and_then(|entry| stored_object_context(entry.format())),
        ReferenceViewState::Prepared(state) => {
            let Some(binding) = state.source_binding(source) else {
                return state
                    .base()
                    .state()
                    .store()
                    .get(source)
                    .ok_or(WorkspaceError::MissingSource(source))
                    .and_then(|entry| stored_object_context(entry.format()));
            };
            let artifact = state
                .artifacts()
                .artifact(binding.artifact())
                .map_err(|error| WorkspaceError::PreparedArtifact(Box::new(error)))?;
            match artifact.format() {
                PreparedArtifactFormat::SerializedFile(proof) => Ok(proof.object_context()),
                PreparedArtifactFormat::VerbatimSource(_) => state
                    .base()
                    .state()
                    .store()
                    .get(source)
                    .ok_or(WorkspaceError::MissingSource(source))
                    .and_then(|entry| stored_object_context(entry.format())),
                _ => Err(invalid_texture_owner()),
            }
        }
    }
}

#[cfg(feature = "decode")]
fn stored_object_context(
    format: &WorkspaceSourceFormatInspection,
) -> Result<SerializedObjectContext, WorkspaceError> {
    match format {
        WorkspaceSourceFormatInspection::SerializedFile(summary) => {
            summary.object_context().ok_or_else(invalid_texture_owner)
        }
        _ => Err(invalid_texture_owner()),
    }
}

#[cfg(feature = "decode")]
fn invalid_texture_owner() -> WorkspaceError {
    WorkspaceError::operation(
        "Texture2D media context",
        io::Error::other("binary object owner is not a SerializedFile"),
    )
}

fn catalog<'parts>(
    parts: &'parts ReferenceViewParts<'_>,
) -> &'parts super::source_catalog::SourceCatalog {
    match parts.state {
        ReferenceViewState::Committed(state) => state.catalog(),
        ReferenceViewState::Prepared(state) => state.catalog(),
    }
}

fn clone_locator(
    locator: &SourceLocator,
    budget: &mut AssetLoadBudget,
) -> Result<SourceLocator, WorkspaceError> {
    let retained = locator
        .retained_clone_bytes()
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "source_inspection_parent_locator",
        })?;
    consume_retained_bytes(retained, "source_inspection_parent_locator", budget)?;
    Ok(locator.clone())
}

fn path_id_summary(
    path_ids: impl Iterator<Item = i64>,
) -> Result<SerializedPathIdSummary, WorkspaceError> {
    let mut summary = SerializedPathIdSummary::default();
    for path_id in path_ids {
        if path_id == 0 {
            return Err(WorkspaceError::operation(
                "SerializedFile path-ID inspection",
                io::Error::other("validated SerializedFile contains a zero path ID"),
            ));
        }
        if path_id < 0 {
            summary.negative =
                summary
                    .negative
                    .checked_add(1)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "serialized_negative_path_ids",
                    })?;
        } else {
            summary.positive =
                summary
                    .positive
                    .checked_add(1)
                    .ok_or(BudgetError::ArithmeticOverflow {
                        resource: "serialized_positive_path_ids",
                    })?;
        }
        summary.minimum = Some(summary.minimum.map_or(path_id, |value| value.min(path_id)));
        summary.maximum = Some(summary.maximum.map_or(path_id, |value| value.max(path_id)));
    }
    Ok(summary)
}

fn consume_scan(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let count = usize_to_u64(count, resource)?;
    budget.check_entries(count)?;
    budget.consume_entries(count)?;
    Ok(())
}

fn clone_text(
    value: &str,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<String, WorkspaceError> {
    let minimum = string_allocation_bytes(value.len())
        .map_err(|error| WorkspaceError::operation(resource, error))?;
    budget.check_bytes(minimum)?;
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: value.len(),
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    let actual = string_allocation_bytes(output.capacity())
        .map_err(|error| WorkspaceError::operation(resource, error))?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    output.push_str(value);
    Ok(output)
}

fn retained_string_clone_bytes(values: &[&str]) -> Result<usize, WorkspaceError> {
    let mut retained = 0_u64;
    for value in values {
        retained = retained
            .checked_add(
                string_allocation_bytes(value.len())
                    .map_err(|error| WorkspaceError::operation("source inspection size", error))?,
            )
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "source_inspection_clone",
            })?;
    }
    usize::try_from(retained).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "source_inspection_clone",
        }
        .into()
    })
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, WorkspaceError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
}

fn validate_stream_path(path: &str) -> Result<(), StreamedResourceRequestError> {
    if path.trim().is_empty() {
        return Err(StreamedResourceRequestError::EmptyPath);
    }
    if path.len() > MAX_STREAM_PATH_BYTES {
        return Err(StreamedResourceRequestError::PathTooLong {
            actual: path.len(),
            maximum: MAX_STREAM_PATH_BYTES,
        });
    }
    if path
        .chars()
        .any(|character| character == '\0' || character.is_control())
    {
        return Err(StreamedResourceRequestError::ControlCharacter);
    }
    if path.ends_with(['/', '\\']) {
        return Err(StreamedResourceRequestError::InvalidBasename);
    }
    stream_basename(path)
        .filter(|basename| !basename.trim().is_empty() && !matches!(*basename, "." | ".."))
        .ok_or(StreamedResourceRequestError::InvalidBasename)?;
    Ok(())
}

fn stream_basename(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
}

fn source_logical_path(source: &WorkspaceSource) -> &str {
    source
        .locator()
        .members()
        .last()
        .map(|step| step.name())
        .unwrap_or_else(|| source.locator().root_alias().as_str())
}

fn stream_candidate_match_rank(
    source: &WorkspaceSource,
    request_path: &str,
    request_basename: &str,
) -> Option<u8> {
    if source.kind() != SourceKind::StreamedResource {
        return None;
    }
    let candidate_path = source_logical_path(source);
    if normalized_path_eq(candidate_path, request_path, true) {
        return Some(0);
    }
    if normalized_path_eq(candidate_path, request_path, false) {
        return Some(1);
    }
    let candidate_basename = stream_basename(candidate_path).unwrap_or(candidate_path);
    if candidate_basename == request_basename {
        return Some(2);
    }
    candidate_basename
        .eq_ignore_ascii_case(request_basename)
        .then_some(3)
}

fn normalized_path_eq(left: &str, right: &str, case_sensitive: bool) -> bool {
    let normalize = |byte: u8| {
        let byte = if byte == b'\\' { b'/' } else { byte };
        if case_sensitive {
            byte
        } else {
            byte.to_ascii_lowercase()
        }
    };
    left.bytes().map(normalize).eq(right.bytes().map(normalize))
}

fn stream_source_score(owner: &WorkspaceSource, candidate: &WorkspaceSource) -> u8 {
    if candidate.parent() == Some(owner.id()) {
        0
    } else if owner.parent().is_some() && owner.parent() == candidate.parent() {
        1
    } else if owner.locator().root_alias() == candidate.locator().root_alias() {
        2
    } else {
        3
    }
}

fn invalid_stream_resolution(
    code: &'static str,
    message: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<StreamedResourceResolution, WorkspaceError> {
    let retained =
        code.len()
            .checked_add(message.len())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "streamed_resource_diagnostic",
            })?;
    consume_single_result(retained, "streamed_resource_diagnostic", budget)?;
    let diagnostic = Diagnostic::new(DiagnosticSeverity::Error, code, message)
        .map_err(|error| WorkspaceError::operation("streamed-resource diagnostic", error))?;
    Ok(StreamedResourceResolution::Invalid { diagnostic })
}
