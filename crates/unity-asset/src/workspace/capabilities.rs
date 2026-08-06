//! Stable capability discovery for the public workspace workflow.

use serde::Serialize;
use unity_asset_core::{CHANGE_SET_VERSION, SourceKind};

use crate::extraction::{
    BUNDLE_CONTAINER_QUERY_VERSION, BUNDLE_CONTAINER_RESULT_VERSION, EXTRACTION_MANIFEST_VERSION,
    EXTRACTION_PLAN_VERSION, EXTRACTION_REPORT_VERSION, EXTRACTION_REQUEST_VERSION,
};
use crate::reference::REFERENCE_GRAPH_PROJECTION_VERSION;

use super::commit::{
    COMMIT_REPORT_VERSION, CommitAtomicity, RECOVERY_DISCOVERY_VERSION, RECOVERY_LOCATOR_VERSION,
    RECOVERY_OUTCOME_VERSION, ROLLBACK_RECEIPT_VERSION,
};
use super::inspection::{
    STREAMED_RESOURCE_QUERY_VERSION, WORKSPACE_OBJECT_INSPECTION_VERSION,
    WORKSPACE_SOURCE_INSPECTION_VERSION,
};
use super::plan::MUTATION_PLAN_VERSION;
use super::preflight::PREPARE_REPORT_VERSION;

/// Stable name of the serialized workspace capability catalog.
pub const WORKSPACE_CAPABILITY_CATALOG_CONTRACT: &str = "unity_asset.workspace_capabilities";
/// Current wire version of the workspace capability catalog.
pub const WORKSPACE_CAPABILITY_CATALOG_VERSION: u16 = 3;

const CAPABILITIES: &[WorkspaceCapability] = &[
    WorkspaceCapability::SourceInspection,
    WorkspaceCapability::ObjectInspection,
    WorkspaceCapability::Plan,
    WorkspaceCapability::Prepare,
    WorkspaceCapability::Preview,
    WorkspaceCapability::Commit,
    WorkspaceCapability::Recover,
    WorkspaceCapability::Reference,
    WorkspaceCapability::Extraction,
    WorkspaceCapability::SearchHandoff,
];

const SOURCE_INSPECTION_KINDS: &[SourceKind] = &[
    SourceKind::Yaml,
    SourceKind::SerializedFile,
    SourceKind::AssetBundle,
    SourceKind::WebFile,
    SourceKind::Archive,
    SourceKind::StreamedResource,
];

const OBJECT_SOURCE_KINDS: &[SourceKind] = &[SourceKind::Yaml, SourceKind::SerializedFile];
const MUTATION_SOURCE_KINDS: &[SourceKind] = &[SourceKind::Yaml, SourceKind::SerializedFile];

const VIEW_KINDS: &[WorkspaceViewKind] =
    &[WorkspaceViewKind::Committed, WorkspaceViewKind::Prepared];

const MUTATION_FAMILIES: &[WorkspaceMutationFamily] = &[
    WorkspaceMutationFamily::FieldReplace,
    WorkspaceMutationFamily::ReferenceReplace,
    WorkspaceMutationFamily::SchemaReplace,
    WorkspaceMutationFamily::ResourceReplace,
    WorkspaceMutationFamily::SequenceEdit,
    WorkspaceMutationFamily::UnsafeRawReplace,
];

const PUBLICATION_ATOMICITY: &[CommitAtomicity] = &[CommitAtomicity::PerArtifactRecoverable];

/// Public workspace operation families discoverable without an untyped command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCapability {
    SourceInspection,
    ObjectInspection,
    Plan,
    Prepare,
    Preview,
    Commit,
    Recover,
    Reference,
    Extraction,
    SearchHandoff,
}

/// Workspace views accepted by a read-only capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceViewKind {
    Committed,
    Prepared,
}

/// Stable family names corresponding to the public mutation-plan operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMutationFamily {
    FieldReplace,
    ReferenceReplace,
    SchemaReplace,
    ResourceReplace,
    SequenceEdit,
    UnsafeRawReplace,
}

/// Typed artifact emitted after commit for a derived search consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSearchHandoffArtifact {
    ChangeSet,
}

/// Wire versions owned by the workspace and extraction contracts.
///
/// Reference queries and prepared previews remain typed in-process views rather than
/// independently persisted request envelopes. A prepared authority is deliberately absent:
/// only its report and projections can cross a serialization boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceContractVersions {
    mutation_plan: u8,
    change_set: u8,
    source_inspection: u8,
    object_inspection: u8,
    streamed_resource_query: u8,
    prepare_report: u8,
    commit_report: u8,
    recovery_locator: u8,
    recovery_discovery: u8,
    recovery_outcome: u8,
    rollback_receipt: u8,
    bundle_container_query: u8,
    bundle_container_result: u8,
    reference_graph_projection: u8,
    extraction_request: u8,
    extraction_plan: u8,
    extraction_manifest: u8,
    extraction_report: u8,
}

impl WorkspaceContractVersions {
    const CURRENT: Self = Self {
        mutation_plan: MUTATION_PLAN_VERSION,
        change_set: CHANGE_SET_VERSION,
        source_inspection: WORKSPACE_SOURCE_INSPECTION_VERSION,
        object_inspection: WORKSPACE_OBJECT_INSPECTION_VERSION,
        streamed_resource_query: STREAMED_RESOURCE_QUERY_VERSION,
        prepare_report: PREPARE_REPORT_VERSION,
        commit_report: COMMIT_REPORT_VERSION,
        recovery_locator: RECOVERY_LOCATOR_VERSION,
        recovery_discovery: RECOVERY_DISCOVERY_VERSION,
        recovery_outcome: RECOVERY_OUTCOME_VERSION,
        rollback_receipt: ROLLBACK_RECEIPT_VERSION,
        bundle_container_query: BUNDLE_CONTAINER_QUERY_VERSION,
        bundle_container_result: BUNDLE_CONTAINER_RESULT_VERSION,
        reference_graph_projection: REFERENCE_GRAPH_PROJECTION_VERSION,
        extraction_request: EXTRACTION_REQUEST_VERSION,
        extraction_plan: EXTRACTION_PLAN_VERSION,
        extraction_manifest: EXTRACTION_MANIFEST_VERSION,
        extraction_report: EXTRACTION_REPORT_VERSION,
    };

    #[must_use]
    pub const fn mutation_plan(self) -> u8 {
        self.mutation_plan
    }

    #[must_use]
    pub const fn change_set(self) -> u8 {
        self.change_set
    }

    #[must_use]
    pub const fn source_inspection(self) -> u8 {
        self.source_inspection
    }

    #[must_use]
    pub const fn object_inspection(self) -> u8 {
        self.object_inspection
    }

    #[must_use]
    pub const fn streamed_resource_query(self) -> u8 {
        self.streamed_resource_query
    }

    #[must_use]
    pub const fn prepare_report(self) -> u8 {
        self.prepare_report
    }

    #[must_use]
    pub const fn commit_report(self) -> u8 {
        self.commit_report
    }

    #[must_use]
    pub const fn recovery_locator(self) -> u8 {
        self.recovery_locator
    }

    #[must_use]
    pub const fn recovery_discovery(self) -> u8 {
        self.recovery_discovery
    }

    #[must_use]
    pub const fn recovery_outcome(self) -> u8 {
        self.recovery_outcome
    }

    #[must_use]
    pub const fn rollback_receipt(self) -> u8 {
        self.rollback_receipt
    }

    #[must_use]
    pub const fn bundle_container_query(self) -> u8 {
        self.bundle_container_query
    }

    #[must_use]
    pub const fn bundle_container_result(self) -> u8 {
        self.bundle_container_result
    }

    #[must_use]
    pub const fn reference_graph_projection(self) -> u8 {
        self.reference_graph_projection
    }

    #[must_use]
    pub const fn extraction_request(self) -> u8 {
        self.extraction_request
    }

    #[must_use]
    pub const fn extraction_plan(self) -> u8 {
        self.extraction_plan
    }

    #[must_use]
    pub const fn extraction_manifest(self) -> u8 {
        self.extraction_manifest
    }

    #[must_use]
    pub const fn extraction_report(self) -> u8 {
        self.extraction_report
    }
}

/// Source kinds and immutable view families accepted by an inspection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceInspectionCapability {
    source_kinds: &'static [SourceKind],
    views: &'static [WorkspaceViewKind],
}

impl WorkspaceInspectionCapability {
    #[must_use]
    pub const fn source_kinds(self) -> &'static [SourceKind] {
        self.source_kinds
    }

    #[must_use]
    pub const fn views(self) -> &'static [WorkspaceViewKind] {
        self.views
    }
}

/// Immutable workspace views accepted by a derived read capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceViewCapability {
    views: &'static [WorkspaceViewKind],
}

impl WorkspaceViewCapability {
    #[must_use]
    pub const fn views(self) -> &'static [WorkspaceViewKind] {
        self.views
    }
}

/// Mutation families and object-bearing source kinds accepted by prepare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceMutationCapability {
    source_kinds: &'static [SourceKind],
    families: &'static [WorkspaceMutationFamily],
}

impl WorkspaceMutationCapability {
    #[must_use]
    pub const fn source_kinds(self) -> &'static [SourceKind] {
        self.source_kinds
    }

    #[must_use]
    pub const fn families(self) -> &'static [WorkspaceMutationFamily] {
        self.families
    }
}

/// Publication guarantees currently implemented by commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspacePublicationCapability {
    atomicity: &'static [CommitAtomicity],
}

impl WorkspacePublicationCapability {
    #[must_use]
    pub const fn atomicity(self) -> &'static [CommitAtomicity] {
        self.atomicity
    }
}

/// Authority semantics of a successful prepare operation.
///
/// `PreparedChange` is an opaque, one-use capability. Serialization of its report does not
/// recreate commit authority, and commit consumes the original value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspacePreparedAuthorityCapability {
    serializable: bool,
    reconstructible_from_report: bool,
    single_use: bool,
    commit_consumes: bool,
    preview_available: bool,
}

impl WorkspacePreparedAuthorityCapability {
    #[must_use]
    pub const fn serializable(self) -> bool {
        self.serializable
    }

    #[must_use]
    pub const fn reconstructible_from_report(self) -> bool {
        self.reconstructible_from_report
    }

    #[must_use]
    pub const fn single_use(self) -> bool {
        self.single_use
    }

    #[must_use]
    pub const fn commit_consumes(self) -> bool {
        self.commit_consumes
    }

    #[must_use]
    pub const fn preview_available(self) -> bool {
        self.preview_available
    }
}

/// Search handoff emitted by the authoritative workspace after commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceSearchHandoffCapability {
    artifact: WorkspaceSearchHandoffArtifact,
    revision_bound: bool,
    transaction_keyed: bool,
    consumer_owned: bool,
}

impl WorkspaceSearchHandoffCapability {
    #[must_use]
    pub const fn artifact(self) -> WorkspaceSearchHandoffArtifact {
        self.artifact
    }

    #[must_use]
    pub const fn revision_bound(self) -> bool {
        self.revision_bound
    }

    #[must_use]
    pub const fn transaction_keyed(self) -> bool {
        self.transaction_keyed
    }

    #[must_use]
    pub const fn consumer_owned(self) -> bool {
        self.consumer_owned
    }
}

/// Machine-facing interaction constraints shared by Rust and transport adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceAutomationCapability {
    structured_input: bool,
    display_text_input: bool,
    generic_command_bus: bool,
}

impl WorkspaceAutomationCapability {
    #[must_use]
    pub const fn structured_input(self) -> bool {
        self.structured_input
    }

    #[must_use]
    pub const fn display_text_input(self) -> bool {
        self.display_text_input
    }

    #[must_use]
    pub const fn generic_command_bus(self) -> bool {
        self.generic_command_bus
    }
}

/// Stable, allocation-free catalog of the public workspace workflow.
///
/// This DTO is intentionally serialize-only. It describes typed public entry points; it is not
/// a request envelope and cannot dispatch an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WorkspaceCapabilityCatalog {
    contract: &'static str,
    contract_version: u16,
    capabilities: &'static [WorkspaceCapability],
    contracts: WorkspaceContractVersions,
    source_inspection: WorkspaceInspectionCapability,
    object_inspection: WorkspaceInspectionCapability,
    mutation: WorkspaceMutationCapability,
    publication: WorkspacePublicationCapability,
    prepared_authority: WorkspacePreparedAuthorityCapability,
    reference: WorkspaceViewCapability,
    extraction: WorkspaceViewCapability,
    search_handoff: WorkspaceSearchHandoffCapability,
    automation: WorkspaceAutomationCapability,
}

impl WorkspaceCapabilityCatalog {
    #[must_use]
    pub const fn contract(self) -> &'static str {
        self.contract
    }

    #[must_use]
    pub const fn contract_version(self) -> u16 {
        self.contract_version
    }

    #[must_use]
    pub const fn capabilities(self) -> &'static [WorkspaceCapability] {
        self.capabilities
    }

    #[must_use]
    pub const fn contracts(self) -> WorkspaceContractVersions {
        self.contracts
    }

    #[must_use]
    pub const fn source_inspection(self) -> WorkspaceInspectionCapability {
        self.source_inspection
    }

    #[must_use]
    pub const fn object_inspection(self) -> WorkspaceInspectionCapability {
        self.object_inspection
    }

    #[must_use]
    pub const fn mutation(self) -> WorkspaceMutationCapability {
        self.mutation
    }

    #[must_use]
    pub const fn publication(self) -> WorkspacePublicationCapability {
        self.publication
    }

    #[must_use]
    pub const fn prepared_authority(self) -> WorkspacePreparedAuthorityCapability {
        self.prepared_authority
    }

    #[must_use]
    pub const fn reference(self) -> WorkspaceViewCapability {
        self.reference
    }

    #[must_use]
    pub const fn extraction(self) -> WorkspaceViewCapability {
        self.extraction
    }

    #[must_use]
    pub const fn search_handoff(self) -> WorkspaceSearchHandoffCapability {
        self.search_handoff
    }

    #[must_use]
    pub const fn automation(self) -> WorkspaceAutomationCapability {
        self.automation
    }
}

/// Returns the current public workspace capability catalog without allocation.
#[must_use]
pub const fn workspace_capabilities() -> WorkspaceCapabilityCatalog {
    WorkspaceCapabilityCatalog {
        contract: WORKSPACE_CAPABILITY_CATALOG_CONTRACT,
        contract_version: WORKSPACE_CAPABILITY_CATALOG_VERSION,
        capabilities: CAPABILITIES,
        contracts: WorkspaceContractVersions::CURRENT,
        source_inspection: WorkspaceInspectionCapability {
            source_kinds: SOURCE_INSPECTION_KINDS,
            views: VIEW_KINDS,
        },
        object_inspection: WorkspaceInspectionCapability {
            source_kinds: OBJECT_SOURCE_KINDS,
            views: VIEW_KINDS,
        },
        mutation: WorkspaceMutationCapability {
            source_kinds: MUTATION_SOURCE_KINDS,
            families: MUTATION_FAMILIES,
        },
        publication: WorkspacePublicationCapability {
            atomicity: PUBLICATION_ATOMICITY,
        },
        prepared_authority: WorkspacePreparedAuthorityCapability {
            serializable: false,
            reconstructible_from_report: false,
            single_use: true,
            commit_consumes: true,
            preview_available: true,
        },
        reference: WorkspaceViewCapability { views: VIEW_KINDS },
        extraction: WorkspaceViewCapability { views: VIEW_KINDS },
        search_handoff: WorkspaceSearchHandoffCapability {
            artifact: WorkspaceSearchHandoffArtifact::ChangeSet,
            revision_bound: true,
            transaction_keyed: true,
            consumer_owned: true,
        },
        automation: WorkspaceAutomationCapability {
            structured_input: true,
            display_text_input: false,
            generic_command_bus: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_catalog_has_stable_version_and_semantics() {
        let catalog = workspace_capabilities();

        assert_eq!(catalog.contract(), WORKSPACE_CAPABILITY_CATALOG_CONTRACT);
        assert_eq!(
            catalog.contract_version(),
            WORKSPACE_CAPABILITY_CATALOG_VERSION
        );
        assert_eq!(catalog.contracts().mutation_plan(), MUTATION_PLAN_VERSION);
        assert_eq!(catalog.contracts().change_set(), CHANGE_SET_VERSION);
        assert_eq!(
            catalog.contracts().reference_graph_projection(),
            REFERENCE_GRAPH_PROJECTION_VERSION
        );
        assert_eq!(
            catalog.source_inspection().source_kinds(),
            SOURCE_INSPECTION_KINDS
        );
        assert_eq!(
            catalog.object_inspection().source_kinds(),
            OBJECT_SOURCE_KINDS
        );
        assert_eq!(catalog.mutation().families(), MUTATION_FAMILIES);
        assert_eq!(catalog.publication().atomicity(), PUBLICATION_ATOMICITY);
        assert!(!catalog.prepared_authority().serializable());
        assert!(!catalog.prepared_authority().reconstructible_from_report());
        assert!(catalog.prepared_authority().single_use());
        assert!(catalog.prepared_authority().commit_consumes());
        assert!(catalog.prepared_authority().preview_available());
        assert!(catalog.automation().structured_input());
        assert!(!catalog.automation().display_text_input());
        assert!(!catalog.automation().generic_command_bus());
    }

    #[test]
    fn json_field_order_and_contract_versions_are_stable() {
        let json = serde_json::to_string(&workspace_capabilities()).unwrap();
        let expected = concat!(
            r#"{"contract":"unity_asset.workspace_capabilities","contract_version":3,"#,
            r#""capabilities":["source_inspection","object_inspection","plan","prepare","#,
            r#""preview","commit","recover","reference","extraction","search_handoff"],"#,
            r#""contracts":{"mutation_plan":3,"change_set":2,"source_inspection":1,"#,
            r#""object_inspection":2,"#,
            r#""streamed_resource_query":2,"prepare_report":2,"commit_report":3,"#,
            r#""recovery_locator":1,"recovery_discovery":1,"recovery_outcome":3,"#,
            r#""rollback_receipt":3,"bundle_container_query":1,"bundle_container_result":2,"#,
            r#""reference_graph_projection":2,"extraction_request":4,"extraction_plan":7,"#,
            r#""extraction_manifest":6,"#,
            r#""extraction_report":6},"#,
            r#""source_inspection":{"source_kinds":["yaml","serialized_file","asset_bundle","#,
            r#""web_file","archive","streamed_resource"],"views":["committed","prepared"]},"#,
            r#""object_inspection":{"source_kinds":["yaml","serialized_file"],"#,
            r#""views":["committed","prepared"]},"#,
            r#""mutation":{"source_kinds":["yaml","serialized_file"],"#,
            r#""families":["field_replace","reference_replace","schema_replace","#,
            r#""resource_replace","sequence_edit","unsafe_raw_replace"]},"#,
            r#""publication":{"atomicity":["per_artifact_recoverable"]},"#,
            r#""prepared_authority":{"serializable":false,"reconstructible_from_report":false,"#,
            r#""single_use":true,"commit_consumes":true,"preview_available":true},"#,
            r#""reference":{"views":["committed","prepared"]},"#,
            r#""extraction":{"views":["committed","prepared"]},"#,
            r#""search_handoff":{"artifact":"change_set","revision_bound":true,"#,
            r#""transaction_keyed":true,"consumer_owned":true},"#,
            r#""automation":{"structured_input":true,"display_text_input":false,"#,
            r#""generic_command_bus":false}}"#,
        );

        assert_eq!(json, expected);
    }
}
