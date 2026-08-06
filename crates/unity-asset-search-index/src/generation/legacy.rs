use std::mem::size_of;

use serde::{Deserialize, Deserializer};
use unity_asset_core::{DigestV1, TransactionId, WorkspaceId, WorkspaceRevision};

use super::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationManifestError,
    GenerationProjectionSummary, SearchGenerationId,
};

pub(crate) const LEGACY_SEARCH_GENERATION_STORAGE_CONTRACT_VERSION: u16 = 1;
const LEGACY_GENERATION_DIRECTORY_PREFIX: &str = "generation-v1-";
const LEGACY_GENERATION_ID_DOMAIN: &[u8] = b"unity-asset:search-generation:logical:v1\0";
const MAX_APPLIED_TRANSACTIONS: usize = 4_096;

pub(crate) fn legacy_generation_directory_name(generation: SearchGenerationId) -> String {
    format!(
        "{LEGACY_GENERATION_DIRECTORY_PREFIX}{}",
        hex::encode(generation.digest().as_bytes())
    )
}

pub(crate) fn parse_legacy_generation_directory_name(value: &str) -> Option<SearchGenerationId> {
    let encoded = value.strip_prefix(LEGACY_GENERATION_DIRECTORY_PREFIX)?;
    if encoded.len() != DigestV1::BYTE_LEN * 2
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return None;
    }
    let mut bytes = [0_u8; DigestV1::BYTE_LEN];
    hex::decode_to_slice(encoded, &mut bytes).ok()?;
    Some(SearchGenerationId::new(DigestV1::from_bytes(bytes)))
}

/// Strict reader for the storage-v1 generation manifest.
///
/// The type is intentionally read-only. Current writers can never emit this contract, and callers
/// must explicitly convert it into the current in-memory view after validating its source state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacySearchGenerationManifest {
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

impl LegacySearchGenerationManifest {
    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) const fn revision(&self) -> WorkspaceRevision {
        self.revision
    }

    #[must_use]
    pub(crate) const fn parent_generation(&self) -> Option<SearchGenerationId> {
        self.parent_generation
    }

    #[must_use]
    pub(crate) fn applied_transactions(&self) -> &[TransactionId] {
        &self.applied_transactions
    }

    #[must_use]
    pub(crate) const fn source_state_digest(&self) -> DigestV1 {
        self.source_state_digest
    }

    #[must_use]
    pub(crate) const fn projection_summary(&self) -> GenerationProjectionSummary {
        self.projection_summary
    }

    #[must_use]
    pub(crate) const fn options_digest(&self) -> DigestV1 {
        self.options_digest
    }

    #[must_use]
    pub(crate) const fn artifacts(&self) -> GenerationArtifactEvidence {
        self.artifacts
    }

    #[must_use]
    pub(crate) fn generation_id(&self) -> SearchGenerationId {
        let transaction_bytes = self.applied_transactions.len() * DigestV1::BYTE_LEN;
        let mut canonical = Vec::with_capacity(
            LEGACY_GENERATION_ID_DOMAIN.len()
                + 16
                + DigestV1::BYTE_LEN * 6
                + 1
                + size_of::<u64>()
                + transaction_bytes,
        );
        canonical.extend_from_slice(LEGACY_GENERATION_ID_DOMAIN);
        canonical.extend_from_slice(&self.workspace.get().to_le_bytes());
        canonical.extend_from_slice(self.revision.digest().as_bytes());
        canonical.extend_from_slice(self.search_projection_digest.as_bytes());
        canonical.extend_from_slice(self.reference_projection_digest.as_bytes());
        canonical.extend_from_slice(&self.projection_summary.assets().to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.search_documents().to_le_bytes());
        canonical.extend_from_slice(&self.projection_summary.reference_documents().to_le_bytes());
        canonical.extend_from_slice(
            &self
                .projection_summary
                .projection_truncations()
                .to_le_bytes(),
        );
        canonical.extend_from_slice(&self.projection_summary.incomplete_assets().to_le_bytes());
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
struct LegacyArtifactTreeEvidenceWire {
    contract_version: u16,
    digest: DigestV1,
    files: u64,
    bytes: u64,
}

impl LegacyArtifactTreeEvidenceWire {
    fn into_current<E>(self, contract: &'static str) -> Result<ArtifactTreeEvidence, E>
    where
        E: serde::de::Error,
    {
        validate_legacy_contract::<E>(contract, self.contract_version)?;
        Ok(ArtifactTreeEvidence::new(
            self.digest,
            self.files,
            self.bytes,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGenerationArtifactEvidenceWire {
    contract_version: u16,
    search: LegacyArtifactTreeEvidenceWire,
    references: LegacyArtifactTreeEvidenceWire,
    source_state: LegacyArtifactTreeEvidenceWire,
}

impl LegacyGenerationArtifactEvidenceWire {
    fn into_current<E>(self) -> Result<GenerationArtifactEvidence, E>
    where
        E: serde::de::Error,
    {
        validate_legacy_contract::<E>(
            "legacy generation artifact evidence",
            self.contract_version,
        )?;
        Ok(GenerationArtifactEvidence::new(
            self.search
                .into_current::<E>("legacy search artifact tree evidence")?,
            self.references
                .into_current::<E>("legacy reference artifact tree evidence")?,
            self.source_state
                .into_current::<E>("legacy source-state artifact tree evidence")?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGenerationProjectionSummaryWire {
    contract_version: u16,
    assets: u64,
    search_documents: u64,
    reference_documents: u64,
    projection_truncations: u64,
    incomplete_assets: u64,
}

impl LegacyGenerationProjectionSummaryWire {
    fn into_current<E>(self) -> Result<GenerationProjectionSummary, E>
    where
        E: serde::de::Error,
    {
        validate_legacy_contract::<E>(
            "legacy generation projection summary",
            self.contract_version,
        )?;
        GenerationProjectionSummary::new(
            self.assets,
            self.search_documents,
            self.reference_documents,
            self.projection_truncations,
            self.incomplete_assets,
        )
        .map_err(E::custom)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySearchGenerationManifestWire {
    contract_version: u16,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    search_projection_digest: DigestV1,
    reference_projection_digest: DigestV1,
    projection_summary: LegacyGenerationProjectionSummaryWire,
    parent_generation: Option<SearchGenerationId>,
    applied_transactions: Vec<TransactionId>,
    options_digest: DigestV1,
    source_state_digest: DigestV1,
    artifacts: LegacyGenerationArtifactEvidenceWire,
}

impl<'de> Deserialize<'de> for LegacySearchGenerationManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = LegacySearchGenerationManifestWire::deserialize(deserializer)?;
        validate_legacy_contract::<D::Error>(
            "legacy search generation manifest",
            wire.contract_version,
        )?;
        ensure_canonical_transactions::<D::Error>(&wire.applied_transactions)?;
        Ok(Self {
            workspace: wire.workspace,
            revision: wire.revision,
            search_projection_digest: wire.search_projection_digest,
            reference_projection_digest: wire.reference_projection_digest,
            projection_summary: wire.projection_summary.into_current::<D::Error>()?,
            parent_generation: wire.parent_generation,
            applied_transactions: wire.applied_transactions,
            options_digest: wire.options_digest,
            source_state_digest: wire.source_state_digest,
            artifacts: wire.artifacts.into_current::<D::Error>()?,
        })
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
    if !transactions.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(E::custom(
            GenerationManifestError::NonCanonicalAppliedTransactions,
        ));
    }
    Ok(())
}

fn validate_legacy_contract<E>(contract: &'static str, actual: u16) -> Result<(), E>
where
    E: serde::de::Error,
{
    if actual != LEGACY_SEARCH_GENERATION_STORAGE_CONTRACT_VERSION {
        return Err(E::custom(GenerationManifestError::UnsupportedVersion {
            contract,
            actual,
            expected: LEGACY_SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
        }));
    }
    Ok(())
}
