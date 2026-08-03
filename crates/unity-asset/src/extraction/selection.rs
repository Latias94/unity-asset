use std::fmt::Write as _;

#[cfg(test)]
use unity_asset_binary::asset::class_ids;
use unity_asset_core::{
    AssetLoadBudget, ObjectAddress, ObjectKind, RevisionedObjectHandle, SourceLocator, UnityValue,
};

use super::container::{
    BundleContainerQuery, BundleContainerResult, query_bundle_container_occurrences,
    resolved_addresses,
};
use super::contract::ExtractionAllocationUnit;
use super::manifest::canonical_digest;
use super::model::{
    ExtractionModelError, ExtractionPath, ExtractionPlan, ExtractionRepresentationPolicy,
    ExtractionRequest, ExtractionSelection, PlannedArtifact,
};
use super::planning_contract::{
    ExtractionPlanError, budgeted_vec, clone_string, resolve_required_handle, source_for_id,
    usize_to_u64,
};
use super::representation::RepresentationPlanner;
use crate::reference::{ReferenceGraph, ReferenceTraversal};
#[cfg(test)]
use crate::workspace::StreamedResourceResolver;
use crate::workspace::{WorkspaceObject, WorkspaceSource, WorkspaceView};

/// Plans deterministic extraction artifacts against one immutable workspace view.
pub struct ExtractionPlanner<'view> {
    view: &'view dyn WorkspaceView,
    references: Option<&'view ReferenceGraph>,
}

impl<'view> ExtractionPlanner<'view> {
    #[must_use]
    pub const fn new(view: &'view dyn WorkspaceView) -> Self {
        Self {
            view,
            references: None,
        }
    }

    #[must_use]
    pub const fn with_reference_graph(mut self, references: &'view ReferenceGraph) -> Self {
        self.references = Some(references);
        self
    }

    pub fn plan(
        &self,
        request: ExtractionRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        self.validate_reference_context()?;

        let sources = self.view.sources(budget)?;
        let mut representation_planner = RepresentationPlanner::new(
            self.view,
            self.references,
            &sources,
            request.representation(),
        );
        let handles = selected_handles(self.view, &request, &sources, budget)?;
        let mut candidates = budgeted_vec(handles.len(), "extraction candidate addresses", budget)?;
        for handle in handles {
            let address = self.view.object_address(&handle, budget)?;
            candidates.push((address, handle));
        }
        candidates.sort_by(|left, right| left.0.cmp(&right.0));
        candidates.dedup_by(|left, right| left.0 == right.0);

        let mut source_expectations =
            budgeted_vec(sources.len(), "extraction source expectations", budget)?;
        let mut artifacts = budgeted_vec(candidates.len(), "extraction planned artifacts", budget)?;
        for (address, handle) in candidates {
            let object = self.view.read_object(&handle, budget)?;
            if !request
                .filter()
                .matches_class(object.class().class_id(), object.class().class_name())
            {
                continue;
            }
            let object_name = object_name(&object, budget)?;
            if !request.filter().matches_object_name(object_name.as_deref()) {
                continue;
            }
            if request.filter().limit().is_some_and(|limit| {
                u64::try_from(artifacts.len()).is_ok_and(|count| count >= limit)
            }) {
                break;
            }

            let owner = source_for_id(handle.object().source(), &sources)?;
            let ordinal = u32::try_from(artifacts.len()).map_err(|_| {
                ExtractionPlanError::ArithmeticOverflow {
                    resource: "extraction artifact ordinal",
                }
            })?;
            let choice = representation_planner.select(&address, &object, owner, budget)?;
            let representation = choice.finalize(
                ordinal,
                &address,
                &mut source_expectations,
                owner,
                budget,
                |extension, fallback, budget| {
                    allocate_path(
                        request.prefix(),
                        &address,
                        owner.locator(),
                        object.class().class_id(),
                        object.class().class_name(),
                        object_name.as_deref(),
                        extension,
                        fallback,
                        budget,
                    )
                },
            )?;
            let class_name = clone_string(
                object.class().class_name(),
                "extraction artifact class name",
                budget,
            )?;
            artifacts.push(
                PlannedArtifact::new(
                    ordinal,
                    address,
                    object.class().class_id(),
                    class_name,
                    object_name,
                    representation,
                )
                .map_err(map_model_error)?,
            );
        }

        source_expectations.sort_unstable_by(|left, right| left.locator().cmp(right.locator()));
        ExtractionPlan::new_budgeted(
            self.view.workspace_id(),
            self.view.revision(),
            request,
            source_expectations,
            artifacts,
            budget,
        )
        .map_err(map_model_error)
    }

    pub fn plan_handles(
        &self,
        handles: &[RevisionedObjectHandle],
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let mut addresses = budgeted_vec(handles.len(), "extraction handle addresses", budget)?;
        for handle in handles {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(self.view.object_address(handle, budget)?);
        }
        let request =
            ExtractionRequest::addresses(addresses, representation).map_err(map_model_error)?;
        self.plan(request, budget)
    }

    pub fn plan_traversal(
        &self,
        traversal: &ReferenceTraversal,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        if traversal.workspace_id() != self.view.workspace_id()
            || traversal.revision() != self.view.revision()
        {
            return Err(ExtractionPlanError::ReferenceContextMismatch);
        }
        if !traversal.is_complete() {
            return Err(ExtractionPlanError::IncompleteReferenceTraversal);
        }
        let mut addresses =
            budgeted_vec(traversal.len(), "extraction traversal addresses", budget)?;
        for handle in traversal.nodes() {
            handle.validate_context(self.view.workspace_id(), self.view.revision())?;
            addresses.push(self.view.object_address(handle, budget)?);
        }
        let request =
            ExtractionRequest::addresses(addresses, representation).map_err(map_model_error)?;
        self.plan(request, budget)
    }

    pub fn plan_bundle_containers(
        &self,
        pattern: &str,
        representation: ExtractionRepresentationPolicy,
        budget: &mut AssetLoadBudget,
    ) -> Result<ExtractionPlan, ExtractionPlanError> {
        let pattern = clone_string(pattern, "bundle container query pattern", budget)?;
        let query = BundleContainerQuery::new(pattern)?;
        let result = self.bundle_container_occurrences(query, budget)?;
        if !result.is_complete() {
            return Err(ExtractionPlanError::IncompleteReferenceGraph);
        }
        let addresses = resolved_addresses(&result, budget)?;
        let request_pattern = clone_string(
            result.query().pattern(),
            "bundle container extraction pattern",
            budget,
        )?;
        let request =
            ExtractionRequest::bundle_container(request_pattern, addresses, representation)
                .map_err(map_model_error)?;
        self.plan(request, budget)
    }

    /// Returns every exact `AssetBundle.m_Container` reference occurrence without deduplication.
    pub fn bundle_container_occurrences(
        &self,
        query: BundleContainerQuery,
        budget: &mut AssetLoadBudget,
    ) -> Result<BundleContainerResult, ExtractionPlanError> {
        self.validate_reference_context()?;
        let references = self
            .references
            .ok_or(ExtractionPlanError::ReferenceGraphRequired)?;
        query_bundle_container_occurrences(self.view, references, query, budget)
    }

    fn validate_reference_context(&self) -> Result<(), ExtractionPlanError> {
        if let Some(references) = self.references
            && (references.workspace_id() != self.view.workspace_id()
                || references.revision() != self.view.revision())
        {
            return Err(ExtractionPlanError::ReferenceContextMismatch);
        }
        Ok(())
    }
}

fn map_model_error(error: ExtractionModelError) -> ExtractionPlanError {
    match error {
        ExtractionModelError::Budget(source) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Budget(source),
        ) => ExtractionPlanError::Budget(source),
        ExtractionModelError::InvalidPath(
            unity_asset_write::artifact::ArtifactNameError::Allocation {
                resource,
                requested,
                source,
            },
        ) => ExtractionPlanError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::Bytes,
            source,
        },
        ExtractionModelError::Allocation {
            resource,
            requested,
            source,
        } => ExtractionPlanError::Allocation {
            resource,
            requested,
            unit: ExtractionAllocationUnit::CapacityUnits,
            source,
        },
        other => ExtractionPlanError::ModelValidation(other),
    }
}

fn selected_handles(
    view: &dyn WorkspaceView,
    request: &ExtractionRequest,
    sources: &[WorkspaceSource],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<RevisionedObjectHandle>, ExtractionPlanError> {
    match request.selection() {
        ExtractionSelection::All => view.objects(budget).map_err(Into::into),
        ExtractionSelection::Sources { sources: selected } => {
            let mut handles = view.objects(budget)?;
            handles.retain(|handle| {
                source_for_id(handle.object().source(), sources)
                    .is_ok_and(|source| selected.binary_search(source.locator()).is_ok())
            });
            Ok(handles)
        }
        ExtractionSelection::Addresses { addresses }
        | ExtractionSelection::BundleContainer { addresses, .. }
        | ExtractionSelection::ReferenceTraversal { addresses } => {
            let mut handles = budgeted_vec(addresses.len(), "extraction selected handles", budget)?;
            for address in addresses {
                handles.push(resolve_required_handle(view, address, budget)?);
            }
            Ok(handles)
        }
    }
}

fn object_name(
    object: &WorkspaceObject,
    budget: &mut AssetLoadBudget,
) -> Result<Option<String>, ExtractionPlanError> {
    object
        .class()
        .get("m_Name")
        .or_else(|| object.class().get("name"))
        .and_then(UnityValue::as_str)
        .map(|name| clone_string(name, "extraction artifact object name", budget))
        .transpose()
}

fn allocate_path(
    prefix: Option<&ExtractionPath>,
    address: &ObjectAddress,
    source: &SourceLocator,
    class_id: i32,
    class_name: &str,
    object_name: Option<&str>,
    extension: &str,
    raw_fallback: bool,
    budget: &mut AssetLoadBudget,
) -> Result<ExtractionPath, ExtractionPlanError> {
    const SOURCE_LIMIT: usize = 48;
    const CLASS_LIMIT: usize = 48;
    const NAME_LIMIT: usize = 64;
    const DIGEST_HEX_BYTES: usize = 64;

    let digest = canonical_digest(address)
        .map_err(|error| ExtractionPlanError::CanonicalAddress(error.to_string()))?;
    let source_name = source.root_alias().as_str();
    let artifact_name = object_name.unwrap_or_else(|| {
        address
            .yaml_anchor()
            .unwrap_or(if address.kind() == ObjectKind::Binary {
                "object"
            } else {
                "document"
            })
    });
    let fallback = if raw_fallback { ".raw" } else { "" };
    let requested = [
        prefix.map_or(0, |prefix| prefix.as_str().len() + 1),
        "sources/source-".len(),
        slug_capacity_bound(source_name, SOURCE_LIMIT),
        "/class-".len(),
        11,
        "-".len(),
        slug_capacity_bound(class_name, CLASS_LIMIT),
        "/".len(),
        slug_capacity_bound(artifact_name, NAME_LIMIT),
        "--".len(),
        DIGEST_HEX_BYTES,
        fallback.len(),
        ".".len(),
        extension.len(),
    ]
    .into_iter()
    .try_fold(0_usize, |total, part| total.checked_add(part))
    .ok_or(ExtractionPlanError::ArithmeticOverflow {
        resource: "extraction relative path",
    })?;
    budget.check_bytes(usize_to_u64(requested, "extraction relative path")?)?;
    let mut relative = String::new();
    relative
        .try_reserve_exact(requested)
        .map_err(|source| ExtractionPlanError::Allocation {
            resource: "extraction relative path",
            requested,
            unit: ExtractionAllocationUnit::Bytes,
            source,
        })?;
    if let Some(prefix) = prefix {
        relative.push_str(prefix.as_str());
        relative.push('/');
    }
    relative.push_str("sources/source-");
    push_slug(&mut relative, source_name, SOURCE_LIMIT);
    relative.push_str("/class-");
    write!(&mut relative, "{class_id}").map_err(|_| ExtractionPlanError::PathFormatting)?;
    relative.push('-');
    push_slug(&mut relative, class_name, CLASS_LIMIT);
    relative.push('/');
    push_slug(&mut relative, artifact_name, NAME_LIMIT);
    relative.push_str("--");
    push_digest_hex(&mut relative, digest);
    relative.push_str(fallback);
    relative.push('.');
    relative.push_str(extension);

    ExtractionPath::from_string_with_budget(relative, budget)
        .map_err(ExtractionModelError::from)
        .map_err(map_model_error)
}

const fn slug_capacity_bound(value: &str, maximum: usize) -> usize {
    let input_bound = if value.len() < maximum {
        value.len()
    } else {
        maximum
    };
    if input_bound < "unnamed".len() {
        "unnamed".len()
    } else {
        input_bound
    }
}

fn push_slug(output: &mut String, value: &str, maximum: usize) {
    let start = output.len();
    let mut separator = false;
    for character in value.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            Some(character.to_ascii_lowercase())
        } else if matches!(character, '-' | '_') {
            Some(character)
        } else {
            None
        };
        match mapped {
            Some(character) if output.len() - start < maximum => {
                output.push(character);
                separator = false;
            }
            Some(_) => break,
            None if !separator && output.len() != start && output.len() - start < maximum => {
                output.push('_');
                separator = true;
            }
            None => {}
        }
    }
    while output.len() != start && (output.ends_with('_') || output.ends_with('-')) {
        output.pop();
    }
    if output.len() == start {
        output.push_str("unnamed");
    }
}

fn push_digest_hex(output: &mut String, digest: unity_asset_core::DigestV1) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &byte in digest.as_bytes() {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_media_slugs_keep_stable_identity_paths() {
        let source = SourceLocator::path("media.assets").unwrap();
        let texture = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let sprite = ObjectAddress::binary_direct(source.clone(), 42).unwrap();
        let mut budget = AssetLoadBudget::default();

        let texture_path = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();
        let sprite_path = allocate_path(
            None,
            &sprite,
            &source,
            class_ids::SPRITE,
            "Sprite",
            Some("UI?Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_ne!(texture_path, sprite_path);
        assert!(texture_path.as_str().contains("/ui_hero_icon--"));
        assert!(sprite_path.as_str().contains("/ui_hero_icon--"));

        let repeated = allocate_path(
            None,
            &texture,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(texture_path, repeated);
    }

    #[test]
    fn allocated_path_charges_every_retained_name_byte() {
        let source = SourceLocator::path("media.assets").unwrap();
        let address = ObjectAddress::binary_direct(source.clone(), 41).unwrap();
        let mut budget = AssetLoadBudget::default();

        let path = allocate_path(
            None,
            &address,
            &source,
            class_ids::TEXTURE_2D,
            "Texture2D",
            Some("UI/Hero Icon"),
            "png",
            false,
            &mut budget,
        )
        .unwrap();

        assert_eq!(budget.usage().bytes, path.retained_bytes().unwrap());
    }

    #[cfg(feature = "decode")]
    #[test]
    fn decoded_batch_builds_the_streamed_resource_index_once() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded)
            .with_filter(
                super::super::model::ExtractionFilter::new(
                    [class_ids::AUDIO_CLIP],
                    None,
                    None,
                    None,
                )
                .unwrap(),
            );
        let plan = ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .unwrap();

        assert!(plan.artifacts().len() > 1, "fixture must exercise a batch");
        assert_eq!(StreamedResourceResolver::test_build_count(), 1);
    }

    #[cfg(not(feature = "decode"))]
    #[test]
    fn unavailable_decode_does_not_build_a_streamed_resource_index() {
        let sample = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/banner_1");
        let mut workspace = crate::workspace::AssetWorkspace::new().unwrap();
        workspace
            .load_path(&sample, &mut AssetLoadBudget::default())
            .unwrap();
        let snapshot = workspace.snapshot();
        StreamedResourceResolver::reset_test_build_count();

        let request = ExtractionRequest::all(ExtractionRepresentationPolicy::RequireDecoded);
        ExtractionPlanner::new(&snapshot)
            .plan(request, &mut AssetLoadBudget::default())
            .expect_err("decode must remain unavailable without the feature");

        assert_eq!(StreamedResourceResolver::test_build_count(), 0);
    }
}
