use std::fmt::Write as _;
use std::mem::size_of;

use unity_asset_core::{
    AssetLoadBudget, BudgetError, Diagnostic, DiagnosticSeverity, FieldPath, FieldPathSegment,
    ObjectId, RevisionedObjectHandle, SourceId, SourceKind, UnityDocument, WorkspaceId,
    WorkspaceRevision,
};

use super::ReferenceGraphError;
use super::fact::{
    BinaryExternalReference, RawReferenceTarget, ReferenceGuid, ReferenceResolution,
};
use super::input::{ReferenceInput, ReferenceSource};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PathClaim {
    key: String,
    source: SourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DescribablePathClaim {
    parent: Option<SourceId>,
    key: String,
    same_name_occurrence: Option<u32>,
    source: SourceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GuidClaim {
    guid: [u8; 16],
    source: SourceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceGuidClaim {
    source: SourceId,
    guid: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SourceParent {
    source: SourceId,
    parent: Option<SourceId>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MetaGuidClaim {
    described_path: String,
    parent: Option<SourceId>,
    same_name_occurrence: Option<u32>,
    guid: [u8; 16],
}

pub(crate) struct ResolutionCatalog<'input, I: ReferenceInput + ?Sized> {
    input: &'input I,
    nodes: &'input [RevisionedObjectHandle],
    identity: ResolutionIdentityIndex,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
}

/// Bidirectional, allocation-accounted source identity index shared by reference decoding and
/// mutation-time destination encoding.
pub(super) struct ResolutionIdentityIndex {
    exact_paths: Vec<PathClaim>,
    basenames: Vec<PathClaim>,
    guids: Vec<GuidClaim>,
    source_guids: Vec<SourceGuidClaim>,
    source_parents: Vec<SourceParent>,
}

impl ResolutionIdentityIndex {
    pub(super) fn build(
        sources: &[ReferenceSource<'_>],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceGraphError> {
        budget.consume_entries(usize_to_u64(
            sources.len(),
            "reference source identity scan",
        )?)?;
        let mut exact_paths = Vec::new();
        let mut basenames = Vec::new();
        let mut describable_paths = Vec::new();
        let mut meta_claims = Vec::new();
        let source_count = sources.len();
        let mut source_parents =
            reserve_vec(source_count, "reference source parent claims", budget)?;

        for source in sources.iter().copied() {
            let source_id = source.source();
            let locator = source.locator();
            let parent = source.parent();
            source_parents.push(SourceParent {
                source: source_id,
                parent,
            });
            let physical_path = source
                .physical_path()
                .and_then(|path| path.to_str())
                .map(|path| {
                    normalize_external_path(path, "reference describable physical paths", budget)
                })
                .transpose()?;

            let alias = normalize_external_path(
                locator.root_alias().as_str(),
                "reference exact source aliases",
                budget,
            )?;
            let alias_basename = external_basename(&alias)
                .map(|name| clone_string(name, "reference alias basenames", budget))
                .transpose()?;
            push_path_claim(
                &mut exact_paths,
                clone_string(&alias, "reference exact source aliases", budget)?,
                source_id,
                "reference exact source aliases",
                budget,
            )?;
            if let Some(name) = alias_basename {
                push_path_claim(
                    &mut basenames,
                    name,
                    source_id,
                    "reference alias basenames",
                    budget,
                )?;
            }
            let member_step = locator.members().last();
            let same_name_occurrence = member_step.map(|step| step.member().same_name_occurrence());
            let member = member_step
                .map(|member| {
                    normalize_external_path(member.name(), "reference exact member paths", budget)
                })
                .transpose()?;
            if let Some(member) = member.as_ref() {
                let member_basename = external_basename(member)
                    .map(|name| clone_string(name, "reference member basenames", budget))
                    .transpose()?;
                push_path_claim(
                    &mut exact_paths,
                    clone_string(member, "reference exact member paths", budget)?,
                    source_id,
                    "reference exact member paths",
                    budget,
                )?;
                if let Some(name) = member_basename {
                    push_path_claim(
                        &mut basenames,
                        name,
                        source_id,
                        "reference member basenames",
                        budget,
                    )?;
                }
            }

            let logical_path = member
                .as_deref()
                .or(physical_path.as_deref())
                .unwrap_or(&alias);
            push_value(
                &mut describable_paths,
                DescribablePathClaim {
                    parent,
                    key: clone_string(logical_path, "reference describable source paths", budget)?,
                    same_name_occurrence,
                    source: source_id,
                },
                "reference describable source paths",
                budget,
            )?;

            if source_id.kind() == SourceKind::Yaml
                && let Some(described) = logical_path.strip_suffix(".meta")
            {
                for guid in meta_guids(&source, budget)? {
                    push_value(
                        &mut meta_claims,
                        MetaGuidClaim {
                            described_path: clone_string(
                                described,
                                "reference meta described paths",
                                budget,
                            )?,
                            parent,
                            same_name_occurrence,
                            guid,
                        },
                        "reference meta GUID claims",
                        budget,
                    )?;
                }
            }
        }
        exact_paths.sort_unstable();
        exact_paths.dedup();
        basenames.sort_unstable();
        basenames.dedup();
        describable_paths.sort_unstable();
        describable_paths.dedup();
        meta_claims.sort_unstable();

        let mut guids = Vec::new();
        let mut source_guids = Vec::new();
        for meta in meta_claims {
            let candidates = describable_path_candidates(
                &describable_paths,
                meta.parent,
                &meta.described_path,
                meta.same_name_occurrence,
                budget,
            )?;
            for claim in candidates {
                push_value(
                    &mut guids,
                    GuidClaim {
                        guid: meta.guid,
                        source: claim.source,
                    },
                    "reference GUID claims",
                    budget,
                )?;
                push_value(
                    &mut source_guids,
                    SourceGuidClaim {
                        source: claim.source,
                        guid: meta.guid,
                    },
                    "reference source GUID claims",
                    budget,
                )?;
            }
        }
        guids.sort_unstable();
        guids.dedup();
        source_guids.sort_unstable();
        source_guids.dedup();
        source_parents.sort_unstable();
        if source_parents
            .windows(2)
            .any(|pair| pair[0].source == pair[1].source)
        {
            return Err(ReferenceGraphError::Invariant(
                "reference input exposed a duplicate object source",
            ));
        }

        Ok(Self {
            exact_paths,
            basenames,
            guids,
            source_guids,
            source_parents,
        })
    }

    pub(super) fn source_guids(&self, source: SourceId) -> impl Iterator<Item = [u8; 16]> + '_ {
        let start = self
            .source_guids
            .partition_point(|claim| claim.source < source);
        let end = self
            .source_guids
            .partition_point(|claim| claim.source <= source);
        self.source_guids[start..end].iter().map(|claim| claim.guid)
    }

    pub(super) fn guid_sources(&self, guid: [u8; 16]) -> impl Iterator<Item = SourceId> + '_ {
        let start = self.guids.partition_point(|claim| claim.guid < guid);
        let end = self.guids.partition_point(|claim| claim.guid <= guid);
        self.guids[start..end].iter().map(|claim| claim.source)
    }

    #[cfg(test)]
    pub(super) fn unique_path_source(
        &self,
        context: SourceId,
        raw_path: &str,
        kind: SourceKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<SourceId>, ReferenceGraphError> {
        let normalized =
            normalize_external_path(raw_path, "unique external reference lookup path", budget)?;
        let candidates = self.path_candidates(context, &normalized, kind, budget)?;
        Ok(match candidates.as_slice() {
            [source] => Some(*source),
            _ => None,
        })
    }

    /// Tests whether one binary external identity resolves uniquely to `expected`.
    ///
    /// This follows the same GUID/path reconciliation rules as graph construction without
    /// allocating a diagnostic or a temporary GUID candidate vector.
    pub(super) fn binary_external_resolves_to(
        &self,
        context: SourceId,
        raw_guid: Option<[u8; 16]>,
        raw_path: &str,
        expected: SourceId,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, ReferenceGraphError> {
        let normalized =
            normalize_external_path(raw_path, "external reference lookup path", budget)?;
        if raw_guid.is_none() && normalized.is_empty() {
            return Ok(false);
        }
        let path =
            self.path_candidates(context, &normalized, SourceKind::SerializedFile, budget)?;
        let Some(raw_guid) = raw_guid else {
            return Ok(path.as_slice() == [expected]);
        };

        let guid_start = self.guids.partition_point(|claim| claim.guid < raw_guid);
        let guid_end = self.guids.partition_point(|claim| claim.guid <= raw_guid);
        let guid = self.guids[guid_start..guid_end]
            .iter()
            .map(|claim| claim.source)
            .filter(|source| source.kind() == SourceKind::SerializedFile);
        let mut guid_count = 0_usize;
        let mut guid_is_expected = false;
        for source in guid {
            guid_count = guid_count.saturating_add(1);
            guid_is_expected |= source == expected;
        }

        if guid_count == 0 {
            let mut candidates = path.iter().copied().filter(|source| {
                !matches!(
                    source_guid_evidence(&self.source_guids, *source, raw_guid),
                    SourceGuidEvidence::Conflicting
                )
            });
            return Ok(candidates.next() == Some(expected) && candidates.next().is_none());
        }
        if path.is_empty() {
            return Ok(guid_count == 1 && guid_is_expected);
        }

        let mut intersection = self.guids[guid_start..guid_end]
            .iter()
            .map(|claim| claim.source)
            .filter(|source| source.kind() == SourceKind::SerializedFile)
            .filter(|source| path.binary_search(source).is_ok());
        Ok(intersection.next() == Some(expected) && intersection.next().is_none())
    }

    fn path_candidates(
        &self,
        context: SourceId,
        normalized: &str,
        kind: SourceKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, ReferenceGraphError> {
        let mut candidates = collect_path_claims(&self.exact_paths, normalized, kind, budget)?;
        if candidates.is_empty()
            && let Some(name) = external_basename(normalized)
        {
            candidates = collect_path_claims(&self.basenames, name, kind, budget)?;
        }
        if candidates.len() > 1 {
            let context_parent = self.source_parent(context)?;
            if context_parent.is_some() {
                let mut same_parent = reserve_vec(
                    candidates.len(),
                    "same-parent external path candidates",
                    budget,
                )?;
                for candidate in candidates.iter().copied() {
                    if self.source_parent(candidate)? == context_parent {
                        same_parent.push(candidate);
                    }
                }
                if !same_parent.is_empty() {
                    candidates = same_parent;
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        Ok(candidates)
    }

    fn source_parent(&self, source: SourceId) -> Result<Option<SourceId>, ReferenceGraphError> {
        let position = self
            .source_parents
            .binary_search_by_key(&source, |claim| claim.source)
            .map_err(|_| {
                ReferenceGraphError::Invariant(
                    "reference path candidate is absent from the source parent table",
                )
            })?;
        self.source_parents
            .get(position)
            .map(|claim| claim.parent)
            .ok_or(ReferenceGraphError::Invariant(
                "reference source parent ordinal is out of bounds",
            ))
    }
}

impl<'input, I: ReferenceInput + ?Sized> ResolutionCatalog<'input, I> {
    pub(crate) fn build(
        input: &'input I,
        sources: &[ReferenceSource<'_>],
        nodes: &'input [RevisionedObjectHandle],
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ReferenceGraphError> {
        Ok(Self {
            input,
            nodes,
            identity: ResolutionIdentityIndex::build(sources, budget)?,
            workspace: input.workspace_id(),
            revision: input.revision(),
        })
    }

    pub(crate) fn resolve(
        &self,
        source: &RevisionedObjectHandle,
        field_path: &FieldPath,
        raw_target: &RawReferenceTarget,
        invalid: Option<Diagnostic>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        source.validate_context(self.workspace, self.revision)?;
        if let Some(diagnostic) = invalid {
            return Ok(ReferenceResolution::Invalid { diagnostic });
        }
        match raw_target {
            RawReferenceTarget::Binary {
                file_id,
                path_id,
                external,
            } => self.resolve_binary(
                source,
                field_path,
                *file_id,
                *path_id,
                external.as_ref(),
                budget,
            ),
            RawReferenceTarget::Yaml {
                file_id,
                guid,
                type_id: _,
            } => self.resolve_yaml(source, field_path, *file_id, guid.as_ref(), budget),
        }
    }

    fn resolve_binary(
        &self,
        source: &RevisionedObjectHandle,
        field_path: &FieldPath,
        file_id: i32,
        path_id: i64,
        external: Option<&BinaryExternalReference>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        if path_id == 0 {
            return Ok(ReferenceResolution::Null);
        }
        if file_id < 0 {
            return self.invalid(
                source,
                field_path,
                "REFERENCE_NEGATIVE_BINARY_FILE_ID",
                "binary PPtr file ID is negative",
                budget,
            );
        }
        if file_id == 0 {
            return self.resolve_source_object(source.object().source(), path_id, budget);
        }
        let Some(external) = external else {
            return self.invalid(
                source,
                field_path,
                "REFERENCE_EXTERNAL_INDEX_OUT_OF_RANGE",
                "binary PPtr file ID is outside the external table",
                budget,
            );
        };

        let external_guid = external.guid();
        let external_path =
            normalize_external_path(external.path(), "external reference lookup path", budget)?;
        if external_guid.is_none() && external_path.is_empty() {
            return self.invalid(
                source,
                field_path,
                "REFERENCE_EXTERNAL_IDENTITY_MISSING",
                "binary external reference has neither a GUID nor a path",
                budget,
            );
        }
        let guid_candidates = external_guid
            .map(|guid| self.guid_candidates(guid, SourceKind::SerializedFile, budget))
            .transpose()?
            .unwrap_or_default();
        let path_candidates = self.path_candidates(
            source.object().source(),
            &external_path,
            SourceKind::SerializedFile,
            budget,
        )?;
        let candidates = reconcile_external_candidates(
            external_guid,
            guid_candidates,
            path_candidates,
            source,
            field_path,
            self,
            budget,
        )?;
        match candidates {
            ExternalCandidates::Candidates(candidates) => {
                self.resolve_candidates(candidates, path_id, budget)
            }
            ExternalCandidates::Invalid(diagnostic) => {
                Ok(ReferenceResolution::Invalid { diagnostic })
            }
        }
    }

    fn resolve_yaml(
        &self,
        source: &RevisionedObjectHandle,
        field_path: &FieldPath,
        file_id: Option<i64>,
        guid: Option<&ReferenceGuid>,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        let Some(file_id) = file_id else {
            return self.invalid(
                source,
                field_path,
                "YAML_REFERENCE_MISSING_FILE_ID",
                "YAML reference has no integer file ID",
                budget,
            );
        };
        if file_id == 0 {
            return Ok(ReferenceResolution::Null);
        }
        let Some(guid) = guid else {
            return self.resolve_yaml_object(source.object().source(), file_id, budget);
        };
        let Some(parsed) = guid.parsed() else {
            return self.invalid(
                source,
                field_path,
                "YAML_REFERENCE_INVALID_GUID",
                "YAML reference GUID is not 32 hexadecimal characters",
                budget,
            );
        };
        if parsed == [0; 16] {
            return self.invalid(
                source,
                field_path,
                "YAML_REFERENCE_EMPTY_GUID",
                "YAML external reference GUID is all zeroes",
                budget,
            );
        }
        let candidates = self.guid_candidates_any(parsed, budget)?;
        self.resolve_yaml_candidates(candidates, file_id, budget)
    }

    fn resolve_source_object(
        &self,
        source: SourceId,
        path_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        if source.kind() != SourceKind::SerializedFile {
            return Ok(ReferenceResolution::Invalid {
                diagnostic: diagnostic(
                    self.input,
                    None,
                    None,
                    "REFERENCE_SOURCE_KIND_MISMATCH",
                    clone_string(
                        "binary reference owner is not a SerializedFile",
                        "reference diagnostic message",
                        budget,
                    )?,
                    budget,
                )?,
            });
        }
        let object = ObjectId::binary(source, path_id)?;
        self.resolve_object(object, budget)
    }

    fn resolve_yaml_object(
        &self,
        source: SourceId,
        file_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        if source.kind() != SourceKind::Yaml {
            return Ok(ReferenceResolution::Invalid {
                diagnostic: diagnostic(
                    self.input,
                    None,
                    None,
                    "REFERENCE_SOURCE_KIND_MISMATCH",
                    clone_string(
                        "local YAML reference owner is not a YAML source",
                        "reference diagnostic message",
                        budget,
                    )?,
                    budget,
                )?,
            });
        }
        let object = ObjectId::yaml(source, decimal_i64(file_id, budget)?)?;
        self.resolve_object(object, budget)
    }

    fn resolve_candidates(
        &self,
        candidates: Vec<SourceId>,
        path_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        match candidates.as_slice() {
            [] => Ok(ReferenceResolution::Unloaded { source: None }),
            [source] => self.resolve_source_object(*source, path_id, budget),
            _ => self.ambiguous_binary(candidates, path_id, budget),
        }
    }

    fn resolve_yaml_candidates(
        &self,
        candidates: Vec<SourceId>,
        file_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        match candidates.as_slice() {
            [] => Ok(ReferenceResolution::Unloaded { source: None }),
            [source] if source.kind() == SourceKind::SerializedFile => {
                self.resolve_source_object(*source, file_id, budget)
            }
            [source] if source.kind() == SourceKind::Yaml => {
                self.resolve_yaml_object(*source, file_id, budget)
            }
            [_] => Ok(ReferenceResolution::Unloaded { source: None }),
            _ => self.ambiguous_yaml(candidates, file_id, budget),
        }
    }

    fn resolve_object(
        &self,
        object: ObjectId,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        if let Ok(position) = self
            .nodes
            .binary_search_by(|candidate| candidate.object().cmp(&object))
        {
            let handle = self
                .nodes
                .get(position)
                .ok_or(ReferenceGraphError::Invariant(
                    "resolved object index is out of bounds",
                ))?;
            budget.consume_bytes(usize_to_u64(
                handle.retained_clone_bytes(),
                "resolved reference handle",
            )?)?;
            return Ok(ReferenceResolution::Resolved(handle.clone()));
        }
        Ok(ReferenceResolution::Missing {
            target: Some(self.input.address_for_object(&object, budget)?),
        })
    }

    fn ambiguous_binary(
        &self,
        candidates: Vec<SourceId>,
        path_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        let mut addresses = reserve_vec(
            candidates.len(),
            "ambiguous binary reference candidates",
            budget,
        )?;
        for source in candidates {
            if source.kind() == SourceKind::SerializedFile {
                addresses.push(
                    self.input
                        .address_for_object(&ObjectId::binary(source, path_id)?, budget)?,
                );
            }
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(ReferenceResolution::Ambiguous {
            candidates: addresses.into_boxed_slice(),
        })
    }

    fn ambiguous_yaml(
        &self,
        candidates: Vec<SourceId>,
        file_id: i64,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        let mut addresses = reserve_vec(
            candidates.len(),
            "ambiguous YAML reference candidates",
            budget,
        )?;
        for source in candidates {
            let object = match source.kind() {
                SourceKind::SerializedFile => ObjectId::binary(source, file_id)?,
                SourceKind::Yaml => ObjectId::yaml(source, decimal_i64(file_id, budget)?)?,
                SourceKind::AssetBundle
                | SourceKind::WebFile
                | SourceKind::Archive
                | SourceKind::StreamedResource => continue,
            };
            addresses.push(self.input.address_for_object(&object, budget)?);
        }
        addresses.sort_unstable();
        addresses.dedup();
        Ok(ReferenceResolution::Ambiguous {
            candidates: addresses.into_boxed_slice(),
        })
    }

    fn guid_candidates(
        &self,
        guid: [u8; 16],
        kind: SourceKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, ReferenceGraphError> {
        let mut candidates = self.guid_candidates_any(guid, budget)?;
        candidates.retain(|source| source.kind() == kind);
        Ok(candidates)
    }

    fn guid_candidates_any(
        &self,
        guid: [u8; 16],
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, ReferenceGraphError> {
        let start = self
            .identity
            .guids
            .partition_point(|claim| claim.guid < guid);
        let end = self
            .identity
            .guids
            .partition_point(|claim| claim.guid <= guid);
        let mut candidates = reserve_vec(
            end.saturating_sub(start),
            "GUID reference candidates",
            budget,
        )?;
        for claim in &self.identity.guids[start..end] {
            candidates.push(claim.source);
        }
        candidates.sort_unstable();
        candidates.dedup();
        Ok(candidates)
    }

    fn path_candidates(
        &self,
        context: SourceId,
        normalized: &str,
        kind: SourceKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<SourceId>, ReferenceGraphError> {
        self.identity
            .path_candidates(context, normalized, kind, budget)
    }

    fn source_guid_evidence(&self, source: SourceId, expected: [u8; 16]) -> SourceGuidEvidence {
        source_guid_evidence(&self.identity.source_guids, source, expected)
    }

    fn invalid(
        &self,
        source: &RevisionedObjectHandle,
        field_path: &FieldPath,
        code: &'static str,
        message: &'static str,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferenceResolution, ReferenceGraphError> {
        Ok(ReferenceResolution::Invalid {
            diagnostic: diagnostic(
                self.input,
                Some(source.object()),
                Some(field_path),
                code,
                clone_string(message, "reference diagnostic message", budget)?,
                budget,
            )?,
        })
    }
}

fn describable_path_candidates<'claims>(
    claims: &'claims [DescribablePathClaim],
    parent: Option<SourceId>,
    path: &str,
    same_name_occurrence: Option<u32>,
    budget: &mut AssetLoadBudget,
) -> Result<&'claims [DescribablePathClaim], ReferenceGraphError> {
    let compare = |claim: &DescribablePathClaim| {
        claim
            .parent
            .cmp(&parent)
            .then_with(|| claim.key.as_str().cmp(path))
            .then_with(|| claim.same_name_occurrence.cmp(&same_name_occurrence))
    };
    let start = claims.partition_point(|claim| compare(claim).is_lt());
    let end = claims.partition_point(|claim| !compare(claim).is_gt());
    budget.consume_members(usize_to_u64(
        end.saturating_sub(start),
        "reference meta path candidates",
    )?)?;
    Ok(&claims[start..end])
}

enum ExternalCandidates {
    Candidates(Vec<SourceId>),
    Invalid(Diagnostic),
}

fn reconcile_external_candidates<I: ReferenceInput + ?Sized>(
    raw_guid: Option<[u8; 16]>,
    mut guid: Vec<SourceId>,
    mut path: Vec<SourceId>,
    source: &RevisionedObjectHandle,
    field_path: &FieldPath,
    catalog: &ResolutionCatalog<'_, I>,
    budget: &mut AssetLoadBudget,
) -> Result<ExternalCandidates, ReferenceGraphError> {
    if guid.is_empty() {
        if let Some(raw_guid) = raw_guid {
            let had_path_candidates = !path.is_empty();
            path.retain(|candidate| {
                !matches!(
                    catalog.source_guid_evidence(*candidate, raw_guid),
                    SourceGuidEvidence::Conflicting
                )
            });
            if had_path_candidates && path.is_empty() {
                return invalid_external_identity(source, field_path, catalog, budget);
            }
        }
        return Ok(ExternalCandidates::Candidates(path));
    }
    if path.is_empty() {
        return Ok(ExternalCandidates::Candidates(guid));
    }
    guid.sort_unstable();
    path.sort_unstable();
    let mut intersection = reserve_vec(
        guid.len().min(path.len()),
        "external reference evidence intersection",
        budget,
    )?;
    let (mut left, mut right) = (0, 0);
    while left < guid.len() && right < path.len() {
        match guid[left].cmp(&path[right]) {
            std::cmp::Ordering::Less => left += 1,
            std::cmp::Ordering::Greater => right += 1,
            std::cmp::Ordering::Equal => {
                intersection.push(guid[left]);
                left += 1;
                right += 1;
            }
        }
    }
    if intersection.is_empty() {
        return invalid_external_identity(source, field_path, catalog, budget);
    }
    Ok(ExternalCandidates::Candidates(intersection))
}

fn invalid_external_identity<I: ReferenceInput + ?Sized>(
    source: &RevisionedObjectHandle,
    field_path: &FieldPath,
    catalog: &ResolutionCatalog<'_, I>,
    budget: &mut AssetLoadBudget,
) -> Result<ExternalCandidates, ReferenceGraphError> {
    let ReferenceResolution::Invalid { diagnostic } = catalog.invalid(
        source,
        field_path,
        "REFERENCE_EXTERNAL_IDENTITY_CONFLICT",
        "external GUID and path identify different loaded sources",
        budget,
    )?
    else {
        return Err(ReferenceGraphError::Invariant(
            "invalid reference helper returned a non-invalid state",
        ));
    };
    Ok(ExternalCandidates::Invalid(diagnostic))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceGuidEvidence {
    Unknown,
    Matching,
    Conflicting,
}

fn source_guid_evidence(
    source_guids: &[SourceGuidClaim],
    source: SourceId,
    expected: [u8; 16],
) -> SourceGuidEvidence {
    let start = source_guids.partition_point(|claim| claim.source < source);
    let end = source_guids.partition_point(|claim| claim.source <= source);
    let claims = &source_guids[start..end];
    if claims.is_empty() {
        SourceGuidEvidence::Unknown
    } else if claims.iter().any(|claim| claim.guid == expected) {
        SourceGuidEvidence::Matching
    } else {
        SourceGuidEvidence::Conflicting
    }
}

pub(crate) fn diagnostic<I: ReferenceInput + ?Sized>(
    input: &I,
    object: Option<&ObjectId>,
    field_path: Option<&FieldPath>,
    code: &'static str,
    message: String,
    budget: &mut AssetLoadBudget,
) -> Result<Diagnostic, ReferenceGraphError> {
    diagnostic_with_severity(
        input,
        object,
        field_path,
        DiagnosticSeverity::Warning,
        code,
        message,
        budget,
    )
}

pub(crate) fn diagnostic_with_severity<I: ReferenceInput + ?Sized>(
    input: &I,
    object: Option<&ObjectId>,
    field_path: Option<&FieldPath>,
    severity: DiagnosticSeverity,
    code: &'static str,
    message: String,
    budget: &mut AssetLoadBudget,
) -> Result<Diagnostic, ReferenceGraphError> {
    let code = clone_string(code, "reference diagnostic code", budget)?;
    let address = object
        .map(|object| input.address_for_object(object, budget))
        .transpose()?;
    let field_path = field_path
        .map(|field_path| clone_field_path(field_path, budget))
        .transpose()?;
    let mut diagnostic = Diagnostic::new(severity, code, message)?;
    if let Some(address) = address {
        diagnostic = diagnostic.at_address(address);
    }
    if let Some(field_path) = field_path {
        diagnostic = diagnostic.at_field(field_path);
    }
    Ok(diagnostic)
}

fn clone_field_path(
    field_path: &FieldPath,
    budget: &mut AssetLoadBudget,
) -> Result<FieldPath, ReferenceGraphError> {
    let resource = "reference diagnostic field path";
    let mut segments = reserve_vec(field_path.segments().len(), resource, budget)?;
    for segment in field_path.segments() {
        segments.push(match segment {
            FieldPathSegment::Field(name) => {
                FieldPathSegment::field(clone_string(name, resource, budget)?)?
            }
            FieldPathSegment::Index(index) => FieldPathSegment::Index(*index),
        });
    }
    Ok(FieldPath::from_segments(segments)?)
}

fn collect_path_claims(
    claims: &[PathClaim],
    key: &str,
    kind: SourceKind,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<SourceId>, ReferenceGraphError> {
    let start = claims.partition_point(|claim| claim.key.as_str() < key);
    let end = claims.partition_point(|claim| claim.key.as_str() <= key);
    let mut candidates = reserve_vec(
        end.saturating_sub(start),
        "external path candidates",
        budget,
    )?;
    for claim in &claims[start..end] {
        if claim.source.kind() == kind {
            candidates.push(claim.source);
        }
    }
    Ok(candidates)
}

fn push_path_claim(
    claims: &mut Vec<PathClaim>,
    key: String,
    source: SourceId,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    if key.is_empty() {
        return Ok(());
    }
    push_value(claims, PathClaim { key, source }, resource, budget)
}

fn push_value<T>(
    values: &mut Vec<T>,
    value: T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), ReferenceGraphError> {
    if values.len() == values.capacity() {
        let previous_capacity = values.capacity();
        let required = values
            .len()
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        let target = geometric_vec_capacity(previous_capacity, required, resource)?;
        let additional = target
            .checked_sub(values.len())
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        budget.check_bytes(vec_capacity_bytes::<T>(
            target
                .checked_sub(previous_capacity)
                .ok_or(BudgetError::ArithmeticOverflow { resource })?,
            resource,
        )?)?;
        values
            .try_reserve_exact(additional)
            .map_err(|error| ReferenceGraphError::Allocation {
                resource,
                requested: additional,
                unit: super::ReferenceAllocationUnit::Elements,
                source: error,
            })?;
        let retained = values
            .capacity()
            .checked_sub(previous_capacity)
            .ok_or(BudgetError::ArithmeticOverflow { resource })?;
        budget.consume_bytes(vec_capacity_bytes::<T>(retained, resource)?)?;
    }
    values.push(value);
    Ok(())
}

pub(super) fn reserve_vec<T>(
    capacity: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, ReferenceGraphError> {
    budget.check_bytes(vec_capacity_bytes::<T>(capacity, resource)?)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| ReferenceGraphError::Allocation {
            resource,
            requested: capacity,
            unit: super::ReferenceAllocationUnit::Elements,
            source: error,
        })?;
    budget.consume_bytes(vec_capacity_bytes::<T>(values.capacity(), resource)?)?;
    Ok(values)
}

fn geometric_vec_capacity(
    current: usize,
    required: usize,
    resource: &'static str,
) -> Result<usize, BudgetError> {
    if required <= current {
        return Ok(current);
    }
    let doubled = current
        .checked_mul(2)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    Ok(required.max(doubled).max(4))
}

fn vec_capacity_bytes<T>(capacity: usize, resource: &'static str) -> Result<u64, BudgetError> {
    let bytes = capacity
        .checked_mul(size_of::<T>())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    usize_to_u64(bytes, resource)
}

fn normalize_external_path(
    path: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
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

    let input = &path[start..end];
    let retained = usize_to_u64(input.len(), resource)?;
    budget.check_bytes(retained)?;
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(input.len())
        .map_err(|source| ReferenceGraphError::Allocation {
            resource,
            requested: input.len(),
            unit: super::ReferenceAllocationUnit::Bytes,
            source,
        })?;
    let retained = usize_to_u64(normalized.capacity(), resource)?;
    budget.check_bytes(retained)?;
    for character in input.chars() {
        normalized.push(if character == '\\' {
            '/'
        } else {
            character.to_ascii_lowercase()
        });
    }
    budget.consume_bytes(retained)?;
    Ok(normalized)
}

const fn is_path_separator(byte: u8) -> bool {
    matches!(byte, b'/' | b'\\')
}

fn external_basename(path: &str) -> Option<&str> {
    path.rsplit('/').find(|component| !component.is_empty())
}

fn meta_guids(
    source: &ReferenceSource<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<[u8; 16]>, ReferenceGraphError> {
    let Some(document) = source.yaml_document() else {
        return Ok(Vec::new());
    };
    let document_count = document.entries().len();
    budget.consume_entries(usize_to_u64(
        document_count,
        "reference meta document scan",
    )?)?;
    let mut guids = Vec::new();
    for index in 0..document_count {
        let guid = source
            .yaml_class(index)
            .and_then(|class| class.get("guid"))
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(parse_guid)
                    .or_else(|| (value.as_u64() == Some(0)).then_some([0; 16]))
            });
        if let Some(guid) = guid {
            push_value(&mut guids, guid, "reference meta source GUIDs", budget)?;
        }
    }
    guids.sort_unstable();
    guids.dedup();
    Ok(guids)
}

fn parse_guid(value: &str) -> Option<[u8; 16]> {
    if value.len() != 32 {
        return None;
    }
    let mut guid = [0_u8; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        guid[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Some(guid)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(super) fn clone_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, ReferenceGraphError> {
    budget.check_bytes(usize_to_u64(value.len(), resource)?)?;
    let mut cloned = String::new();
    cloned
        .try_reserve_exact(value.len())
        .map_err(|source| ReferenceGraphError::Allocation {
            resource,
            requested: value.len(),
            unit: super::ReferenceAllocationUnit::Bytes,
            source,
        })?;
    let retained = usize_to_u64(cloned.capacity(), resource)?;
    budget.check_bytes(retained)?;
    cloned.push_str(value);
    budget.consume_bytes(retained)?;
    Ok(cloned)
}

fn decimal_i64(value: i64, budget: &mut AssetLoadBudget) -> Result<String, ReferenceGraphError> {
    let mut magnitude = value.unsigned_abs();
    let mut length = usize::from(value < 0);
    loop {
        length += 1;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    let resource = "YAML reference file ID";
    budget.check_bytes(usize_to_u64(length, resource)?)?;
    let mut rendered = String::new();
    rendered
        .try_reserve_exact(length)
        .map_err(|source| ReferenceGraphError::Allocation {
            resource,
            requested: length,
            unit: super::ReferenceAllocationUnit::Bytes,
            source,
        })?;
    let retained = usize_to_u64(rendered.capacity(), resource)?;
    budget.check_bytes(retained)?;
    write!(rendered, "{value}")
        .map_err(|_| ReferenceGraphError::Invariant("failed to format YAML reference file ID"))?;
    budget.consume_bytes(retained)?;
    Ok(rendered)
}

fn usize_to_u64(value: usize, resource: &'static str) -> Result<u64, BudgetError> {
    u64::try_from(value).map_err(|_| BudgetError::ArithmeticOverflow { resource })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use unity_asset_core::{
        AssetLoadLimits, AssetLoadUsage, SourceFingerprint, SourceKind, SourceLocator, UnityClass,
        UnityValue, WorkspaceId,
    };
    use unity_asset_yaml::YamlDocument;

    use super::*;
    use crate::workspace::{AssetWorkspace, WorkspaceView, reference_view_parts};

    const TRANSFORM_BINARY: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/transform_hierarchy_v22.assets.bin"
    );

    #[test]
    fn path_normalization_is_fallible_and_format_faithful() {
        let mut budget = AssetLoadBudget::default();
        assert_eq!(
            normalize_external_path(
                r"./Archive:\Folder\Dependency.ASSETS/",
                "test path",
                &mut budget,
            )
            .unwrap(),
            "folder/dependency.assets"
        );
        assert_eq!(
            normalize_external_path("./Folder/Ä.asset", "test path", &mut budget).unwrap(),
            "folder/Ä.asset"
        );
    }

    #[test]
    fn reference_strings_charge_retained_capacity() {
        let mut clone_budget = AssetLoadBudget::default();
        let cloned = clone_string("reference.assets", "test string", &mut clone_budget).unwrap();
        assert_eq!(
            clone_budget.usage().bytes,
            u64::try_from(cloned.capacity()).unwrap()
        );

        let mut path_budget = AssetLoadBudget::default();
        let normalized =
            normalize_external_path("./Reference.Assets/", "test path", &mut path_budget).unwrap();
        assert_eq!(
            path_budget.usage().bytes,
            u64::try_from(normalized.capacity()).unwrap()
        );

        let mut decimal_budget = AssetLoadBudget::default();
        let rendered = decimal_i64(i64::MIN, &mut decimal_budget).unwrap();
        assert_eq!(
            decimal_budget.usage().bytes,
            u64::try_from(rendered.capacity()).unwrap()
        );

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: clone_budget.usage().bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            clone_string("reference.assets", "test string", &mut one_short),
            Err(ReferenceGraphError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(one_short.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn loaded_nonmatching_meta_guid_rejects_path_fallback() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = SourceId::new(workspace, SourceKind::SerializedFile, 1).unwrap();
        let expected = [1; 16];
        let conflicting = [2; 16];

        assert_eq!(
            source_guid_evidence(&[], source, expected),
            SourceGuidEvidence::Unknown,
        );
        assert_eq!(
            source_guid_evidence(
                &[SourceGuidClaim {
                    source,
                    guid: expected,
                }],
                source,
                expected,
            ),
            SourceGuidEvidence::Matching,
        );
        assert_eq!(
            source_guid_evidence(
                &[SourceGuidClaim {
                    source,
                    guid: conflicting,
                }],
                source,
                expected,
            ),
            SourceGuidEvidence::Conflicting,
        );
    }

    #[test]
    fn identity_vector_growth_charges_the_retained_capacity_delta() {
        let mut values = Vec::new();
        let mut budget = AssetLoadBudget::default();
        for value in 0_u64..9 {
            push_value(&mut values, value, "test identity vector", &mut budget).unwrap();
        }

        assert!(values.capacity() >= 16);
        assert_eq!(
            budget.usage().bytes,
            u64::try_from(values.capacity() * size_of::<u64>()).unwrap()
        );
    }

    #[test]
    fn identity_index_collects_all_root_meta_guids_with_exact_budgeting() {
        const FIRST_GUID: &str = "11111111111111111111111111111111";
        const SECOND_GUID: &str = "22222222222222222222222222222222";

        fn guid_class(guid: &str) -> UnityClass {
            let mut class = UnityClass::new(0, "YamlDocument".to_owned(), "0".to_owned());
            class.set("guid".to_owned(), UnityValue::String(guid.to_owned()));
            class
        }

        let workspace = WorkspaceId::from_u128(1).unwrap();
        let target_id = SourceId::new(workspace, SourceKind::Yaml, 1).unwrap();
        let meta_id = SourceId::new(workspace, SourceKind::Yaml, 2).unwrap();
        let duplicate_basename_id = SourceId::new(workspace, SourceKind::Yaml, 3).unwrap();
        let target_owner = Arc::<[u8]>::from(&b"target"[..]);
        let meta_owner = Arc::<[u8]>::from(&b"meta"[..]);
        let duplicate_owner = Arc::<[u8]>::from(&b"duplicate"[..]);
        let target_locator = SourceLocator::path("folder/target.prefab").unwrap();
        let meta_locator = SourceLocator::path("folder/target.prefab.meta").unwrap();
        let duplicate_locator = SourceLocator::path("other/target.prefab").unwrap();
        let target_document = YamlDocument::new();
        let duplicate_document = YamlDocument::new();
        let mut meta_document = YamlDocument::new();
        meta_document.add_entry(guid_class(SECOND_GUID));
        meta_document.add_entry(guid_class("not-a-guid"));
        meta_document.add_entry(guid_class(FIRST_GUID));
        meta_document.add_entry(guid_class(SECOND_GUID));

        let sources = [
            ReferenceSource::yaml(
                target_id,
                SourceFingerprint::from_bytes(SourceKind::Yaml, target_owner.as_ref()),
                (&target_owner).into(),
                &target_locator,
                None,
                None,
                &target_document,
            )
            .unwrap(),
            ReferenceSource::yaml(
                meta_id,
                SourceFingerprint::from_bytes(SourceKind::Yaml, meta_owner.as_ref()),
                (&meta_owner).into(),
                &meta_locator,
                None,
                None,
                &meta_document,
            )
            .unwrap(),
            ReferenceSource::yaml(
                duplicate_basename_id,
                SourceFingerprint::from_bytes(SourceKind::Yaml, duplicate_owner.as_ref()),
                (&duplicate_owner).into(),
                &duplicate_locator,
                None,
                None,
                &duplicate_document,
            )
            .unwrap(),
        ];

        let mut measured = AssetLoadBudget::default();
        let identity = ResolutionIdentityIndex::build(&sources, &mut measured).unwrap();
        let first = parse_guid(FIRST_GUID).unwrap();
        let second = parse_guid(SECOND_GUID).unwrap();
        assert_eq!(
            identity.source_guids(target_id).collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(
            identity.guid_sources(first).collect::<Vec<_>>(),
            [target_id]
        );
        assert_eq!(
            identity.guid_sources(second).collect::<Vec<_>>(),
            [target_id]
        );
        assert_eq!(
            identity
                .unique_path_source(
                    meta_id,
                    r"./FOLDER\TARGET.PREFAB",
                    SourceKind::Yaml,
                    &mut measured,
                )
                .unwrap(),
            Some(target_id)
        );
        assert_eq!(
            identity
                .unique_path_source(meta_id, "target.prefab", SourceKind::Yaml, &mut measured,)
                .unwrap(),
            None
        );

        let index_bytes = {
            let mut budget = AssetLoadBudget::default();
            ResolutionIdentityIndex::build(&sources, &mut budget).unwrap();
            budget.usage().bytes
        };
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: index_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        ResolutionIdentityIndex::build(&sources, &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, index_bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: index_bytes - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            ResolutionIdentityIndex::build(&sources, &mut one_short),
            Err(ReferenceGraphError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn describable_paths_isolate_same_named_members_by_parent_before_budgeting() {
        const PARENT_COUNT: u128 = 1_024;
        const TARGET_PARENT: u128 = 512;

        let workspace = WorkspaceId::from_u128(1).unwrap();
        let target_parent =
            SourceId::new(workspace, SourceKind::Archive, TARGET_PARENT + 1).unwrap();
        let mut claims = (0..PARENT_COUNT)
            .map(|ordinal| DescribablePathClaim {
                parent: Some(SourceId::new(workspace, SourceKind::Archive, ordinal + 1).unwrap()),
                key: "nested/target.prefab".to_owned(),
                same_name_occurrence: Some(0),
                source: SourceId::new(workspace, SourceKind::Yaml, ordinal + 1).unwrap(),
            })
            .collect::<Vec<_>>();
        claims.push(DescribablePathClaim {
            parent: Some(target_parent),
            key: "nested/target.prefab".to_owned(),
            same_name_occurrence: Some(0),
            source: SourceId::new(workspace, SourceKind::Yaml, PARENT_COUNT + 1).unwrap(),
        });
        claims.sort_unstable();

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 2,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let matches = describable_path_candidates(
            &claims,
            Some(target_parent),
            "nested/target.prefab",
            Some(0),
            &mut exact,
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        assert!(
            matches
                .iter()
                .all(|claim| claim.parent == Some(target_parent))
        );
        assert_eq!(exact.usage().members, 2);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            describable_path_candidates(
                &claims,
                Some(target_parent),
                "nested/target.prefab",
                Some(0),
                &mut one_short,
            ),
            Err(ReferenceGraphError::Budget(_))
        ));
        assert_eq!(one_short.usage().members, 0);
    }

    #[test]
    fn decimal_rendering_allocates_through_the_budget() {
        let mut budget = AssetLoadBudget::default();
        assert_eq!(
            decimal_i64(i64::MIN, &mut budget).unwrap(),
            i64::MIN.to_string()
        );
    }

    #[test]
    fn external_identity_conflicts_and_empty_identities_are_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner.prefab");
        let target_path = directory.path().join("target.assets");
        let meta_path = directory.path().join("target.assets.meta");
        fs::write(
            &owner_path,
            b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Owner\n",
        )
        .unwrap();
        fs::write(&target_path, TRANSFORM_BINARY).unwrap();
        fs::write(
            &meta_path,
            b"fileFormatVersion: 2\nguid: 22222222222222222222222222222222\n",
        )
        .unwrap();

        let mut workspace = AssetWorkspace::new().unwrap();
        let owner = workspace
            .load_path(&owner_path, &mut AssetLoadBudget::default())
            .unwrap();
        workspace
            .load_path(&target_path, &mut AssetLoadBudget::default())
            .unwrap();
        workspace
            .load_path(&meta_path, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        let nodes = snapshot.objects(&mut AssetLoadBudget::default()).unwrap();
        let source = nodes
            .iter()
            .find(|handle| handle.object().source() == owner)
            .unwrap();
        let parts = reference_view_parts(&snapshot);
        let sources = parts
            .object_sources()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut budget = AssetLoadBudget::default();
        let catalog = ResolutionCatalog::build(&parts, &sources, &nodes, &mut budget).unwrap();
        let field_path = FieldPath::root();

        let conflicting = catalog
            .resolve(
                source,
                &field_path,
                &RawReferenceTarget::Binary {
                    file_id: 1,
                    path_id: 1,
                    external: Some(BinaryExternalReference::new(
                        0,
                        [1; 16],
                        3,
                        "target.assets".to_owned(),
                    )),
                },
                None,
                &mut budget,
            )
            .unwrap();
        assert!(
            matches!(
                conflicting,
                ReferenceResolution::Invalid { ref diagnostic }
                    if diagnostic.code() == "REFERENCE_EXTERNAL_IDENTITY_CONFLICT"
            ),
            "unexpected resolution: {conflicting:?}"
        );

        let empty = catalog
            .resolve(
                source,
                &field_path,
                &RawReferenceTarget::Binary {
                    file_id: 1,
                    path_id: 1,
                    external: Some(BinaryExternalReference::new(0, [0; 16], 0, String::new())),
                },
                None,
                &mut budget,
            )
            .unwrap();
        assert!(matches!(
            empty,
            ReferenceResolution::Invalid { ref diagnostic }
                if diagnostic.code() == "REFERENCE_EXTERNAL_IDENTITY_MISSING"
        ));

        let empty_yaml_guid = catalog
            .resolve(
                source,
                &field_path,
                &RawReferenceTarget::Yaml {
                    file_id: Some(1),
                    guid: Some(ReferenceGuid::Parsed([0; 16])),
                    type_id: Some(3),
                },
                None,
                &mut budget,
            )
            .unwrap();
        assert!(matches!(
            empty_yaml_guid,
            ReferenceResolution::Invalid { ref diagnostic }
                if diagnostic.code() == "YAML_REFERENCE_EMPTY_GUID"
        ));
    }
}
