use serde::{Deserialize, Deserializer, Serialize};
use unity_asset_core::{DigestBuildError, DigestV1, DigestV1Builder};

/// Wire version for the persisted semantic identity tuple.
pub(crate) const SEARCH_SEMANTICS_WIRE_VERSION: u16 = 1;

/// Version of the analysis rules that produce persisted analysis values.
pub(crate) const ANALYSIS_SEMANTICS_VERSION: u16 = 4;

/// Version of the search projection rules that produce Tantivy documents.
pub(crate) const SEARCH_PROJECTION_SEMANTICS_VERSION: u16 = 2;

/// Version of the reference projection rules that produce graph facts.
pub(crate) const REFERENCE_PROJECTION_SEMANTICS_VERSION: u16 = 4;

/// Version of the persisted per-source analysis cache identity.
pub(crate) const ANALYSIS_CACHE_IDENTITY_VERSION: u16 = 2;

// These frozen receipts are derived from deterministic behavior fixtures in
// `semantics/behavior_receipts.rs`. They deliberately identify observable algorithm output rather
// than source text or a manually maintained version label.
const ANALYSIS_BEHAVIOR_RECEIPT: DigestV1 = DigestV1::from_bytes([
    0xc7, 0x5f, 0x54, 0x1d, 0x33, 0x72, 0x15, 0x36, 0xc3, 0x56, 0x4a, 0xa2, 0x51, 0x3a, 0x94, 0x01,
    0xd2, 0x2d, 0x0b, 0xa5, 0x83, 0xb1, 0x0e, 0x48, 0xce, 0xf0, 0x99, 0xe1, 0x25, 0x8b, 0x12, 0x48,
]);
const SEARCH_PROJECTION_BEHAVIOR_RECEIPT: DigestV1 = DigestV1::from_bytes([
    0x3c, 0x2b, 0x8e, 0xa2, 0xd8, 0x67, 0xc6, 0x26, 0xfa, 0xa3, 0x3b, 0x11, 0xad, 0xb9, 0x89, 0x0f,
    0x66, 0x4d, 0x60, 0xab, 0xaf, 0x8d, 0x54, 0x94, 0xd0, 0xbe, 0x13, 0xf2, 0xd6, 0x57, 0x91, 0xd3,
]);
const REFERENCE_PROJECTION_BEHAVIOR_RECEIPT: DigestV1 = DigestV1::from_bytes([
    0x03, 0xfb, 0x9a, 0x86, 0xd0, 0x40, 0xc0, 0xd0, 0x9d, 0xb5, 0xdc, 0xa4, 0x06, 0x63, 0xc8, 0x97,
    0xb2, 0x7d, 0xc1, 0x80, 0x00, 0xdf, 0x3e, 0x34, 0x49, 0xde, 0x38, 0xad, 0x18, 0x2c, 0xa4, 0x70,
]);
const ANALYSIS_CACHE_IDENTITY_DOMAIN: &[u8] = b"unity-asset:search:analysis-cache-identity:v2\0";

/// Exact identity required before persisted per-source analysis may be reused.
///
/// This identity deliberately binds the complete persisted semantic tuple together with the
/// logical index configuration. A projection-only semantic change still invalidates cached
/// analysis because semantic upgrades perform one full re-analysis instead of selectively reusing
/// old source state. It excludes generation content, activation provenance, retention policy, and
/// physical artifact evidence.
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
            analysis_digest: ANALYSIS_BEHAVIOR_RECEIPT,
            search_projection_version: SEARCH_PROJECTION_SEMANTICS_VERSION,
            search_projection_digest: SEARCH_PROJECTION_BEHAVIOR_RECEIPT,
            reference_projection_version: REFERENCE_PROJECTION_SEMANTICS_VERSION,
            reference_projection_digest: REFERENCE_PROJECTION_BEHAVIOR_RECEIPT,
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

    /// Returns whether persisted source-state values may be decoded across a semantic upgrade.
    ///
    /// Digests may change while retaining the same wire layout. Any persisted semantic version
    /// change is treated as structural and requires rebuilding without decoding the previous
    /// source-state payload.
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
mod behavior_receipts;

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
