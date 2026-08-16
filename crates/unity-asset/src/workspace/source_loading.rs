//! Stable source reads, format detection, and recursive preparation.

use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use unity_asset_binary::asset::{ObjectInfo, SerializedFile, SerializedType};
use unity_asset_binary::file::{UnityFileKind, sniff_unity_file_kind_prefix};
use unity_asset_binary::typetree::{
    TypeTree, TypeTreeNode, TypeTreeRegistry, TypeTreeSchema, TypeTreeSemanticDigestError,
    TypeTreeSerializationMode,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedSourceBytes, DigestBuildError, DigestV1, DigestV1Builder,
    SourceKind, YamlDocumentSelector, YamlFileId, arc_slice_allocation_bytes,
    arc_value_allocation_bytes,
};
use unity_asset_yaml::{BudgetedYamlError, YamlDocument, parse_prebudgeted_yaml_source};

use super::adapter::archive::{
    ArchiveLoadError, ArchiveMemberNameError, load_preflighted_zip_archive, preflight_zip_archive,
};
use super::adapter::binary::{
    BinaryAdapterAllocationUnit, BinaryAdapterError, BinaryContainerKind, BinaryMemberContent,
    BinaryPayload, BinaryWorkspaceAdapter,
};
use super::inspection::{
    AssetBundleSummary, SerializedFileSummary, WebFileSummary, WorkspaceSourceFormatInspection,
};
use super::source_catalog::{
    PhysicalOrigin, open_verified_file, physical_file_identity, physical_file_identity_from_path,
};
use super::state::{
    FrozenSourceParse, PreparedSourceChild, PreparedSourceRelation, PreparedSourceTree,
};
use super::view::{
    WorkspaceAllocationUnit, WorkspaceError, WorkspaceSourceContainer,
    WorkspaceSourceIdentityError, WorkspaceSourceMemberIdentityError,
};

const MAX_CONTAINER_DEPTH: u32 = 64;

/// Maximum prefix inspected by the source-recognition authority.
pub const SOURCE_RECOGNITION_PREFIX_LEN: usize = 64;

/// Format evidence derived from a path and a bounded source prefix.
///
/// Recognition is advisory for discovery callers. Workspace admission always parses and verifies
/// the complete retained image before installing a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRecognition {
    kind: SourceRecognitionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceRecognitionKind {
    Recognized(SourceKind),
    YamlOrBinary,
    Unknown,
}

impl SourceRecognition {
    /// Returns whether discovery should retain the path as a workspace source candidate.
    #[must_use]
    pub const fn is_candidate(self) -> bool {
        !matches!(self.kind, SourceRecognitionKind::Unknown)
    }

    /// Returns the safe parser hint carried by decisive recognition evidence.
    #[must_use]
    pub const fn kind_hint(self) -> Option<SourceKind> {
        match self.kind {
            SourceRecognitionKind::Recognized(kind) => Some(kind),
            SourceRecognitionKind::YamlOrBinary | SourceRecognitionKind::Unknown => None,
        }
    }

    /// Returns whether the source is recognized as a raw streamed-resource sidecar.
    #[must_use]
    pub const fn is_streamed_resource(self) -> bool {
        matches!(
            self.kind,
            SourceRecognitionKind::Recognized(SourceKind::StreamedResource)
        )
    }
}

/// Applies the canonical source-recognition rules without opening the path.
#[must_use]
pub fn recognize_source(path: &Path, prefix: &[u8]) -> SourceRecognition {
    let prefix = &prefix[..prefix.len().min(SOURCE_RECOGNITION_PREFIX_LEN)];
    let extension = path.extension().and_then(|extension| extension.to_str());
    if looks_like_zip(prefix) {
        return SourceRecognition {
            kind: SourceRecognitionKind::Recognized(SourceKind::Archive),
        };
    }
    if looks_like_yaml(prefix) {
        return SourceRecognition {
            kind: SourceRecognitionKind::Recognized(SourceKind::Yaml),
        };
    }
    if let Some(kind) = sniff_unity_file_kind_prefix(prefix) {
        return SourceRecognition {
            kind: SourceRecognitionKind::Recognized(binary_file_kind(kind)),
        };
    }
    if has_extension(extension, &["zip", "apk"]) {
        return SourceRecognition {
            kind: SourceRecognitionKind::Recognized(SourceKind::Archive),
        };
    }
    if has_yaml_extension(extension) {
        return SourceRecognition {
            kind: SourceRecognitionKind::YamlOrBinary,
        };
    }
    if has_resource_extension(extension) {
        return SourceRecognition {
            kind: SourceRecognitionKind::Recognized(SourceKind::StreamedResource),
        };
    }
    SourceRecognition {
        kind: SourceRecognitionKind::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrozenRegistryKey {
    Class {
        class_id: i32,
        mode: TypeTreeSerializationMode,
    },
    Script {
        class_id: i32,
        script_id: [u8; 16],
    },
}

#[derive(Debug)]
struct FrozenRegistryEntry {
    key: FrozenRegistryKey,
    tree: Arc<TypeTree>,
    schema_digest: DigestV1,
}

#[derive(Debug)]
struct FrozenTypeTreeRegistry {
    entries: Vec<FrozenRegistryEntry>,
    digest: DigestV1,
}

impl TypeTreeRegistry for FrozenTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.resolve_with_mode(unity_version, class_id, TypeTreeSerializationMode::Release)
    }

    fn resolve_with_mode(
        &self,
        _unity_version: &str,
        class_id: i32,
        mode: TypeTreeSerializationMode,
    ) -> Option<Arc<TypeTree>> {
        self.lookup(FrozenRegistryKey::Class { class_id, mode })
    }

    fn semantic_digest(&self) -> Option<DigestV1> {
        Some(self.digest)
    }

    fn resolve_script(
        &self,
        _unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        self.lookup(FrozenRegistryKey::Script {
            class_id,
            script_id,
        })
    }
}

impl FrozenTypeTreeRegistry {
    fn lookup(&self, key: FrozenRegistryKey) -> Option<Arc<TypeTree>> {
        self.entries
            .binary_search_by_key(&key, |entry| entry.key)
            .ok()
            .map(|index| Arc::clone(&self.entries[index].tree))
    }
}

pub(super) fn prepare_root(
    path: &Path,
    kind_hint: Option<SourceKind>,
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(0, budget)?;
    match kind_hint {
        Some(SourceKind::Yaml) => prepare_yaml(image, budget, 0),
        Some(SourceKind::Archive) => prepare_archive(image, binary, source_registry, budget, 0),
        Some(SourceKind::StreamedResource) => Ok(prepared_raw(image)),
        Some(
            expected @ (SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile),
        ) => {
            let payload = binary
                .parse_budgeted(&image, budget)
                .map_err(map_binary_adapter_error)?;
            let actual = binary_payload_kind(&payload);
            if actual != expected {
                return Err(WorkspaceError::InvalidSource {
                    path: path.to_path_buf(),
                    message: format!("expected {expected:?}, detected {actual:?}"),
                });
            }
            prepare_binary_payload(image, payload, binary, source_registry, budget, 0)
        }
        None => match recognize_source(path, &image).kind {
            SourceRecognitionKind::Recognized(SourceKind::Archive) => {
                prepare_archive(image, binary, source_registry, budget, 0)
            }
            SourceRecognitionKind::Recognized(SourceKind::Yaml) => prepare_yaml(image, budget, 0),
            SourceRecognitionKind::YamlOrBinary => {
                prepare_binary_or_yaml(image, binary, source_registry, budget, 0)
            }
            SourceRecognitionKind::Recognized(SourceKind::StreamedResource) => {
                Ok(prepared_raw(image))
            }
            SourceRecognitionKind::Recognized(
                SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile,
            )
            | SourceRecognitionKind::Unknown => {
                let payload = binary
                    .parse_budgeted(&image, budget)
                    .map_err(map_binary_adapter_error)?;
                prepare_binary_payload(image, payload, binary, source_registry, budget, 0)
            }
        },
    }
}

fn prepare_yaml(
    image: BudgetedSourceBytes,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    let mut scoped = budget.enter_depth(depth)?;
    let parsed = parse_prebudgeted_yaml_source(image, &mut scoped)
        .map_err(|error| map_yaml_error("YAML source parsing", error))?;
    let (image, document) = parsed.into_budgeted_parts(&scoped)?;
    drop(scoped);
    finish_prepared_yaml(image, document, budget)
}

fn finish_prepared_yaml(
    image: BudgetedSourceBytes,
    document: Arc<YamlDocument>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSourceTree, WorkspaceError> {
    validate_yaml_identities(&document, budget)?;
    let document_count = usize_to_u64(document.entries().len(), "yaml_document_count")?;
    Ok(PreparedSourceTree::new(
        SourceKind::Yaml,
        image,
        FrozenSourceParse::Yaml(document),
        WorkspaceSourceFormatInspection::Yaml { document_count },
        Vec::new(),
    ))
}

fn prepare_binary_or_yaml(
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse_budgeted(&image, &mut scoped)
    };
    match binary_result {
        Ok(payload) => {
            prepare_binary_payload(image, payload, binary, source_registry, budget, depth)
        }
        Err(BinaryAdapterError::FormatMismatch) => prepare_yaml(image, budget, depth),
        Err(source) => Err(map_binary_adapter_error(source)),
    }
}

fn prepare_archive(
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let plan = preflight_zip_archive(&image, budget)
        .map_err(|error| map_archive_error("ZIP archive preflight", error))?;
    let child_depth = plan
        .has_file_members()
        .then(|| next_container_depth(depth, budget))
        .transpose()?;
    let mut scoped = budget.enter_depth(depth)?;
    let archive_backing = image.clone_backing(&scoped)?;
    let members = load_preflighted_zip_archive(archive_backing, plan, &mut scoped)
        .map_err(|error| map_archive_error("ZIP archive loading", error))?;
    drop(scoped);
    let mut children = reserve_prepared_children(members.len(), budget)?;
    let mut previous_wire_ordinal = None;
    for member in members {
        if previous_wire_ordinal.is_some_and(|previous| previous >= member.wire_ordinal) {
            return Err(WorkspaceError::operation(
                "ZIP archive ordering",
                std::io::Error::other("archive members are not in strict wire order"),
            ));
        }
        previous_wire_ordinal = Some(member.wire_ordinal);
        let source = prepare_member(
            member.member_id.name(),
            member.bytes,
            binary,
            source_registry,
            budget,
            child_depth.ok_or_else(|| {
                WorkspaceError::operation(
                    "ZIP archive depth",
                    std::io::Error::other("archive preflight omitted a file member"),
                )
            })?,
            false,
        )?;
        children.push(PreparedSourceChild::new(
            PreparedSourceRelation::Archive,
            member.member_id,
            source,
        ));
    }
    let member_count = usize_to_u64(children.len(), "archive_member_count")?;
    Ok(PreparedSourceTree::new(
        SourceKind::Archive,
        image,
        FrozenSourceParse::None,
        WorkspaceSourceFormatInspection::Archive { member_count },
        children,
    ))
}

fn prepare_binary_payload(
    image: BudgetedSourceBytes,
    payload: BinaryPayload,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let kind = binary_payload_kind(&payload);
    let relation = match kind {
        SourceKind::AssetBundle => Some(PreparedSourceRelation::Bundle),
        SourceKind::WebFile => Some(PreparedSourceRelation::WebFile),
        SourceKind::SerializedFile
        | SourceKind::Yaml
        | SourceKind::Archive
        | SourceKind::StreamedResource => None,
    };
    let has_members = relation.is_some() && binary.has_members(&payload);
    let child_depth = if has_members {
        Some(next_container_depth(depth, budget)?)
    } else {
        None
    };
    let members = if has_members {
        let mut scoped = budget.enter_depth(child_depth.ok_or_else(|| {
            WorkspaceError::operation(
                "binary member depth",
                std::io::Error::other("container source has no child depth"),
            )
        })?)?;
        binary
            .members(&payload, &mut scoped)
            .map_err(map_binary_adapter_error)?
    } else {
        Vec::new()
    };
    let (parsed, format) = match payload {
        BinaryPayload::SerializedFile(file) => {
            let file = freeze_serialized_registry(*file, source_registry, budget, depth)?;
            let summary = SerializedFileSummary::from_file(&file, budget)?;
            (
                FrozenSourceParse::Serialized(promote_value_to_arc(
                    file,
                    budget,
                    "workspace_serialized_file",
                )?),
                WorkspaceSourceFormatInspection::SerializedFile(summary),
            )
        }
        BinaryPayload::AssetBundle(bundle) => (
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::AssetBundle(AssetBundleSummary::from_bundle(
                &bundle, budget,
            )?),
        ),
        BinaryPayload::WebFile(web_file) => (
            FrozenSourceParse::None,
            WorkspaceSourceFormatInspection::WebFile(WebFileSummary::from_webfile(
                &web_file, budget,
            )?),
        ),
    };
    let mut children = reserve_prepared_children(members.len(), budget)?;
    for member in members {
        let (_, identity, member_image, content) = member.into_parts();
        let source = match content {
            BinaryMemberContent::Parsed(payload) => prepare_binary_payload(
                member_image,
                payload,
                binary,
                source_registry,
                budget,
                child_depth.ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary member depth",
                        std::io::Error::other("serialized source exposed a member"),
                    )
                })?,
            )?,
            BinaryMemberContent::RawResource => prepare_member(
                identity.name(),
                member_image,
                binary,
                source_registry,
                budget,
                child_depth.ok_or_else(|| {
                    WorkspaceError::operation(
                        "binary member depth",
                        std::io::Error::other("serialized source exposed a member"),
                    )
                })?,
                true,
            )?,
        };
        let relation = relation.clone().ok_or_else(|| {
            WorkspaceError::operation(
                "binary member ownership",
                std::io::Error::other("serialized files cannot own container members"),
            )
        })?;
        children.push(PreparedSourceChild::new(relation, identity, source));
    }
    Ok(PreparedSourceTree::new(
        kind, image, parsed, format, children,
    ))
}

fn prepare_member(
    name: &str,
    image: BudgetedSourceBytes,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
    binary_already_rejected: bool,
) -> Result<PreparedSourceTree, WorkspaceError> {
    image.validate_budget(budget)?;
    observe_container_depth(depth, budget)?;
    let path = Path::new(name);
    match recognize_source(path, &image).kind {
        SourceRecognitionKind::Recognized(SourceKind::Archive) => {
            return prepare_archive(image, binary, source_registry, budget, depth);
        }
        SourceRecognitionKind::Recognized(SourceKind::Yaml) => {
            return prepare_yaml(image, budget, depth);
        }
        SourceRecognitionKind::YamlOrBinary => {
            return if binary_already_rejected {
                prepare_yaml(image, budget, depth)
            } else {
                prepare_binary_or_yaml(image, binary, source_registry, budget, depth)
            };
        }
        SourceRecognitionKind::Recognized(SourceKind::StreamedResource) => {
            return Ok(prepared_raw(image));
        }
        SourceRecognitionKind::Recognized(
            SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile,
        )
        | SourceRecognitionKind::Unknown
            if binary_already_rejected =>
        {
            return Ok(prepared_raw(image));
        }
        SourceRecognitionKind::Recognized(
            SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile,
        )
        | SourceRecognitionKind::Unknown => {}
    }

    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse_budgeted(&image, &mut scoped)
    };
    match binary_result {
        Ok(payload) => {
            prepare_binary_payload(image, payload, binary, source_registry, budget, depth)
        }
        Err(BinaryAdapterError::FormatMismatch) => Ok(prepared_raw(image)),
        Err(source) => Err(map_binary_adapter_error(source)),
    }
}

pub(super) fn prepared_raw(image: BudgetedSourceBytes) -> PreparedSourceTree {
    PreparedSourceTree::new(
        SourceKind::StreamedResource,
        image,
        FrozenSourceParse::None,
        WorkspaceSourceFormatInspection::StreamedResource,
        Vec::new(),
    )
}

fn binary_payload_kind(payload: &BinaryPayload) -> SourceKind {
    match payload {
        BinaryPayload::SerializedFile(_) => SourceKind::SerializedFile,
        BinaryPayload::AssetBundle(_) => SourceKind::AssetBundle,
        BinaryPayload::WebFile(_) => SourceKind::WebFile,
    }
}

const fn binary_file_kind(kind: UnityFileKind) -> SourceKind {
    match kind {
        UnityFileKind::SerializedFile => SourceKind::SerializedFile,
        UnityFileKind::AssetBundle => SourceKind::AssetBundle,
        UnityFileKind::WebFile => SourceKind::WebFile,
    }
}

fn freeze_serialized_registry(
    mut file: SerializedFile,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<SerializedFile, WorkspaceError> {
    file = file.with_type_tree_registry(None);
    let Some(source_registry) = source_registry else {
        return Ok(file);
    };

    let object_count = file.objects().len();
    budget.consume_entries(usize_to_u64(object_count, "frozen_typetree_objects")?)?;
    let key_capacity = object_count
        .checked_mul(2)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree_keys",
        })?;
    let mut keys = reserve_budgeted_vec::<FrozenRegistryKey>(
        key_capacity,
        budget,
        "frozen TypeTree lookup keys",
    )?;
    let serialization_mode = TypeTreeSerializationMode::from_object_context(file.object_context());
    for object in file.objects() {
        let serialized_type = serialized_type_for_object(&file, object);
        if file.type_tree_enabled() && serialized_type.is_some_and(SerializedType::has_type_tree) {
            continue;
        }
        if let Some(serialized_type) = serialized_type
            && serialized_type.is_script_type()
            && serialized_type.script_id != [0; 16]
        {
            keys.push(FrozenRegistryKey::Script {
                class_id: serialized_type.class_id,
                script_id: serialized_type.script_id,
            });
        }
        keys.push(FrozenRegistryKey::Class {
            class_id: object.class_id(),
            mode: serialization_mode,
        });
    }
    keys.sort_unstable();
    keys.dedup();
    budget.consume_entries(usize_to_u64(keys.len(), "frozen_typetree_keys")?)?;

    let mut entries =
        reserve_budgeted_vec::<FrozenRegistryEntry>(keys.len(), budget, "frozen TypeTree entries")?;
    for key in keys {
        let tree = match key {
            FrozenRegistryKey::Class { class_id, mode } => {
                source_registry.resolve_with_mode(&file.unity_version, class_id, mode)
            }
            FrozenRegistryKey::Script {
                class_id,
                script_id,
            } => source_registry.resolve_script(&file.unity_version, class_id, script_id),
        };
        if let Some(tree) = tree {
            account_frozen_type_tree(&tree, budget, depth)?;
            let schema = TypeTreeSchema::compile(&tree, file.ref_types(), budget)?;
            let schema_digest = schema
                .semantic_digest_with_budget(budget)
                .map_err(|error| match error {
                    TypeTreeSemanticDigestError::Budget(error) => WorkspaceError::Budget(error),
                    TypeTreeSemanticDigestError::Digest(error) => {
                        WorkspaceError::operation("frozen TypeTree schema identity", error)
                    }
                })?;
            entries.push(FrozenRegistryEntry {
                key,
                tree,
                schema_digest,
            });
        }
    }
    if entries.is_empty() {
        return Ok(file);
    }

    let allocation = arc_value_allocation_bytes::<FrozenTypeTreeRegistry>().map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree_registry",
        }
    })?;
    budget.consume_bytes(allocation)?;
    let digest = frozen_registry_digest(&entries)
        .map_err(|error| WorkspaceError::operation("frozen TypeTree registry identity", error))?;
    Ok(file.with_type_tree_registry(Some(Arc::new(FrozenTypeTreeRegistry { entries, digest }))))
}

fn frozen_registry_digest(entries: &[FrozenRegistryEntry]) -> Result<DigestV1, DigestBuildError> {
    const PREFIX: &[u8] = b"unity-asset:frozen-typetree-registry:v2\0";
    const COMMON_ENTRY_BYTES: u64 = 1 + 4 + DigestV1::BYTE_LEN as u64;

    let mut logical_length =
        u64::try_from(PREFIX.len()).map_err(|_| DigestBuildError::LengthOverflow)?;
    logical_length = logical_length
        .checked_add(8)
        .ok_or(DigestBuildError::LengthOverflow)?;
    for entry in entries {
        logical_length = logical_length
            .checked_add(COMMON_ENTRY_BYTES)
            .and_then(|length| match entry.key {
                FrozenRegistryKey::Class { .. } => length.checked_add(1),
                FrozenRegistryKey::Script { .. } => length.checked_add(16),
            })
            .ok_or(DigestBuildError::LengthOverflow)?;
    }

    let mut digest = DigestV1Builder::new(logical_length);
    digest.update(PREFIX)?;
    digest.update(
        &u64::try_from(entries.len())
            .map_err(|_| DigestBuildError::LengthOverflow)?
            .to_le_bytes(),
    )?;
    for entry in entries {
        match entry.key {
            FrozenRegistryKey::Class { class_id, mode } => {
                digest.update(&[0])?;
                digest.update(&class_id.to_le_bytes())?;
                digest.update(&[match mode {
                    TypeTreeSerializationMode::Release => 0,
                    TypeTreeSerializationMode::Editor => 1,
                }])?;
            }
            FrozenRegistryKey::Script {
                class_id,
                script_id,
            } => {
                digest.update(&[1])?;
                digest.update(&class_id.to_le_bytes())?;
                digest.update(&script_id)?;
            }
        }
        digest.update(entry.schema_digest.as_bytes())?;
    }
    digest.finalize()
}

fn serialized_type_for_object<'file>(
    file: &'file SerializedFile,
    object: &ObjectInfo,
) -> Option<&'file SerializedType> {
    if let Some(index) = object.serialized_type_index() {
        return usize::try_from(index)
            .ok()
            .and_then(|index| file.types().get(index));
    }
    file.types()
        .iter()
        .find(|serialized_type| serialized_type.class_id == object.class_id())
}

fn account_frozen_type_tree(
    tree: &TypeTree,
    budget: &mut AssetLoadBudget,
    base_depth: u32,
) -> Result<(), WorkspaceError> {
    let mut scoped = budget.enter_depth(base_depth)?;
    if !tree.nodes.is_empty() {
        scoped.check_depth(0)?;
    }
    let top_level = size_of::<TypeTree>()
        .checked_add(
            tree.nodes
                .capacity()
                .checked_mul(size_of::<TypeTreeNode>())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "frozen_typetree",
                })?,
        )
        .and_then(|bytes| bytes.checked_add(tree.string_buffer.capacity()))
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree",
        })?;
    scoped.consume_bytes(top_level)?;

    let mut stack = reserve_budgeted_vec::<(&TypeTreeNode, u32)>(
        tree.nodes.len(),
        &mut scoped,
        "frozen TypeTree traversal",
    )?;
    stack.extend(tree.nodes.iter().map(|node| (node, 0)));
    while let Some((node, depth)) = stack.pop() {
        scoped.consume_entries(1)?;
        scoped.observe_depth(depth)?;
        let retained = node
            .type_name
            .capacity()
            .checked_add(node.name.capacity())
            .and_then(|bytes| {
                node.children
                    .capacity()
                    .checked_mul(size_of::<TypeTreeNode>())
                    .and_then(|children| bytes.checked_add(children))
            })
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "frozen_typetree",
            })?;
        scoped.consume_bytes(retained)?;
        if !node.children.is_empty() {
            let child_depth = depth
                .checked_add(1)
                .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
            scoped.check_depth(child_depth)?;
            let scratch = node
                .children
                .len()
                .checked_mul(size_of::<(&TypeTreeNode, u32)>())
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or(BudgetError::ArithmeticOverflow {
                    resource: "frozen_typetree_traversal",
                })?;
            scoped.check_bytes(scratch)?;
            stack
                .try_reserve_exact(node.children.len())
                .map_err(|error| WorkspaceError::Allocation {
                    resource: "frozen TypeTree traversal",
                    requested: node.children.len(),
                    unit: WorkspaceAllocationUnit::Elements,
                    message: error.to_string(),
                })?;
            scoped.consume_bytes(scratch)?;
            stack.extend(node.children.iter().map(|child| (child, child_depth)));
        }
    }
    Ok(())
}

pub(super) fn validate_yaml_identities(
    document: &YamlDocument,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    budget.consume_entries(usize_to_u64(
        document.entries().len(),
        "yaml_identity_entries",
    )?)?;
    let mut file_ids = reserve_budgeted_vec::<YamlFileId>(
        document.entries().len(),
        budget,
        "YAML identity validation",
    )?;
    for (index, class) in document.entries().iter().enumerate() {
        let selector = YamlDocumentSelector::from_document_header(
            index,
            class.class_id(),
            class.class_name(),
            class.anchor(),
        )?;
        if let YamlDocumentSelector::FileId { file_id } = selector {
            file_ids.push(file_id);
        }
    }
    file_ids.sort_unstable();
    if file_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkspaceError::InvalidSourceIdentity {
            source_kind: SourceKind::Yaml,
            reason: WorkspaceSourceIdentityError::DuplicateYamlFileId,
        });
    }
    Ok(())
}

fn observe_container_depth(depth: u32, budget: &mut AssetLoadBudget) -> Result<(), WorkspaceError> {
    if depth > MAX_CONTAINER_DEPTH {
        return Err(BudgetError::Exceeded {
            resource: "workspace_container_depth",
            limit: u64::from(MAX_CONTAINER_DEPTH),
            requested: u64::from(depth),
        }
        .into());
    }
    budget.observe_depth(depth)?;
    Ok(())
}

fn next_container_depth(depth: u32, budget: &mut AssetLoadBudget) -> Result<u32, WorkspaceError> {
    let next = depth
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource: "depth" })?;
    observe_container_depth(next, budget)?;
    Ok(next)
}

fn reserve_prepared_children(
    count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<PreparedSourceChild>, WorkspaceError> {
    let bytes = count
        .checked_mul(size_of::<PreparedSourceChild>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "prepared_source_tree",
        })?;
    budget.check_bytes(bytes)?;
    let mut children = Vec::new();
    children
        .try_reserve_exact(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource: "prepared source tree",
            requested: count,
            unit: WorkspaceAllocationUnit::Elements,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bytes)?;
    Ok(children)
}

pub(super) fn map_yaml_error(operation: &'static str, error: BudgetedYamlError) -> WorkspaceError {
    match error {
        BudgetedYamlError::Budget(error) => WorkspaceError::Budget(error),
        BudgetedYamlError::AllocationFailed {
            context,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource: context,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        BudgetedYamlError::IndexMapAllocationFailed {
            context,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource: context,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        BudgetedYamlError::DepthExceeded { actual, limit } => {
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "yaml_depth",
                limit: u64::from(limit),
                requested: u64::from(actual),
            })
        }
        error => WorkspaceError::operation(operation, error),
    }
}

fn map_archive_error(operation: &'static str, error: ArchiveLoadError) -> WorkspaceError {
    match error {
        ArchiveLoadError::Budget { source, .. } => WorkspaceError::Budget(source),
        ArchiveLoadError::Allocation {
            resource,
            requested,
            source,
        } => WorkspaceError::Allocation {
            resource,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: source.to_string(),
        },
        ArchiveLoadError::ArithmeticOverflow { resource } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        ArchiveLoadError::InvalidMemberName {
            wire_ordinal,
            reason,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: WorkspaceSourceContainer::Archive,
            wire_ordinal,
            reason: match reason {
                ArchiveMemberNameError::Empty => WorkspaceSourceMemberIdentityError::Empty,
                ArchiveMemberNameError::TooLong => WorkspaceSourceMemberIdentityError::TooLong,
                ArchiveMemberNameError::UnstableEncoding => {
                    WorkspaceSourceMemberIdentityError::UnstableEncoding
                }
                ArchiveMemberNameError::Absolute => WorkspaceSourceMemberIdentityError::Absolute,
                ArchiveMemberNameError::Backslash => WorkspaceSourceMemberIdentityError::Backslash,
                ArchiveMemberNameError::ControlCharacter => {
                    WorkspaceSourceMemberIdentityError::ControlCharacter
                }
                ArchiveMemberNameError::TraversalComponent => {
                    WorkspaceSourceMemberIdentityError::TraversalComponent
                }
            },
        },
        ArchiveLoadError::MemberIdentity {
            wire_ordinal,
            source,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: WorkspaceSourceContainer::Archive,
            wire_ordinal,
            reason: WorkspaceSourceMemberIdentityError::Contract(source),
        },
        error => WorkspaceError::operation(operation, error),
    }
}

pub(super) fn map_binary_adapter_error(error: BinaryAdapterError) -> WorkspaceError {
    match error {
        BinaryAdapterError::Parse { source } => WorkspaceError::from(source),
        BinaryAdapterError::MemberBinary {
            container,
            wire_ordinal,
            source,
        } => map_binary_member_error(container, wire_ordinal, source),
        BinaryAdapterError::InvalidMemberIdentity {
            container,
            wire_ordinal,
            source,
        } => WorkspaceError::InvalidSourceMemberIdentity {
            container: map_binary_container(container),
            wire_ordinal,
            reason: WorkspaceSourceMemberIdentityError::Contract(source),
        },
        BinaryAdapterError::Budget(source) => WorkspaceError::Budget(source),
        BinaryAdapterError::Allocation {
            resource,
            requested,
            unit,
            source,
        } => WorkspaceError::Allocation {
            resource,
            requested,
            unit: match unit {
                BinaryAdapterAllocationUnit::Bytes => WorkspaceAllocationUnit::Bytes,
                BinaryAdapterAllocationUnit::Elements => WorkspaceAllocationUnit::Elements,
            },
            message: source.to_string(),
        },
        BinaryAdapterError::RetainedSizeOverflow { resource } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow { resource })
        }
        BinaryAdapterError::WireOrdinalOverflow => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
                resource: "binary_member_ordinal",
            })
        }
        BinaryAdapterError::SameNameOccurrenceOverflow { .. } => {
            WorkspaceError::Budget(BudgetError::ArithmeticOverflow {
                resource: "binary_member_same_name_occurrence",
            })
        }
        BinaryAdapterError::FormatMismatch => {
            WorkspaceError::from(unity_asset_binary::error::BinaryError::invalid_format(
                "input is not a recognized Unity binary source",
            ))
        }
    }
}

fn map_binary_member_error(
    container: BinaryContainerKind,
    wire_ordinal: u64,
    source: unity_asset_binary::error::BinaryError,
) -> WorkspaceError {
    match source {
        source @ (unity_asset_binary::error::BinaryError::Budget(_)
        | unity_asset_binary::error::BinaryError::ObjectIdentity(_)) => {
            WorkspaceError::from(source)
        }
        source => WorkspaceError::BinaryMember {
            container: map_binary_container(container),
            wire_ordinal,
            source,
        },
    }
}

const fn map_binary_container(container: BinaryContainerKind) -> WorkspaceSourceContainer {
    match container {
        BinaryContainerKind::AssetBundle => WorkspaceSourceContainer::AssetBundle,
        BinaryContainerKind::WebFile => WorkspaceSourceContainer::WebFile,
    }
}

pub(super) fn reserve_budgeted_vec<T>(
    count: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Vec<T>, WorkspaceError> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| WorkspaceError::Allocation {
            resource,
            requested: count,
            unit: WorkspaceAllocationUnit::Elements,
            message: error.to_string(),
        })?;
    budget.consume_bytes(bytes)?;
    Ok(values)
}

fn consume_arc_allocation<T>(
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), WorkspaceError> {
    let bytes = arc_value_allocation_bytes::<T>()
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

pub(super) fn promote_value_to_arc<T>(
    value: T,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Arc<T>, WorkspaceError> {
    consume_arc_allocation::<T>(budget, resource)?;
    Ok(Arc::new(value))
}

pub(super) fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, WorkspaceError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
}

pub(super) fn read_owned_image(
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
) -> Result<BudgetedSourceBytes, WorkspaceError> {
    let path = origin.path();
    let mut file = open_verified_file(path).map_err(|error| WorkspaceError::io(path, error))?;
    let before = physical_file_identity(&file, path)?;
    let length = before.length();
    let length_usize = usize::try_from(length).map_err(|_| WorkspaceError::SourceTooLarge {
        path: path.to_path_buf(),
        length,
    })?;
    let retained_bytes = arc_slice_allocation_bytes::<u8>(length_usize).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "workspace_source_image",
        }
    })?;
    let planned_bytes = length
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(retained_bytes))
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_source_image",
        })?;
    budget.check_bytes(planned_bytes)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length_usize)
        .map_err(|error| WorkspaceError::Allocation {
            resource: "workspace source image",
            requested: length_usize,
            unit: WorkspaceAllocationUnit::Bytes,
            message: error.to_string(),
        })?;
    budget.consume_bytes(length)?;
    bytes.resize(length_usize, 0);
    read_exact_stable(&mut file, &mut bytes, path)?;
    verify_stable_contents(&mut file, &bytes, path, budget)?;

    let after = physical_file_identity(&file, path)?;
    let current = physical_file_identity_from_path(path)?;
    if before != after || before != current {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    BudgetedSourceBytes::from_vec(bytes, budget).map_err(WorkspaceError::from)
}

fn verify_stable_contents(
    reader: &mut (impl Read + Seek),
    expected: &[u8],
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let length = usize_to_u64(expected.len(), "workspace_source_verification")?;
    budget.consume_bytes(length)?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| WorkspaceError::io(path, error))?;

    let mut verified = 0;
    let mut chunk = [0_u8; 64 * 1024];
    while verified < expected.len() {
        let count = chunk.len().min(expected.len() - verified);
        read_exact_stable(reader, &mut chunk[..count], path)?;
        if chunk[..count] != expected[verified..verified + count] {
            return Err(WorkspaceError::SourceChanged {
                path: path.to_path_buf(),
            });
        }
        verified += count;
    }

    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .map_err(|error| WorkspaceError::io(path, error))?
        != 0
    {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn read_exact_stable(
    reader: &mut impl Read,
    bytes: &mut [u8],
    path: &Path,
) -> Result<(), WorkspaceError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            WorkspaceError::SourceChanged {
                path: path.to_path_buf(),
            }
        } else {
            WorkspaceError::io(path, error)
        }
    })
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x06\x06")
}

fn looks_like_yaml(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let start = bytes.trim_ascii_start();
    start.starts_with(b"%YAML") || start.starts_with(b"--- !u!")
}

fn has_yaml_extension(extension: Option<&str>) -> bool {
    has_extension(
        extension,
        &[
            "anim",
            "asset",
            "controller",
            "mat",
            "meta",
            "prefab",
            "unity",
        ],
    )
}

fn has_resource_extension(extension: Option<&str>) -> bool {
    has_extension(extension, &["resource", "ress"])
}

fn has_extension(extension: Option<&str>, expected: &[&str]) -> bool {
    extension.is_some_and(|extension| {
        expected
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

#[cfg(test)]
mod tests;
