use thiserror::Error;
use unity_asset_binary::asset::{ExternalEncoding, FileIdentifier};
use unity_asset_core::{
    AssetLoadBudget, ContractError, ObjectId, ObjectKind, RevisionedObjectHandle, SourceId,
    SourceKind, WorkspaceId, WorkspaceRevision,
};

use crate::workspace::{
    ReferenceTarget, WorkspaceError, WorkspaceLookup, WorkspaceView, reference_view_parts,
};

use super::ReferenceGraphError;
use super::fact::{RawReferenceTarget, ReferenceFormat};
use super::input::{ReferenceInput, collect_object_sources};
use super::resolution::{ResolutionIdentityIndex, clone_string, reserve_vec};

const DEFAULT_YAML_EXTERNAL_TYPE_ID: i64 = 3;

/// Format-specific wire identity produced from one logical, revision-bound reference target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReferenceDestination {
    Null,
    BinaryLocal {
        path_id: i64,
    },
    BinaryExternal {
        path_id: i64,
        identifier: FileIdentifier,
    },
    YamlLocal {
        file_id: i64,
    },
    YamlExternal {
        file_id: i64,
        guid: [u8; 16],
        type_id: i64,
    },
}

/// Existing format metadata worth preserving while re-encoding a logical reference.
///
/// This intentionally excludes file IDs, GUIDs, and paths: those identities are derived from the
/// revision-bound target. Keeping the hint `Copy` prevents mutation preflight from fabricating a
/// complete raw reference merely to retain one optional type field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferenceEncodingHint {
    Binary { external_type_id: Option<i32> },
    Yaml { type_id: Option<i64> },
}

impl ReferenceEncodingHint {
    pub(crate) const fn binary(external_type_id: Option<i32>) -> Self {
        Self::Binary { external_type_id }
    }

    pub(crate) const fn yaml(type_id: Option<i64>) -> Self {
        Self::Yaml { type_id }
    }

    const fn format(self) -> ReferenceFormat {
        match self {
            Self::Binary { .. } => ReferenceFormat::Binary,
            Self::Yaml { .. } => ReferenceFormat::Yaml,
        }
    }
}

impl From<&RawReferenceTarget> for ReferenceEncodingHint {
    fn from(target: &RawReferenceTarget) -> Self {
        match target {
            RawReferenceTarget::Binary { external, .. } => {
                Self::binary(external.as_ref().map(|external| external.type_id()))
            }
            RawReferenceTarget::Yaml { type_id, .. } => Self::yaml(*type_id),
        }
    }
}

/// Immutable reverse-identity catalog used by mutation preflight.
///
/// Building the encoder scans `.meta` GUID claims and loaded object identities once. Every later
/// encode is rejected if it is attempted against a different workspace revision.
pub(crate) struct ReferenceDestinationEncoder {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    nodes: Vec<RevisionedObjectHandle>,
    identity: ResolutionIdentityIndex,
    binary_external_encodings: Vec<(SourceId, ExternalEncoding)>,
}

impl ReferenceDestinationEncoder {
    pub(crate) fn build(
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceEncodingError> {
        let input = reference_view_parts(view);
        let sources = reserve_vec(
            input.object_source_count(),
            "reference destination source inputs",
            budget,
        )?;
        let sources = collect_object_sources(&input, sources)?;

        let identity = ResolutionIdentityIndex::build(&sources, budget)?;
        let mut binary_external_encodings = reserve_vec(
            sources.len(),
            "reference destination binary format inputs",
            budget,
        )?;
        for source in sources.iter().copied() {
            if let Some(file) = source.serialized_file() {
                binary_external_encodings
                    .push((source.source(), file.format().external_encoding()));
            }
        }
        binary_external_encodings.sort_unstable_by_key(|(source, _)| *source);
        let mut nodes = view.objects(budget)?;
        nodes.sort_unstable();
        if nodes
            .windows(2)
            .any(|pair| pair[0].object() == pair[1].object())
        {
            return Err(ReferenceEncodingError::DuplicateObjectIdentity);
        }
        for node in &nodes {
            node.validate_context(view.workspace_id(), view.revision())?;
        }

        Ok(Self {
            workspace: view.workspace_id(),
            revision: view.revision(),
            nodes,
            identity,
            binary_external_encodings,
        })
    }

    pub(crate) fn encode(
        &self,
        view: &dyn WorkspaceView,
        owner: &RevisionedObjectHandle,
        target: &ReferenceTarget,
        hint: ReferenceEncodingHint,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceDestination, ReferenceEncodingError> {
        self.validate_context(view, owner)?;
        let owner_format = reference_format(owner.object())?;
        if hint.format() != owner_format {
            return Err(ReferenceEncodingError::HintFormatMismatch {
                expected: owner_format,
                actual: hint.format(),
            });
        }

        if !self.contains_object(owner.object()) {
            return Err(ReferenceEncodingError::OwnerMissing {
                source_id: owner.object().source(),
                kind: owner.object().kind(),
            });
        }

        let ReferenceTarget::Object { address } = target else {
            return Ok(ReferenceDestination::Null);
        };
        let target = self.resolve_target(view, address, budget)?;
        match owner_format {
            ReferenceFormat::Binary => {
                self.encode_binary(owner.object(), target.object(), address, hint, budget)
            }
            ReferenceFormat::Yaml => self.encode_yaml(owner.object(), target.object(), hint),
        }
    }

    /// Resolves the current binary wire identity and compares it with one logical target.
    pub(crate) fn binary_current_matches(
        &self,
        view: &dyn WorkspaceView,
        owner: &RevisionedObjectHandle,
        expected: &ReferenceTarget,
        file_id: i32,
        path_id: i64,
        external: Option<&FileIdentifier>,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, ReferenceEncodingError> {
        self.validate_owner(view, owner, ReferenceFormat::Binary)?;
        let ReferenceTarget::Object { address } = expected else {
            return Ok(path_id == 0);
        };
        let target = self.resolve_target(view, address, budget)?;
        let Some(target_path_id) = target.object().binary_path_id() else {
            return Err(ReferenceEncodingError::TargetKindMismatch {
                owner: ObjectKind::Binary,
                target: target.object().kind(),
            });
        };
        if path_id == 0 || target_path_id != path_id {
            return Ok(false);
        }

        let source = match file_id {
            ..=-1 => return Ok(false),
            0 => owner.object().source(),
            1.. => {
                let Some(external) = external else {
                    return Ok(false);
                };
                if !self.binary_external_identity_matches(
                    owner.object().source(),
                    target.object().source(),
                    external,
                    budget,
                )? {
                    return Ok(false);
                }
                target.object().source()
            }
        };
        Ok(source == target.object().source())
    }

    /// Resolves the current YAML wire identity and compares it with one logical target.
    pub(crate) fn yaml_current_matches(
        &self,
        view: &dyn WorkspaceView,
        owner: &RevisionedObjectHandle,
        expected: &ReferenceTarget,
        file_id: i64,
        guid: Option<[u8; 16]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, ReferenceEncodingError> {
        self.validate_owner(view, owner, ReferenceFormat::Yaml)?;
        let ReferenceTarget::Object { address } = expected else {
            return Ok(file_id == 0);
        };
        let target = self.resolve_target(view, address, budget)?;
        if file_id == 0 || yaml_file_id(target.object())? != file_id {
            return Ok(false);
        }

        let source = match guid {
            None => owner.object().source(),
            Some(guid) if guid == [0; 16] => return Ok(false),
            Some(guid) => {
                let mut sources = self.identity.guid_sources(guid);
                let Some(source) = sources.next() else {
                    return Ok(false);
                };
                if sources.next().is_some() {
                    return Ok(false);
                }
                source
            }
        };
        Ok(source == target.object().source())
    }

    fn validate_owner(
        &self,
        view: &dyn WorkspaceView,
        owner: &RevisionedObjectHandle,
        expected_format: ReferenceFormat,
    ) -> Result<(), ReferenceEncodingError> {
        self.validate_context(view, owner)?;
        let actual_format = reference_format(owner.object())?;
        if actual_format != expected_format {
            return Err(ReferenceEncodingError::OwnerFormatMismatch {
                expected: expected_format,
                actual: actual_format,
            });
        }
        if !self.contains_object(owner.object()) {
            return Err(ReferenceEncodingError::OwnerMissing {
                source_id: owner.object().source(),
                kind: owner.object().kind(),
            });
        }
        Ok(())
    }

    fn validate_context(
        &self,
        view: &dyn WorkspaceView,
        owner: &RevisionedObjectHandle,
    ) -> Result<(), ReferenceEncodingError> {
        if view.workspace_id() != self.workspace {
            return Err(ReferenceEncodingError::ViewWorkspaceMismatch {
                expected: self.workspace,
                actual: view.workspace_id(),
            });
        }
        if view.revision() != self.revision {
            return Err(ReferenceEncodingError::ViewRevisionMismatch {
                expected: self.revision,
                actual: view.revision(),
            });
        }
        owner.validate_context(self.workspace, self.revision)?;
        Ok(())
    }

    fn contains_object(&self, object: &ObjectId) -> bool {
        self.nodes
            .binary_search_by(|candidate| candidate.object().cmp(object))
            .is_ok()
    }

    fn resolve_target(
        &self,
        view: &dyn WorkspaceView,
        address: &unity_asset_core::ObjectAddress,
        budget: &mut AssetLoadBudget,
    ) -> Result<RevisionedObjectHandle, ReferenceEncodingError> {
        let target = require_resolved_target(view.resolve_object(address, budget)?)?;
        if target.workspace() != self.workspace {
            return Err(ReferenceEncodingError::TargetWorkspaceMismatch {
                expected: self.workspace,
                actual: target.workspace(),
            });
        }
        if target.revision() != self.revision {
            return Err(ReferenceEncodingError::TargetRevisionMismatch {
                expected: self.revision,
                actual: target.revision(),
            });
        }
        if !self.contains_object(target.object()) {
            return Err(ReferenceEncodingError::TargetMissingFromIdentityCatalog);
        }
        Ok(target)
    }

    fn encode_binary(
        &self,
        owner: &ObjectId,
        target: &ObjectId,
        address: &unity_asset_core::ObjectAddress,
        hint: ReferenceEncodingHint,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceDestination, ReferenceEncodingError> {
        let Some(path_id) = target.binary_path_id() else {
            return Err(ReferenceEncodingError::TargetKindMismatch {
                owner: ObjectKind::Binary,
                target: target.kind(),
            });
        };
        if target.source() == owner.source() {
            return Ok(ReferenceDestination::BinaryLocal { path_id });
        }
        if target.source().kind() != SourceKind::SerializedFile {
            return Err(ReferenceEncodingError::TargetKindMismatch {
                owner: ObjectKind::Binary,
                target: target.kind(),
            });
        }

        let path = external_path(address.source_locator(), budget)?;
        let external_type_id = match hint {
            ReferenceEncodingHint::Binary { external_type_id } => external_type_id.unwrap_or(0),
            ReferenceEncodingHint::Yaml { .. } => {
                return Err(ReferenceEncodingError::HintFormatMismatch {
                    expected: ReferenceFormat::Binary,
                    actual: ReferenceFormat::Yaml,
                });
            }
        };
        let encoding = self.binary_external_encoding(owner.source())?;
        let guid_identity = self.guid_identity(target.source());
        let (guid, type_id) = match encoding {
            ExternalEncoding::PathOnly => ([0; 16], 0),
            ExternalEncoding::GuidAndType | ExternalEncoding::AssetPathGuidAndType => {
                let guid = match guid_identity {
                    GuidIdentity::Unique(guid) => guid,
                    GuidIdentity::Missing
                    | GuidIdentity::Zero
                    | GuidIdentity::MultipleForSource { .. }
                    | GuidIdentity::ClaimedByMultipleSources { .. } => [0; 16],
                };
                (guid, external_type_id)
            }
        };
        let identifier = FileIdentifier::new(guid, type_id, path);
        if !self.binary_external_identity_matches(
            owner.source(),
            target.source(),
            &identifier,
            budget,
        )? {
            return Err(ReferenceEncodingError::BinaryExternalIdentityNotUnique {
                target_source: target.source(),
            });
        }
        Ok(ReferenceDestination::BinaryExternal {
            path_id,
            identifier,
        })
    }

    fn binary_external_identity_matches(
        &self,
        owner_source: SourceId,
        target_source: SourceId,
        identifier: &FileIdentifier,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, ReferenceEncodingError> {
        let guid = (identifier.guid != [0; 16]).then_some(identifier.guid);
        Ok(self.identity.binary_external_resolves_to(
            owner_source,
            guid,
            &identifier.path,
            target_source,
            budget,
        )?)
    }

    fn binary_external_encoding(
        &self,
        source: SourceId,
    ) -> Result<ExternalEncoding, ReferenceEncodingError> {
        self.binary_external_encodings
            .binary_search_by_key(&source, |(candidate, _)| *candidate)
            .ok()
            .map(|index| self.binary_external_encodings[index].1)
            .ok_or(ReferenceEncodingError::OwnerSerializedFormatMissing { source_id: source })
    }

    fn encode_yaml(
        &self,
        owner: &ObjectId,
        target: &ObjectId,
        hint: ReferenceEncodingHint,
    ) -> Result<ReferenceDestination, ReferenceEncodingError> {
        let file_id = yaml_file_id(target)?;
        if target.source() == owner.source() {
            return Ok(ReferenceDestination::YamlLocal { file_id });
        }

        let guid = match self.guid_identity(target.source()) {
            GuidIdentity::Unique(guid) => guid,
            GuidIdentity::Missing => {
                return Err(ReferenceEncodingError::TargetMetaGuidMissing {
                    source_id: target.source(),
                });
            }
            GuidIdentity::Zero => {
                return Err(ReferenceEncodingError::TargetMetaGuidIsZero {
                    source_id: target.source(),
                });
            }
            GuidIdentity::MultipleForSource { claims } => {
                return Err(ReferenceEncodingError::TargetMetaGuidAmbiguous {
                    source_id: target.source(),
                    claims,
                });
            }
            GuidIdentity::ClaimedByMultipleSources { guid, claims } => {
                return Err(ReferenceEncodingError::MetaGuidClaimAmbiguous { guid, claims });
            }
        };
        let type_id = match hint {
            ReferenceEncodingHint::Yaml { type_id } => {
                type_id.unwrap_or(DEFAULT_YAML_EXTERNAL_TYPE_ID)
            }
            ReferenceEncodingHint::Binary { .. } => {
                return Err(ReferenceEncodingError::HintFormatMismatch {
                    expected: ReferenceFormat::Yaml,
                    actual: ReferenceFormat::Binary,
                });
            }
        };
        Ok(ReferenceDestination::YamlExternal {
            file_id,
            guid,
            type_id,
        })
    }

    fn guid_identity(&self, source: SourceId) -> GuidIdentity {
        let mut source_guids = self.identity.source_guids(source);
        let Some(guid) = source_guids.next() else {
            return GuidIdentity::Missing;
        };
        let source_claims = 1_usize.saturating_add(source_guids.count());
        if source_claims != 1 {
            return GuidIdentity::MultipleForSource {
                claims: source_claims,
            };
        }
        if guid == [0; 16] {
            return GuidIdentity::Zero;
        }
        let mut guid_sources = self.identity.guid_sources(guid);
        let first = guid_sources.next();
        let claim_count = usize::from(first.is_some()).saturating_add(guid_sources.count());
        if first != Some(source) || claim_count != 1 {
            return GuidIdentity::ClaimedByMultipleSources {
                guid,
                claims: claim_count,
            };
        }
        GuidIdentity::Unique(guid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuidIdentity {
    Missing,
    Zero,
    MultipleForSource { claims: usize },
    ClaimedByMultipleSources { guid: [u8; 16], claims: usize },
    Unique([u8; 16]),
}

fn reference_format(object: &ObjectId) -> Result<ReferenceFormat, ReferenceEncodingError> {
    match object.source().kind() {
        SourceKind::SerializedFile => Ok(ReferenceFormat::Binary),
        SourceKind::Yaml => Ok(ReferenceFormat::Yaml),
        actual => Err(ReferenceEncodingError::UnsupportedOwnerKind { actual }),
    }
}

fn yaml_file_id(target: &ObjectId) -> Result<i64, ReferenceEncodingError> {
    if let Some(path_id) = target.binary_path_id() {
        return Ok(path_id);
    }
    let Some(file_id) = target.yaml_file_id() else {
        return Err(ReferenceEncodingError::YamlDocumentHasNoFileId);
    };
    Ok(file_id.get())
}

fn external_path(
    locator: &unity_asset_core::SourceLocator,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceEncodingError> {
    let path = locator.members().last().map_or_else(
        || locator.root_alias().as_str(),
        |step| step.member().name(),
    );
    Ok(clone_string(
        path,
        "reference destination external path",
        budget,
    )?)
}

fn require_resolved_target<T>(lookup: WorkspaceLookup<T>) -> Result<T, ReferenceEncodingError> {
    match lookup {
        WorkspaceLookup::Resolved(target) => Ok(target),
        WorkspaceLookup::Unloaded => Err(ReferenceEncodingError::TargetUnloaded),
        WorkspaceLookup::Missing => Err(ReferenceEncodingError::TargetMissing),
        WorkspaceLookup::Ambiguous { candidates } => Err(ReferenceEncodingError::TargetAmbiguous {
            candidates: candidates.len(),
        }),
        WorkspaceLookup::Invalid { .. } => Err(ReferenceEncodingError::TargetInvalid),
    }
}

#[derive(Debug, Error)]
pub(crate) enum ReferenceEncodingError {
    #[error(transparent)]
    Graph(#[from] ReferenceGraphError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("reference destination encoder belongs to workspace {expected:?}, not {actual:?}")]
    ViewWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("reference destination encoder belongs to revision {expected}, not {actual}")]
    ViewRevisionMismatch {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error("resolved reference target belongs to workspace {actual:?}, not {expected:?}")]
    TargetWorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("resolved reference target belongs to revision {actual}, not {expected}")]
    TargetRevisionMismatch {
        expected: WorkspaceRevision,
        actual: WorkspaceRevision,
    },
    #[error(
        "{kind:?} reference owner from source {source_id:?} is absent from this workspace revision"
    )]
    OwnerMissing {
        source_id: SourceId,
        kind: ObjectKind,
    },
    #[error("logical reference target resolves to an unloaded source")]
    TargetUnloaded,
    #[error("logical reference target is missing")]
    TargetMissing,
    #[error("logical reference target is ambiguous across {candidates} objects")]
    TargetAmbiguous { candidates: usize },
    #[error("logical reference target is invalid")]
    TargetInvalid,
    #[error("resolved reference target is absent from the frozen identity catalog")]
    TargetMissingFromIdentityCatalog,
    #[error("binary owner source {source_id:?} has no frozen SerializedFile format")]
    OwnerSerializedFormatMissing { source_id: SourceId },
    #[error(
        "binary external GUID and path do not uniquely identify target source {target_source:?}"
    )]
    BinaryExternalIdentityNotUnique { target_source: SourceId },
    #[error("reference identity catalog contains duplicate object identities")]
    DuplicateObjectIdentity,
    #[error("object source kind {actual:?} cannot own a reference")]
    UnsupportedOwnerKind { actual: SourceKind },
    #[error("reference owner format is {actual}, expected {expected}")]
    OwnerFormatMismatch {
        expected: ReferenceFormat,
        actual: ReferenceFormat,
    },
    #[error("{owner:?} reference cannot target a {target:?} object")]
    TargetKindMismatch {
        owner: ObjectKind,
        target: ObjectKind,
    },
    #[error("reference encoding hint format is {actual}, expected {expected}")]
    HintFormatMismatch {
        expected: ReferenceFormat,
        actual: ReferenceFormat,
    },
    #[error("YAML document ordinals cannot be encoded as PPtr file IDs")]
    YamlDocumentHasNoFileId,
    #[error("external YAML target source {source_id:?} has no matching .meta GUID")]
    TargetMetaGuidMissing { source_id: SourceId },
    #[error("external YAML target source {source_id:?} has an all-zero .meta GUID")]
    TargetMetaGuidIsZero { source_id: SourceId },
    #[error("external YAML target source {source_id:?} has {claims} distinct .meta GUID claims")]
    TargetMetaGuidAmbiguous { source_id: SourceId, claims: usize },
    #[error("external YAML target GUID {guid:02x?} is claimed by {claims} loaded sources")]
    MetaGuidClaimAmbiguous { guid: [u8; 16], claims: usize },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Write};
    use std::path::Path;

    use indexmap::IndexMap;
    use unity_asset_core::{
        AssetLoadLimits, ContainmentKind, Diagnostic, DiagnosticSeverity, DigestV1, ObjectAddress,
        SourceLocator, SourceMemberId, UnityClass, UnityValue, WorkspaceId, WorkspaceRevision,
        YamlFileId,
    };
    use unity_asset_yaml::{YamlReferenceShape, scan_reference_class_occurrences};
    use zip::{CompressionMethod, ZipWriter, write::FileOptions};

    use super::*;
    use crate::reference::fact::{BinaryExternalReference, ReferenceGuid};
    use crate::workspace::{AssetWorkspace, WorkspaceSnapshot};

    const TRANSFORM_BINARY: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
    );
    const LEGACY_V2_BINARY: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v2.assets.bin"
    );

    #[test]
    fn yaml_destinations_cover_null_local_external_and_type_preservation() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner.prefab");
        let target_path = directory.path().join("target.prefab");
        let target_meta_path = directory.path().join("target.prefab.meta");
        write_yaml(&owner_path, &[(1, "Owner"), (2, "Local")]);
        write_yaml(&target_path, &[(3, "External")]);
        write_meta(&target_meta_path, "11111111111111111111111111111111");

        let mut workspace = deterministic_workspace(1);
        let owner_source = load(&mut workspace, &owner_path);
        let target_source = load(&mut workspace, &target_path);
        load(&mut workspace, &target_meta_path);
        let snapshot = workspace.snapshot();
        let owner = RevisionedObjectHandle::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            ObjectId::yaml(owner_source, YamlFileId::new(1).unwrap()).unwrap(),
        )
        .unwrap();
        let local = target(&snapshot, owner_source, ObjectKind::Yaml, 2);
        let external = target(&snapshot, target_source, ObjectKind::Yaml, 3);
        let mut budget = AssetLoadBudget::default();
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();

        assert!(
            encoder
                .yaml_current_matches(&snapshot, &owner, &local, 2, None, &mut budget,)
                .unwrap()
        );
        assert!(
            !encoder
                .yaml_current_matches(&snapshot, &owner, &external, 2, None, &mut budget,)
                .unwrap()
        );
        assert!(
            encoder
                .yaml_current_matches(
                    &snapshot,
                    &owner,
                    &external,
                    3,
                    Some([0x11; 16]),
                    &mut budget,
                )
                .unwrap()
        );

        assert_eq!(
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &ReferenceTarget::Null,
                    ReferenceEncodingHint::yaml(None),
                    &mut budget,
                )
                .unwrap(),
            ReferenceDestination::Null,
        );
        assert_eq!(
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &local,
                    ReferenceEncodingHint::yaml(None),
                    &mut budget,
                )
                .unwrap(),
            ReferenceDestination::YamlLocal { file_id: 2 },
        );
        assert_eq!(
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &external,
                    ReferenceEncodingHint::yaml(None),
                    &mut budget,
                )
                .unwrap(),
            ReferenceDestination::YamlExternal {
                file_id: 3,
                guid: [0x11; 16],
                type_id: DEFAULT_YAML_EXTERNAL_TYPE_ID,
            },
        );
        let class = UnityClass::with_properties(
            1,
            "GameObject".to_owned(),
            "1".to_owned(),
            IndexMap::from([(
                "m_Target".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("fileID".to_owned(), UnityValue::Integer(3)),
                    (
                        "guid".to_owned(),
                        UnityValue::String("11111111111111111111111111111111".to_owned()),
                    ),
                    (
                        "type".to_owned(),
                        UnityValue::Integer(DEFAULT_YAML_EXTERNAL_TYPE_ID),
                    ),
                ])),
            )]),
        );
        let scan =
            scan_reference_class_occurrences(1, |_| Some(&class), &mut AssetLoadBudget::default())
                .unwrap();
        assert!(matches!(
            scan.occurrences.as_slice(),
            [occurrence]
                if matches!(
                    &occurrence.shape,
                    YamlReferenceShape::Valid(target)
                        if target.file_id == 3
                            && target.type_id == Some(DEFAULT_YAML_EXTERNAL_TYPE_ID)
                )
        ));

        let existing = RawReferenceTarget::Yaml {
            file_id: Some(99),
            guid: Some(ReferenceGuid::Parsed([0x22; 16])),
            type_id: Some(17),
        };
        assert!(matches!(
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &external,
                    ReferenceEncodingHint::from(&existing),
                    &mut budget,
                )
                .unwrap(),
            ReferenceDestination::YamlExternal { type_id: 17, .. }
        ));

        let missing = ReferenceTarget::object(
            ObjectAddress::yaml(
                locator(&snapshot, owner_source),
                YamlFileId::new(999).unwrap(),
            )
            .unwrap(),
        );
        assert!(matches!(
            encoder.encode(
                &snapshot,
                &owner,
                &missing,
                ReferenceEncodingHint::yaml(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::TargetMissing)
        ));
        let unloaded = ReferenceTarget::object(
            ObjectAddress::yaml(
                SourceLocator::path("unloaded.prefab").unwrap(),
                YamlFileId::new(1).unwrap(),
            )
            .unwrap(),
        );
        assert!(matches!(
            encoder.encode(
                &snapshot,
                &owner,
                &unloaded,
                ReferenceEncodingHint::yaml(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::TargetUnloaded)
        ));
    }

    #[test]
    fn yaml_external_rejects_missing_zero_ambiguous_and_ordinal_identities() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner.prefab");
        let missing_path = directory.path().join("missing.prefab");
        let zero_path = directory.path().join("zero.prefab");
        let alpha_path = directory.path().join("alpha.prefab");
        let first_path = directory.path().join("first.prefab");
        let second_path = directory.path().join("second.prefab");
        write_yaml(&owner_path, &[(1, "Owner")]);
        write_yaml(&missing_path, &[(2, "MissingGuid")]);
        write_yaml(&zero_path, &[(3, "ZeroGuid")]);
        fs::write(&alpha_path, "name: Alpha\n").unwrap();
        write_yaml(&first_path, &[(4, "First")]);
        write_yaml(&second_path, &[(5, "Second")]);
        write_meta(
            &directory.path().join("zero.prefab.meta"),
            "00000000000000000000000000000000",
        );
        write_meta(
            &directory.path().join("alpha.prefab.meta"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        write_meta(
            &directory.path().join("first.prefab.meta"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        write_meta(
            &directory.path().join("second.prefab.meta"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );

        let mut workspace = deterministic_workspace(2);
        let owner_source = load(&mut workspace, &owner_path);
        let missing_source = load(&mut workspace, &missing_path);
        let zero_source = load(&mut workspace, &zero_path);
        let alpha_source = load(&mut workspace, &alpha_path);
        let first_source = load(&mut workspace, &first_path);
        load(&mut workspace, &second_path);
        for meta in [
            "zero.prefab.meta",
            "alpha.prefab.meta",
            "first.prefab.meta",
            "second.prefab.meta",
        ] {
            load(&mut workspace, &directory.path().join(meta));
        }
        let snapshot = workspace.snapshot();
        let owner = RevisionedObjectHandle::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            ObjectId::yaml(owner_source, YamlFileId::new(1).unwrap()).unwrap(),
        )
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();

        let cases = [
            (target(&snapshot, missing_source, ObjectKind::Yaml, 2), 0_u8),
            (target(&snapshot, zero_source, ObjectKind::Yaml, 3), 1_u8),
            (
                ReferenceTarget::object(
                    ObjectAddress::yaml_document(locator(&snapshot, alpha_source), 0).unwrap(),
                ),
                2_u8,
            ),
            (target(&snapshot, first_source, ObjectKind::Yaml, 4), 3_u8),
        ];
        for (target, expected) in cases {
            let error = encoder
                .encode(
                    &snapshot,
                    &owner,
                    &target,
                    ReferenceEncodingHint::yaml(None),
                    &mut budget,
                )
                .unwrap_err();
            let matches_expected = match expected {
                0 => matches!(&error, ReferenceEncodingError::TargetMetaGuidMissing { .. }),
                1 => matches!(&error, ReferenceEncodingError::TargetMetaGuidIsZero { .. }),
                2 => matches!(&error, ReferenceEncodingError::YamlDocumentHasNoFileId),
                3 => matches!(
                    &error,
                    ReferenceEncodingError::MetaGuidClaimAmbiguous { claims: 2, .. }
                ),
                _ => false,
            };
            assert!(
                matches_expected,
                "unexpected error for case {expected}: {error:?}"
            );
        }
    }

    #[test]
    fn binary_destination_emits_file_identifier_and_rejects_yaml_target() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner.assets");
        let target_path = directory.path().join("target.assets");
        let yaml_path = directory.path().join("target.prefab");
        fs::write(&owner_path, TRANSFORM_BINARY).unwrap();
        fs::write(&target_path, TRANSFORM_BINARY).unwrap();
        write_yaml(&yaml_path, &[(7, "Yaml")]);

        let mut workspace = deterministic_workspace(3);
        let owner_source = load(&mut workspace, &owner_path);
        let target_source = load(&mut workspace, &target_path);
        let yaml_source = load(&mut workspace, &yaml_path);
        let snapshot = workspace.snapshot();
        let mut budget = AssetLoadBudget::default();
        let owner_objects = source_objects(&snapshot, owner_source, &mut budget);
        assert!(owner_objects.len() >= 2);
        let owner = RevisionedObjectHandle::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            owner_objects[0].clone(),
        )
        .unwrap();
        let local_target =
            ReferenceTarget::object(address(&snapshot, &owner_objects[1], &mut budget));
        let external_object = source_objects(&snapshot, target_source, &mut budget)
            .into_iter()
            .next()
            .unwrap();
        let external_target =
            ReferenceTarget::object(address(&snapshot, &external_object, &mut budget));
        let yaml_target = target(&snapshot, yaml_source, ObjectKind::Yaml, 7);
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();

        assert!(matches!(
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &local_target,
                    ReferenceEncodingHint::binary(None),
                    &mut budget,
                )
                .unwrap(),
            ReferenceDestination::BinaryLocal { .. }
        ));
        let existing = RawReferenceTarget::Binary {
            file_id: 1,
            path_id: 1,
            external: Some(BinaryExternalReference::new(
                0,
                [0; 16],
                3,
                "old.assets".to_owned(),
            )),
        };
        let destination = encoder
            .encode(
                &snapshot,
                &owner,
                &external_target,
                ReferenceEncodingHint::from(&existing),
                &mut budget,
            )
            .unwrap();
        assert_eq!(
            destination,
            encoder
                .encode(
                    &snapshot,
                    &owner,
                    &external_target,
                    ReferenceEncodingHint::from(&existing),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap()
        );
        let ReferenceDestination::BinaryExternal {
            path_id,
            identifier,
        } = destination
        else {
            panic!("expected an external binary destination");
        };
        assert_eq!(path_id, external_object.binary_path_id().unwrap());
        assert_eq!(identifier.type_, 3);
        assert_eq!(identifier.guid, [0; 16]);
        assert!(identifier.path.ends_with("target.assets"));
        assert!(
            encoder
                .binary_current_matches(
                    &snapshot,
                    &owner,
                    &external_target,
                    1,
                    path_id,
                    Some(&identifier),
                    &mut budget,
                )
                .unwrap()
        );
        assert!(
            !encoder
                .binary_current_matches(
                    &snapshot,
                    &owner,
                    &local_target,
                    1,
                    path_id,
                    Some(&identifier),
                    &mut budget,
                )
                .unwrap()
        );

        let mut no_output_bytes = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            encoder.encode(
                &snapshot,
                &owner,
                &external_target,
                ReferenceEncodingHint::from(&existing),
                &mut no_output_bytes,
            ),
            Err(ReferenceEncodingError::Graph(ReferenceGraphError::Budget(
                _
            )))
        ));

        assert!(matches!(
            encoder.encode(
                &snapshot,
                &owner,
                &yaml_target,
                ReferenceEncodingHint::binary(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::TargetKindMismatch {
                owner: ObjectKind::Binary,
                target: ObjectKind::Yaml,
            })
        ));
    }

    #[test]
    fn binary_destination_reconciles_guid_and_duplicate_basename_before_emission() {
        const FIRST_GUID: [u8; 16] = [0x11; 16];
        const SECOND_GUID: [u8; 16] = [0x22; 16];
        const FIRST_META: &[u8] = b"fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n";
        const SECOND_META: &[u8] =
            b"fileFormatVersion: 2\nguid: 22222222222222222222222222222222\n";

        let directory = tempfile::tempdir().unwrap();
        let root_owner_path = directory.path().join("root-owner.assets");
        let first_archive_path = directory.path().join("first.zip");
        let second_archive_path = directory.path().join("second.zip");
        fs::write(&root_owner_path, TRANSFORM_BINARY).unwrap();
        fs::write(
            &first_archive_path,
            fixture_archive(&[
                ("owner.assets", TRANSFORM_BINARY),
                ("target.assets", TRANSFORM_BINARY),
                ("target.assets.meta", FIRST_META),
            ]),
        )
        .unwrap();
        fs::write(
            &second_archive_path,
            fixture_archive(&[
                ("target.assets", TRANSFORM_BINARY),
                ("target.assets.meta", SECOND_META),
            ]),
        )
        .unwrap();

        let mut workspace = deterministic_workspace(7);
        let root_owner_source = load(&mut workspace, &root_owner_path);
        load(&mut workspace, &first_archive_path);
        load(&mut workspace, &second_archive_path);
        let snapshot = workspace.snapshot();
        let first_owner_source = archive_member_source(&snapshot, "first.zip", "owner.assets");
        let first_target_source = archive_member_source(&snapshot, "first.zip", "target.assets");
        let second_target_source = archive_member_source(&snapshot, "second.zip", "target.assets");
        let mut budget = AssetLoadBudget::default();
        let root_owner = first_revisioned_object(&snapshot, root_owner_source, &mut budget);
        let first_owner = first_revisioned_object(&snapshot, first_owner_source, &mut budget);
        let first_target_object = source_objects(&snapshot, first_target_source, &mut budget)
            .into_iter()
            .next()
            .unwrap();
        let second_target_object = source_objects(&snapshot, second_target_source, &mut budget)
            .into_iter()
            .next()
            .unwrap();
        let first_target =
            ReferenceTarget::object(address(&snapshot, &first_target_object, &mut budget));
        let second_target =
            ReferenceTarget::object(address(&snapshot, &second_target_object, &mut budget));
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();

        let ReferenceDestination::BinaryExternal {
            path_id,
            identifier,
        } = encoder
            .encode(
                &snapshot,
                &root_owner,
                &second_target,
                ReferenceEncodingHint::binary(Some(3)),
                &mut budget,
            )
            .unwrap()
        else {
            panic!("cross-archive target must encode as an external reference");
        };
        assert_eq!(identifier.guid, SECOND_GUID);
        assert_eq!(identifier.path, "target.assets");
        assert!(
            encoder
                .binary_current_matches(
                    &snapshot,
                    &root_owner,
                    &second_target,
                    1,
                    path_id,
                    Some(&identifier),
                    &mut budget,
                )
                .unwrap()
        );
        assert!(
            !encoder
                .binary_current_matches(
                    &snapshot,
                    &root_owner,
                    &first_target,
                    1,
                    path_id,
                    Some(&identifier),
                    &mut budget,
                )
                .unwrap()
        );

        let error = encoder
            .encode(
                &snapshot,
                &first_owner,
                &second_target,
                ReferenceEncodingHint::binary(Some(3)),
                &mut budget,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ReferenceEncodingError::BinaryExternalIdentityNotUnique { target_source }
                if target_source == second_target_source
        ));

        let mut first_identifier = identifier;
        first_identifier.guid = FIRST_GUID;
        assert!(
            encoder
                .binary_current_matches(
                    &snapshot,
                    &root_owner,
                    &first_target,
                    1,
                    first_target_object.binary_path_id().unwrap(),
                    Some(&first_identifier),
                    &mut budget,
                )
                .unwrap()
        );
    }

    #[test]
    fn legacy_binary_destination_uses_path_only_external_encoding() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("legacy-owner.assets");
        let target_path = directory.path().join("legacy-target.assets");
        fs::write(&owner_path, LEGACY_V2_BINARY).unwrap();
        fs::write(&target_path, LEGACY_V2_BINARY).unwrap();

        let mut workspace = deterministic_workspace(6);
        let owner_source = load(&mut workspace, &owner_path);
        let target_source = load(&mut workspace, &target_path);
        let snapshot = workspace.snapshot();
        let mut budget = AssetLoadBudget::default();
        let owner_object = source_objects(&snapshot, owner_source, &mut budget)
            .into_iter()
            .next()
            .unwrap();
        let target_object = source_objects(&snapshot, target_source, &mut budget)
            .into_iter()
            .next()
            .unwrap();
        let owner =
            RevisionedObjectHandle::new(snapshot.workspace_id(), snapshot.revision(), owner_object)
                .unwrap();
        let target = ReferenceTarget::object(address(&snapshot, &target_object, &mut budget));
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();

        let ReferenceDestination::BinaryExternal { identifier, .. } = encoder
            .encode(
                &snapshot,
                &owner,
                &target,
                ReferenceEncodingHint::binary(Some(19)),
                &mut budget,
            )
            .unwrap()
        else {
            panic!("legacy cross-file target must encode as an external reference");
        };
        assert_eq!(identifier.guid, [0; 16]);
        assert_eq!(identifier.type_, 0);
        assert!(identifier.temp_empty.is_empty());
        assert!(identifier.path.ends_with("legacy-target.assets"));
    }

    #[test]
    fn encoder_rejects_workspace_revision_format_and_lookup_state_mismatches() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner.prefab");
        let later_path = directory.path().join("later.prefab");
        write_yaml(&owner_path, &[(1, "Owner")]);
        write_yaml(&later_path, &[(2, "Later")]);

        let mut workspace = deterministic_workspace(4);
        let owner_source = load(&mut workspace, &owner_path);
        let snapshot = workspace.snapshot();
        let owner = RevisionedObjectHandle::new(
            snapshot.workspace_id(),
            snapshot.revision(),
            ObjectId::yaml(owner_source, YamlFileId::new(1).unwrap()).unwrap(),
        )
        .unwrap();
        let mut budget = AssetLoadBudget::default();
        let encoder = ReferenceDestinationEncoder::build(&snapshot, &mut budget).unwrap();
        let mut no_scan_entries = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        no_scan_entries.consume_entries(1).unwrap();
        assert!(matches!(
            ReferenceDestinationEncoder::build(&snapshot, &mut no_scan_entries),
            Err(ReferenceEncodingError::Graph(ReferenceGraphError::Budget(
                _
            )))
        ));
        assert_eq!(no_scan_entries.usage().entries, 1);
        let wrong_format = RawReferenceTarget::Binary {
            file_id: 0,
            path_id: 0,
            external: None,
        };
        assert!(matches!(
            encoder.encode(
                &snapshot,
                &owner,
                &ReferenceTarget::Null,
                ReferenceEncodingHint::from(&wrong_format),
                &mut budget,
            ),
            Err(ReferenceEncodingError::HintFormatMismatch { .. })
        ));

        let stale_owner = owner
            .clone()
            .with_revision(WorkspaceRevision::new(DigestV1::hash_bytes(b"stale owner")));
        assert!(matches!(
            encoder.encode(
                &snapshot,
                &stale_owner,
                &ReferenceTarget::Null,
                ReferenceEncodingHint::yaml(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::Contract(
                ContractError::RevisionMismatch { .. }
            ))
        ));

        let mut other_workspace = deterministic_workspace(5);
        load(&mut other_workspace, &owner_path);
        let other_snapshot = other_workspace.snapshot();
        assert!(matches!(
            encoder.encode(
                &other_snapshot,
                &owner,
                &ReferenceTarget::Null,
                ReferenceEncodingHint::yaml(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::ViewWorkspaceMismatch { .. })
        ));

        load(&mut workspace, &later_path);
        let later_snapshot = workspace.snapshot();
        assert!(matches!(
            encoder.encode(
                &later_snapshot,
                &owner,
                &ReferenceTarget::Null,
                ReferenceEncodingHint::yaml(None),
                &mut budget,
            ),
            Err(ReferenceEncodingError::ViewRevisionMismatch { .. })
        ));

        assert!(matches!(
            require_resolved_target::<u8>(WorkspaceLookup::Ambiguous {
                candidates: vec![1, 2]
            }),
            Err(ReferenceEncodingError::TargetAmbiguous { candidates: 2 })
        ));
        assert!(matches!(
            require_resolved_target::<u8>(WorkspaceLookup::Missing),
            Err(ReferenceEncodingError::TargetMissing)
        ));
        assert!(matches!(
            require_resolved_target::<u8>(WorkspaceLookup::Unloaded),
            Err(ReferenceEncodingError::TargetUnloaded)
        ));
        let diagnostic =
            Diagnostic::new(DiagnosticSeverity::Error, "INVALID", "invalid target").unwrap();
        assert!(matches!(
            require_resolved_target::<u8>(WorkspaceLookup::Invalid { diagnostic }),
            Err(ReferenceEncodingError::TargetInvalid)
        ));
    }

    fn deterministic_workspace(id: u128) -> AssetWorkspace {
        AssetWorkspace::with_workspace_id(
            WorkspaceId::from_u128(id).unwrap(),
            crate::workspace::WorkspaceOptions::default(),
        )
        .unwrap()
    }

    fn load(workspace: &mut AssetWorkspace, path: &Path) -> SourceId {
        workspace
            .load_path(path, &mut AssetLoadBudget::default())
            .unwrap()
    }

    fn locator(snapshot: &WorkspaceSnapshot, source: SourceId) -> SourceLocator {
        match snapshot
            .source(source, &mut AssetLoadBudget::default())
            .unwrap()
        {
            WorkspaceLookup::Resolved(source) => source.locator().clone(),
            other => panic!("source lookup did not resolve: {other:?}"),
        }
    }

    fn target(
        snapshot: &WorkspaceSnapshot,
        source: SourceId,
        kind: ObjectKind,
        id: i64,
    ) -> ReferenceTarget {
        let locator = locator(snapshot, source);
        ReferenceTarget::object(match kind {
            ObjectKind::Binary => ObjectAddress::binary_at(locator, id).unwrap(),
            ObjectKind::Yaml => ObjectAddress::yaml(locator, YamlFileId::new(id).unwrap()).unwrap(),
        })
    }

    fn source_objects(
        snapshot: &WorkspaceSnapshot,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Vec<ObjectId> {
        snapshot
            .objects(budget)
            .unwrap()
            .into_iter()
            .filter(|object| object.object().source() == source)
            .map(RevisionedObjectHandle::into_object)
            .collect()
    }

    fn address(
        snapshot: &WorkspaceSnapshot,
        object: &ObjectId,
        budget: &mut AssetLoadBudget,
    ) -> ObjectAddress {
        let locator = locator(snapshot, object.source());
        let address = match object.kind() {
            ObjectKind::Binary => {
                ObjectAddress::binary_at(locator, object.binary_path_id().unwrap()).unwrap()
            }
            ObjectKind::Yaml => {
                ObjectAddress::yaml(locator, object.yaml_file_id().unwrap()).unwrap()
            }
        };
        assert!(matches!(
            snapshot.resolve_object(&address, budget).unwrap(),
            WorkspaceLookup::Resolved(_)
        ));
        address
    }

    fn first_revisioned_object(
        snapshot: &WorkspaceSnapshot,
        source: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> RevisionedObjectHandle {
        let object = source_objects(snapshot, source, budget)
            .into_iter()
            .next()
            .unwrap();
        RevisionedObjectHandle::new(snapshot.workspace_id(), snapshot.revision(), object).unwrap()
    }

    fn archive_member_source(
        snapshot: &WorkspaceSnapshot,
        archive: &str,
        member: &str,
    ) -> SourceId {
        let locator = SourceLocator::path(archive)
            .unwrap()
            .child(
                ContainmentKind::Archive,
                SourceMemberId::with_occurrence(member, 0).unwrap(),
            )
            .unwrap();
        match snapshot
            .resolve_source(&locator, &mut AssetLoadBudget::default())
            .unwrap()
        {
            WorkspaceLookup::Resolved(source) => source.id(),
            other => panic!("archive member source did not resolve: {other:?}"),
        }
    }

    fn fixture_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, payload) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(payload).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn write_yaml(path: &Path, documents: &[(i64, &str)]) {
        let mut contents = String::from("%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n");
        for (anchor, name) in documents {
            contents.push_str(&format!(
                "--- !u!1 &{anchor}\nGameObject:\n  m_Name: {name}\n"
            ));
        }
        fs::write(path, contents).unwrap();
    }

    fn write_meta(path: &Path, guid: &str) {
        fs::write(path, format!("fileFormatVersion: 2\nguid: {guid}\n")).unwrap();
    }
}
