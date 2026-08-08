use serde::{Deserialize, Deserializer, Serialize};
use unity_asset_core::{DigestBuildError, DigestV1, DigestV1Builder};

/// Wire version for the persisted semantic identity tuple.
pub(crate) const SEARCH_SEMANTICS_WIRE_VERSION: u16 = 1;

/// Version of the analysis rules that produce persisted analysis values.
pub(crate) const ANALYSIS_SEMANTICS_VERSION: u16 = 3;

/// Version of the search projection rules that produce Tantivy documents.
pub(crate) const SEARCH_PROJECTION_SEMANTICS_VERSION: u16 = 2;

/// Version of the reference projection rules that produce graph facts.
pub(crate) const REFERENCE_PROJECTION_SEMANTICS_VERSION: u16 = 4;

/// Version of the persisted per-source analysis cache identity.
pub(crate) const ANALYSIS_CACHE_IDENTITY_VERSION: u16 = 2;

const ANALYSIS_SEMANTICS_DOMAIN: &[u8] = b"unity-asset:search:analysis-semantics:v3\0";
const SEARCH_PROJECTION_SEMANTICS_DOMAIN: &[u8] =
    b"unity-asset:search:search-projection-semantics:v2\0";
const REFERENCE_PROJECTION_SEMANTICS_DOMAIN: &[u8] =
    b"unity-asset:search:reference-projection-semantics:v4\0";
const ANALYSIS_CACHE_IDENTITY_DOMAIN: &[u8] = b"unity-asset:search:analysis-cache-identity:v2\0";

/// Exact identity required before persisted per-source analysis may be reused.
///
/// This identity deliberately binds every semantic rule that can influence persisted
/// [`crate::analysis::AssetAnalysis`] together with the logical index configuration. It excludes
/// generation content, activation provenance, retention policy, and physical artifact evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct AnalysisCacheIdentityV1 {
    identity_version: u16,
    digest: DigestV1,
}

impl AnalysisCacheIdentityV1 {
    #[must_use]
    const fn new(digest: DigestV1) -> Self {
        Self {
            identity_version: ANALYSIS_CACHE_IDENTITY_VERSION,
            digest,
        }
    }
}

impl<'de> Deserialize<'de> for AnalysisCacheIdentityV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            identity_version: u16,
            digest: DigestV1,
        }

        let wire = Wire::deserialize(deserializer)?;
        if wire.identity_version != ANALYSIS_CACHE_IDENTITY_VERSION {
            return Err(serde::de::Error::custom(format_args!(
                "analysis cache identity version {} is unsupported; expected {}",
                wire.identity_version, ANALYSIS_CACHE_IDENTITY_VERSION
            )));
        }
        Ok(Self::new(wire.digest))
    }
}

/// Independent semantic identities for persisted analysis and projections.
///
/// The tuple is deliberately separate from activation provenance. A parent generation or a set
/// of applied transactions may change how a generation became active without changing the content
/// represented by the current source state and projection rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct SearchSemantics {
    contract_version: u16,
    analysis_version: u16,
    analysis_digest: DigestV1,
    search_projection_version: u16,
    search_projection_digest: DigestV1,
    reference_projection_version: u16,
    reference_projection_digest: DigestV1,
}

impl SearchSemantics {
    #[must_use]
    pub(crate) fn current() -> Self {
        Self {
            contract_version: SEARCH_SEMANTICS_WIRE_VERSION,
            analysis_version: ANALYSIS_SEMANTICS_VERSION,
            analysis_digest: DigestV1::hash_bytes(ANALYSIS_SEMANTICS_DOMAIN),
            search_projection_version: SEARCH_PROJECTION_SEMANTICS_VERSION,
            search_projection_digest: DigestV1::hash_bytes(SEARCH_PROJECTION_SEMANTICS_DOMAIN),
            reference_projection_version: REFERENCE_PROJECTION_SEMANTICS_VERSION,
            reference_projection_digest: DigestV1::hash_bytes(
                REFERENCE_PROJECTION_SEMANTICS_DOMAIN,
            ),
        }
    }

    #[must_use]
    pub(crate) const fn analysis_digest(self) -> DigestV1 {
        self.analysis_digest
    }

    #[must_use]
    pub(crate) const fn analysis_version(self) -> u16 {
        self.analysis_version
    }

    pub(crate) fn analysis_cache_identity(
        self,
        logical_configuration_digest: DigestV1,
    ) -> Result<AnalysisCacheIdentityV1, DigestBuildError> {
        let declared_length = ANALYSIS_CACHE_IDENTITY_DOMAIN
            .len()
            .checked_add(3 * std::mem::size_of::<u16>())
            .and_then(|length| length.checked_add(4 * DigestV1::BYTE_LEN))
            .and_then(|length| u64::try_from(length).ok())
            .ok_or(DigestBuildError::LengthOverflow)?;
        let mut digest = DigestV1Builder::new(declared_length);
        digest.update(ANALYSIS_CACHE_IDENTITY_DOMAIN)?;
        digest.update(&self.analysis_version.to_le_bytes())?;
        digest.update(self.analysis_digest.as_bytes())?;
        digest.update(&self.search_projection_version.to_le_bytes())?;
        digest.update(self.search_projection_digest.as_bytes())?;
        digest.update(&self.reference_projection_version.to_le_bytes())?;
        digest.update(self.reference_projection_digest.as_bytes())?;
        digest.update(logical_configuration_digest.as_bytes())?;
        Ok(AnalysisCacheIdentityV1::new(digest.finalize()?))
    }

    #[must_use]
    pub(crate) const fn search_projection_digest(self) -> DigestV1 {
        self.search_projection_digest
    }

    #[must_use]
    pub(crate) const fn search_projection_version(self) -> u16 {
        self.search_projection_version
    }

    #[must_use]
    pub(crate) const fn reference_projection_digest(self) -> DigestV1 {
        self.reference_projection_digest
    }

    #[must_use]
    pub(crate) const fn reference_projection_version(self) -> u16 {
        self.reference_projection_version
    }

    /// Returns whether persisted source-state values have the same structural semantic shape.
    ///
    /// Digests may change while retaining the same wire layout; version changes are treated as
    /// structural and require rebuilding without decoding the previous source-state payload.
    #[must_use]
    pub(crate) const fn source_state_layout_compatible_with(self, other: Self) -> bool {
        self.contract_version == other.contract_version
            && self.analysis_version == other.analysis_version
            && self.search_projection_version == other.search_projection_version
            && self.reference_projection_version == other.reference_projection_version
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_analysis_digest(mut self, digest: DigestV1) -> Self {
        self.analysis_digest = digest;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_analysis_version(mut self, version: u16) -> Self {
        self.analysis_version = version;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_search_projection_digest(mut self, digest: DigestV1) -> Self {
        self.search_projection_digest = digest;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn with_reference_projection_digest(mut self, digest: DigestV1) -> Self {
        self.reference_projection_digest = digest;
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchSemanticsWire {
    contract_version: u16,
    analysis_version: u16,
    analysis_digest: DigestV1,
    search_projection_version: u16,
    search_projection_digest: DigestV1,
    reference_projection_version: u16,
    reference_projection_digest: DigestV1,
}

impl<'de> Deserialize<'de> for SearchSemantics {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SearchSemanticsWire::deserialize(deserializer)?;
        if wire.contract_version != SEARCH_SEMANTICS_WIRE_VERSION {
            return Err(serde::de::Error::custom(format_args!(
                "search semantics version {} is unsupported; expected {}",
                wire.contract_version, SEARCH_SEMANTICS_WIRE_VERSION
            )));
        }
        for (field, version) in [
            ("analysis_version", wire.analysis_version),
            ("search_projection_version", wire.search_projection_version),
            (
                "reference_projection_version",
                wire.reference_projection_version,
            ),
        ] {
            if version == 0 {
                return Err(serde::de::Error::custom(format_args!(
                    "search semantics {field} must be nonzero"
                )));
            }
        }
        Ok(Self {
            contract_version: wire.contract_version,
            analysis_version: wire.analysis_version,
            analysis_digest: wire.analysis_digest,
            search_projection_version: wire.search_projection_version,
            search_projection_digest: wire.search_projection_digest,
            reference_projection_version: wire.reference_projection_version,
            reference_projection_digest: wire.reference_projection_digest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(label: &str) -> DigestV1 {
        DigestV1::hash_bytes(label.as_bytes())
    }

    #[test]
    fn analysis_cache_identity_binds_all_persisted_semantics_and_configuration() {
        let current = SearchSemantics::current();
        let configuration = digest("configuration");
        let current_identity = current.analysis_cache_identity(configuration).unwrap();

        for changed in [
            current.with_analysis_digest(digest("analysis v-next")),
            current.with_search_projection_digest(digest("search projection v-next")),
            current.with_reference_projection_digest(digest("reference projection v-next")),
        ] {
            assert_ne!(
                changed.analysis_cache_identity(configuration).unwrap(),
                current_identity
            );
        }
        assert_ne!(
            current
                .analysis_cache_identity(digest("configuration v-next"))
                .unwrap(),
            current_identity
        );
    }

    #[test]
    fn source_state_layout_compatibility_tracks_versions_not_rule_digests() {
        let current = SearchSemantics::current();
        assert!(current.source_state_layout_compatible_with(
            current.with_reference_projection_digest(digest("reference rules v-next"))
        ));
        assert!(!current.source_state_layout_compatible_with(
            current.with_analysis_version(current.analysis_version().saturating_add(1))
        ));
    }
}
