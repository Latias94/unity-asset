//! Authoritative workspace mutation boundary.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use unity_asset_binary::asset::{ObjectInfo, SerializedFile, SerializedType};
use unity_asset_binary::typetree::{
    CompositeTypeTreeRegistry, TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions,
    TypeTreeRegistry,
};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, SourceAlias, SourceFingerprint, SourceId, SourceKind,
    SourceMemberId, UnityClass, UnityDocument, WorkspaceId, WorkspaceRevision, YamlAnchor,
};
use unity_asset_yaml::YamlDocument;

use super::adapter::archive::{
    ArchiveLoadError, ArchiveMemberNameError, load_preflighted_zip_archive, preflight_zip_archive,
};
use super::adapter::binary::{
    BinaryAdapterAllocationUnit, BinaryAdapterError, BinaryContainerKind, BinaryMemberContent,
    BinaryPayload, BinaryWorkspaceAdapter,
};
use super::adapter::yaml::{ParsedYamlSource, YamlAdapterError, parse_yaml_source};
use super::snapshot::WorkspaceSnapshot;
use super::source_catalog::{PhysicalOrigin, SourceDescriptor};
use super::state::WorkspaceState;
use super::store::{FrozenSourceParse, SourceImage, SourceStore};
use super::view::{
    WorkspaceAllocationUnit, WorkspaceError, WorkspaceSourceContainer,
    WorkspaceSourceIdentityError, WorkspaceSourceMemberIdentityError,
};

const MAX_CONTAINER_DEPTH: u32 = 64;

/// Immutable parsing policy shared by a workspace and every snapshot derived from it.
#[derive(Clone, Default)]
pub struct WorkspaceOptions {
    typetree: TypeTreeParseOptions,
    type_tree_registry: Option<Arc<dyn TypeTreeRegistry>>,
}

impl WorkspaceOptions {
    #[must_use]
    pub fn strict() -> Self {
        Self {
            typetree: TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
            type_tree_registry: None,
        }
    }

    #[must_use]
    pub fn lenient() -> Self {
        Self::default()
    }

    /// Loads an immutable JSON/TPK registry under the caller's budget.
    ///
    /// Workspace loads deliberately reject arbitrary registry callbacks: snapshot state may only
    /// retain registries whose construction is budgeted and whose lookups are allocation-free.
    pub fn with_type_tree_registry_paths<P: AsRef<Path>>(
        mut self,
        paths: &[P],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, WorkspaceError> {
        self.type_tree_registry = CompositeTypeTreeRegistry::from_paths(paths, budget)?;
        Ok(self)
    }

    #[must_use]
    pub const fn typetree_options(&self) -> TypeTreeParseOptions {
        self.typetree
    }
}

impl fmt::Debug for WorkspaceOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceOptions")
            .field("typetree_mode", &self.typetree.mode)
            .field("has_type_tree_registry", &self.type_tree_registry.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct WorkspaceConfig {
    pub(crate) typetree: TypeTreeParseOptions,
}

impl fmt::Debug for WorkspaceConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkspaceConfig")
            .field("typetree_mode", &self.typetree.mode)
            .finish_non_exhaustive()
    }
}

/// One explicit filesystem source load.
#[derive(Debug, Clone)]
pub struct SourceOpenRequest {
    path: PathBuf,
    alias: SourceAlias,
    kind_hint: Option<SourceKind>,
}

impl SourceOpenRequest {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, alias: SourceAlias) -> Self {
        Self {
            path: path.into(),
            alias,
            kind_hint: None,
        }
    }

    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, WorkspaceError> {
        let path = path.into();
        let alias = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| WorkspaceError::InvalidSource {
                path: path.clone(),
                message: "the path has no portable UTF-8 file name; provide an explicit alias"
                    .to_owned(),
            })?
            .to_owned();
        Ok(Self::new(path, SourceAlias::new(alias)?))
    }

    #[must_use]
    pub fn with_kind_hint(mut self, kind: SourceKind) -> Self {
        self.kind_hint = Some(kind);
        self
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn alias(&self) -> &SourceAlias {
        &self.alias
    }

    #[must_use]
    pub const fn kind_hint(&self) -> Option<SourceKind> {
        self.kind_hint
    }
}

/// Mutable owner of one revisioned Unity source namespace.
pub struct AssetWorkspace {
    state: Arc<WorkspaceState>,
    config: Arc<WorkspaceConfig>,
    reference_store: Arc<crate::reference::ReferenceStore>,
    binary: BinaryWorkspaceAdapter,
    source_registry: Option<Arc<dyn TypeTreeRegistry>>,
}

impl fmt::Debug for AssetWorkspace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssetWorkspace")
            .field("workspace_id", &self.workspace_id())
            .field("revision", &self.revision())
            .field("source_count", &self.state.store().len())
            .field("config", &self.config)
            .finish()
    }
}

impl AssetWorkspace {
    pub fn new() -> Result<Self, WorkspaceError> {
        Self::with_options(WorkspaceOptions::default())
    }

    pub fn with_options(options: WorkspaceOptions) -> Result<Self, WorkspaceError> {
        loop {
            if let Ok(workspace) = WorkspaceId::from_u128(rand::random()) {
                return Self::with_workspace_id(workspace, options);
            }
        }
    }

    pub(crate) fn with_workspace_id(
        workspace: WorkspaceId,
        options: WorkspaceOptions,
    ) -> Result<Self, WorkspaceError> {
        let state = WorkspaceState::empty(workspace)
            .map_err(|source| WorkspaceError::operation("initialization", source))?;
        Ok(Self {
            state: Arc::new(state),
            config: Arc::new(WorkspaceConfig {
                typetree: options.typetree,
            }),
            reference_store: Arc::new(crate::reference::ReferenceStore::new()),
            binary: BinaryWorkspaceAdapter::new(),
            source_registry: options.type_tree_registry,
        })
    }

    #[must_use]
    pub fn workspace_id(&self) -> WorkspaceId {
        self.state.workspace()
    }

    #[must_use]
    pub fn revision(&self) -> WorkspaceRevision {
        self.state.revision()
    }

    #[must_use]
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot::new(
            Arc::clone(&self.state),
            Arc::clone(&self.config),
            Arc::clone(&self.reference_store),
        )
    }

    pub fn load_path(
        &mut self,
        path: impl Into<PathBuf>,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        self.load_source(SourceOpenRequest::from_path(path)?, budget)
    }

    pub fn load_source(
        &mut self,
        request: SourceOpenRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let SourceOpenRequest {
            path,
            alias,
            kind_hint,
        } = request;
        let absolute = absolute_path(&path)?;
        let origin = PhysicalOrigin::from_existing_path(&absolute)
            .map_err(|source| WorkspaceError::operation("physical-origin validation", source))?;
        account_root_descriptor_backing(&alias, &origin, budget)?;
        let image = read_owned_image(&origin, budget)?;
        let prepared = prepare_root(
            &path,
            kind_hint,
            image,
            &self.binary,
            self.source_registry.as_ref(),
            budget,
        )?;
        self.publish_prepared(alias, origin, prepared, budget)
    }

    pub fn unload_source(
        &mut self,
        root: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), WorkspaceError> {
        if root.workspace() != self.workspace_id() {
            return Err(unity_asset_core::ContractError::WorkspaceMismatch {
                expected: self.workspace_id(),
                actual: root.workspace(),
            }
            .into());
        }
        let descriptor = self.state.catalog().resolve(root)?;
        if descriptor.parent().is_some() {
            return Err(WorkspaceError::NotRootSource(root));
        }

        let mut catalog = self.state.catalog().begin_transaction(budget)?;
        let removed = catalog.remove_subtree(root, budget)?;
        let mut store = self
            .state
            .store()
            .clone_for_update(budget)
            .map_err(WorkspaceError::from)?;
        store
            .remove_all(&removed, budget)
            .map_err(WorkspaceError::from)?;
        let catalog = catalog.commit()?;
        let next = WorkspaceState::new(self.workspace_id(), catalog, store, budget)
            .map_err(WorkspaceError::from)?;
        consume_arc_allocation::<WorkspaceState>(budget, "workspace_state")?;
        self.state = Arc::new(next);
        Ok(())
    }

    fn publish_prepared(
        &mut self,
        alias: SourceAlias,
        origin: PhysicalOrigin,
        prepared: PreparedSource,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceId, WorkspaceError> {
        let existing = self.state.catalog().find_physical(&origin);
        let mut catalog = self.state.catalog().begin_transaction(budget)?;
        let mut store = self
            .state
            .store()
            .clone_for_update(budget)
            .map_err(WorkspaceError::from)?;

        if let Some(existing) = existing {
            let removed = catalog.remove_subtree(existing, budget)?;
            store
                .remove_all(&removed, budget)
                .map_err(WorkspaceError::from)?;
        }

        let root_descriptor = SourceDescriptor::root(prepared.kind, alias, origin);
        let root = register_prepared(prepared, root_descriptor, &mut catalog, &mut store, budget)?;
        let catalog = catalog.commit()?;
        let next = WorkspaceState::new(self.workspace_id(), catalog, store, budget)
            .map_err(WorkspaceError::from)?;
        if next.revision() != self.state.revision() {
            consume_arc_allocation::<WorkspaceState>(budget, "workspace_state")?;
            self.state = Arc::new(next);
        }
        Ok(root)
    }
}

#[derive(Debug)]
struct PreparedSource {
    kind: SourceKind,
    image: Arc<[u8]>,
    parsed: PreparedParse,
    children: Vec<PreparedChild>,
}

#[derive(Debug)]
enum PreparedParse {
    None,
    Serialized(Arc<SerializedFile>),
    Yaml(Arc<YamlDocument>),
}

#[derive(Debug)]
struct PreparedChild {
    relation: ChildRelation,
    identity: SourceMemberId,
    source: PreparedSource,
}

#[derive(Debug, Clone, Copy)]
enum ChildRelation {
    Archive,
    Bundle,
    WebFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrozenRegistryKey {
    Class(i32),
    Script { class_id: i32, script_id: [u8; 16] },
}

#[derive(Debug)]
struct FrozenRegistryEntry {
    key: FrozenRegistryKey,
    tree: Arc<TypeTree>,
}

#[derive(Debug)]
struct FrozenTypeTreeRegistry {
    entries: Vec<FrozenRegistryEntry>,
}

impl TypeTreeRegistry for FrozenTypeTreeRegistry {
    fn resolve(&self, _unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.lookup(FrozenRegistryKey::Class(class_id))
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

fn prepare_root(
    path: &Path,
    kind_hint: Option<SourceKind>,
    image: Arc<[u8]>,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSource, WorkspaceError> {
    observe_container_depth(0, budget)?;
    match kind_hint {
        Some(SourceKind::Yaml) => prepare_yaml(image, budget, 0),
        Some(SourceKind::Archive) => prepare_archive(image, binary, source_registry, budget, 0),
        Some(SourceKind::StreamedResource) => Ok(prepared_raw(image)),
        Some(
            expected @ (SourceKind::SerializedFile | SourceKind::AssetBundle | SourceKind::WebFile),
        ) => {
            let payload = binary
                .parse(Arc::clone(&image), budget)
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
        None if looks_like_zip(&image) || has_extension(path, &["zip", "apk"]) => {
            prepare_archive(image, binary, source_registry, budget, 0)
        }
        None if looks_like_yaml(&image) => prepare_yaml(image, budget, 0),
        None if has_yaml_extension(path) => {
            prepare_binary_or_yaml(image, binary, source_registry, budget, 0)
        }
        None if has_resource_extension(path) => Ok(prepared_raw(image)),
        None => {
            let payload = binary
                .parse(Arc::clone(&image), budget)
                .map_err(map_binary_adapter_error)?;
            prepare_binary_payload(image, payload, binary, source_registry, budget, 0)
        }
    }
}

fn prepare_yaml(
    image: Arc<[u8]>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSource, WorkspaceError> {
    let mut scoped = budget.enter_depth(depth)?;
    let parsed = parse_yaml_source(Arc::clone(&image), &mut scoped)
        .map_err(|error| map_yaml_adapter_error("YAML source parsing", error))?;
    drop(scoped);
    finish_prepared_yaml(image, parsed, budget)
}

fn finish_prepared_yaml(
    image: Arc<[u8]>,
    parsed: ParsedYamlSource,
    budget: &mut AssetLoadBudget,
) -> Result<PreparedSource, WorkspaceError> {
    debug_assert_eq!(parsed.encoded().as_ref(), image.as_ref());
    validate_yaml_identities(parsed.document(), budget)?;
    Ok(PreparedSource {
        kind: SourceKind::Yaml,
        image,
        parsed: PreparedParse::Yaml(Arc::clone(parsed.document())),
        children: Vec::new(),
    })
}

fn prepare_binary_or_yaml(
    image: Arc<[u8]>,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSource, WorkspaceError> {
    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse(Arc::clone(&image), &mut scoped)
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
    image: Arc<[u8]>,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSource, WorkspaceError> {
    observe_container_depth(depth, budget)?;
    let plan = preflight_zip_archive(&image, budget)
        .map_err(|error| map_archive_error("ZIP archive preflight", error))?;
    let child_depth = plan
        .has_file_members()
        .then(|| next_container_depth(depth, budget))
        .transpose()?;
    let mut scoped = budget.enter_depth(depth)?;
    let members = load_preflighted_zip_archive(Arc::clone(&image), plan, &mut scoped)
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
            Arc::clone(&member.bytes),
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
        children.push(PreparedChild {
            relation: ChildRelation::Archive,
            identity: member.member_id,
            source,
        });
    }
    Ok(PreparedSource {
        kind: SourceKind::Archive,
        image,
        parsed: PreparedParse::None,
        children,
    })
}

fn prepare_binary_payload(
    image: Arc<[u8]>,
    payload: BinaryPayload,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<PreparedSource, WorkspaceError> {
    observe_container_depth(depth, budget)?;
    let kind = binary_payload_kind(&payload);
    let relation = match kind {
        SourceKind::AssetBundle => Some(ChildRelation::Bundle),
        SourceKind::WebFile => Some(ChildRelation::WebFile),
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
    let parsed = match payload {
        BinaryPayload::SerializedFile(mut file) => {
            freeze_serialized_registry(&mut file, source_registry, budget, depth)?;
            PreparedParse::Serialized(promote_box_to_arc(
                file,
                budget,
                "workspace_serialized_file",
            )?)
        }
        BinaryPayload::AssetBundle(_) | BinaryPayload::WebFile(_) => PreparedParse::None,
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
        let relation = relation.ok_or_else(|| {
            WorkspaceError::operation(
                "binary member ownership",
                std::io::Error::other("serialized files cannot own container members"),
            )
        })?;
        children.push(PreparedChild {
            relation,
            identity,
            source,
        });
    }
    Ok(PreparedSource {
        kind,
        image,
        parsed,
        children,
    })
}

fn prepare_member(
    name: &str,
    image: Arc<[u8]>,
    binary: &BinaryWorkspaceAdapter,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
    binary_already_rejected: bool,
) -> Result<PreparedSource, WorkspaceError> {
    observe_container_depth(depth, budget)?;
    let path = Path::new(name);
    if looks_like_zip(&image) || has_extension(path, &["zip", "apk"]) {
        return prepare_archive(image, binary, source_registry, budget, depth);
    }
    if looks_like_yaml(&image) {
        return prepare_yaml(image, budget, depth);
    }
    if has_yaml_extension(path) {
        return if binary_already_rejected {
            prepare_yaml(image, budget, depth)
        } else {
            prepare_binary_or_yaml(image, binary, source_registry, budget, depth)
        };
    }
    if has_resource_extension(path) || binary_already_rejected {
        return Ok(prepared_raw(image));
    }

    let binary_result = {
        let mut scoped = budget.enter_depth(depth)?;
        binary.parse(Arc::clone(&image), &mut scoped)
    };
    match binary_result {
        Ok(payload) => {
            prepare_binary_payload(image, payload, binary, source_registry, budget, depth)
        }
        Err(BinaryAdapterError::FormatMismatch) => Ok(prepared_raw(image)),
        Err(source) => Err(map_binary_adapter_error(source)),
    }
}

fn prepared_raw(image: Arc<[u8]>) -> PreparedSource {
    PreparedSource {
        kind: SourceKind::StreamedResource,
        image,
        parsed: PreparedParse::None,
        children: Vec::new(),
    }
}

fn binary_payload_kind(payload: &BinaryPayload) -> SourceKind {
    match payload {
        BinaryPayload::SerializedFile(_) => SourceKind::SerializedFile,
        BinaryPayload::AssetBundle(_) => SourceKind::AssetBundle,
        BinaryPayload::WebFile(_) => SourceKind::WebFile,
    }
}

fn freeze_serialized_registry(
    file: &mut SerializedFile,
    source_registry: Option<&Arc<dyn TypeTreeRegistry>>,
    budget: &mut AssetLoadBudget,
    depth: u32,
) -> Result<(), WorkspaceError> {
    file.set_type_tree_registry(None);
    let Some(source_registry) = source_registry else {
        return Ok(());
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
    for object in file.objects() {
        let serialized_type = serialized_type_for_object(file, object);
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
        keys.push(FrozenRegistryKey::Class(object.class_id()));
    }
    keys.sort_unstable();
    keys.dedup();
    budget.consume_entries(usize_to_u64(keys.len(), "frozen_typetree_keys")?)?;

    let mut entries =
        reserve_budgeted_vec::<FrozenRegistryEntry>(keys.len(), budget, "frozen TypeTree entries")?;
    for key in keys {
        let tree = match key {
            FrozenRegistryKey::Class(class_id) => {
                source_registry.resolve(&file.unity_version, class_id)
            }
            FrozenRegistryKey::Script {
                class_id,
                script_id,
            } => source_registry.resolve_script(&file.unity_version, class_id, script_id),
        };
        if let Some(tree) = tree {
            account_frozen_type_tree(&tree, budget, depth)?;
            entries.push(FrozenRegistryEntry { key, tree });
        }
    }
    if entries.is_empty() {
        return Ok(());
    }

    let allocation = size_of::<FrozenTypeTreeRegistry>()
        .checked_add(size_of::<usize>() * 2)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "frozen_typetree_registry",
        })?;
    budget.consume_bytes(allocation)?;
    file.set_type_tree_registry(Some(Arc::new(FrozenTypeTreeRegistry { entries })));
    Ok(())
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

fn validate_yaml_identities(
    document: &YamlDocument,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    budget.consume_entries(usize_to_u64(
        document.entries().len(),
        "yaml_identity_entries",
    )?)?;
    let mut anchors =
        reserve_budgeted_vec::<&str>(document.entries().len(), budget, "YAML identity validation")?;
    for (index, class) in document.entries().iter().enumerate() {
        if is_plain_yaml_document(index, class) {
            u32::try_from(index).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "yaml_document_ordinal",
            })?;
        } else {
            YamlAnchor::validate(&class.anchor)?;
            anchors.push(class.anchor.as_str());
        }
    }
    anchors.sort_unstable();
    if anchors.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkspaceError::InvalidSourceIdentity {
            source_kind: SourceKind::Yaml,
            reason: WorkspaceSourceIdentityError::DuplicateYamlAnchor,
        });
    }
    Ok(())
}

fn is_plain_yaml_document(index: usize, class: &UnityClass) -> bool {
    class.class_id == 0
        && class.class_name == "YamlDocument"
        && class
            .anchor
            .strip_prefix("doc_")
            .and_then(|ordinal| ordinal.parse::<usize>().ok())
            == Some(index)
}

fn register_prepared(
    prepared: PreparedSource,
    descriptor: SourceDescriptor,
    catalog: &mut super::source_catalog::SourceCatalogTransaction,
    store: &mut SourceStore,
    budget: &mut AssetLoadBudget,
) -> Result<SourceId, WorkspaceError> {
    let PreparedSource {
        kind,
        image,
        parsed,
        children,
    } = prepared;
    let fingerprint = SourceFingerprint::from_bytes(kind, &image);
    let source = catalog.register(descriptor, fingerprint, budget)?;
    let parse = match parsed {
        PreparedParse::None => FrozenSourceParse::None,
        PreparedParse::Serialized(file) => FrozenSourceParse::Serialized(file),
        PreparedParse::Yaml(document) => FrozenSourceParse::Yaml(document),
    };
    store
        .insert(source, SourceImage::from_arc(kind, image), parse, budget)
        .map_err(WorkspaceError::from)?;

    for child in children {
        let descriptor =
            child_descriptor(source, child.relation, child.source.kind, child.identity)?;
        register_prepared(child.source, descriptor, catalog, store, budget)?;
    }
    Ok(source)
}

fn child_descriptor(
    parent: SourceId,
    relation: ChildRelation,
    kind: SourceKind,
    identity: SourceMemberId,
) -> Result<SourceDescriptor, WorkspaceError> {
    if kind == SourceKind::StreamedResource {
        return SourceDescriptor::sidecar(parent, identity).map_err(WorkspaceError::from);
    }
    match relation {
        ChildRelation::Archive => SourceDescriptor::archive_member(parent, kind, identity),
        ChildRelation::Bundle => SourceDescriptor::bundle_member(parent, kind, identity),
        ChildRelation::WebFile => SourceDescriptor::webfile_member(parent, kind, identity),
    }
    .map_err(WorkspaceError::from)
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
) -> Result<Vec<PreparedChild>, WorkspaceError> {
    let bytes = count
        .checked_mul(size_of::<PreparedChild>())
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

fn map_yaml_adapter_error(operation: &'static str, error: YamlAdapterError) -> WorkspaceError {
    match error {
        YamlAdapterError::Budget(error) => WorkspaceError::Budget(error),
        YamlAdapterError::AllocationFailed { context, requested } => WorkspaceError::Allocation {
            resource: context,
            requested,
            unit: WorkspaceAllocationUnit::Bytes,
            message: "allocator rejected the requested capacity".to_owned(),
        },
        YamlAdapterError::DepthExceeded { actual, limit } => {
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

fn map_binary_adapter_error(error: BinaryAdapterError) -> WorkspaceError {
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

fn reserve_budgeted_vec<T>(
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
    let bytes = size_of::<T>()
        .checked_add(size_of::<usize>() * 2)
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    Ok(())
}

fn promote_box_to_arc<T>(
    value: Box<T>,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<Arc<T>, WorkspaceError> {
    consume_arc_allocation::<T>(budget, resource)?;
    Ok(Arc::from(value))
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, WorkspaceError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource }.into())
}

fn absolute_path(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|error| WorkspaceError::io(path, error))
}

fn account_root_descriptor_backing(
    alias: &SourceAlias,
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
) -> Result<(), WorkspaceError> {
    let retained_bytes = alias
        .retained_clone_bytes()
        .checked_add(origin.path().as_os_str().len())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_root_descriptor",
        })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(())
}

fn read_owned_image(
    origin: &PhysicalOrigin,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>, WorkspaceError> {
    let path = origin.path();
    let mut file = File::open(path).map_err(|error| WorkspaceError::io(path, error))?;
    let length = file
        .metadata()
        .map_err(|error| WorkspaceError::io(path, error))?
        .len();
    let length_usize = usize::try_from(length).map_err(|_| WorkspaceError::SourceTooLarge {
        path: path.to_path_buf(),
        length,
    })?;
    budget.check_bytes(length)?;
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
    file.read_exact(&mut bytes)
        .map_err(|error| WorkspaceError::io(path, error))?;
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|error| WorkspaceError::io(path, error))?
        != 0
    {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    let position = file
        .stream_position()
        .map_err(|error| WorkspaceError::io(path, error))?;
    if position != length {
        return Err(WorkspaceError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    let arc_allocation = length
        .checked_add(u64::try_from(size_of::<usize>() * 2).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "workspace_source_arc",
            }
        })?)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "workspace_source_arc",
        })?;
    budget.check_bytes(arc_allocation)?;
    budget.consume_bytes(arc_allocation)?;
    Ok(Arc::from(bytes))
}

fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x06\x06")
}

fn looks_like_yaml(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let Some(text) = std::str::from_utf8(bytes).ok() else {
        return false;
    };
    let start = text.trim_start();
    start.starts_with("%YAML") || start.starts_with("--- !u!")
}

fn has_yaml_extension(path: &Path) -> bool {
    has_extension(
        path,
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

fn has_resource_extension(path: &Path) -> bool {
    has_extension(path, &["resource", "ress"])
}

fn has_extension(path: &Path, expected: &[&str]) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            expected
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn tree_with_child() -> TypeTree {
        let mut root = TypeTreeNode::new();
        root.children.push(TypeTreeNode::new());
        TypeTree {
            nodes: vec![root],
            ..TypeTree::default()
        }
    }

    #[test]
    fn frozen_leaf_root_uses_the_same_zero_based_depth_as_embedded_trees() {
        let tree = TypeTree {
            nodes: vec![TypeTreeNode::new()],
            ..TypeTree::default()
        };
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        account_frozen_type_tree(&tree, &mut budget, 1).unwrap();

        assert!(budget.usage().bytes > 0);
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn frozen_tree_rejects_child_depth_before_child_traversal_scratch() {
        let tree = tree_with_child();
        let root = &tree.nodes[0];
        let expected_bytes = size_of::<TypeTree>()
            + tree.nodes.capacity() * size_of::<TypeTreeNode>()
            + tree.string_buffer.capacity()
            + size_of::<(&TypeTreeNode, u32)>()
            + root.type_name.capacity()
            + root.name.capacity()
            + root.children.capacity() * size_of::<TypeTreeNode>();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_depth: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = account_frozen_type_tree(&tree, &mut budget, 1).unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "depth",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().bytes, u64::try_from(expected_bytes).unwrap());
        assert_eq!(budget.usage().entries, 1);
        assert_eq!(budget.usage().max_observed_depth, 1);
    }

    #[test]
    fn owned_root_image_accounts_working_and_arc_backings_before_promotion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payload.resource");
        fs::write(&path, b"four").unwrap();
        let origin = PhysicalOrigin::from_existing_path(&path).unwrap();
        let arc_bytes = u64::try_from(4 + size_of::<usize>() * 2).unwrap();
        let exact_bytes = 4 + arc_bytes;

        let mut short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = read_owned_image(&origin, &mut short).unwrap_err();
        assert!(matches!(
            error,
            WorkspaceError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == exact_bytes - 1 && requested == exact_bytes
        ));
        assert_eq!(short.usage().bytes, 4);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let image = read_owned_image(&origin, &mut exact).unwrap();
        assert_eq!(image.as_ref(), b"four");
        assert_eq!(exact.usage().bytes, exact_bytes);
    }

    #[test]
    fn binary_adapter_resource_failures_keep_their_public_error_variants() {
        let memory = map_binary_adapter_error(BinaryAdapterError::Parse {
            source: unity_asset_binary::error::BinaryError::MemoryError(
                "allocation failed".to_owned(),
            ),
        });
        assert!(matches!(
            memory,
            WorkspaceError::Binary(unity_asset_binary::error::BinaryError::MemoryError(message))
                if message == "allocation failed"
        ));

        let hard_limit = map_binary_adapter_error(BinaryAdapterError::MemberBinary {
            container: BinaryContainerKind::WebFile,
            wire_ordinal: 7,
            source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(
                "member limit".to_owned(),
            ),
        });
        assert!(matches!(
            hard_limit,
            WorkspaceError::BinaryMember {
                container: WorkspaceSourceContainer::WebFile,
                wire_ordinal: 7,
                source: unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message),
            } if message == "member limit"
        ));
    }

    #[test]
    fn allocation_mappers_preserve_bytes_elements_and_slots() {
        let reserve_error = || {
            Vec::<u8>::new()
                .try_reserve(usize::MAX)
                .expect_err("an impossible capacity must fail")
        };
        for (adapter_unit, expected) in [
            (
                BinaryAdapterAllocationUnit::Bytes,
                WorkspaceAllocationUnit::Bytes,
            ),
            (
                BinaryAdapterAllocationUnit::Elements,
                WorkspaceAllocationUnit::Elements,
            ),
        ] {
            let error = map_binary_adapter_error(BinaryAdapterError::Allocation {
                resource: "binary allocation",
                requested: 9,
                unit: adapter_unit,
                source: reserve_error(),
            });
            assert!(matches!(
                error,
                WorkspaceError::Allocation {
                    resource: "binary allocation",
                    requested: 9,
                    unit,
                    ..
                } if unit == expected
            ));
        }

        for (catalog_unit, expected) in [
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Bytes,
                WorkspaceAllocationUnit::Bytes,
            ),
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Elements,
                WorkspaceAllocationUnit::Elements,
            ),
            (
                crate::workspace::source_catalog::CatalogAllocationUnit::Slots,
                WorkspaceAllocationUnit::Slots,
            ),
        ] {
            let error = WorkspaceError::from(
                crate::workspace::source_catalog::CatalogError::AllocationFailed {
                    resource: "catalog allocation",
                    requested: 11,
                    unit: catalog_unit,
                    message: "allocation failed".to_owned(),
                },
            );
            assert!(matches!(
                error,
                WorkspaceError::Allocation {
                    resource: "catalog allocation",
                    requested: 11,
                    unit,
                    ..
                } if unit == expected
            ));
        }
    }

    #[test]
    fn workspace_binary_errors_preserve_the_standard_source_chain() {
        let root = WorkspaceError::from(
            unity_asset_binary::error::BinaryError::ResourceLimitExceeded("hard limit".to_owned()),
        );
        assert!(
            std::error::Error::source(&root)
                .and_then(|source| {
                    source.downcast_ref::<unity_asset_binary::error::BinaryError>()
                })
                .is_some_and(|source| matches!(source, unity_asset_binary::error::BinaryError::ResourceLimitExceeded(message) if message == "hard limit"))
        );
    }
}
