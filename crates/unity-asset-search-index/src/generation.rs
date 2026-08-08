use std::fmt;
use std::mem::size_of;

use serde::{Deserialize, Deserializer, Serialize};
use unity_asset_core::{DigestV1, WorkspaceId, WorkspaceRevision};

use crate::ProjectPathSet;
use crate::semantics::SearchSemantics;

/// Version of the durable generation namespace and activation-to-directory binding.
pub(crate) const SEARCH_GENERATION_STORAGE_CONTRACT_VERSION: u16 = 5;
/// The previous storage contract coupled the physical namespace to the manifest envelope.
pub(crate) const LEGACY_COUPLED_GENERATION_STORAGE_CONTRACT_VERSION: u16 = 4;
/// The older storage contract kept source-state v4 as an opaque projection sidecar.
#[cfg(test)]
pub(crate) const LEGACY_SOURCE_STATE_V4_STORAGE_CONTRACT_VERSION: u16 = 3;
/// Version of the manifest and artifact-evidence envelope.
///
/// The namespace can migrate independently from the logical manifest. Keeping this contract
/// stable lets a rebuilt index continue serving a validated stale projection while its source
/// state is being upgraded.
pub(crate) const SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION: u16 = 3;
const GENERATION_ID_DOMAIN: &[u8] = b"unity-asset:search-generation:logical:v3\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SearchGenerationId(DigestV1);

impl SearchGenerationId {
    #[must_use]
    pub const fn new(digest: DigestV1) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(self) -> DigestV1 {
        self.0
    }

    /// Returns the portable directory component used by the generation store.
    #[must_use]
    pub(crate) fn directory_name(self) -> String {
        self.directory_name_for_storage_contract(SEARCH_GENERATION_STORAGE_CONTRACT_VERSION)
    }

    /// Returns the directory component used by a specific generation storage contract.
    #[must_use]
    pub(crate) fn directory_name_for_storage_contract(self, contract: u16) -> String {
        format!("generation-v{contract}-{}", hex::encode(self.0.as_bytes()))
    }

    /// Parses a directory component emitted by [`Self::directory_name`].
    #[must_use]
    pub(crate) fn from_directory_name(value: &str) -> Option<Self> {
        Self::from_directory_name_for_storage_contract(
            value,
            SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
        )
    }

    /// Parses a directory component emitted by a specific generation storage contract.
    #[must_use]
    pub(crate) fn from_directory_name_for_storage_contract(
        value: &str,
        contract: u16,
    ) -> Option<Self> {
        let prefix = format!("generation-v{contract}-");
        let encoded = value.strip_prefix(&prefix)?;
        if encoded.len() != DigestV1::BYTE_LEN * 2
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return None;
        }
        let mut bytes = [0_u8; DigestV1::BYTE_LEN];
        hex::decode_to_slice(encoded, &mut bytes).ok()?;
        Some(Self::new(DigestV1::from_bytes(bytes)))
    }
}

impl fmt::Display for SearchGenerationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationStamp {
    pub(crate) generation: SearchGenerationId,
    pub(crate) workspace: WorkspaceId,
    pub(crate) actual_revision: WorkspaceRevision,
    pub(crate) desired_revision: WorkspaceRevision,
    pub(crate) semantics_current: bool,
    pub(crate) configuration_current: bool,
    pub(crate) stale: bool,
}

impl GenerationStamp {
    #[must_use]
    pub const fn current(
        generation: SearchGenerationId,
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
    ) -> Self {
        Self {
            generation,
            workspace,
            actual_revision: revision,
            desired_revision: revision,
            semantics_current: true,
            configuration_current: true,
            stale: false,
        }
    }

    #[must_use]
    pub fn with_desired_revision(mut self, desired_revision: WorkspaceRevision) -> Self {
        self.desired_revision = desired_revision;
        self.refresh_stale();
        self
    }

    #[must_use]
    pub fn with_semantics_current(mut self, semantics_current: bool) -> Self {
        self.semantics_current = semantics_current;
        self.refresh_stale();
        self
    }

    #[must_use]
    pub fn with_configuration_current(mut self, configuration_current: bool) -> Self {
        self.configuration_current = configuration_current;
        self.refresh_stale();
        self
    }

    fn refresh_stale(&mut self) {
        self.stale = self.actual_revision != self.desired_revision
            || !self.semantics_current
            || !self.configuration_current;
    }
}

/// Filesystem discovery work accepted by [`crate::SearchIndex::reindex`].
///
/// Authoritative workspace change sets are intentionally not representable here; they require
/// [`crate::SearchIndex::reindex_workspace`] and its revision-bound [`unity_asset::workspace::WorkspaceView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesystemReindexScope {
    Full,
    Reconcile,
    ChangedPaths { paths: ProjectPathSet },
}

/// In-process request for one filesystem-backed search generation build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemReindexIntent {
    pub scope: FilesystemReindexScope,
}

impl FilesystemReindexIntent {
    #[must_use]
    pub const fn full() -> Self {
        Self {
            scope: FilesystemReindexScope::Full,
        }
    }

    #[must_use]
    pub const fn reconcile() -> Self {
        Self {
            scope: FilesystemReindexScope::Reconcile,
        }
    }

    #[must_use]
    pub fn changed_paths(paths: ProjectPathSet) -> Self {
        Self {
            scope: FilesystemReindexScope::ChangedPaths { paths },
        }
    }
}

/// Evidence for one immutable artifact tree in a completed generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ArtifactTreeEvidence {
    contract_version: u16,
    digest: DigestV1,
    files: u64,
    bytes: u64,
}

impl ArtifactTreeEvidence {
    #[must_use]
    pub const fn new(digest: DigestV1, files: u64, bytes: u64) -> Self {
        Self {
            contract_version: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
            digest,
            files,
            bytes,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn files(self) -> u64 {
        self.files
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactTreeEvidenceWire {
    contract_version: u16,
    digest: DigestV1,
    files: u64,
    bytes: u64,
}

impl<'de> Deserialize<'de> for ArtifactTreeEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ArtifactTreeEvidenceWire::deserialize(deserializer)?;
        validate_generation_manifest_contract_version::<D::Error>(
            "artifact tree evidence",
            wire.contract_version,
        )?;
        Ok(Self::new(wire.digest, wire.files, wire.bytes))
    }
}

/// Physical evidence for every independently readable generation projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct GenerationArtifactEvidence {
    contract_version: u16,
    search: ArtifactTreeEvidence,
    references: ArtifactTreeEvidence,
    source_state: ArtifactTreeEvidence,
}

impl GenerationArtifactEvidence {
    #[must_use]
    pub const fn new(
        search: ArtifactTreeEvidence,
        references: ArtifactTreeEvidence,
        source_state: ArtifactTreeEvidence,
    ) -> Self {
        Self {
            contract_version: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
            search,
            references,
            source_state,
        }
    }

    #[must_use]
    pub const fn source_state(self) -> ArtifactTreeEvidence {
        self.source_state
    }

    #[must_use]
    pub const fn total_bytes(self) -> Option<u64> {
        match self.search.bytes.checked_add(self.references.bytes) {
            Some(bytes) => bytes.checked_add(self.source_state.bytes),
            None => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationArtifactEvidenceWire {
    contract_version: u16,
    search: ArtifactTreeEvidence,
    references: ArtifactTreeEvidence,
    source_state: ArtifactTreeEvidence,
}

impl<'de> Deserialize<'de> for GenerationArtifactEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GenerationArtifactEvidenceWire::deserialize(deserializer)?;
        validate_generation_manifest_contract_version::<D::Error>(
            "generation artifact evidence",
            wire.contract_version,
        )?;
        Ok(Self::new(wire.search, wire.references, wire.source_state))
    }
}

/// Logical digests produced by the shared search and reference projection pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationProjectionDigests {
    search: DigestV1,
    references: DigestV1,
}

impl GenerationProjectionDigests {
    #[must_use]
    pub const fn new(search: DigestV1, references: DigestV1) -> Self {
        Self { search, references }
    }

    #[must_use]
    pub const fn search(self) -> DigestV1 {
        self.search
    }

    #[must_use]
    pub const fn references(self) -> DigestV1 {
        self.references
    }
}

/// Persisted counts for the exact projection represented by a generation.
///
/// These values are produced during projection and must be reused after restart. Recomputing them
/// from source state with current options would describe a different logical generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct GenerationProjectionSummary {
    contract_version: u16,
    assets: u64,
    search_documents: u64,
    reference_documents: u64,
    projection_truncations: u64,
    incomplete_assets: u64,
}

impl GenerationProjectionSummary {
    pub fn new(
        assets: u64,
        search_documents: u64,
        reference_documents: u64,
        projection_truncations: u64,
        incomplete_assets: u64,
    ) -> Result<Self, GenerationManifestError> {
        if incomplete_assets > assets {
            return Err(GenerationManifestError::InvalidProjectionSummary {
                assets,
                incomplete_assets,
            });
        }
        Ok(Self {
            contract_version: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
            assets,
            search_documents,
            reference_documents,
            projection_truncations,
            incomplete_assets,
        })
    }

    #[must_use]
    pub const fn assets(self) -> u64 {
        self.assets
    }

    #[must_use]
    pub const fn search_documents(self) -> u64 {
        self.search_documents
    }

    #[must_use]
    pub const fn reference_documents(self) -> u64 {
        self.reference_documents
    }

    #[must_use]
    pub const fn projection_truncations(self) -> u64 {
        self.projection_truncations
    }

    #[must_use]
    pub const fn incomplete_assets(self) -> u64 {
        self.incomplete_assets
    }
}

impl Default for GenerationProjectionSummary {
    fn default() -> Self {
        Self {
            contract_version: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
            assets: 0,
            search_documents: 0,
            reference_documents: 0,
            projection_truncations: 0,
            incomplete_assets: 0,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationProjectionSummaryWire {
    contract_version: u16,
    assets: u64,
    search_documents: u64,
    reference_documents: u64,
    projection_truncations: u64,
    incomplete_assets: u64,
}

impl<'de> Deserialize<'de> for GenerationProjectionSummary {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GenerationProjectionSummaryWire::deserialize(deserializer)?;
        validate_generation_manifest_contract_version::<D::Error>(
            "generation projection summary",
            wire.contract_version,
        )?;
        Self::new(
            wire.assets,
            wire.search_documents,
            wire.reference_documents,
            wire.projection_truncations,
            wire.incomplete_assets,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Canonical logical identity used to construct a generation manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchGenerationIdentityV1 {
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    projections: GenerationProjectionDigests,
    projection_summary: GenerationProjectionSummary,
    semantics: SearchSemantics,
    options_digest: DigestV1,
    source_state_digest: DigestV1,
}

impl SearchGenerationIdentityV1 {
    #[cfg(test)]
    pub fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        projections: GenerationProjectionDigests,
        projection_summary: GenerationProjectionSummary,
        options_digest: DigestV1,
        source_state_digest: DigestV1,
    ) -> Result<Self, GenerationManifestError> {
        Self::new_with_semantics(
            workspace,
            revision,
            projections,
            projection_summary,
            SearchSemantics::current(),
            options_digest,
            source_state_digest,
        )
    }

    pub fn new_with_semantics(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        projections: GenerationProjectionDigests,
        projection_summary: GenerationProjectionSummary,
        semantics: SearchSemantics,
        options_digest: DigestV1,
        source_state_digest: DigestV1,
    ) -> Result<Self, GenerationManifestError> {
        Ok(Self {
            workspace,
            revision,
            projections,
            projection_summary,
            semantics,
            options_digest,
            source_state_digest,
        })
    }
}

/// Immutable metadata for one complete search and reference generation.
///
/// Physical artifact evidence is deliberately excluded from [`Self::generation_id`]. Tantivy may
/// produce a different segment layout while representing the same revision and logical
/// projections; that implementation detail must not change the generation's logical identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SearchGenerationManifestV1 {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    search_projection_digest: DigestV1,
    reference_projection_digest: DigestV1,
    projection_summary: GenerationProjectionSummary,
    semantics: SearchSemantics,
    options_digest: DigestV1,
    source_state_digest: DigestV1,
    artifacts: GenerationArtifactEvidence,
}

impl SearchGenerationManifestV1 {
    #[must_use]
    pub fn new(
        identity: SearchGenerationIdentityV1,
        artifacts: GenerationArtifactEvidence,
    ) -> Self {
        Self {
            contract_version: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
            workspace: identity.workspace,
            revision: identity.revision,
            search_projection_digest: identity.projections.search,
            reference_projection_digest: identity.projections.references,
            projection_summary: identity.projection_summary,
            semantics: identity.semantics,
            options_digest: identity.options_digest,
            source_state_digest: identity.source_state_digest,
            artifacts,
        }
    }

    #[must_use]
    pub const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub const fn projection_summary(&self) -> GenerationProjectionSummary {
        self.projection_summary
    }

    #[must_use]
    pub(crate) const fn semantics(&self) -> SearchSemantics {
        self.semantics
    }

    #[must_use]
    pub const fn options_digest(&self) -> DigestV1 {
        self.options_digest
    }

    #[must_use]
    pub const fn source_state_digest(&self) -> DigestV1 {
        self.source_state_digest
    }

    #[must_use]
    pub const fn artifacts(&self) -> GenerationArtifactEvidence {
        self.artifacts
    }

    /// Computes the content address of the logical generation identity.
    #[must_use]
    pub fn generation_id(&self) -> SearchGenerationId {
        let mut canonical = Vec::with_capacity(
            GENERATION_ID_DOMAIN.len()
                + 16
                + DigestV1::BYTE_LEN * 8
                + 3 * size_of::<u16>()
                + 5 * size_of::<u64>(),
        );
        canonical.extend_from_slice(GENERATION_ID_DOMAIN);
        canonical.extend_from_slice(&self.workspace.get().to_le_bytes());
        canonical.extend_from_slice(self.revision.digest().as_bytes());
        canonical.extend_from_slice(self.search_projection_digest.as_bytes());
        canonical.extend_from_slice(self.reference_projection_digest.as_bytes());
        canonical.extend_from_slice(&self.semantics.analysis_version().to_le_bytes());
        canonical.extend_from_slice(self.semantics.analysis_digest().as_bytes());
        canonical.extend_from_slice(&self.semantics.search_projection_version().to_le_bytes());
        canonical.extend_from_slice(self.semantics.search_projection_digest().as_bytes());
        canonical.extend_from_slice(&self.semantics.reference_projection_version().to_le_bytes());
        canonical.extend_from_slice(self.semantics.reference_projection_digest().as_bytes());
        canonical.extend_from_slice(&self.projection_summary.assets.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.search_documents.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.reference_documents.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.projection_truncations.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.incomplete_assets.to_le_bytes());
        canonical.extend_from_slice(self.options_digest.as_bytes());
        canonical.extend_from_slice(self.source_state_digest.as_bytes());
        SearchGenerationId::new(DigestV1::hash_bytes(&canonical))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchGenerationManifestWire {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    search_projection_digest: DigestV1,
    reference_projection_digest: DigestV1,
    projection_summary: GenerationProjectionSummary,
    semantics: SearchSemantics,
    options_digest: DigestV1,
    source_state_digest: DigestV1,
    artifacts: GenerationArtifactEvidence,
}

impl<'de> Deserialize<'de> for SearchGenerationManifestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SearchGenerationManifestWire::deserialize(deserializer)?;
        validate_generation_manifest_contract_version::<D::Error>(
            "search generation manifest",
            wire.contract_version,
        )?;
        let identity = SearchGenerationIdentityV1::new_with_semantics(
            wire.workspace,
            wire.revision,
            GenerationProjectionDigests::new(
                wire.search_projection_digest,
                wire.reference_projection_digest,
            ),
            wire.projection_summary,
            wire.semantics,
            wire.options_digest,
            wire.source_state_digest,
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self::new(identity, wire.artifacts))
    }
}

fn validate_generation_manifest_contract_version<E>(
    contract: &'static str,
    actual: u16,
) -> Result<(), E>
where
    E: serde::de::Error,
{
    if !is_supported_generation_manifest_contract_version(actual) {
        return Err(E::custom(GenerationManifestError::UnsupportedVersion {
            contract,
            actual,
            expected: SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION,
        }));
    }
    Ok(())
}

pub(crate) const fn is_supported_generation_manifest_contract_version(actual: u16) -> bool {
    matches!(
        actual,
        SEARCH_GENERATION_MANIFEST_CONTRACT_VERSION
            | LEGACY_COUPLED_GENERATION_STORAGE_CONTRACT_VERSION
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GenerationManifestError {
    UnsupportedVersion {
        contract: &'static str,
        actual: u16,
        expected: u16,
    },
    InvalidProjectionSummary {
        assets: u64,
        incomplete_assets: u64,
    },
}

impl fmt::Display for GenerationManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion {
                contract,
                actual,
                expected,
            } => write!(
                formatter,
                "{contract} version {actual} is unsupported; expected {expected}"
            ),
            Self::InvalidProjectionSummary {
                assets,
                incomplete_assets,
            } => write!(
                formatter,
                "projection summary contains {incomplete_assets} incomplete assets but only {assets} assets"
            ),
        }
    }
}

impl std::error::Error for GenerationManifestError {}
