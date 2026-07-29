use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use unity_asset_core::{DigestV1, TransactionId, WorkspaceId, WorkspaceRevision};

pub(crate) const SEARCH_GENERATION_STORAGE_CONTRACT_VERSION: u16 = 1;
const GENERATION_DIRECTORY_PREFIX: &str = "generation-v1-";
const GENERATION_ID_DOMAIN: &[u8] = b"unity-asset:search-generation:logical:v1\0";
const MAX_APPLIED_TRANSACTIONS: usize = 4_096;

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
        format!(
            "{GENERATION_DIRECTORY_PREFIX}{}",
            hex::encode(self.0.as_bytes())
        )
    }

    /// Parses a directory component emitted by [`Self::directory_name`].
    #[must_use]
    pub(crate) fn from_directory_name(value: &str) -> Option<Self> {
        let encoded = value.strip_prefix(GENERATION_DIRECTORY_PREFIX)?;
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
            stale: false,
        }
    }

    #[must_use]
    pub fn with_desired_revision(mut self, desired_revision: WorkspaceRevision) -> Self {
        self.desired_revision = desired_revision;
        self.stale = self.actual_revision != desired_revision;
        self
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
    ChangedPaths { paths: Vec<PathBuf> },
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
    pub fn changed_paths(paths: Vec<PathBuf>) -> Self {
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
            contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
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
        validate_storage_contract_version::<D::Error>(
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
            contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
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
        validate_storage_contract_version::<D::Error>(
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
            contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
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
            contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
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
        validate_storage_contract_version::<D::Error>(
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
    parent_generation: Option<SearchGenerationId>,
    applied_transactions: Vec<TransactionId>,
    options_digest: DigestV1,
    source_state_digest: DigestV1,
}

impl SearchGenerationIdentityV1 {
    pub fn new(
        workspace: WorkspaceId,
        revision: WorkspaceRevision,
        projections: GenerationProjectionDigests,
        projection_summary: GenerationProjectionSummary,
        parent_generation: Option<SearchGenerationId>,
        mut applied_transactions: Vec<TransactionId>,
        options_digest: DigestV1,
        source_state_digest: DigestV1,
    ) -> Result<Self, GenerationManifestError> {
        if applied_transactions.len() > MAX_APPLIED_TRANSACTIONS {
            return Err(GenerationManifestError::TooManyAppliedTransactions {
                actual: applied_transactions.len(),
                maximum: MAX_APPLIED_TRANSACTIONS,
            });
        }
        applied_transactions.sort_unstable();
        applied_transactions.dedup();
        Ok(Self {
            workspace,
            revision,
            projections,
            projection_summary,
            parent_generation,
            applied_transactions,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_generation: Option<SearchGenerationId>,
    applied_transactions: Vec<TransactionId>,
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
            contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
            workspace: identity.workspace,
            revision: identity.revision,
            search_projection_digest: identity.projections.search,
            reference_projection_digest: identity.projections.references,
            projection_summary: identity.projection_summary,
            parent_generation: identity.parent_generation,
            applied_transactions: identity.applied_transactions,
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
    pub const fn parent_generation(&self) -> Option<SearchGenerationId> {
        self.parent_generation
    }

    #[must_use]
    pub fn applied_transactions(&self) -> &[TransactionId] {
        &self.applied_transactions
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
        let transaction_bytes = self.applied_transactions.len() * DigestV1::BYTE_LEN;
        let mut canonical = Vec::with_capacity(
            GENERATION_ID_DOMAIN.len() + 16 + DigestV1::BYTE_LEN * 6 + 1 + 8 + transaction_bytes,
        );
        canonical.extend_from_slice(GENERATION_ID_DOMAIN);
        canonical.extend_from_slice(&self.workspace.get().to_le_bytes());
        canonical.extend_from_slice(self.revision.digest().as_bytes());
        canonical.extend_from_slice(self.search_projection_digest.as_bytes());
        canonical.extend_from_slice(self.reference_projection_digest.as_bytes());
        canonical.extend_from_slice(&self.projection_summary.assets.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.search_documents.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.reference_documents.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.projection_truncations.to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.incomplete_assets.to_le_bytes());
        match self.parent_generation {
            Some(parent) => {
                canonical.push(1);
                canonical.extend_from_slice(parent.digest().as_bytes());
            }
            None => {
                canonical.push(0);
                canonical.extend_from_slice(&[0_u8; DigestV1::BYTE_LEN]);
            }
        }
        canonical.extend_from_slice(&(self.applied_transactions.len() as u64).to_le_bytes());
        for transaction in &self.applied_transactions {
            canonical.extend_from_slice(transaction.digest().as_bytes());
        }
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
    parent_generation: Option<SearchGenerationId>,
    applied_transactions: Vec<TransactionId>,
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
        validate_storage_contract_version::<D::Error>(
            "search generation manifest",
            wire.contract_version,
        )?;
        ensure_canonical_transactions::<D::Error>(&wire.applied_transactions)?;
        let identity = SearchGenerationIdentityV1::new(
            wire.workspace,
            wire.revision,
            GenerationProjectionDigests::new(
                wire.search_projection_digest,
                wire.reference_projection_digest,
            ),
            wire.projection_summary,
            wire.parent_generation,
            wire.applied_transactions,
            wire.options_digest,
            wire.source_state_digest,
        )
        .map_err(serde::de::Error::custom)?;
        Ok(Self::new(identity, wire.artifacts))
    }
}

fn ensure_canonical_transactions<E>(transactions: &[TransactionId]) -> Result<(), E>
where
    E: serde::de::Error,
{
    if transactions.len() > MAX_APPLIED_TRANSACTIONS {
        return Err(E::custom(
            GenerationManifestError::TooManyAppliedTransactions {
                actual: transactions.len(),
                maximum: MAX_APPLIED_TRANSACTIONS,
            },
        ));
    }
    let unique = transactions.iter().copied().collect::<BTreeSet<_>>();
    let canonical = unique.into_iter().collect::<Vec<_>>();
    if canonical != transactions {
        return Err(E::custom(
            GenerationManifestError::NonCanonicalAppliedTransactions,
        ));
    }
    Ok(())
}

fn validate_storage_contract_version<E>(contract: &'static str, actual: u16) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual != SEARCH_GENERATION_STORAGE_CONTRACT_VERSION {
        return Err(E::custom(GenerationManifestError::UnsupportedVersion {
            contract,
            actual,
            expected: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
        }));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GenerationManifestError {
    UnsupportedVersion {
        contract: &'static str,
        actual: u16,
        expected: u16,
    },
    TooManyAppliedTransactions {
        actual: usize,
        maximum: usize,
    },
    NonCanonicalAppliedTransactions,
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
            Self::TooManyAppliedTransactions { actual, maximum } => write!(
                formatter,
                "generation records {actual} applied transactions; maximum is {maximum}"
            ),
            Self::NonCanonicalAppliedTransactions => {
                formatter.write_str("applied transactions must be sorted and unique")
            }
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
