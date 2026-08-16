//! Deterministic leaf-to-root construction of prepared container artifacts.

use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::bundle::{AssetBundle, BundleParser};
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_binary::webfile::WebFile;
use unity_asset_core::{
    BudgetError, ContainmentKind, SourceId, SourceKind, SourceMemberId, WorkspaceId,
    vec_allocation_bytes,
};
use unity_asset_write::PackingPolicy;
use unity_asset_write::artifact::{
    ArtifactBatch, ArtifactBuildError, ArtifactBuildFailurePhase, ArtifactHandle, ArtifactPayload,
    ArtifactPayloadError,
};
use unity_asset_write::bundle::{BundleArtifactEntry, BundleArtifactError, BundleWriter};
use unity_asset_write::webfile::{
    WebFileArtifactMember, WebFilePackingPolicy, WebFileWriteError, WebFileWriter,
};

use super::super::snapshot::WorkspaceSnapshot;
use super::super::source_catalog::{CatalogError, SourceCatalog, SourceLocationKind};

/// One exact prepared artifact bound to the logical source whose bytes it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PreparedSourceArtifact {
    source: SourceId,
    artifact: ArtifactHandle,
}

impl PreparedSourceArtifact {
    const fn new(source: SourceId, artifact: ArtifactHandle) -> Self {
        Self { source, artifact }
    }

    #[must_use]
    pub(crate) const fn source(self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) const fn artifact(self) -> ArtifactHandle {
        self.artifact
    }
}

/// Complete changed-source graph plus the roots a later runner may bind to declared outputs.
#[derive(Debug)]
pub(crate) struct PreparedArtifactGraph {
    bindings: Vec<PreparedSourceArtifact>,
    publication_roots: Vec<PreparedSourceArtifact>,
}

impl PreparedArtifactGraph {
    #[must_use]
    pub(crate) fn bindings(&self) -> &[PreparedSourceArtifact] {
        &self.bindings
    }

    #[must_use]
    pub(crate) fn publication_roots(&self) -> &[PreparedSourceArtifact] {
        &self.publication_roots
    }
}

/// Rejections raised before output binding or filesystem publication is allowed.
#[derive(Debug, Error)]
pub(crate) enum ArtifactGraphError {
    #[error(transparent)]
    Catalog(Box<CatalogError>),
    #[error(transparent)]
    Artifact(Box<ArtifactBuildError>),
    #[error(transparent)]
    Bundle(Box<BundleArtifactError>),
    #[error(transparent)]
    WebFile(Box<WebFileWriteError>),
    #[error(transparent)]
    Payload(Box<ArtifactPayloadError>),
    #[error("prepared leaf source {source_id:?} is not present in the candidate catalog")]
    UnknownLeafSource { source_id: SourceId },
    #[error("source {source_id:?} has no immutable baseline image")]
    MissingBaselineSource { source_id: SourceId },
    #[error("candidate catalog belongs to workspace {actual}, expected {expected}")]
    CandidateWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("prepared leaf source {source_id:?} occurs more than once")]
    DuplicateLeaf { source_id: SourceId },
    #[error(
        "prepared source {source_id:?} is both an input leaf and an affected container ancestor"
    )]
    OverlappingLeafAndAncestor { source_id: SourceId },
    #[error(
        "source {source_id:?} has containment {containment:?}, but its parent {parent:?} has kind {parent_kind:?}"
    )]
    InvalidContainmentParent {
        source_id: SourceId,
        containment: ContainmentKind,
        parent: SourceId,
        parent_kind: SourceKind,
    },
    #[error("source {source_id:?} has containment metadata but no catalog parent")]
    MissingContainmentParent { source_id: SourceId },
    #[error(
        "archive {archive:?} is an affected ancestor of {changed_descendant:?}; prepared ZIP writing is unsupported"
    )]
    UnsupportedArchiveAncestor {
        archive: SourceId,
        changed_descendant: SourceId,
    },
    #[error(
        "container {container:?} wire member {name:?} occurrence {occurrence} at ordinal {wire_ordinal} has no catalog source"
    )]
    OrphanWireMember {
        container: SourceId,
        wire_ordinal: usize,
        name: String,
        occurrence: u32,
    },
    #[error(
        "container {container:?} catalog source {source_id:?} is absent from the parsed wire directory"
    )]
    MissingWireMember {
        container: SourceId,
        source_id: SourceId,
    },
    #[error(transparent)]
    DuplicateCatalogMember(Box<DuplicateCatalogMemberError>),
    #[error(
        "container {container:?} wire member identity at ordinal {wire_ordinal} matched more than once"
    )]
    DuplicateWireMember {
        container: SourceId,
        wire_ordinal: usize,
    },
    #[error(
        "container {container:?} has too many same-name members to encode occurrence at ordinal {wire_ordinal}"
    )]
    MemberOccurrenceOverflow {
        container: SourceId,
        wire_ordinal: usize,
    },
    #[error("affected ancestor {source_id:?} has unsupported source kind {kind:?}")]
    UnsupportedAncestorKind {
        source_id: SourceId,
        kind: SourceKind,
    },
    #[error(
        "catalog source {source_id:?} under container {container:?} has no matching final locator member"
    )]
    InvalidCatalogMemberLocator {
        container: SourceId,
        source_id: SourceId,
    },
    #[error(
        "new catalog member {source_id:?} under container {container:?} has unsupported kind {kind:?}"
    )]
    UnsupportedAddedMember {
        container: SourceId,
        source_id: SourceId,
        kind: SourceKind,
    },
    #[error(
        "new catalog member {source_id:?} under container {container:?} has no prepared artifact"
    )]
    MissingAddedMemberArtifact {
        container: SourceId,
        source_id: SourceId,
    },
}

#[derive(Debug, Error)]
#[error(
    "container {container:?} catalog sources {first:?} and {second:?} claim the same member identity"
)]
pub(crate) struct DuplicateCatalogMemberError {
    container: SourceId,
    first: SourceId,
    second: SourceId,
}

impl ArtifactGraphError {
    pub(crate) const fn failure_phase(&self) -> ArtifactBuildFailurePhase {
        match self {
            Self::Artifact(error) => error.failure_phase(),
            Self::Bundle(error) => error.failure_phase(),
            Self::WebFile(error) => error.failure_phase(),
            _ => ArtifactBuildFailurePhase::Encoding,
        }
    }
}

impl From<CatalogError> for ArtifactGraphError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(Box::new(error))
    }
}

impl From<ArtifactBuildError> for ArtifactGraphError {
    fn from(error: ArtifactBuildError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl From<BudgetError> for ArtifactGraphError {
    fn from(error: BudgetError) -> Self {
        ArtifactBuildError::from(error).into()
    }
}

impl From<BundleArtifactError> for ArtifactGraphError {
    fn from(error: BundleArtifactError) -> Self {
        Self::Bundle(Box::new(error))
    }
}

impl From<WebFileWriteError> for ArtifactGraphError {
    fn from(error: WebFileWriteError) -> Self {
        Self::WebFile(Box::new(error))
    }
}

impl From<ArtifactPayloadError> for ArtifactGraphError {
    fn from(error: ArtifactPayloadError) -> Self {
        Self::Payload(Box::new(error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AffectedAncestor {
    source: SourceId,
    depth: usize,
}

#[derive(Debug)]
struct CatalogMember<'snapshot> {
    container: SourceId,
    source: SourceId,
    member: &'snapshot SourceMemberId,
    seen: bool,
}

#[derive(Debug, Clone, Copy)]
struct WireOrdinal {
    wire_index: usize,
    occurrence: u32,
}

/// Expands already-prepared logical leaves into exact parent container artifacts.
///
/// This function never declares or binds outputs. The caller retains that authority and receives
/// the deterministically sorted publication roots after every affected container has been rebuilt.
pub(crate) fn prepare_artifact_graph(
    snapshot: &WorkspaceSnapshot,
    catalog: &SourceCatalog,
    batch: &mut ArtifactBatch<'_, '_>,
    leaves: &[(SourceId, ArtifactHandle)],
) -> Result<PreparedArtifactGraph, ArtifactGraphError> {
    batch.run_fail_stop(|batch| prepare_artifact_graph_inner(snapshot, catalog, batch, leaves))
}

fn prepare_artifact_graph_inner(
    snapshot: &WorkspaceSnapshot,
    catalog: &SourceCatalog,
    batch: &mut ArtifactBatch<'_, '_>,
    leaves: &[(SourceId, ArtifactHandle)],
) -> Result<PreparedArtifactGraph, ArtifactGraphError> {
    if catalog.workspace() != snapshot.workspace_id() {
        return Err(ArtifactGraphError::CandidateWorkspaceMismatch {
            expected: snapshot.workspace_id(),
            actual: catalog.workspace(),
        });
    }

    let mut normalized_leaves =
        budgeted_vec::<PreparedSourceArtifact>(batch, leaves.len(), "artifact_graph_leaves")?;
    normalized_leaves.extend(
        leaves
            .iter()
            .map(|(source, artifact)| PreparedSourceArtifact::new(*source, *artifact)),
    );
    normalized_leaves.sort_unstable_by_key(|leaf| leaf.source);
    if let Some(duplicate) = normalized_leaves
        .windows(2)
        .find(|pair| pair[0].source == pair[1].source)
    {
        return Err(ArtifactGraphError::DuplicateLeaf {
            source_id: duplicate[0].source,
        });
    }

    let mut ancestor_capacity = 0_usize;
    for leaf in &normalized_leaves {
        let locator = catalog
            .source_locator(leaf.source)
            .map_err(|error| match error {
                CatalogError::UnknownSource(_) => ArtifactGraphError::UnknownLeafSource {
                    source_id: leaf.source,
                },
                error => error.into(),
            })?;
        ancestor_capacity = ancestor_capacity
            .checked_add(locator.members().len())
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_ancestors",
            })?;
    }

    let mut ancestors =
        budgeted_vec::<AffectedAncestor>(batch, ancestor_capacity, "artifact_graph_ancestors")?;
    for leaf in &normalized_leaves {
        batch.artifact_len(leaf.artifact)?;
        collect_ancestors(catalog, leaf.source, &mut ancestors)?;
    }
    ancestors.sort_unstable_by(|left, right| {
        right
            .depth
            .cmp(&left.depth)
            .then_with(|| left.source.cmp(&right.source))
    });
    ancestors.dedup_by_key(|ancestor| (ancestor.depth, ancestor.source));

    let mut catalog_members = if ancestors.is_empty() {
        Vec::new()
    } else {
        collect_catalog_member_index(catalog, batch)?
    };

    let binding_capacity = normalized_leaves.len().checked_add(ancestors.len()).ok_or(
        BudgetError::ArithmeticOverflow {
            resource: "artifact_graph_bindings",
        },
    )?;
    let mut bindings =
        budgeted_vec::<PreparedSourceArtifact>(batch, binding_capacity, "artifact_graph_bindings")?;
    bindings.extend(normalized_leaves);

    for ancestor in ancestors {
        let insertion =
            match bindings.binary_search_by_key(&ancestor.source, |binding| binding.source) {
                Ok(_) => {
                    return Err(ArtifactGraphError::OverlappingLeafAndAncestor {
                        source_id: ancestor.source,
                    });
                }
                Err(insertion) => insertion,
            };
        let member_range = catalog_member_range(&catalog_members, ancestor.source);
        let artifact = match ancestor.source.kind() {
            SourceKind::AssetBundle => prepare_bundle_ancestor(
                snapshot,
                batch,
                ancestor.source,
                &mut catalog_members[member_range.clone()],
                &bindings,
            )?,
            SourceKind::WebFile => prepare_webfile_ancestor(
                snapshot,
                batch,
                ancestor.source,
                &mut catalog_members[member_range],
                &bindings,
            )?,
            SourceKind::Archive => {
                return Err(ArtifactGraphError::UnsupportedArchiveAncestor {
                    archive: ancestor.source,
                    changed_descendant: ancestor.source,
                });
            }
            SourceKind::Yaml | SourceKind::SerializedFile | SourceKind::StreamedResource => {
                return Err(ArtifactGraphError::UnsupportedAncestorKind {
                    source_id: ancestor.source,
                    kind: ancestor.source.kind(),
                });
            }
        };
        bindings.insert(
            insertion,
            PreparedSourceArtifact::new(ancestor.source, artifact),
        );
    }

    let root_count = bindings.iter().try_fold(0_usize, |count, binding| {
        let increment = usize::from(contained_parent(catalog, binding.source)?.is_none());
        count
            .checked_add(increment)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_publication_roots",
            })
            .map_err(ArtifactGraphError::from)
    })?;
    let mut publication_roots = budgeted_vec::<PreparedSourceArtifact>(
        batch,
        root_count,
        "artifact_graph_publication_roots",
    )?;
    for binding in &bindings {
        if contained_parent(catalog, binding.source)?.is_none() {
            publication_roots.push(*binding);
        }
    }

    Ok(PreparedArtifactGraph {
        bindings,
        publication_roots,
    })
}

fn collect_ancestors(
    catalog: &SourceCatalog,
    changed_source: SourceId,
    ancestors: &mut Vec<AffectedAncestor>,
) -> Result<(), ArtifactGraphError> {
    let mut current = changed_source;
    while let Some(parent) = contained_parent(catalog, current)? {
        match parent.kind() {
            SourceKind::AssetBundle | SourceKind::WebFile => {
                let depth = catalog.source_locator(parent)?.members().len();
                ancestors.push(AffectedAncestor {
                    source: parent,
                    depth,
                });
            }
            SourceKind::Archive => {
                return Err(ArtifactGraphError::UnsupportedArchiveAncestor {
                    archive: parent,
                    changed_descendant: changed_source,
                });
            }
            SourceKind::Yaml | SourceKind::SerializedFile | SourceKind::StreamedResource => {
                let containment = final_containment(catalog, current)?
                    .ok_or(ArtifactGraphError::MissingContainmentParent { source_id: current })?;
                return Err(ArtifactGraphError::InvalidContainmentParent {
                    source_id: current,
                    containment,
                    parent,
                    parent_kind: parent.kind(),
                });
            }
        }
        current = parent;
    }
    Ok(())
}

fn contained_parent(
    catalog: &SourceCatalog,
    source: SourceId,
) -> Result<Option<SourceId>, ArtifactGraphError> {
    let Some(containment) = final_containment(catalog, source)? else {
        return Ok(None);
    };
    if containment == ContainmentKind::Companion {
        return Ok(None);
    }
    let parent = catalog
        .parent(source)?
        .ok_or(ArtifactGraphError::MissingContainmentParent { source_id: source })?;
    let expected = match containment {
        ContainmentKind::Archive => SourceKind::Archive,
        ContainmentKind::Bundle => SourceKind::AssetBundle,
        ContainmentKind::WebFile => SourceKind::WebFile,
        ContainmentKind::Companion => return Ok(None),
    };
    if parent.kind() != expected {
        return Err(ArtifactGraphError::InvalidContainmentParent {
            source_id: source,
            containment,
            parent,
            parent_kind: parent.kind(),
        });
    }
    Ok(Some(parent))
}

fn final_containment(
    catalog: &SourceCatalog,
    source: SourceId,
) -> Result<Option<ContainmentKind>, ArtifactGraphError> {
    Ok(catalog
        .source_locator(source)?
        .members()
        .last()
        .map(|step| step.container()))
}

fn prepare_bundle_ancestor(
    snapshot: &WorkspaceSnapshot,
    batch: &mut ArtifactBatch<'_, '_>,
    source: SourceId,
    catalog_members: &mut [CatalogMember<'_>],
    bindings: &[PreparedSourceArtifact],
) -> Result<ArtifactHandle, ArtifactGraphError> {
    let image = source_image(snapshot, source)?;
    let shared = SharedBytes::from_arc(Arc::clone(image.backing()));
    let length = shared.len();
    let bundle = batch.inspect_with_budget(|budget| {
        BundleParser::from_shared_range_with_budget(shared, 0..length, budget)
            .map_err(ArtifactBuildError::from)
    })?;
    let wire_ordinals = bundle_wire_ordinals(batch, source, &bundle)?;
    let added_count = catalog_members
        .iter()
        .filter(|member| !snapshot.state().catalog().contains(member.source))
        .count();
    let entry_capacity =
        bundle
            .nodes
            .len()
            .checked_add(added_count)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_bundle_entries",
            })?;
    let mut entries = budgeted_vec::<BundleArtifactEntry<'_>>(
        batch,
        entry_capacity,
        "artifact_graph_bundle_entries",
    )?;
    let mut file_ordinal = 0_usize;

    for (wire_index, node) in bundle.nodes.iter().enumerate() {
        if node.is_file() {
            let ordinal = wire_ordinals[file_ordinal];
            debug_assert_eq!(ordinal.wire_index, wire_index);
            let member_index = match_catalog_member(
                batch,
                source,
                wire_index,
                &node.name,
                ordinal.occurrence,
                catalog_members,
            )?;
            if catalog_members[member_index].seen {
                return Err(ArtifactGraphError::DuplicateWireMember {
                    container: source,
                    wire_ordinal: wire_index,
                });
            }
            catalog_members[member_index].seen = true;
            let child = catalog_members[member_index].source;
            let artifact = binding_for(bindings, child)
                .map(PreparedSourceArtifact::artifact)
                .map_or_else(|| prepare_unchanged_source(snapshot, batch, child), Ok)?;
            entries.push(BundleArtifactEntry::file(
                batch, &node.name, node.flags, artifact,
            )?);
            file_ordinal += 1;
        } else if node.is_deleted() {
            entries.push(BundleArtifactEntry::deleted_from_node(node)?);
        } else {
            entries.push(BundleArtifactEntry::empty_directory_from_node(node)?);
        }
    }
    for member in catalog_members.iter_mut().filter(|member| !member.seen) {
        if snapshot.state().catalog().contains(member.source) {
            continue;
        }
        if member.source.kind() != SourceKind::StreamedResource {
            return Err(ArtifactGraphError::UnsupportedAddedMember {
                container: source,
                source_id: member.source,
                kind: member.source.kind(),
            });
        }
        let artifact = binding_for(bindings, member.source).ok_or(
            ArtifactGraphError::MissingAddedMemberArtifact {
                container: source,
                source_id: member.source,
            },
        )?;
        entries.push(BundleArtifactEntry::file(
            batch,
            member.member.name(),
            0,
            artifact.artifact(),
        )?);
        member.seen = true;
    }
    ensure_all_catalog_members_seen(source, catalog_members)?;

    Ok(BundleWriter::prepare_artifact(
        batch,
        &bundle,
        &entries,
        PackingPolicy::Preserve,
    )?)
}

fn prepare_webfile_ancestor(
    snapshot: &WorkspaceSnapshot,
    batch: &mut ArtifactBatch<'_, '_>,
    source: SourceId,
    catalog_members: &mut [CatalogMember<'_>],
    bindings: &[PreparedSourceArtifact],
) -> Result<ArtifactHandle, ArtifactGraphError> {
    let image = source_image(snapshot, source)?;
    let shared = SharedBytes::from_arc(Arc::clone(image.backing()));
    let length = shared.len();
    let web = batch.inspect_with_budget(|budget| {
        WebFile::from_shared_range_with_budget(shared, 0..length, budget)
            .map_err(ArtifactBuildError::from)
    })?;
    let wire_ordinals = webfile_wire_ordinals(batch, source, &web)?;
    let added_count = catalog_members
        .iter()
        .filter(|member| !snapshot.state().catalog().contains(member.source))
        .count();
    let member_capacity =
        web.files()
            .len()
            .checked_add(added_count)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_webfile_members",
            })?;
    let mut members = budgeted_vec::<WebFileArtifactMember<'_>>(
        batch,
        member_capacity,
        "artifact_graph_webfile_members",
    )?;

    for (wire_index, file) in web.files().iter().enumerate() {
        let ordinal = wire_ordinals[wire_index];
        debug_assert_eq!(ordinal.wire_index, wire_index);
        let member_index = match_catalog_member(
            batch,
            source,
            wire_index,
            &file.name,
            ordinal.occurrence,
            catalog_members,
        )?;
        if catalog_members[member_index].seen {
            return Err(ArtifactGraphError::DuplicateWireMember {
                container: source,
                wire_ordinal: wire_index,
            });
        }
        catalog_members[member_index].seen = true;
        let child = catalog_members[member_index].source;
        let artifact = binding_for(bindings, child)
            .map(PreparedSourceArtifact::artifact)
            .map_or_else(|| prepare_unchanged_source(snapshot, batch, child), Ok)?;
        members.push(WebFileArtifactMember::new(batch, &file.name, artifact)?);
    }
    for member in catalog_members.iter_mut().filter(|member| !member.seen) {
        if snapshot.state().catalog().contains(member.source) {
            continue;
        }
        if member.source.kind() != SourceKind::StreamedResource {
            return Err(ArtifactGraphError::UnsupportedAddedMember {
                container: source,
                source_id: member.source,
                kind: member.source.kind(),
            });
        }
        let artifact = binding_for(bindings, member.source).ok_or(
            ArtifactGraphError::MissingAddedMemberArtifact {
                container: source,
                source_id: member.source,
            },
        )?;
        members.push(WebFileArtifactMember::new(
            batch,
            member.member.name(),
            artifact.artifact(),
        )?);
        member.seen = true;
    }
    ensure_all_catalog_members_seen(source, catalog_members)?;

    Ok(WebFileWriter::prepare(
        batch,
        &web,
        &members,
        WebFilePackingPolicy::Preserve,
    )?)
}

fn source_image(
    snapshot: &WorkspaceSnapshot,
    source: SourceId,
) -> Result<&unity_asset_core::VerifiedSourceImage, ArtifactGraphError> {
    snapshot
        .state()
        .store()
        .get(source)
        .map(|entry| entry.image())
        .ok_or(ArtifactGraphError::MissingBaselineSource { source_id: source })
}

fn prepare_unchanged_source(
    snapshot: &WorkspaceSnapshot,
    batch: &mut ArtifactBatch<'_, '_>,
    source: SourceId,
) -> Result<ArtifactHandle, ArtifactGraphError> {
    let payload = ArtifactPayload::source_backed(source, source_image(snapshot, source)?.clone())?;
    Ok(batch.prepare_verbatim_source(&payload)?)
}

fn collect_catalog_member_index<'snapshot>(
    catalog: &'snapshot SourceCatalog,
    batch: &mut ArtifactBatch<'_, '_>,
) -> Result<Vec<CatalogMember<'snapshot>>, ArtifactGraphError> {
    let catalog_visits = u64::try_from(catalog.len())
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "artifact_graph_catalog_visits",
        })?
        .checked_mul(2)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "artifact_graph_catalog_visits",
        })?;
    batch.inspect_with_budget(|budget| {
        budget.check_members(catalog_visits)?;
        budget.consume_members(catalog_visits)?;
        Ok(())
    })?;

    let count = catalog
        .iter()
        .filter(|(_, descriptor)| {
            descriptor.parent().is_some()
                && descriptor.location_kind() != SourceLocationKind::Companion
        })
        .count();
    let mut members = budgeted_vec::<CatalogMember<'snapshot>>(
        batch,
        count,
        "artifact_graph_catalog_member_index",
    )?;
    for (source, descriptor) in catalog.iter() {
        let Some(container) = descriptor.parent() else {
            continue;
        };
        if descriptor.location_kind() == SourceLocationKind::Companion {
            continue;
        }
        let containment = match container.kind() {
            SourceKind::Archive => ContainmentKind::Archive,
            SourceKind::AssetBundle => ContainmentKind::Bundle,
            SourceKind::WebFile => ContainmentKind::WebFile,
            SourceKind::Yaml | SourceKind::SerializedFile | SourceKind::StreamedResource => {
                return Err(ArtifactGraphError::InvalidCatalogMemberLocator {
                    container,
                    source_id: source,
                });
            }
        };
        let member = catalog
            .source_locator(source)?
            .members()
            .last()
            .filter(|step| step.container() == containment)
            .map(|step| step.member())
            .ok_or(ArtifactGraphError::InvalidCatalogMemberLocator {
                container,
                source_id: source,
            })?;
        members.push(CatalogMember {
            container,
            source,
            member,
            seen: false,
        });
    }
    members.sort_unstable_by(|left, right| {
        left.container
            .cmp(&right.container)
            .then_with(|| compare_member(left.member, right.member))
            .then_with(|| left.source.cmp(&right.source))
    });
    if let Some(pair) = members.windows(2).find(|pair| {
        pair[0].container == pair[1].container
            && compare_member(pair[0].member, pair[1].member) == Ordering::Equal
    }) {
        return Err(ArtifactGraphError::DuplicateCatalogMember(Box::new(
            DuplicateCatalogMemberError {
                container: pair[0].container,
                first: pair[0].source,
                second: pair[1].source,
            },
        )));
    }
    Ok(members)
}

fn catalog_member_range(members: &[CatalogMember<'_>], container: SourceId) -> Range<usize> {
    let start = members.partition_point(|member| member.container < container);
    let count = members[start..].partition_point(|member| member.container == container);
    start..start + count
}

fn compare_member(left: &SourceMemberId, right: &SourceMemberId) -> Ordering {
    left.name().cmp(right.name()).then_with(|| {
        left.same_name_occurrence()
            .cmp(&right.same_name_occurrence())
    })
}

fn match_catalog_member(
    batch: &mut ArtifactBatch<'_, '_>,
    container: SourceId,
    wire_ordinal: usize,
    name: &str,
    occurrence: u32,
    members: &[CatalogMember<'_>],
) -> Result<usize, ArtifactGraphError> {
    members
        .binary_search_by(|candidate| {
            candidate
                .member
                .name()
                .cmp(name)
                .then_with(|| candidate.member.same_name_occurrence().cmp(&occurrence))
        })
        .map_err(|_| {
            clone_member_name(batch, name).map_or_else(
                |error| error,
                |name| ArtifactGraphError::OrphanWireMember {
                    container,
                    wire_ordinal,
                    name,
                    occurrence,
                },
            )
        })
}

fn clone_member_name(
    batch: &mut ArtifactBatch<'_, '_>,
    name: &str,
) -> Result<String, ArtifactGraphError> {
    Ok(batch.inspect_with_budget(|budget| {
        let minimum_bytes =
            u64::try_from(name.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_member_error_name",
            })?;
        budget.check_bytes(minimum_bytes)?;
        let mut owned = String::new();
        owned.try_reserve_exact(name.len()).map_err(|error| {
            ArtifactBuildError::Binary(unity_asset_binary::BinaryError::memory_error(format!(
                "failed to reserve artifact graph member error name: {error}"
            )))
        })?;
        let retained_bytes =
            u64::try_from(owned.capacity()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "artifact_graph_member_error_name",
            })?;
        budget.check_bytes(retained_bytes)?;
        owned.push_str(name);
        budget.consume_bytes(retained_bytes)?;
        Ok(owned)
    })?)
}

fn ensure_all_catalog_members_seen(
    container: SourceId,
    members: &[CatalogMember<'_>],
) -> Result<(), ArtifactGraphError> {
    if let Some(member) = members.iter().find(|member| !member.seen) {
        return Err(ArtifactGraphError::MissingWireMember {
            container,
            source_id: member.source,
        });
    }
    Ok(())
}

fn bundle_wire_ordinals(
    batch: &mut ArtifactBatch<'_, '_>,
    container: SourceId,
    bundle: &AssetBundle,
) -> Result<Vec<WireOrdinal>, ArtifactGraphError> {
    let count = bundle.nodes.iter().filter(|node| node.is_file()).count();
    let mut ordinals =
        budgeted_vec::<WireOrdinal>(batch, count, "artifact_graph_bundle_wire_ordinals")?;
    ordinals.extend(
        bundle
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_file())
            .map(|(wire_index, _)| WireOrdinal {
                wire_index,
                occurrence: 0,
            }),
    );
    assign_same_name_occurrences(container, &mut ordinals, |wire_index| {
        bundle.nodes[wire_index].name.as_str()
    })?;
    Ok(ordinals)
}

fn webfile_wire_ordinals(
    batch: &mut ArtifactBatch<'_, '_>,
    container: SourceId,
    web: &WebFile,
) -> Result<Vec<WireOrdinal>, ArtifactGraphError> {
    let mut ordinals = budgeted_vec::<WireOrdinal>(
        batch,
        web.files().len(),
        "artifact_graph_webfile_wire_ordinals",
    )?;
    ordinals.extend((0..web.files().len()).map(|wire_index| WireOrdinal {
        wire_index,
        occurrence: 0,
    }));
    assign_same_name_occurrences(container, &mut ordinals, |wire_index| {
        web.files()[wire_index].name.as_str()
    })?;
    Ok(ordinals)
}

fn assign_same_name_occurrences<'name>(
    container: SourceId,
    ordinals: &mut [WireOrdinal],
    name_at: impl Fn(usize) -> &'name str,
) -> Result<(), ArtifactGraphError> {
    ordinals.sort_unstable_by(|left, right| {
        name_at(left.wire_index)
            .cmp(name_at(right.wire_index))
            .then_with(|| left.wire_index.cmp(&right.wire_index))
    });
    let mut group_start = 0_usize;
    for index in 0..ordinals.len() {
        if index > 0
            && name_at(ordinals[index - 1].wire_index) != name_at(ordinals[index].wire_index)
        {
            group_start = index;
        }
        ordinals[index].occurrence = u32::try_from(index - group_start).map_err(|_| {
            ArtifactGraphError::MemberOccurrenceOverflow {
                container,
                wire_ordinal: ordinals[index].wire_index,
            }
        })?;
    }
    ordinals.sort_unstable_by_key(|ordinal| ordinal.wire_index);
    Ok(())
}

fn binding_for(
    bindings: &[PreparedSourceArtifact],
    source: SourceId,
) -> Option<PreparedSourceArtifact> {
    bindings
        .binary_search_by_key(&source, |binding| binding.source)
        .ok()
        .map(|index| bindings[index])
}

fn budgeted_vec<T>(
    batch: &mut ArtifactBatch<'_, '_>,
    capacity: usize,
    resource: &'static str,
) -> Result<Vec<T>, ArtifactGraphError> {
    Ok(batch.inspect_with_budget(|budget| {
        let entries =
            u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
        let minimum_bytes = vec_allocation_bytes::<T>(capacity)
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
        budget.check_entries(entries)?;
        budget.check_bytes(minimum_bytes)?;
        let mut values = Vec::new();
        values.try_reserve_exact(capacity).map_err(|error| {
            ArtifactBuildError::Binary(unity_asset_binary::BinaryError::memory_error(format!(
                "failed to reserve {capacity} entries for {resource}: {error}"
            )))
        })?;
        let retained_bytes = vec_allocation_bytes::<T>(values.capacity())
            .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_entries(entries)?;
        budget.consume_bytes(retained_bytes)?;
        Ok(values)
    })?)
}

#[cfg(test)]
mod tests;
