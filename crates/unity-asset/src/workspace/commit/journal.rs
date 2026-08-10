//! Durable, untrusted publication journal.
//!
//! The journal owns two persistence contracts: an immutable transaction
//! manifest and an append-only, hash-linked event chain. Absolute paths never
//! enter the wire format. Recovery callers must still re-establish no-follow
//! containment and stable file identity before acting on a stored path.

use std::fmt::{self, Write as _};
#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{DeserializeOwned, SeqAccess, Visitor},
};
use thiserror::Error;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, ChangeSet, ContractJsonLimits,
    ContractJsonResourceModel, DigestV1, DigestV1Builder, IdentityRemap, ObjectId,
    SourceFingerprint, SourceId, SourceKind, SourceMemberId, TransactionId, WorkspaceId,
    WorkspaceRevision, read_contract_json, vec_allocation_bytes,
};

use super::super::WorkspaceInstallationDigest;
use super::platform::{
    DIRECTORY_VISIT_ENTRY_BYTES, DIRECTORY_VISIT_SETUP_BYTES, DirectoryEntryName,
    DirectoryIdentity, DirectoryVisitError, FileIdentity, JournalAccess, JournalDirectory,
    atomic_replace_journal_regular, atomic_replace_journal_regular_in_directory,
    create_journal_regular, create_journal_regular_in_directory, observe_directory_identity,
    sync_journal_access, sync_journal_directory,
};
#[cfg(test)]
use super::platform::{
    atomic_replace_tracked, create_private_file_in_parent, open_readonly_regular_in_parent,
};
use super::publication_protocol::{PublicationAction, RecoveryDirection};
use super::{
    CommitArtifactReport, CommitAtomicity, CommitReport, CommitReportFields, RecoveryLocator,
};

mod preparation;

pub(crate) use preparation::{
    JournalPreparation, JournalPreparationOutput, OpenedJournalPreparation,
};

pub(crate) const JOURNAL_VERSION: u8 = 5;
pub(crate) const JOURNAL_TRANSACTION_SEED_VERSION: u8 = 2;
const LEGACY_EVENT_VERSION: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JournalExpectedDestination {
    Existing,
    Absent,
}

#[derive(Debug, Serialize)]
pub(crate) struct JournalTransactionOutputSeed<'a> {
    pub(crate) ordinal: u32,
    pub(crate) logical_name: &'a str,
    pub(crate) source: SourceId,
    pub(crate) relative_target: &'a str,
    pub(crate) expected: JournalExpectedDestination,
    pub(crate) expected_digest: Option<DigestV1>,
    pub(crate) expected_identity: Option<&'a FileIdentity>,
    pub(crate) destination_parent_identity: &'a DirectoryIdentity,
    pub(crate) digest: DigestV1,
    pub(crate) bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct JournalTransactionSeed<'a> {
    pub(crate) version: u8,
    pub(crate) workspace: WorkspaceId,
    pub(crate) base_revision: WorkspaceRevision,
    pub(crate) committed_revision: WorkspaceRevision,
    pub(crate) base_installation: WorkspaceInstallationDigest,
    pub(crate) committed_installation: WorkspaceInstallationDigest,
    pub(crate) plan_digest: DigestV1,
    pub(crate) atomicity: CommitAtomicity,
    pub(crate) containment_root: &'a str,
    pub(crate) containment_root_identity: &'a DirectoryIdentity,
    pub(crate) outputs: &'a [JournalTransactionOutputSeed<'a>],
    pub(crate) changed_sources: &'a [SourceId],
    pub(crate) changed_objects: &'a [ObjectId],
    pub(crate) identity_remaps: &'a [IdentityRemap],
    pub(crate) baseline: &'a JournalBaseline,
}

pub(crate) fn transaction_id_from_seed(
    seed: &JournalTransactionSeed<'_>,
    budget: &mut AssetLoadBudget,
) -> Result<TransactionId, JournalError> {
    Ok(TransactionId::new(digest_serialized(
        seed,
        "journal transaction seed",
        budget,
    )?))
}

fn digest_serialized<T: Serialize>(
    value: &T,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<DigestV1, JournalError> {
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, value)?;
    budget.consume_bytes(counter.bytes)?;
    let mut builder = DigestV1Builder::new(counter.bytes);
    serde_json::to_writer(&mut DigestWriter(&mut builder), value)?;
    builder
        .finalize()
        .map_err(|error| JournalError::InvalidManifest(format!("{resource}: {error}")))
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let amount = u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("transaction seed length overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(amount)
            .ok_or_else(|| io::Error::other("transaction seed length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'builder>(&'builder mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .update(bytes)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) const RECOVERY_DIRECTORY: &str = ".unity-asset-recovery";
pub(crate) const RECOVERY_VERSION_DIRECTORY: &str = "v2";
const RECOVERY_DIGEST_PREFIX: &[u8] = b"blake3-v1:";
pub(crate) const RECOVERY_TRANSACTION_SLUG_BYTES: usize = DigestV1::BYTE_LEN * 2;
const MANIFEST_FILE: &str = "manifest.v2.json";
const PREPARATION_SUFFIX: &str = "prepare.v2.json";
const PREPARATION_TEMPORARY_SUFFIX: &str = "prepare.v2.tmp";
const ROLLBACK_SUFFIX: &str = "rollback.v2.json";
pub(crate) const MANIFEST_TEMPORARY_FILE: &str = ".manifest.v2.json.tmp";
pub(crate) const EVENTS_DIRECTORY: &str = "events";
pub(crate) const STAGE_DIRECTORY: &str = "stage";
pub(crate) const BACKUP_DIRECTORY: &str = "backup";
pub(crate) const BASELINE_DIRECTORY: &str = "baseline";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MANIFEST_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_EVENT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EVENT_ENCODED_BYTES: usize = 4 * 1024 * 1024;
const MAX_EVENT_COUNT: usize = 65_536;
const EVENT_FILENAME_BYTES: usize = 20 + 1 + DigestV1::BYTE_LEN * 2 + 5;
const EVENT_TEMPORARY_FILENAME_BYTES: usize = 1 + EVENT_FILENAME_BYTES + 9 + 8 + 4;
const MAX_ARTIFACT_COUNT: usize = 1_000_000;
const MAX_REASON_BYTES: usize = 8 * 1024;
const MAX_JOURNAL_DEPTH: u32 = 64;
const PARSER_WORK_BYTES_PER_INPUT_BYTE: u64 = 6;
const PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
const MANIFEST_MATERIALIZATION_FIXED_BYTES: u64 = MAX_MANIFEST_BYTES;
const MANIFEST_MATERIALIZATION_BYTES_PER_ENTRY: u64 = 1024;
const PREPARATION_MATERIALIZATION_FIXED_BYTES: u64 = MAX_MANIFEST_BYTES;
const PREPARATION_MATERIALIZATION_BYTES_PER_ENTRY: u64 = 1024;
const EVENT_MATERIALIZATION_FIXED_BYTES: u64 = MAX_EVENT_BYTES;
const EVENT_MATERIALIZATION_BYTES_PER_ENTRY: u64 = 512;

// Each read charges parser_fixed + encoded * (1 + parser_multiplier), then
// materialization_fixed + size_of::<T>() + entries * materialization_per_entry.
// The first term covers the source image and parser; the second covers decoded
// strings up to the raw cap plus retained structs and collection capacity. Both
// coexist at the deserialization boundary, so these are not duplicate charges.
// Later budget-aware validation continues to charge only its added resources.
// A valid JSON document cannot contain more value or member nodes than encoded
// bytes, so the raw cap is also a non-breaking local structure ceiling.
const MANIFEST_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    PARSER_WORK_BYTES_PER_INPUT_BYTE,
    PARSER_FIXED_WORK_BYTES,
    MANIFEST_MATERIALIZATION_FIXED_BYTES,
    MANIFEST_MATERIALIZATION_BYTES_PER_ENTRY,
);
const PREPARATION_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    PARSER_WORK_BYTES_PER_INPUT_BYTE,
    PARSER_FIXED_WORK_BYTES,
    PREPARATION_MATERIALIZATION_FIXED_BYTES,
    PREPARATION_MATERIALIZATION_BYTES_PER_ENTRY,
);
const EVENT_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    PARSER_WORK_BYTES_PER_INPUT_BYTE,
    PARSER_FIXED_WORK_BYTES,
    EVENT_MATERIALIZATION_FIXED_BYTES,
    EVENT_MATERIALIZATION_BYTES_PER_ENTRY,
);

const MANIFEST_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.journal.manifest",
    MAX_MANIFEST_ENCODED_BYTES,
    MAX_JOURNAL_DEPTH,
    MAX_MANIFEST_BYTES,
    MAX_MANIFEST_BYTES,
    MANIFEST_JSON_RESOURCES,
);
const PREPARATION_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.journal.preparation",
    MAX_MANIFEST_ENCODED_BYTES,
    MAX_JOURNAL_DEPTH,
    MAX_MANIFEST_BYTES,
    MAX_MANIFEST_BYTES,
    PREPARATION_JSON_RESOURCES,
);
const EVENT_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "unity_asset.journal.event",
    MAX_EVENT_ENCODED_BYTES,
    MAX_JOURNAL_DEPTH,
    MAX_EVENT_BYTES,
    MAX_EVENT_BYTES,
    EVENT_JSON_RESOURCES,
);
const EXISTING_TARGET_EVENT_COUNT: usize = 4;
const ABSENT_TARGET_EVENT_COUNT: usize = 2;
const TRANSACTION_EVENT_RESERVE: usize = 6;

/// Canonical v2 transaction evidence named directly in the recovery version
/// directory. The parser is intentionally shared by discovery and layout
/// validation so accepted slugs cannot drift between those boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryEvidenceName {
    Transaction(TransactionId),
    Preparation(TransactionId),
    Rollback(TransactionId),
    PreparationTemporary(TransactionId),
}

impl RecoveryEvidenceName {
    #[must_use]
    pub(crate) const fn transaction(self) -> TransactionId {
        match self {
            Self::Transaction(transaction)
            | Self::Preparation(transaction)
            | Self::Rollback(transaction)
            | Self::PreparationTemporary(transaction) => transaction,
        }
    }
}

/// Parses only the four v2 names which can identify a recoverable
/// transaction. Unknown and noncanonical names deliberately return `None`.
#[must_use]
pub(crate) fn parse_recovery_evidence_name(name: &str) -> Option<RecoveryEvidenceName> {
    if let Some(transaction) = transaction_from_recovery_slug(name) {
        return Some(RecoveryEvidenceName::Transaction(transaction));
    }
    if let Some(slug) = name.strip_suffix(".prepare.v2.json") {
        return transaction_from_recovery_slug(slug).map(RecoveryEvidenceName::Preparation);
    }
    if let Some(slug) = name.strip_suffix(".rollback.v2.json") {
        return transaction_from_recovery_slug(slug).map(RecoveryEvidenceName::Rollback);
    }
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".prepare.v2.tmp"))
        .and_then(transaction_from_recovery_slug)
        .map(RecoveryEvidenceName::PreparationTemporary)
}

fn transaction_from_recovery_slug(slug: &str) -> Option<TransactionId> {
    if slug.len() != RECOVERY_TRANSACTION_SLUG_BYTES
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut wire = [0_u8; RECOVERY_DIGEST_PREFIX.len() + RECOVERY_TRANSACTION_SLUG_BYTES];
    wire[..RECOVERY_DIGEST_PREFIX.len()].copy_from_slice(RECOVERY_DIGEST_PREFIX);
    wire[RECOVERY_DIGEST_PREFIX.len()..].copy_from_slice(slug.as_bytes());
    let digest = std::str::from_utf8(&wire).ok()?.parse::<DigestV1>().ok()?;
    Some(TransactionId::new(digest))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BoundedSequence<T, const MAX: usize>(Vec<T>);

impl<'de, T, const MAX: usize> Deserialize<'de> for BoundedSequence<T, MAX>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedSequenceVisitor<T, const MAX: usize>(std::marker::PhantomData<T>);

        impl<'de, T, const MAX: usize> Visitor<'de> for BoundedSequenceVisitor<T, MAX>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedSequence<T, MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "an array with at most {MAX} entries")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    if values.len() >= MAX {
                        return Err(serde::de::Error::custom(format!(
                            "array exceeds maximum of {MAX} entries"
                        )));
                    }
                    values.push(value);
                }
                Ok(BoundedSequence(values))
            }
        }

        deserializer.deserialize_seq(BoundedSequenceVisitor(std::marker::PhantomData))
    }
}

/// A validated relative path stored in a journal.
///
/// Forward-slash descendants are accepted. Parent/root/prefix components,
/// backslashes, ADS syntax, control characters, trailing dots/spaces, and
/// Windows device names are rejected on every host.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct JournalPath(String);

impl JournalPath {
    pub(crate) fn new(value: impl AsRef<str>) -> Result<Self, JournalError> {
        let value = value.as_ref();
        validate_relative_path(value)?;
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn new_budgeted(
        value: &str,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        validate_relative_path(value)?;
        let requested = u64::try_from(value.len()).map_err(|_| {
            JournalError::Budget(BudgetError::ArithmeticOverflow {
                resource: "journal path",
            })
        })?;
        budget.check_bytes(requested)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(value.len())
            .map_err(|error| JournalError::Allocation {
                resource: "journal path",
                requested: value.len(),
                message: error.to_string(),
            })?;
        owned.push_str(value);
        budget.consume_bytes(u64::try_from(owned.capacity()).map_err(|_| {
            JournalError::Budget(BudgetError::ArithmeticOverflow {
                resource: "journal path",
            })
        })?)?;
        Ok(Self(owned))
    }

    pub(crate) fn from_owned(value: String) -> Result<Self, JournalError> {
        validate_relative_path(&value)?;
        Ok(Self(value))
    }

    pub(crate) fn clone_budgeted(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        Self::new_budgeted(self.as_str(), budget)
    }

    #[must_use]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn join_root_budgeted(
        &self,
        root: &Path,
        resource: &'static str,
        budget: &mut AssetLoadBudget,
    ) -> Result<PathBuf, JournalError> {
        budgeted_journal_join(root, self.as_str(), resource, budget)
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn join_root(&self, root: &Path) -> PathBuf {
        root.join(Path::new(self.as_str()))
    }
}

pub(super) fn matches_ordinal_journal_path(
    path: &JournalPath,
    prefix: &str,
    ordinal: usize,
    suffix: &str,
) -> bool {
    let encoded = path.as_str().as_bytes();
    let Some(expected_length) = prefix
        .len()
        .checked_add(8)
        .and_then(|length| length.checked_add(suffix.len()))
    else {
        return false;
    };
    if encoded.len() != expected_length
        || !encoded.starts_with(prefix.as_bytes())
        || !encoded.ends_with(suffix.as_bytes())
    {
        return false;
    }
    let mut parsed = 0_usize;
    for byte in &encoded[prefix.len()..prefix.len() + 8] {
        if !byte.is_ascii_digit() {
            return false;
        }
        let Some(next) = parsed
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(*byte - b'0')))
        else {
            return false;
        };
        parsed = next;
    }
    parsed == ordinal
}

impl Serialize for JournalPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JournalPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

fn validate_relative_path(value: &str) -> Result<(), JournalError> {
    if value.is_empty() || value.len() > 1024 {
        return Err(JournalError::InvalidPath {
            path: value.to_owned(),
            reason: "path is empty or too long",
        });
    }
    if value.starts_with('/') || value.starts_with('\\') || value.contains('\\') {
        return Err(JournalError::InvalidPath {
            path: value.to_owned(),
            reason: "absolute and backslash paths are forbidden",
        });
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(JournalError::InvalidPath {
            path: value.to_owned(),
            reason: "absolute paths are forbidden",
        });
    }
    let mut components = 0_usize;
    for component in path.components() {
        match component {
            Component::Normal(component) => {
                components += 1;
                validate_component(&component.to_string_lossy(), value)?;
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(JournalError::InvalidPath {
                    path: value.to_owned(),
                    reason: "dot, parent, root, and prefix components are forbidden",
                });
            }
        }
    }
    if components == 0 || value.split('/').any(str::is_empty) {
        return Err(JournalError::InvalidPath {
            path: value.to_owned(),
            reason: "empty path components are forbidden",
        });
    }
    Ok(())
}

fn validate_component(component: &str, whole: &str) -> Result<(), JournalError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with('.')
        || component.ends_with(' ')
        || component.chars().any(|character| {
            character.is_control() || matches!(character, ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
    {
        return Err(JournalError::InvalidPath {
            path: whole.to_owned(),
            reason: "invalid path component",
        });
    }
    let stem = component.split('.').next().unwrap_or(component);
    if is_windows_device_name(stem) {
        return Err(JournalError::InvalidPath {
            path: whole.to_owned(),
            reason: "reserved Windows device name",
        });
    }
    Ok(())
}

fn is_windows_device_name(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CLOCK$"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalArtifact {
    logical_name: String,
    source: SourceId,
    target: JournalPath,
    destination_parent_identity: DirectoryIdentity,
    staging: JournalPath,
    backup: Option<JournalPath>,
    old_digest: Option<DigestV1>,
    old_identity: Option<FileIdentity>,
    new_digest: DigestV1,
    new_identity: FileIdentity,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalDirectoryIdentities {
    transaction: DirectoryIdentity,
    events: DirectoryIdentity,
    stage: DirectoryIdentity,
    backup: DirectoryIdentity,
    baseline: DirectoryIdentity,
}

impl JournalDirectoryIdentities {
    pub(crate) const fn new(
        transaction: DirectoryIdentity,
        events: DirectoryIdentity,
        stage: DirectoryIdentity,
        backup: DirectoryIdentity,
        baseline: DirectoryIdentity,
    ) -> Self {
        Self {
            transaction,
            events,
            stage,
            backup,
            baseline,
        }
    }

    #[cfg(test)]
    pub(crate) fn observe(layout: &JournalLayout) -> Result<Self, JournalError> {
        Ok(Self::new(
            observe_directory_identity(layout.directory())?,
            observe_directory_identity(layout.events_directory())?,
            observe_directory_identity(layout.stage_directory())?,
            observe_directory_identity(layout.backup_directory())?,
            observe_directory_identity(layout.baseline_directory())?,
        ))
    }

    #[must_use]
    pub(crate) const fn transaction(&self) -> &DirectoryIdentity {
        &self.transaction
    }

    #[must_use]
    pub(crate) const fn events(&self) -> &DirectoryIdentity {
        &self.events
    }

    #[must_use]
    pub(crate) const fn stage(&self) -> &DirectoryIdentity {
        &self.stage
    }

    #[must_use]
    pub(crate) const fn backup(&self) -> &DirectoryIdentity {
        &self.backup
    }

    #[must_use]
    pub(crate) const fn baseline(&self) -> &DirectoryIdentity {
        &self.baseline
    }
}

/// One exact source image retained for rebuilding the next workspace baseline.
///
/// Publication roots can refer to their promoted target. Nested sources have no
/// independent target, so their proof image is retained in the private journal
/// directory and addressed only by a validated relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub(crate) enum JournalBaselineImage {
    Published {
        artifact: u32,
    },
    Blob {
        path: JournalPath,
        digest: DigestV1,
        bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
pub(crate) enum JournalCatalogAction {
    Existing {
        base_fingerprint: SourceFingerprint,
    },
    AddCompanion {
        parent: SourceId,
        member: SourceMemberId,
    },
    AddContainedSidecar {
        parent: SourceId,
        member: SourceMemberId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalBaselineSource {
    source: SourceId,
    fingerprint: SourceFingerprint,
    catalog: JournalCatalogAction,
    image: JournalBaselineImage,
}

impl JournalBaselineSource {
    pub(crate) const fn new(
        source: SourceId,
        fingerprint: SourceFingerprint,
        catalog: JournalCatalogAction,
        image: JournalBaselineImage,
    ) -> Self {
        Self {
            source,
            fingerprint,
            catalog,
            image,
        }
    }

    #[must_use]
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) const fn fingerprint(&self) -> SourceFingerprint {
        self.fingerprint
    }

    #[must_use]
    pub(crate) const fn catalog(&self) -> &JournalCatalogAction {
        &self.catalog
    }

    #[must_use]
    pub(crate) const fn image(&self) -> &JournalBaselineImage {
        &self.image
    }
}

/// Bounded source-image delta needed to install a published baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalBaseline {
    sources: Vec<JournalBaselineSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalBaselineWire {
    sources: BoundedSequence<JournalBaselineSource, MAX_ARTIFACT_COUNT>,
}

impl<'de> Deserialize<'de> for JournalBaseline {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JournalBaselineWire::deserialize(deserializer)?;
        Ok(Self {
            sources: wire.sources.0,
        })
    }
}

impl JournalBaseline {
    pub(crate) fn from_sorted(
        sources: Vec<JournalBaselineSource>,
        workspace: WorkspaceId,
    ) -> Result<Self, JournalError> {
        let baseline = Self { sources };
        baseline.validate(workspace)?;
        Ok(baseline)
    }

    pub(crate) fn validate(&self, workspace: WorkspaceId) -> Result<(), JournalError> {
        if self.sources.is_empty() || self.sources.len() > MAX_ARTIFACT_COUNT {
            return Err(JournalError::InvalidManifest(
                "baseline source count is outside the allowed range".to_owned(),
            ));
        }
        for source in &self.sources {
            if source.source.workspace() != workspace
                || source.source.kind() != source.fingerprint.kind()
            {
                return Err(JournalError::InvalidManifest(
                    "baseline source identity disagrees with its workspace or kind".to_owned(),
                ));
            }
            match source.catalog() {
                JournalCatalogAction::Existing { base_fingerprint } => {
                    if base_fingerprint.kind() != source.source.kind() {
                        return Err(JournalError::InvalidManifest(
                            "existing baseline source has a mismatched base fingerprint kind"
                                .to_owned(),
                        ));
                    }
                }
                JournalCatalogAction::AddCompanion { parent, .. } => {
                    if source.source.kind() != SourceKind::StreamedResource
                        || parent.workspace() != workspace
                        || !matches!(parent.kind(), SourceKind::Yaml | SourceKind::SerializedFile)
                        || !matches!(source.image(), JournalBaselineImage::Published { .. })
                    {
                        return Err(JournalError::InvalidManifest(
                            "baseline companion declaration has invalid ownership or image"
                                .to_owned(),
                        ));
                    }
                }
                JournalCatalogAction::AddContainedSidecar { parent, .. } => {
                    if source.source.kind() != SourceKind::StreamedResource
                        || parent.workspace() != workspace
                        || !matches!(
                            parent.kind(),
                            SourceKind::Archive | SourceKind::AssetBundle | SourceKind::WebFile
                        )
                        || !matches!(source.image(), JournalBaselineImage::Blob { .. })
                    {
                        return Err(JournalError::InvalidManifest(
                            "baseline contained-sidecar declaration has invalid ownership or image"
                                .to_owned(),
                        ));
                    }
                }
            }
            if let JournalBaselineImage::Blob {
                path: _,
                digest,
                bytes,
            } = source.image()
                && (*digest != source.fingerprint.digest() || *bytes == u64::MAX)
            {
                return Err(JournalError::InvalidManifest(
                    "baseline blob metadata disagrees with its source fingerprint".to_owned(),
                ));
            }
        }
        for pair in self.sources.windows(2) {
            if pair[0].source >= pair[1].source {
                return Err(JournalError::InvalidManifest(
                    "baseline sources are not in strict SourceId order".to_owned(),
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn sources(&self) -> &[JournalBaselineSource] {
        &self.sources
    }
}

impl JournalArtifact {
    pub(crate) fn new(
        report: &CommitArtifactReport,
        target: JournalPath,
        destination_parent_identity: DirectoryIdentity,
        staging: JournalPath,
        backup: Option<JournalPath>,
        old_digest: Option<DigestV1>,
        old_identity: Option<FileIdentity>,
        new_identity: FileIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        let logical_name = budgeted_journal_string(
            report.logical_name(),
            "journal artifact logical name",
            budget,
        )?;
        let artifact = Self {
            logical_name,
            source: report.source(),
            target,
            destination_parent_identity,
            staging,
            backup,
            old_digest,
            old_identity,
            new_digest: report.digest(),
            new_identity,
            bytes: report.bytes(),
        };
        artifact.validate()?;
        Ok(artifact)
    }

    fn validate(&self) -> Result<(), JournalError> {
        if self.logical_name.is_empty() || self.logical_name.len() > 1024 {
            return Err(JournalError::InvalidManifest(
                "artifact logical name is empty or too long".to_owned(),
            ));
        }
        if self.target == self.staging
            || self
                .backup
                .as_ref()
                .is_some_and(|backup| backup == &self.target || backup == &self.staging)
        {
            return Err(JournalError::InvalidManifest(
                "artifact target, staging, and backup paths overlap".to_owned(),
            ));
        }
        let has_old = self.old_digest.is_some();
        if has_old != self.backup.is_some() || has_old != self.old_identity.is_some() {
            return Err(JournalError::InvalidManifest(
                "artifact old digest, identity, and backup declaration disagree".to_owned(),
            ));
        }
        if self.new_identity.length() != self.bytes {
            return Err(JournalError::InvalidManifest(
                "artifact staged identity length disagrees with the result".to_owned(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn logical_name(&self) -> &str {
        &self.logical_name
    }

    #[must_use]
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) fn target(&self) -> &JournalPath {
        &self.target
    }

    #[must_use]
    pub(crate) const fn destination_parent_identity(&self) -> &DirectoryIdentity {
        &self.destination_parent_identity
    }

    #[must_use]
    pub(crate) fn staging(&self) -> &JournalPath {
        &self.staging
    }

    #[must_use]
    pub(crate) fn backup(&self) -> Option<&JournalPath> {
        self.backup.as_ref()
    }

    #[must_use]
    pub(crate) const fn old_digest(&self) -> Option<DigestV1> {
        self.old_digest
    }

    #[must_use]
    pub(crate) const fn old_identity(&self) -> Option<&FileIdentity> {
        self.old_identity.as_ref()
    }

    #[must_use]
    pub(crate) const fn new_digest(&self) -> DigestV1 {
        self.new_digest
    }

    #[must_use]
    pub(crate) const fn new_identity(&self) -> &FileIdentity {
        &self.new_identity
    }

    #[must_use]
    pub(crate) const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Canonical commit result without an absolute recovery locator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalResult {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    committed_installation: WorkspaceInstallationDigest,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    artifacts: Vec<CommitArtifactReport>,
    changes: ChangeSet,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalResultWire {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    committed_installation: WorkspaceInstallationDigest,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    artifacts: BoundedSequence<CommitArtifactReport, MAX_ARTIFACT_COUNT>,
    changes: ChangeSet,
}

impl<'de> Deserialize<'de> for JournalResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JournalResultWire::deserialize(deserializer)?;
        Ok(Self {
            version: wire.version,
            transaction: wire.transaction,
            workspace_id: wire.workspace_id,
            base_revision: wire.base_revision,
            committed_revision: wire.committed_revision,
            base_installation: wire.base_installation,
            committed_installation: wire.committed_installation,
            plan_digest: wire.plan_digest,
            atomicity: wire.atomicity,
            artifacts: wire.artifacts.0,
            changes: wire.changes,
        })
    }
}

impl JournalResult {
    fn from_report(
        report: &CommitReport,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        Ok(Self {
            version: report.version(),
            transaction: report.transaction(),
            workspace_id: report.workspace_id(),
            base_revision: report.base_revision(),
            committed_revision: report.committed_revision(),
            base_installation: report.base_installation(),
            committed_installation: report.committed_installation(),
            plan_digest: report.plan_digest(),
            atomicity: report.atomicity(),
            artifacts: clone_artifact_reports(report.artifacts(), budget)?,
            changes: clone_change_set(report.changes(), budget)?,
        })
    }

    pub(crate) fn into_report(
        self,
        root: PathBuf,
        root_identity: DirectoryIdentity,
    ) -> Result<CommitReport, JournalError> {
        let report = CommitReport::new(CommitReportFields {
            transaction: self.transaction,
            workspace_id: self.workspace_id,
            base_revision: self.base_revision,
            committed_revision: self.committed_revision,
            base_installation: self.base_installation,
            committed_installation: self.committed_installation,
            plan_digest: self.plan_digest,
            atomicity: self.atomicity,
            artifacts: self.artifacts,
            changes: self.changes,
            recovery: RecoveryLocator::new(root, self.transaction, root_identity),
        });
        report
            .validate()
            .map_err(|error| JournalError::InvalidManifest(error.to_string()))?;
        Ok(report)
    }

    fn validate(&self) -> Result<(), JournalError> {
        if self.version != super::COMMIT_REPORT_VERSION
            || self.changes.transaction() != self.transaction
            || self.changes.workspace() != self.workspace_id
            || self.changes.from_revision() != self.base_revision
            || self.changes.to_revision() != self.committed_revision
            || self.artifacts.is_empty()
        {
            return Err(JournalError::InvalidManifest(
                "canonical journal result violates the commit contract".to_owned(),
            ));
        }
        if self
            .artifacts
            .windows(2)
            .any(|pair| pair[0].logical_name() >= pair[1].logical_name())
        {
            return Err(JournalError::InvalidManifest(
                "canonical journal result artifacts are not strictly ordered".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalManifest {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    committed_installation: WorkspaceInstallationDigest,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    containment_root_identity: DirectoryIdentity,
    directories: JournalDirectoryIdentities,
    artifacts: Vec<JournalArtifact>,
    baseline: JournalBaseline,
    result: JournalResult,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalManifestWire {
    version: u8,
    transaction: TransactionId,
    workspace_id: WorkspaceId,
    base_revision: WorkspaceRevision,
    committed_revision: WorkspaceRevision,
    base_installation: WorkspaceInstallationDigest,
    committed_installation: WorkspaceInstallationDigest,
    plan_digest: DigestV1,
    atomicity: CommitAtomicity,
    containment_root_identity: DirectoryIdentity,
    directories: JournalDirectoryIdentities,
    artifacts: BoundedSequence<JournalArtifact, MAX_ARTIFACT_COUNT>,
    baseline: JournalBaseline,
    result: JournalResult,
}

impl<'de> Deserialize<'de> for JournalManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = JournalManifestWire::deserialize(deserializer)?;
        Ok(Self {
            version: wire.version,
            transaction: wire.transaction,
            workspace_id: wire.workspace_id,
            base_revision: wire.base_revision,
            committed_revision: wire.committed_revision,
            base_installation: wire.base_installation,
            committed_installation: wire.committed_installation,
            plan_digest: wire.plan_digest,
            atomicity: wire.atomicity,
            containment_root_identity: wire.containment_root_identity,
            directories: wire.directories,
            artifacts: wire.artifacts.0,
            baseline: wire.baseline,
            result: wire.result,
        })
    }
}

impl JournalManifest {
    pub(crate) fn new(
        report: &CommitReport,
        containment_root_identity: DirectoryIdentity,
        directories: JournalDirectoryIdentities,
        artifacts: Vec<JournalArtifact>,
        baseline: JournalBaseline,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        if report.recovery().root_identity() != &containment_root_identity {
            return Err(JournalError::InvalidManifest(
                "commit report recovery locator does not match the journal containment root"
                    .to_owned(),
            ));
        }
        let manifest = Self {
            version: JOURNAL_VERSION,
            transaction: report.transaction(),
            workspace_id: report.workspace_id(),
            base_revision: report.base_revision(),
            committed_revision: report.committed_revision(),
            base_installation: report.base_installation(),
            committed_installation: report.committed_installation(),
            plan_digest: report.plan_digest(),
            atomicity: report.atomicity(),
            containment_root_identity,
            directories,
            artifacts,
            baseline,
            result: JournalResult::from_report(report, budget)?,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub(crate) fn validate(&self) -> Result<(), JournalError> {
        if self.version != JOURNAL_VERSION {
            return Err(JournalError::UnsupportedVersion(self.version));
        }
        if self.artifacts.is_empty() || self.artifacts.len() > MAX_ARTIFACT_COUNT {
            return Err(JournalError::InvalidManifest(
                "manifest artifact count is outside the allowed range".to_owned(),
            ));
        }
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        let existing_targets = self
            .artifacts
            .iter()
            .filter(|artifact| artifact.old_digest.is_some() || artifact.backup.is_some())
            .count();
        validate_event_capacity(existing_targets, self.artifacts.len() - existing_targets)?;
        self.baseline.validate(self.workspace_id)?;
        for pair in self.artifacts.windows(2) {
            if pair[0].logical_name >= pair[1].logical_name {
                return Err(JournalError::InvalidManifest(
                    "manifest artifacts are not in strict logical-name order".to_owned(),
                ));
            }
        }
        if self.result.version != super::COMMIT_REPORT_VERSION
            || self.result.transaction != self.transaction
            || self.result.workspace_id != self.workspace_id
            || self.result.base_revision != self.base_revision
            || self.result.committed_revision != self.committed_revision
            || self.result.base_installation != self.base_installation
            || self.result.committed_installation != self.committed_installation
            || self.result.plan_digest != self.plan_digest
            || self.result.atomicity != self.atomicity
        {
            return Err(JournalError::InvalidManifest(
                "manifest result disagrees with its transaction header".to_owned(),
            ));
        }
        self.result.validate()?;
        if self.result.artifacts.len() != self.artifacts.len() {
            return Err(JournalError::InvalidManifest(
                "manifest artifact and result counts disagree".to_owned(),
            ));
        }
        for (artifact, report) in self.artifacts.iter().zip(&self.result.artifacts) {
            if artifact.logical_name != report.logical_name()
                || artifact.source != report.source()
                || artifact.new_digest != report.digest()
                || artifact.bytes != report.bytes()
            {
                return Err(JournalError::InvalidManifest(
                    "manifest artifact disagrees with the canonical result".to_owned(),
                ));
            }
        }
        for (source_index, source) in self.baseline.sources().iter().enumerate() {
            match source.image() {
                JournalBaselineImage::Published { artifact } => {
                    let artifact_index = usize::try_from(*artifact).map_err(|_| {
                        JournalError::InvalidManifest(
                            "published baseline artifact index overflowed".to_owned(),
                        )
                    })?;
                    let published = self.artifacts.get(artifact_index).ok_or_else(|| {
                        JournalError::InvalidManifest(
                            "published baseline artifact index is out of range".to_owned(),
                        )
                    })?;
                    if published.source() != source.source()
                        || published.new_digest() != source.fingerprint().digest()
                    {
                        return Err(JournalError::InvalidManifest(
                            "published baseline source disagrees with its artifact".to_owned(),
                        ));
                    }
                }
                JournalBaselineImage::Blob { path, .. } => {
                    if !matches_ordinal_journal_path(path, "baseline/", source_index, ".image") {
                        return Err(JournalError::InvalidManifest(
                            "baseline blob path does not match its source ordinal".to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn validate_transaction_identity(
        &self,
        containment_root: &Path,
        containment_root_identity: &DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), JournalError> {
        if &self.containment_root_identity != containment_root_identity {
            return Err(JournalError::InvalidManifest(
                "journal containment root identity does not match its trusted locator".to_owned(),
            ));
        }
        let containment_root = containment_root.to_str().ok_or_else(|| {
            JournalError::InvalidManifest(
                "transaction containment root is not valid UTF-8".to_owned(),
            )
        })?;
        let mut outputs = journal_budgeted_vec(
            self.artifacts.len(),
            "journal transaction identity outputs",
            budget,
        )?;
        for (ordinal, artifact) in self.artifacts.iter().enumerate() {
            let ordinal = u32::try_from(ordinal).map_err(|_| {
                JournalError::Budget(BudgetError::ArithmeticOverflow {
                    resource: "journal transaction identity ordinal",
                })
            })?;
            outputs.push(JournalTransactionOutputSeed {
                ordinal,
                logical_name: artifact.logical_name(),
                source: artifact.source(),
                relative_target: artifact.target().as_str(),
                expected: if artifact.old_identity().is_some() {
                    JournalExpectedDestination::Existing
                } else {
                    JournalExpectedDestination::Absent
                },
                expected_digest: artifact.old_digest(),
                expected_identity: artifact.old_identity(),
                destination_parent_identity: artifact.destination_parent_identity(),
                digest: artifact.new_digest(),
                bytes: artifact.bytes(),
            });
        }
        let actual = transaction_id_from_seed(
            &JournalTransactionSeed {
                version: JOURNAL_TRANSACTION_SEED_VERSION,
                workspace: self.workspace_id,
                base_revision: self.base_revision,
                committed_revision: self.committed_revision,
                base_installation: self.base_installation,
                committed_installation: self.committed_installation,
                plan_digest: self.plan_digest,
                atomicity: self.atomicity,
                containment_root,
                containment_root_identity,
                outputs: &outputs,
                changed_sources: self.result.changes.changed_sources(),
                changed_objects: self.result.changes.changed_objects(),
                identity_remaps: self.result.changes.identity_remaps(),
                baseline: &self.baseline,
            },
            budget,
        )?;
        if actual != self.transaction {
            return Err(JournalError::TransactionMismatch {
                expected: self.transaction,
                actual,
            });
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    #[must_use]
    pub(crate) const fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    #[must_use]
    pub(crate) const fn committed_revision(&self) -> WorkspaceRevision {
        self.committed_revision
    }

    #[must_use]
    pub(crate) const fn base_installation(&self) -> WorkspaceInstallationDigest {
        self.base_installation
    }

    #[must_use]
    pub(crate) const fn committed_installation(&self) -> WorkspaceInstallationDigest {
        self.committed_installation
    }

    #[must_use]
    pub(crate) fn artifacts(&self) -> &[JournalArtifact] {
        &self.artifacts
    }

    #[must_use]
    pub(crate) const fn directories(&self) -> &JournalDirectoryIdentities {
        &self.directories
    }

    #[must_use]
    pub(crate) const fn baseline(&self) -> &JournalBaseline {
        &self.baseline
    }

    fn event_capacity(&self) -> Result<usize, JournalError> {
        let existing = self
            .artifacts
            .iter()
            .filter(|artifact| artifact.old_digest().is_some())
            .count();
        required_event_capacity(existing, self.artifacts.len() - existing)
    }

    pub(crate) fn report(
        &self,
        root: &Path,
        root_identity: &DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<CommitReport, JournalError> {
        if &self.containment_root_identity != root_identity {
            return Err(JournalError::InvalidManifest(
                "journal report root identity does not match its manifest".to_owned(),
            ));
        }
        let result = JournalResult {
            version: self.result.version,
            transaction: self.result.transaction,
            workspace_id: self.result.workspace_id,
            base_revision: self.result.base_revision,
            committed_revision: self.result.committed_revision,
            base_installation: self.result.base_installation,
            committed_installation: self.result.committed_installation,
            plan_digest: self.result.plan_digest,
            atomicity: self.result.atomicity,
            artifacts: clone_artifact_reports(&self.result.artifacts, budget)?,
            changes: clone_change_set(&self.result.changes, budget)?,
        };
        result.into_report(
            budgeted_journal_path(root, "journal report recovery path", budget)?,
            root_identity.clone(),
        )
    }
}

fn journal_budgeted_vec<T>(
    count: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<T>, JournalError> {
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    let requested = vec_allocation_bytes::<T>(count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_entries(entries)?;
    budget.check_bytes(requested)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|error| JournalError::Allocation {
            resource,
            requested: count,
            message: error.to_string(),
        })?;
    let actual = size_of::<T>()
        .checked_mul(values.capacity())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(actual)?;
    Ok(values)
}

fn journal_reserve_one<T>(
    values: &mut Vec<T>,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), JournalError> {
    if values.len() < values.capacity() {
        return Ok(());
    }
    let requested_count = values
        .len()
        .checked_add(1)
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    let requested = vec_allocation_bytes::<T>(requested_count)
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(requested)?;
    values
        .try_reserve_exact(1)
        .map_err(|error| JournalError::Allocation {
            resource,
            requested: requested_count,
            message: error.to_string(),
        })?;
    let actual = size_of::<T>()
        .checked_mul(values.capacity())
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(())
}

fn budgeted_journal_string(
    value: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, JournalError> {
    let mut copy = budgeted_empty_journal_string(value.len(), resource, budget)?;
    copy.push_str(value);
    Ok(copy)
}

fn budgeted_empty_journal_string(
    requested: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, JournalError> {
    let requested_u64 =
        u64::try_from(requested).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(requested_u64)?;
    let mut value = String::new();
    value
        .try_reserve_exact(requested)
        .map_err(|error| JournalError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    let actual = u64::try_from(value.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(value)
}

fn budgeted_journal_path(
    value: &Path,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, JournalError> {
    let requested = value.as_os_str().len();
    budget.check_bytes(
        u64::try_from(requested).map_err(|_| BudgetError::ArithmeticOverflow { resource })?,
    )?;
    let mut copy = PathBuf::new();
    copy.try_reserve_exact(requested)
        .map_err(|error| JournalError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    copy.push(value);
    let actual =
        u64::try_from(copy.capacity()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(copy)
}

fn budgeted_journal_join(
    root: &Path,
    leaf: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<PathBuf, JournalError> {
    let requested = root
        .as_os_str()
        .len()
        .checked_add(leaf.len())
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(
        u64::try_from(requested).map_err(|_| BudgetError::ArithmeticOverflow { resource })?,
    )?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(requested)
        .map_err(|error| JournalError::Allocation {
            resource,
            requested,
            message: error.to_string(),
        })?;
    path.push(root);
    path.push(leaf);
    let actual =
        u64::try_from(path.capacity()).map_err(|_| BudgetError::ArithmeticOverflow { resource })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(path)
}

fn visit_opened_journal_directory(
    directory: &JournalDirectory,
    budget: &mut AssetLoadBudget,
    visitor: impl FnMut(&mut AssetLoadBudget, DirectoryEntryName<'_>) -> Result<(), JournalError>,
) -> Result<(), JournalError> {
    budget.consume_bytes(DIRECTORY_VISIT_SETUP_BYTES)?;
    match super::platform::visit_journal_directory_entries(
        directory,
        budget,
        |budget| {
            budget.consume_bytes(DIRECTORY_VISIT_ENTRY_BYTES)?;
            Ok(())
        },
        visitor,
    ) {
        Ok(()) => Ok(()),
        Err(DirectoryVisitError::Visitor(error)) => Err(error),
        Err(DirectoryVisitError::Io(error)) => Err(JournalError::Io(error)),
    }
}

fn journal_entry_name<'a>(
    entry: DirectoryEntryName<'_>,
    scratch: &'a mut [u8; EVENT_TEMPORARY_FILENAME_BYTES],
) -> Result<Option<&'a str>, JournalError> {
    let length = entry.copy_ascii_into(scratch).ok_or_else(|| {
        JournalError::InvalidEvent("event directory entry name is not canonical ASCII".to_owned())
    })?;
    let name = std::str::from_utf8(&scratch[..length]).map_err(|_| {
        JournalError::InvalidEvent("event directory entry name is not canonical ASCII".to_owned())
    })?;
    if matches!(name, "." | "..") {
        Ok(None)
    } else {
        Ok(Some(name))
    }
}

fn clone_artifact_reports(
    reports: &[CommitArtifactReport],
    budget: &mut AssetLoadBudget,
) -> Result<Vec<CommitArtifactReport>, JournalError> {
    let mut cloned =
        journal_budgeted_vec(reports.len(), "journal result artifact reports", budget)?;
    for report in reports {
        cloned.push(CommitArtifactReport::new(
            budgeted_journal_string(
                report.logical_name(),
                "journal result artifact logical name",
                budget,
            )?,
            report.source(),
            report.digest(),
            report.bytes(),
        ));
    }
    Ok(cloned)
}

fn clone_change_set(
    changes: &ChangeSet,
    budget: &mut AssetLoadBudget,
) -> Result<ChangeSet, JournalError> {
    let mut changed_sources = journal_budgeted_vec(
        changes.changed_sources().len(),
        "journal result changed sources",
        budget,
    )?;
    changed_sources.extend_from_slice(changes.changed_sources());

    let mut changed_objects = journal_budgeted_vec(
        changes.changed_objects().len(),
        "journal result changed objects",
        budget,
    )?;
    for object in changes.changed_objects() {
        let retained = u64::try_from(object.retained_clone_bytes()).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "journal result changed object identity",
            }
        })?;
        budget.check_bytes(retained)?;
        changed_objects.push(object.clone());
        budget.consume_bytes(retained)?;
    }

    let mut identity_remaps = journal_budgeted_vec::<IdentityRemap>(
        changes.identity_remaps().len(),
        "journal result identity remaps",
        budget,
    )?;
    for remap in changes.identity_remaps() {
        let retained = remap
            .from()
            .retained_clone_bytes()
            .and_then(|bytes| {
                remap
                    .to()
                    .retained_clone_bytes()
                    .and_then(|other| bytes.checked_add(other))
            })
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "journal result identity remap",
            })?;
        let retained = u64::try_from(retained).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "journal result identity remap",
        })?;
        budget.check_bytes(retained)?;
        identity_remaps.push(
            IdentityRemap::new(remap.from().clone(), remap.to().clone())
                .map_err(|error| JournalError::InvalidManifest(error.to_string()))?,
        );
        budget.consume_bytes(retained)?;
    }

    ChangeSet::new(
        changes.transaction(),
        changes.workspace(),
        changes.from_revision(),
        changes.to_revision(),
        changed_sources,
        changed_objects,
        identity_remaps,
    )
    .map_err(|error| JournalError::InvalidManifest(error.to_string()))
}

fn validate_event_capacity(
    existing_targets: usize,
    absent_targets: usize,
) -> Result<(), JournalError> {
    required_event_capacity(existing_targets, absent_targets).map(|_| ())
}

fn required_event_capacity(
    existing_targets: usize,
    absent_targets: usize,
) -> Result<usize, JournalError> {
    let required = existing_targets
        .checked_mul(EXISTING_TARGET_EVENT_COUNT)
        .and_then(|events| {
            absent_targets
                .checked_mul(ABSENT_TARGET_EVENT_COUNT)
                .and_then(|absent| events.checked_add(absent))
        })
        .and_then(|events| events.checked_add(TRANSACTION_EVENT_RESERVE));
    if required.is_none_or(|events| events > MAX_EVENT_COUNT) {
        return Err(JournalError::InvalidManifest(format!(
            "manifest can require more than {MAX_EVENT_COUNT} durable events"
        )));
    }
    Ok(required.expect("validated event capacity"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub(crate) enum JournalEventKind {
    StagingVerified,
    Journaled,
    BackupIntent {
        artifact: JournalPath,
    },
    BackupCaptured {
        artifact: JournalPath,
    },
    PromotionIntent {
        artifact: JournalPath,
    },
    Promoted {
        artifact: JournalPath,
    },
    Published,
    BaselineInstalled,
    Finalized,
    RecoveryDecision {
        direction: RecoveryDirection,
    },
    Abandoned,
    RecoveryBlocked {
        reason: String,
    },
    /// Read-only compatibility record retained by journal version 3.
    Marker {
        name: String,
    },
}

fn event_kind_for_action(
    action: PublicationAction,
    manifest: &JournalManifest,
    budget: &mut AssetLoadBudget,
) -> Result<JournalEventKind, JournalError> {
    let target = |ordinal: u32, budget: &mut AssetLoadBudget| {
        let index = usize::try_from(ordinal).map_err(|_| {
            JournalError::InvalidEvent("artifact event ordinal overflowed".to_owned())
        })?;
        manifest
            .artifacts()
            .get(index)
            .ok_or_else(|| {
                JournalError::InvalidEvent("artifact event ordinal is out of range".to_owned())
            })?
            .target()
            .clone_budgeted(budget)
    };
    Ok(match action {
        PublicationAction::StagingVerified => JournalEventKind::StagingVerified,
        PublicationAction::Journaled => JournalEventKind::Journaled,
        PublicationAction::BackupIntent(ordinal) => JournalEventKind::BackupIntent {
            artifact: target(ordinal, budget)?,
        },
        PublicationAction::BackupCaptured(ordinal) => JournalEventKind::BackupCaptured {
            artifact: target(ordinal, budget)?,
        },
        PublicationAction::PromotionIntent(ordinal) => JournalEventKind::PromotionIntent {
            artifact: target(ordinal, budget)?,
        },
        PublicationAction::Promoted(ordinal) => JournalEventKind::Promoted {
            artifact: target(ordinal, budget)?,
        },
        PublicationAction::Published => JournalEventKind::Published,
        PublicationAction::BaselineInstalled => JournalEventKind::BaselineInstalled,
        PublicationAction::Finalized => JournalEventKind::Finalized,
        PublicationAction::RecoveryDecision(direction) => {
            JournalEventKind::RecoveryDecision { direction }
        }
        PublicationAction::Abandoned => JournalEventKind::Abandoned,
    })
}

impl JournalEventKind {
    fn validate(&self) -> Result<(), JournalError> {
        if let Self::RecoveryBlocked { reason } = self
            && (reason.is_empty() || reason.len() > MAX_REASON_BYTES)
        {
            return Err(JournalError::InvalidEvent(
                "recovery-blocked reason is empty or too long".to_owned(),
            ));
        }
        if let Self::Marker { name } = self
            && (name.is_empty() || name.len() > 256)
        {
            return Err(JournalError::InvalidEvent(
                "event marker name is empty or too long".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct JournalEventBody<'a> {
    version: u8,
    sequence: u64,
    previous: Option<DigestV1>,
    kind: &'a JournalEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct JournalEvent {
    version: u8,
    sequence: u64,
    previous: Option<DigestV1>,
    kind: JournalEventKind,
    digest: DigestV1,
}

impl JournalEvent {
    pub(crate) fn new(
        sequence: u64,
        previous: Option<DigestV1>,
        kind: JournalEventKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        kind.validate()?;
        let body = JournalEventBody {
            version: JOURNAL_VERSION,
            sequence,
            previous,
            kind: &kind,
        };
        let digest = digest_serialized(&body, "journal event digest", budget)?;
        let event = Self {
            version: JOURNAL_VERSION,
            sequence,
            previous,
            kind,
            digest,
        };
        Ok(event)
    }

    pub(crate) fn validate(&self, budget: &mut AssetLoadBudget) -> Result<(), JournalError> {
        if self.version != JOURNAL_VERSION && self.version != LEGACY_EVENT_VERSION {
            return Err(JournalError::UnsupportedVersion(self.version));
        }
        self.kind.validate()?;
        let body = JournalEventBody {
            version: self.version,
            sequence: self.sequence,
            previous: self.previous,
            kind: &self.kind,
        };
        let expected = digest_serialized(&body, "journal event digest", budget)?;
        if self.digest != expected {
            return Err(JournalError::DigestMismatch {
                expected,
                actual: self.digest,
            });
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub(crate) const fn digest(&self) -> DigestV1 {
        self.digest
    }

    #[must_use]
    pub(crate) fn kind(&self) -> &JournalEventKind {
        &self.kind
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct EventChain {
    events: Vec<JournalEvent>,
}

impl EventChain {
    pub(crate) fn from_events(
        events: Vec<JournalEvent>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        if events.len() > MAX_EVENT_COUNT {
            return Err(JournalError::TooManyEvents(events.len()));
        }
        let mut previous = None;
        for (index, event) in events.iter().enumerate() {
            event.validate(budget)?;
            let expected_sequence = u64::try_from(index)
                .map_err(|_| JournalError::InvalidEvent("event sequence overflow".to_owned()))?;
            if event.sequence != expected_sequence {
                return Err(JournalError::SequenceMismatch {
                    expected: expected_sequence,
                    actual: event.sequence,
                });
            }
            if event.previous != previous {
                return Err(JournalError::PreviousDigestMismatch {
                    sequence: event.sequence,
                    expected: previous,
                    actual: event.previous,
                });
            }
            previous = Some(event.digest);
        }
        Ok(Self { events })
    }

    #[must_use]
    pub(crate) fn events(&self) -> &[JournalEvent] {
        &self.events
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct JournalLayout {
    parent: PathBuf,
    root_identity: DirectoryIdentity,
    transaction: TransactionId,
    directory: PathBuf,
    manifest: PathBuf,
    preparation: PathBuf,
    preparation_temporary: PathBuf,
    rollback: PathBuf,
    events: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    baseline: PathBuf,
}

impl JournalLayout {
    pub(crate) fn new(
        parent: impl Into<PathBuf>,
        transaction: TransactionId,
        root_identity: DirectoryIdentity,
    ) -> Self {
        let parent = parent.into();
        let slug = transaction_slug(transaction);
        let slug = std::str::from_utf8(&slug).expect("transaction digest hex is valid UTF-8");
        let recovery = parent.join(RECOVERY_DIRECTORY);
        let version = recovery.join(RECOVERY_VERSION_DIRECTORY);
        let directory = version.join(slug);
        let manifest = directory.join(MANIFEST_FILE);
        let preparation = version.join(journal_suffixed_filename(slug, PREPARATION_SUFFIX));
        let preparation_temporary = version.join(journal_prefixed_suffixed_filename(
            ".",
            slug,
            PREPARATION_TEMPORARY_SUFFIX,
        ));
        let rollback = version.join(journal_suffixed_filename(slug, ROLLBACK_SUFFIX));
        let events = directory.join(EVENTS_DIRECTORY);
        let stage = directory.join(STAGE_DIRECTORY);
        let backup = directory.join(BACKUP_DIRECTORY);
        let baseline = directory.join(BASELINE_DIRECTORY);
        Self {
            parent,
            root_identity,
            transaction,
            directory,
            manifest,
            preparation,
            preparation_temporary,
            rollback,
            events,
            stage,
            backup,
            baseline,
        }
    }

    pub(crate) fn new_budgeted(
        parent: &Path,
        transaction: TransactionId,
        root_identity: DirectoryIdentity,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        let parent = budgeted_journal_path(parent, "journal layout parent path", budget)?;
        let slug = transaction_slug(transaction);
        let slug = std::str::from_utf8(&slug).expect("transaction digest hex is valid UTF-8");
        let recovery = budgeted_journal_join(
            &parent,
            RECOVERY_DIRECTORY,
            "journal recovery namespace path",
            budget,
        )?;
        let version = budgeted_journal_join(
            &recovery,
            RECOVERY_VERSION_DIRECTORY,
            "journal recovery version path",
            budget,
        )?;
        let directory =
            budgeted_journal_join(&version, slug, "journal transaction directory path", budget)?;
        let manifest =
            budgeted_journal_join(&directory, MANIFEST_FILE, "journal manifest path", budget)?;
        let preparation_name = budgeted_journal_suffixed_filename(
            "",
            slug,
            PREPARATION_SUFFIX,
            "journal preparation filename",
            budget,
        )?;
        let preparation = budgeted_journal_join(
            &version,
            &preparation_name,
            "journal preparation path",
            budget,
        )?;
        let preparation_temporary_name = budgeted_journal_suffixed_filename(
            ".",
            slug,
            PREPARATION_TEMPORARY_SUFFIX,
            "journal preparation temporary filename",
            budget,
        )?;
        let preparation_temporary = budgeted_journal_join(
            &version,
            &preparation_temporary_name,
            "journal preparation temporary path",
            budget,
        )?;
        let rollback_name = budgeted_journal_suffixed_filename(
            "",
            slug,
            ROLLBACK_SUFFIX,
            "journal rollback filename",
            budget,
        )?;
        let rollback =
            budgeted_journal_join(&version, &rollback_name, "journal rollback path", budget)?;
        let events = budgeted_journal_join(
            &directory,
            EVENTS_DIRECTORY,
            "journal event directory path",
            budget,
        )?;
        let stage = budgeted_journal_join(
            &directory,
            STAGE_DIRECTORY,
            "journal stage directory path",
            budget,
        )?;
        let backup = budgeted_journal_join(
            &directory,
            BACKUP_DIRECTORY,
            "journal backup directory path",
            budget,
        )?;
        let baseline = budgeted_journal_join(
            &directory,
            BASELINE_DIRECTORY,
            "journal baseline directory path",
            budget,
        )?;

        Ok(Self {
            parent,
            root_identity,
            transaction,
            directory,
            manifest,
            preparation,
            preparation_temporary,
            rollback,
            events,
            stage,
            backup,
            baseline,
        })
    }

    #[must_use]
    pub(crate) fn parent(&self) -> &Path {
        &self.parent
    }

    /// Rejects a pathname that was rebound after this transaction acquired its
    /// publication root. Callers use this only as an early stop; every write
    /// still carries the identity of its immediate opened parent.
    pub(crate) fn verify_root_path_binding(&self) -> io::Result<()> {
        if observe_directory_identity(&self.parent)? != self.root_identity {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "publication root identity changed during journal operation",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn root_identity(&self) -> &DirectoryIdentity {
        &self.root_identity
    }

    #[must_use]
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn directory_path_budgeted(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<PathBuf, JournalError> {
        budgeted_journal_path(&self.directory, "journal recovery locator path", budget)
    }

    #[must_use]
    pub(crate) const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    #[must_use]
    pub(crate) fn manifest_path(&self) -> &Path {
        &self.manifest
    }

    #[must_use]
    pub(crate) fn preparation_path(&self) -> &Path {
        &self.preparation
    }

    #[must_use]
    pub(crate) fn preparation_temporary_path(&self) -> &Path {
        &self.preparation_temporary
    }

    #[must_use]
    pub(crate) fn rollback_path(&self) -> &Path {
        &self.rollback
    }

    #[must_use]
    pub(crate) fn events_directory(&self) -> &Path {
        &self.events
    }

    #[must_use]
    pub(crate) fn stage_directory(&self) -> &Path {
        &self.stage
    }

    #[must_use]
    pub(crate) fn backup_directory(&self) -> &Path {
        &self.backup
    }

    #[must_use]
    pub(crate) fn baseline_directory(&self) -> &Path {
        &self.baseline
    }
}

fn transaction_slug(transaction: TransactionId) -> [u8; DigestV1::BYTE_LEN * 2] {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut slug = [0_u8; DigestV1::BYTE_LEN * 2];
    for (index, byte) in transaction.digest().as_bytes().iter().copied().enumerate() {
        slug[index * 2] = HEX[usize::from(byte >> 4)];
        slug[index * 2 + 1] = HEX[usize::from(byte & 0x0f)];
    }
    slug
}

fn journal_suffixed_filename(slug: &str, suffix: &str) -> String {
    journal_prefixed_suffixed_filename("", slug, suffix)
}

fn journal_prefixed_suffixed_filename(prefix: &str, slug: &str, suffix: &str) -> String {
    format!("{prefix}{slug}.{suffix}")
}

fn budgeted_journal_suffixed_filename(
    prefix: &str,
    slug: &str,
    suffix: &str,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<String, JournalError> {
    let requested = prefix
        .len()
        .checked_add(slug.len())
        .and_then(|length| length.checked_add(1))
        .and_then(|length| length.checked_add(suffix.len()))
        .ok_or(BudgetError::ArithmeticOverflow { resource })?;
    let mut name = budgeted_empty_journal_string(requested, resource, budget)?;
    name.push_str(prefix);
    name.push_str(slug);
    name.push('.');
    name.push_str(suffix);
    Ok(name)
}

#[derive(Debug)]
pub(crate) struct Journal {
    layout: JournalLayout,
    manifest: JournalManifest,
    chain: EventChain,
    _transaction_directory: JournalDirectory,
    events_directory: JournalDirectory,
    stage_directory: JournalDirectory,
    backup_directory: JournalDirectory,
    _baseline_directory: JournalDirectory,
    next_temporary_attempt: u32,
}

struct OpenedJournalDirectories {
    transaction: JournalDirectory,
    events: JournalDirectory,
    stage: JournalDirectory,
    backup: JournalDirectory,
    baseline: JournalDirectory,
}

pub(crate) struct PlannedJournalEvent {
    key: PublicationAction,
    event: JournalEvent,
    destination: PathBuf,
    temporary: PathBuf,
    encoded: Vec<u8>,
}

impl PlannedJournalEvent {
    #[must_use]
    pub(crate) const fn action(&self) -> PublicationAction {
        self.key
    }
}

pub(crate) struct JournalEventPlan {
    events: std::vec::IntoIter<PlannedJournalEvent>,
}

impl Iterator for JournalEventPlan {
    type Item = PlannedJournalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.events.next()
    }
}

#[derive(Debug, Error)]
#[error("{source}")]
pub(crate) struct JournalCreateError {
    layout: Box<JournalLayout>,
    #[source]
    source: Box<JournalError>,
    manifest_installed: bool,
}

impl JournalCreateError {
    #[must_use]
    pub(crate) const fn manifest_installed(&self) -> bool {
        self.manifest_installed
    }

    pub(crate) fn into_parts(self) -> (JournalLayout, JournalError) {
        (*self.layout, *self.source)
    }

    #[cfg(test)]
    pub(crate) fn journal_error(&self) -> &JournalError {
        self.source.as_ref()
    }
}

fn open_journal_directories(
    access: &JournalAccess<'_>,
    layout: &JournalLayout,
    manifest: &JournalManifest,
) -> Result<OpenedJournalDirectories, JournalError> {
    let transaction = super::platform::open_journal_directory(access, layout.directory())?;
    validate_opened_journal_directory(
        &transaction,
        manifest.directories.transaction(),
        "transaction",
    )?;
    let events = super::platform::open_journal_directory_in_directory(
        &transaction,
        layout.events_directory(),
    )?;
    validate_opened_journal_directory(&events, manifest.directories.events(), "events")?;
    let stage = super::platform::open_journal_directory_in_directory(
        &transaction,
        layout.stage_directory(),
    )?;
    validate_opened_journal_directory(&stage, manifest.directories.stage(), "stage")?;
    let backup = super::platform::open_journal_directory_in_directory(
        &transaction,
        layout.backup_directory(),
    )?;
    validate_opened_journal_directory(&backup, manifest.directories.backup(), "backup")?;
    let baseline = super::platform::open_journal_directory_in_directory(
        &transaction,
        layout.baseline_directory(),
    )?;
    validate_opened_journal_directory(&baseline, manifest.directories.baseline(), "baseline")?;
    Ok(OpenedJournalDirectories {
        transaction,
        events,
        stage,
        backup,
        baseline,
    })
}

fn validate_opened_journal_directory(
    directory: &JournalDirectory,
    expected: &DirectoryIdentity,
    role: &'static str,
) -> Result<(), JournalError> {
    if super::platform::journal_directory_identity(directory)? != *expected {
        return Err(JournalError::InvalidManifest(format!(
            "opened {role} directory does not match the journal manifest"
        )));
    }
    Ok(())
}

impl Journal {
    #[cfg(test)]
    pub(crate) fn create(
        layout: JournalLayout,
        manifest: JournalManifest,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalCreateError> {
        let (journal, event_plan) = Self::create_planned(layout, manifest, &[], budget)?;
        debug_assert_eq!(event_plan.events.len(), 0);
        Ok(journal)
    }

    #[cfg(test)]
    pub(crate) fn create_planned(
        layout: JournalLayout,
        manifest: JournalManifest,
        event_keys: &[PublicationAction],
        budget: &mut AssetLoadBudget,
    ) -> Result<(Self, JournalEventPlan), JournalCreateError> {
        let root = super::platform::open_commit_root(layout.parent(), layout.root_identity())
            .map_err(|source| JournalCreateError {
                layout: Box::new(layout.clone()),
                source: Box::new(JournalError::Io(source)),
                manifest_installed: false,
            })?;
        let namespace = super::platform::open_journal_namespace(&root).map_err(|source| {
            JournalCreateError {
                layout: Box::new(layout.clone()),
                source: Box::new(JournalError::Io(source)),
                manifest_installed: false,
            }
        })?;
        let access = super::platform::journal_access(&root, &namespace);
        Self::create_planned_in_access(layout, manifest, event_keys, &access, budget)
    }

    pub(crate) fn create_planned_in_access(
        layout: JournalLayout,
        manifest: JournalManifest,
        event_keys: &[PublicationAction],
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<(Self, JournalEventPlan), JournalCreateError> {
        let mut manifest_installed = false;
        let result = (|| {
            manifest.validate()?;
            manifest.validate_transaction_identity(
                layout.parent(),
                layout.root_identity(),
                budget,
            )?;
            JournalPreparation::open_in_access(&layout, access, budget)?
                .document()
                .validate_manifest(&manifest)?;
            let directories = open_journal_directories(access, &layout, &manifest)?;
            let chain = EventChain {
                events: journal_budgeted_vec(
                    manifest.event_capacity()?,
                    "journal event chain",
                    budget,
                )?,
            };
            let manifest_temporary = budgeted_journal_join(
                layout.directory(),
                MANIFEST_TEMPORARY_FILE,
                "journal manifest temporary path",
                budget,
            )?;
            let manifest_bytes = encode_json_bounded(
                layout.manifest_path(),
                &manifest,
                MAX_MANIFEST_BYTES,
                budget,
            )?;
            let event_plan =
                Self::plan_events_for(&layout, &manifest, &chain, 0, event_keys, budget)?;
            write_encoded_atomic_in_journal_directory_tracked(
                &directories.transaction,
                layout.manifest_path(),
                &manifest_bytes,
                false,
                &manifest_temporary,
                &mut manifest_installed,
            )?;
            Ok::<(EventChain, JournalEventPlan, OpenedJournalDirectories), JournalError>((
                chain,
                event_plan,
                directories,
            ))
        })();
        match result {
            Ok((chain, event_plan, directories)) => Ok((
                Self {
                    layout,
                    manifest,
                    chain,
                    _transaction_directory: directories.transaction,
                    events_directory: directories.events,
                    stage_directory: directories.stage,
                    backup_directory: directories.backup,
                    _baseline_directory: directories.baseline,
                    next_temporary_attempt: 0,
                },
                event_plan,
            )),
            Err(source) => Err(JournalCreateError {
                layout: Box::new(layout),
                source: Box::new(source),
                manifest_installed,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn open(
        layout: JournalLayout,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        let root = super::platform::open_commit_root(layout.parent(), layout.root_identity())?;
        let namespace = super::platform::open_journal_namespace(&root)?;
        let access = super::platform::journal_access(&root, &namespace);
        Self::open_in_access(layout, &access, budget)
    }

    pub(crate) fn open_in_access(
        layout: JournalLayout,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        let transaction = super::platform::open_journal_directory(access, layout.directory())?;
        let manifest = read_json_bounded_from_file(
            layout.manifest_path(),
            super::platform::open_journal_regular_in_directory(
                &transaction,
                layout.manifest_path(),
            )?,
            MANIFEST_JSON_LIMITS,
            budget,
        )?;
        let preparation = JournalPreparation::open_in_access(&layout, access, budget)?;
        Self::open_loaded_in_access(layout, manifest, preparation, access, budget)
    }

    fn open_loaded_in_access(
        layout: JournalLayout,
        manifest: JournalManifest,
        preparation: OpenedJournalPreparation,
        access: &JournalAccess<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, JournalError> {
        manifest.validate()?;
        manifest.validate_transaction_identity(layout.parent(), layout.root_identity(), budget)?;
        preparation.document().validate_manifest(&manifest)?;
        if manifest.transaction() != layout.transaction {
            return Err(JournalError::TransactionMismatch {
                expected: layout.transaction,
                actual: manifest.transaction(),
            });
        }
        let directories = open_journal_directories(access, &layout, &manifest)?;
        let mut events =
            journal_budgeted_vec(manifest.event_capacity()?, "journal event chain", budget)?;
        let mut maximum_temporary = None;
        let mut directory_entries = 0_usize;
        let mut scratch = [0_u8; EVENT_TEMPORARY_FILENAME_BYTES];
        visit_opened_journal_directory(&directories.events, budget, |budget, entry| {
            let Some(name) = journal_entry_name(entry, &mut scratch)? else {
                return Ok(());
            };
            directory_entries = directory_entries.checked_add(1).ok_or_else(|| {
                JournalError::InvalidEvent("event directory entry count overflow".to_owned())
            })?;
            if directory_entries > MAX_EVENT_COUNT {
                return Err(JournalError::TooManyEvents(directory_entries));
            }
            budget.consume_entries(1)?;
            let path = budgeted_journal_join(
                layout.events_directory(),
                name,
                "journal event entry path",
                budget,
            )?;
            if name.starts_with('.') && name.ends_with(".tmp") {
                super::platform::open_journal_regular_in_directory(&directories.events, &path)?;
                let temporary = parse_event_temporary_filename(name)?;
                if maximum_temporary.is_none_or(|current| temporary > current) {
                    maximum_temporary = Some(temporary);
                }
                return Ok(());
            }
            let (sequence, digest) = parse_event_filename(name)?;
            let event: JournalEvent = read_json_bounded_from_file(
                &path,
                super::platform::open_journal_regular_in_directory(&directories.events, &path)?,
                EVENT_JSON_LIMITS,
                budget,
            )?;
            if event.sequence() != sequence || event.digest() != digest {
                return Err(JournalError::InvalidEvent(
                    "event filename does not match its record".to_owned(),
                ));
            }
            if events.len() == events.capacity() {
                journal_reserve_one(&mut events, "journal event chain growth", budget)?;
            }
            events.push(event);
            Ok(())
        })?;
        events.sort_by_key(JournalEvent::sequence);
        let chain = EventChain::from_events(events, budget)?;
        let expected = u64::try_from(chain.events().len())
            .map_err(|_| JournalError::InvalidEvent("event sequence overflow".to_owned()))?;
        let next_temporary_attempt = if let Some((sequence, attempt)) = maximum_temporary {
            if chain.events().len() >= MAX_EVENT_COUNT || sequence > expected {
                return Err(JournalError::SequenceMismatch {
                    expected,
                    actual: sequence,
                });
            }
            if sequence == expected {
                attempt.checked_add(1).ok_or_else(|| {
                    JournalError::InvalidEvent("event temporary attempt overflow".to_owned())
                })?
            } else {
                0
            }
        } else {
            0
        };
        Ok(Self {
            layout,
            manifest,
            chain,
            _transaction_directory: directories.transaction,
            events_directory: directories.events,
            stage_directory: directories.stage,
            backup_directory: directories.backup,
            _baseline_directory: directories.baseline,
            next_temporary_attempt,
        })
    }

    #[must_use]
    pub(crate) fn layout(&self) -> &JournalLayout {
        &self.layout
    }

    #[must_use]
    pub(crate) fn manifest(&self) -> &JournalManifest {
        &self.manifest
    }

    #[must_use]
    pub(crate) const fn stage_directory(&self) -> &JournalDirectory {
        &self.stage_directory
    }

    #[must_use]
    pub(crate) const fn backup_directory(&self) -> &JournalDirectory {
        &self.backup_directory
    }

    #[must_use]
    pub(crate) fn events(&self) -> &[JournalEvent] {
        self.chain.events()
    }

    pub(crate) fn plan_events(
        &self,
        keys: &[PublicationAction],
        budget: &mut AssetLoadBudget,
    ) -> Result<JournalEventPlan, JournalError> {
        Self::plan_events_for(
            &self.layout,
            &self.manifest,
            &self.chain,
            self.next_temporary_attempt,
            keys,
            budget,
        )
    }

    fn plan_events_for(
        layout: &JournalLayout,
        manifest: &JournalManifest,
        chain: &EventChain,
        next_temporary_attempt: u32,
        keys: &[PublicationAction],
        budget: &mut AssetLoadBudget,
    ) -> Result<JournalEventPlan, JournalError> {
        let total = chain
            .events()
            .len()
            .checked_add(keys.len())
            .ok_or(JournalError::TooManyEvents(usize::MAX))?;
        if total > MAX_EVENT_COUNT {
            return Err(JournalError::TooManyEvents(total));
        }
        if total > chain.events.capacity() {
            return Err(JournalError::InvalidEvent(
                "journal event chain was not preallocated for its execution plan".to_owned(),
            ));
        }
        budget.consume_entries(u64::try_from(keys.len()).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "journal event plan",
            }
        })?)?;
        let mut events = journal_budgeted_vec(keys.len(), "journal event plan", budget)?;
        let events_directory = budgeted_journal_join(
            layout.directory(),
            EVENTS_DIRECTORY,
            "journal event directory path",
            budget,
        )?;
        let mut sequence = u64::try_from(chain.events().len())
            .map_err(|_| JournalError::InvalidEvent("event sequence overflow".to_owned()))?;
        let mut previous = chain.events().last().map(JournalEvent::digest);
        for (offset, key) in keys.iter().copied().enumerate() {
            let kind = event_kind_for_action(key, manifest, budget)?;
            let event = JournalEvent::new(sequence, previous, kind, budget)?;
            let filename = budgeted_event_filename(&event, budget)?;
            let destination = budgeted_journal_join(
                &events_directory,
                &filename,
                "journal event destination",
                budget,
            )?;
            let attempt = if offset == 0 {
                next_temporary_attempt
            } else {
                0
            };
            let temporary_name = budgeted_event_temporary_filename(&filename, attempt, budget)?;
            let temporary = budgeted_journal_join(
                &events_directory,
                &temporary_name,
                "journal event temporary path",
                budget,
            )?;
            let encoded = encode_json_bounded(&destination, &event, MAX_EVENT_BYTES, budget)?;
            previous = Some(event.digest());
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| JournalError::InvalidEvent("event sequence overflow".to_owned()))?;
            events.push(PlannedJournalEvent {
                key,
                event,
                destination,
                temporary,
                encoded,
            });
        }
        Ok(JournalEventPlan {
            events: events.into_iter(),
        })
    }

    pub(crate) fn append_planned(
        &mut self,
        prepared: PlannedJournalEvent,
    ) -> Result<(), JournalError> {
        self.layout.verify_root_path_binding()?;
        if self.chain.events.len() >= self.chain.events.capacity() {
            return Err(JournalError::InvalidEvent(
                "journal event chain capacity was exhausted before durable append".to_owned(),
            ));
        }
        if prepared.event.sequence()
            != u64::try_from(self.chain.events.len())
                .map_err(|_| JournalError::InvalidEvent("event sequence overflow".to_owned()))?
            || prepared.event.previous != self.chain.events.last().map(JournalEvent::digest)
        {
            return Err(JournalError::InvalidEvent(
                "event chain changed after plan construction".to_owned(),
            ));
        }
        let mut installed = false;
        write_encoded_atomic_in_journal_directory_tracked(
            &self.events_directory,
            &prepared.destination,
            &prepared.encoded,
            false,
            &prepared.temporary,
            &mut installed,
        )?;
        self.chain.events.push(prepared.event);
        self.next_temporary_attempt = 0;
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        kind: JournalEventKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), JournalError> {
        if self.chain.events.len() >= MAX_EVENT_COUNT {
            return Err(JournalError::TooManyEvents(self.chain.events.len() + 1));
        }
        if self.chain.events.len() == self.chain.events.capacity() {
            journal_reserve_one(&mut self.chain.events, "journal event chain growth", budget)?;
        }
        let sequence = u64::try_from(self.chain.events.len())
            .map_err(|_| JournalError::InvalidEvent("event sequence overflow".to_owned()))?;
        let previous = self.chain.events.last().map(JournalEvent::digest);
        budget.consume_entries(1)?;
        let event = JournalEvent::new(sequence, previous, kind, budget)?;
        let events_directory = budgeted_journal_join(
            self.layout.directory(),
            EVENTS_DIRECTORY,
            "journal event directory path",
            budget,
        )?;
        let filename = budgeted_event_filename(&event, budget)?;
        let destination = budgeted_journal_join(
            &events_directory,
            &filename,
            "journal event destination",
            budget,
        )?;
        let temporary_attempt = self.next_temporary_attempt;
        self.next_temporary_attempt = temporary_attempt.checked_add(1).ok_or_else(|| {
            JournalError::InvalidEvent("event temporary attempt overflow".to_owned())
        })?;
        let encoded = encode_json_bounded(&destination, &event, MAX_EVENT_BYTES, budget)?;
        let temporary_name =
            budgeted_event_temporary_filename(&filename, temporary_attempt, budget)?;
        let temporary = budgeted_journal_join(
            &events_directory,
            &temporary_name,
            "journal event temporary path",
            budget,
        )?;
        let mut installed = false;
        write_encoded_atomic_in_journal_directory_tracked(
            &self.events_directory,
            &destination,
            &encoded,
            false,
            &temporary,
            &mut installed,
        )?;
        self.chain.events.push(event);
        self.next_temporary_attempt = 0;
        Ok(())
    }
}

fn budgeted_event_filename(
    event: &JournalEvent,
    budget: &mut AssetLoadBudget,
) -> Result<String, JournalError> {
    let mut name =
        budgeted_empty_journal_string(EVENT_FILENAME_BYTES, "journal event filename", budget)?;
    write!(&mut name, "{:020}-", event.sequence())
        .map_err(|_| JournalError::InvalidEvent("event filename formatting failed".to_owned()))?;
    for byte in event.digest().as_bytes() {
        write!(&mut name, "{byte:02x}").map_err(|_| {
            JournalError::InvalidEvent("event filename formatting failed".to_owned())
        })?;
    }
    name.push_str(".json");
    if name.len() != EVENT_FILENAME_BYTES {
        return Err(JournalError::InvalidEvent(
            "event filename has a non-canonical length".to_owned(),
        ));
    }
    Ok(name)
}

fn budgeted_event_temporary_filename(
    event_filename: &str,
    attempt: u32,
    budget: &mut AssetLoadBudget,
) -> Result<String, JournalError> {
    if attempt > 99_999_999 {
        return Err(JournalError::InvalidEvent(
            "event temporary attempt limit was exhausted".to_owned(),
        ));
    }
    let mut name = budgeted_empty_journal_string(
        EVENT_TEMPORARY_FILENAME_BYTES,
        "journal event temporary name",
        budget,
    )?;
    name.push('.');
    name.push_str(event_filename);
    write!(&mut name, ".attempt-{attempt:08}.tmp").map_err(|_| {
        JournalError::InvalidEvent("event temporary filename formatting failed".to_owned())
    })?;
    if name.len() != EVENT_TEMPORARY_FILENAME_BYTES {
        return Err(JournalError::InvalidEvent(
            "event temporary filename has a non-canonical length".to_owned(),
        ));
    }
    Ok(name)
}

#[cfg(test)]
fn event_temporary_filename(event: &JournalEvent, attempt: u32) -> String {
    let mut budget = AssetLoadBudget::default();
    let filename = budgeted_event_filename(event, &mut budget).expect("event filename");
    budgeted_event_temporary_filename(&filename, attempt, &mut budget)
        .expect("event temporary filename")
}

fn parse_event_temporary_filename(name: &str) -> Result<(u64, u32), JournalError> {
    if name.len() != EVENT_TEMPORARY_FILENAME_BYTES {
        return Err(JournalError::InvalidEvent(
            "event temporary filename has a non-canonical length".to_owned(),
        ));
    }
    let temporary = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .ok_or_else(|| {
            JournalError::InvalidEvent("event temporary filename is malformed".to_owned())
        })?;
    let (event_name, attempt) = temporary.rsplit_once(".attempt-").ok_or_else(|| {
        JournalError::InvalidEvent("event temporary filename is malformed".to_owned())
    })?;
    if attempt.len() != 8 || !attempt.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(JournalError::InvalidEvent(
            "event temporary attempt is malformed".to_owned(),
        ));
    }
    let attempt = attempt.parse::<u32>().map_err(|_| {
        JournalError::InvalidEvent("event temporary attempt is malformed".to_owned())
    })?;
    let (sequence, _) = parse_event_filename(event_name)?;
    Ok((sequence, attempt))
}

fn parse_event_filename(name: &str) -> Result<(u64, DigestV1), JournalError> {
    if name.len() != EVENT_FILENAME_BYTES {
        return Err(JournalError::InvalidEvent(
            "event filename has a non-canonical length".to_owned(),
        ));
    }
    let stem = name
        .strip_suffix(".json")
        .ok_or_else(|| JournalError::InvalidEvent("event filename is not JSON".to_owned()))?;
    let (sequence, digest) = stem
        .split_once('-')
        .ok_or_else(|| JournalError::InvalidEvent("event filename is malformed".to_owned()))?;
    if sequence.len() != 20 || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(JournalError::InvalidEvent(
            "event filename sequence is not canonical".to_owned(),
        ));
    }
    let sequence = sequence
        .parse::<u64>()
        .map_err(|_| JournalError::InvalidEvent("event sequence is malformed".to_owned()))?;
    if digest.len() != DigestV1::BYTE_LEN * 2
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(JournalError::InvalidEvent(
            "event filename digest is malformed".to_owned(),
        ));
    }
    let mut decoded = [0_u8; DigestV1::BYTE_LEN];
    for (index, output) in decoded.iter_mut().enumerate() {
        let offset = index * 2;
        *output = (decode_lower_hex(digest.as_bytes()[offset])? << 4)
            | decode_lower_hex(digest.as_bytes()[offset + 1])?;
    }
    Ok((sequence, DigestV1::from_bytes(decoded)))
}

fn decode_lower_hex(byte: u8) -> Result<u8, JournalError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(JournalError::InvalidEvent(
            "event filename digest is malformed".to_owned(),
        )),
    }
}

#[cfg(test)]
fn write_encoded_atomic_with_temporary_path_tracked(
    path: &Path,
    bytes: &[u8],
    replace_existing: bool,
    temporary: &Path,
    expected_parent: &DirectoryIdentity,
    installed: &mut bool,
) -> Result<(), JournalError> {
    let parent = path.parent().ok_or_else(|| JournalError::InvalidPath {
        path: path.display().to_string(),
        reason: "journal file has no parent",
    })?;
    if temporary.parent() != Some(parent) {
        return Err(JournalError::InvalidPath {
            path: temporary.display().to_string(),
            reason: "journal temporary file does not share its destination directory",
        });
    }
    let mut file = create_private_file_in_parent(temporary, expected_parent)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    test_journal_temporary_sync_failpoint(path);
    drop(file);
    match atomic_replace_tracked(
        temporary,
        path,
        replace_existing,
        expected_parent,
        expected_parent,
    ) {
        Ok(()) => *installed = true,
        Err(error) => {
            if error.moved_or_unknown_state()
                || error.error().kind() == io::ErrorKind::AlreadyExists
            {
                *installed = true;
            }
            return Err(JournalError::Io(error.into_error()));
        }
    }
    Ok(())
}

pub(super) fn write_encoded_atomic_in_journal_access_tracked(
    access: &JournalAccess<'_>,
    path: &Path,
    bytes: &[u8],
    replace_existing: bool,
    temporary: &Path,
    installed: &mut bool,
) -> Result<(), JournalError> {
    let mut file = create_journal_regular(access, temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    test_journal_temporary_sync_failpoint(path);
    drop(file);
    match atomic_replace_journal_regular(access, temporary, path, replace_existing) {
        Ok(()) => *installed = true,
        Err(error) => {
            if error.moved_or_unknown_state()
                || error.error().kind() == io::ErrorKind::AlreadyExists
            {
                *installed = true;
            }
            return Err(JournalError::Io(error.into_error()));
        }
    }
    sync_journal_access(access)?;
    Ok(())
}

pub(super) fn write_encoded_atomic_in_journal_directory_tracked(
    directory: &JournalDirectory,
    path: &Path,
    bytes: &[u8],
    replace_existing: bool,
    temporary: &Path,
    installed: &mut bool,
) -> Result<(), JournalError> {
    let mut file = create_journal_regular_in_directory(directory, temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    test_journal_temporary_sync_failpoint(path);
    drop(file);
    match atomic_replace_journal_regular_in_directory(directory, temporary, path, replace_existing)
    {
        Ok(()) => *installed = true,
        Err(error) => {
            if error.moved_or_unknown_state()
                || error.error().kind() == io::ErrorKind::AlreadyExists
            {
                *installed = true;
            }
            return Err(JournalError::Io(error.into_error()));
        }
    }
    sync_journal_directory(directory)?;
    Ok(())
}

fn test_journal_temporary_sync_failpoint(path: &Path) {
    #[cfg(test)]
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        if name == MANIFEST_FILE {
            super::test_crash_failpoint("manifest_temporary_synced");
        } else if name.ends_with(".prepare.v2.json") {
            super::test_crash_failpoint("preparation_temporary_synced");
        }
    }
    #[cfg(not(test))]
    let _ = path;
}

fn encode_json_bounded<T: Serialize>(
    path: &Path,
    value: &T,
    maximum: u64,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, JournalError> {
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, value)?;
    if counter.bytes > maximum {
        return Err(JournalError::DocumentTooLarge {
            path: path.to_owned(),
            bytes: counter.bytes,
            maximum,
        });
    }
    budget.check_bytes(counter.bytes)?;
    let requested = usize::try_from(counter.bytes).map_err(|_| {
        JournalError::Budget(BudgetError::ArithmeticOverflow {
            resource: "journal JSON encoding",
        })
    })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(requested)
        .map_err(|error| JournalError::Allocation {
            resource: "journal JSON encoding",
            requested,
            message: error.to_string(),
        })?;
    serde_json::to_writer(&mut encoded, value)?;
    if encoded.len() != requested {
        return Err(JournalError::InvalidManifest(
            "canonical JSON length changed between encoding passes".to_owned(),
        ));
    }
    let actual = u64::try_from(encoded.capacity()).map_err(|_| {
        JournalError::Budget(BudgetError::ArithmeticOverflow {
            resource: "journal JSON encoding",
        })
    })?;
    budget.check_bytes(actual)?;
    budget.consume_bytes(actual)?;
    Ok(encoded)
}

#[cfg(test)]
fn read_json_bounded_in_parent<T: DeserializeOwned>(
    path: &Path,
    expected_parent: &DirectoryIdentity,
    limits: ContractJsonLimits,
    budget: &mut AssetLoadBudget,
) -> Result<T, JournalError> {
    read_json_bounded_from_file(
        path,
        open_readonly_regular_in_parent(path, expected_parent)?,
        limits,
        budget,
    )
}

fn read_json_bounded_from_file<T: DeserializeOwned>(
    path: &Path,
    file: File,
    limits: ContractJsonLimits,
    budget: &mut AssetLoadBudget,
) -> Result<T, JournalError> {
    let maximum = u64::try_from(limits.max_encoded_bytes()).map_err(|_| {
        JournalError::Budget(BudgetError::ArithmeticOverflow {
            resource: "journal JSON encoded limit",
        })
    })?;
    let metadata = file.metadata()?;
    if metadata.len() > maximum {
        return Err(JournalError::DocumentTooLarge {
            path: path.to_owned(),
            bytes: metadata.len(),
            maximum,
        });
    }
    let reader = ExactLengthJournalJsonReader::new(file, metadata.len());
    read_contract_json(reader, budget, limits).map_err(|error| map_contract_json_error(path, error))
}

struct ExactLengthJournalJsonReader {
    file: File,
    expected: u64,
    observed: u64,
}

impl ExactLengthJournalJsonReader {
    fn new(file: File, expected: u64) -> Self {
        Self {
            file,
            expected,
            observed: 0,
        }
    }
}

impl Read for ExactLengthJournalJsonReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let read = self.file.read(buffer)?;
        let read_u64 = u64::try_from(read).map_err(|_| journal_length_changed_grew())?;
        let observed = self
            .observed
            .checked_add(read_u64)
            .ok_or_else(journal_length_changed_grew)?;
        if observed > self.expected {
            return Err(journal_length_changed_grew());
        }
        self.observed = observed;
        if read == 0 && observed != self.expected {
            return Err(journal_length_changed_shrank());
        }
        Ok(read)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
enum JournalLengthChange {
    #[error("journal entry grew while it was read")]
    Grew,
    #[error("journal entry shrank while it was read")]
    Shrank,
}

fn journal_length_changed_grew() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, JournalLengthChange::Grew)
}

fn journal_length_changed_shrank() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, JournalLengthChange::Shrank)
}

fn map_contract_json_error(path: &Path, error: BudgetedJsonError) -> JournalError {
    match error {
        BudgetedJsonError::Io(source) => {
            let length_change = source
                .get_ref()
                .and_then(|error| error.downcast_ref::<JournalLengthChange>())
                .copied();
            if length_change == Some(JournalLengthChange::Grew) {
                JournalError::InvalidEvent(
                    "journal entry length changed while it was read".to_owned(),
                )
            } else {
                JournalError::Io(source)
            }
        }
        BudgetedJsonError::Budget(source) => JournalError::Budget(source),
        BudgetedJsonError::AllocationFailed { requested } => JournalError::Allocation {
            resource: "journal JSON input",
            requested,
            message: "contract JSON input allocation failed".to_owned(),
        },
        BudgetedJsonError::Json(source) => JournalError::Json(source),
        BudgetedJsonError::InvalidLimit { resource, .. } => {
            JournalError::Budget(BudgetError::InvalidLimit { resource })
        }
        BudgetedJsonError::EncodedLimitExceeded {
            limit, requested, ..
        } => JournalError::DocumentTooLarge {
            path: path.to_owned(),
            bytes: u64::try_from(requested).unwrap_or(u64::MAX),
            maximum: u64::try_from(limit).unwrap_or(u64::MAX),
        },
        BudgetedJsonError::StructureLimitExceeded {
            resource: "depth",
            requested,
            ..
        } => JournalError::NestingDepthExceeded {
            actual: u32::try_from(requested).unwrap_or(u32::MAX),
        },
        BudgetedJsonError::StructureLimitExceeded {
            resource,
            limit,
            requested,
            ..
        } => JournalError::Budget(BudgetError::Exceeded {
            resource,
            limit,
            requested,
        }),
    }
}

#[derive(Debug, Error)]
pub(crate) enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("journal JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("failed to allocate {requested} entries for {resource}: {message}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        message: String,
    },
    #[error("journal JSON nesting depth {actual} exceeds the hard limit {MAX_JOURNAL_DEPTH}")]
    NestingDepthExceeded { actual: u32 },
    #[error("journal version {0} is unsupported")]
    UnsupportedVersion(u8),
    #[error("journal path {path:?} is invalid: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("invalid journal manifest: {0}")]
    InvalidManifest(String),
    #[error("invalid journal event: {0}")]
    InvalidEvent(String),
    #[error("journal document {path:?} has {bytes} bytes; maximum is {maximum}")]
    DocumentTooLarge {
        path: PathBuf,
        bytes: u64,
        maximum: u64,
    },
    #[error("journal contains {0} events; maximum is {MAX_EVENT_COUNT}")]
    TooManyEvents(usize),
    #[error("journal event sequence expected {expected}, got {actual}")]
    SequenceMismatch { expected: u64, actual: u64 },
    #[error(
        "journal event {sequence} has wrong previous digest (expected {expected:?}, got {actual:?})"
    )]
    PreviousDigestMismatch {
        sequence: u64,
        expected: Option<DigestV1>,
        actual: Option<DigestV1>,
    },
    #[error("journal event digest mismatch (expected {expected}, got {actual})")]
    DigestMismatch {
        expected: DigestV1,
        actual: DigestV1,
    },
    #[error("journal belongs to transaction {actual}, expected {expected}")]
    TransactionMismatch {
        expected: TransactionId,
        actual: TransactionId,
    },
}

impl fmt::Display for JournalPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use unity_asset_core::{AssetLoadLimits, BudgetError, SourceKind, WorkspaceId};

    #[derive(Debug, PartialEq, Eq)]
    struct MaterializationProbe;

    impl<'de> Deserialize<'de> for MaterializationProbe {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            <serde::de::IgnoredAny as serde::Deserialize<'de>>::deserialize(deserializer)?;
            Ok(Self)
        }
    }

    #[derive(Debug)]
    struct DeserializationMustNotStart;

    impl<'de> Deserialize<'de> for DeserializationMustNotStart {
        fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            panic!("typed journal deserialization started before materialization was reserved");
        }
    }

    fn materialization_gate_bytes(fixed: u64, per_entry: u64) -> u64 {
        let encoded = u64::try_from(b"null".len()).expect("fixture length");
        let parser_and_input = encoded
            .checked_mul(PARSER_WORK_BYTES_PER_INPUT_BYTE + 1)
            .and_then(|bytes| bytes.checked_add(PARSER_FIXED_WORK_BYTES))
            .expect("parser charge");
        parser_and_input
            .checked_add(fixed)
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(size_of::<MaterializationProbe>()).expect("root layout"),
                )
            })
            .and_then(|bytes| bytes.checked_add(per_entry))
            .expect("materialization charge")
    }

    fn assert_materialization_gate(
        fixture_name: &str,
        limits: ContractJsonLimits,
        fixed: u64,
        per_entry: u64,
    ) {
        assert_eq!(
            size_of::<MaterializationProbe>(),
            size_of::<DeserializationMustNotStart>()
        );
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join(fixture_name);
        fs::write(&path, b"null").expect("write JSON fixture");
        let required = materialization_gate_bytes(fixed, per_entry);
        let load_limits = AssetLoadLimits {
            max_entries: 1,
            max_bytes: required,
            max_depth: MAX_JOURNAL_DEPTH,
            max_members: 1,
            ..AssetLoadLimits::default()
        };

        let mut exact = AssetLoadBudget::new(load_limits).expect("exact materialization budget");
        let value = read_json_bounded_from_file::<MaterializationProbe>(
            &path,
            File::open(&path).expect("open JSON fixture"),
            limits,
            &mut exact,
        )
        .expect("exact materialization budget must permit deserialization");
        assert_eq!(value, MaterializationProbe);
        assert_eq!(exact.usage().bytes, required);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: required - 1,
            ..load_limits
        })
        .expect("one-short materialization budget");
        let error = read_json_bounded_from_file::<DeserializationMustNotStart>(
            &path,
            File::open(&path).expect("reopen JSON fixture"),
            limits,
            &mut one_short,
        )
        .expect_err("one-short materialization budget must fail");
        assert!(matches!(
            error,
            JournalError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == required - 1 && requested == required
        ));
        let root_layout =
            u64::try_from(size_of::<DeserializationMustNotStart>()).expect("sentinel root layout");
        let materialization = fixed
            .checked_add(root_layout)
            .and_then(|bytes| bytes.checked_add(per_entry))
            .expect("materialization reservation");
        assert_eq!(one_short.usage().bytes, required - materialization);
        assert_eq!(one_short.usage().entries, 1);
    }

    #[test]
    fn preparation_contract_reserves_materialization_before_deserialization() {
        assert_materialization_gate(
            "preparation.json",
            PREPARATION_JSON_LIMITS,
            PREPARATION_MATERIALIZATION_FIXED_BYTES,
            PREPARATION_MATERIALIZATION_BYTES_PER_ENTRY,
        );
    }

    #[test]
    fn manifest_contract_reserves_materialization_before_deserialization() {
        assert_materialization_gate(
            "manifest.json",
            MANIFEST_JSON_LIMITS,
            MANIFEST_MATERIALIZATION_FIXED_BYTES,
            MANIFEST_MATERIALIZATION_BYTES_PER_ENTRY,
        );
    }

    #[test]
    fn event_contract_reserves_materialization_before_deserialization() {
        assert_materialization_gate(
            "event.json",
            EVENT_JSON_LIMITS,
            EVENT_MATERIALIZATION_FIXED_BYTES,
            EVENT_MATERIALIZATION_BYTES_PER_ENTRY,
        );
    }

    fn journal_fixture(
        parent: &Path,
        existing: bool,
    ) -> (CommitReport, JournalManifest, JournalLayout) {
        let workspace = WorkspaceId::from_u128(7).expect("workspace id");
        let source = SourceId::new(workspace, SourceKind::Yaml, 1).expect("source id");
        let from = WorkspaceRevision::new(DigestV1::hash_bytes(b"from"));
        let to = WorkspaceRevision::new(DigestV1::hash_bytes(b"to"));
        let base_installation =
            WorkspaceInstallationDigest::new(DigestV1::hash_bytes(b"base installation"));
        let committed_installation =
            WorkspaceInstallationDigest::new(DigestV1::hash_bytes(b"committed installation"));
        let plan_digest = DigestV1::hash_bytes(b"plan");
        let report_artifact =
            CommitArtifactReport::new("root".to_owned(), source, DigestV1::hash_bytes(b"bytes"), 5);
        let old_digest = existing.then(|| DigestV1::hash_bytes(b"old"));
        let old_identity = existing.then(|| FileIdentity::test_identity(1, 3));
        let destination_parent_identity =
            observe_directory_identity(parent).expect("destination parent identity");
        let root_identity = destination_parent_identity.clone();
        let outputs = [JournalTransactionOutputSeed {
            ordinal: 0,
            logical_name: report_artifact.logical_name(),
            source,
            relative_target: "target",
            expected: if existing {
                JournalExpectedDestination::Existing
            } else {
                JournalExpectedDestination::Absent
            },
            expected_digest: old_digest,
            expected_identity: old_identity.as_ref(),
            destination_parent_identity: &destination_parent_identity,
            digest: report_artifact.digest(),
            bytes: report_artifact.bytes(),
        }];
        let changed_sources = [source];
        let changed_objects = [];
        let identity_remaps = [];
        let baseline = baseline(workspace, source, report_artifact.digest());
        let transaction = transaction_id_from_seed(
            &JournalTransactionSeed {
                version: JOURNAL_TRANSACTION_SEED_VERSION,
                workspace,
                base_revision: from,
                committed_revision: to,
                base_installation,
                committed_installation,
                plan_digest,
                atomicity: CommitAtomicity::PerArtifactRecoverable,
                containment_root: parent.to_str().expect("UTF-8 fixture path"),
                containment_root_identity: &root_identity,
                outputs: &outputs,
                changed_sources: &changed_sources,
                changed_objects: &changed_objects,
                identity_remaps: &identity_remaps,
                baseline: &baseline,
            },
            &mut AssetLoadBudget::default(),
        )
        .expect("transaction seed");
        let changes = ChangeSet::new(
            transaction,
            workspace,
            from,
            to,
            vec![source],
            Vec::new(),
            Vec::new(),
        )
        .expect("change set");
        let layout = JournalLayout::new(parent, transaction, root_identity);
        std::fs::create_dir_all(layout.events_directory()).expect("events directory");
        std::fs::create_dir(layout.stage_directory()).expect("stage directory");
        std::fs::create_dir(layout.backup_directory()).expect("backup directory");
        std::fs::create_dir(layout.baseline_directory()).expect("baseline directory");
        let directories =
            JournalDirectoryIdentities::observe(&layout).expect("journal directory identities");
        let report = CommitReport::new(CommitReportFields {
            transaction,
            workspace_id: workspace,
            base_revision: from,
            committed_revision: to,
            base_installation,
            committed_installation,
            plan_digest,
            atomicity: CommitAtomicity::PerArtifactRecoverable,
            artifacts: vec![report_artifact],
            changes,
            recovery: RecoveryLocator::new(
                layout.directory().to_path_buf(),
                transaction,
                layout.root_identity().clone(),
            ),
        });
        let artifact = JournalArtifact::new(
            &report.artifacts()[0],
            JournalPath::new("target").unwrap(),
            destination_parent_identity,
            JournalPath::new("stage").unwrap(),
            existing.then(|| JournalPath::new("backup").unwrap()),
            old_digest,
            old_identity,
            FileIdentity::test_identity(2, report.artifacts()[0].bytes()),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let manifest = JournalManifest::new(
            &report,
            layout.root_identity().clone(),
            directories,
            vec![artifact],
            baseline,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        install_fixture_preparation(&layout, &report, &manifest);
        (report, manifest, layout)
    }

    #[test]
    fn transaction_identity_isolated_by_literal_seed_version() {
        assert_eq!(JOURNAL_TRANSACTION_SEED_VERSION, 2);
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        let outputs = manifest
            .artifacts
            .iter()
            .enumerate()
            .map(|(ordinal, artifact)| JournalTransactionOutputSeed {
                ordinal: u32::try_from(ordinal).unwrap(),
                logical_name: artifact.logical_name(),
                source: artifact.source(),
                relative_target: artifact.target().as_str(),
                expected: if artifact.old_identity().is_some() {
                    JournalExpectedDestination::Existing
                } else {
                    JournalExpectedDestination::Absent
                },
                expected_digest: artifact.old_digest(),
                expected_identity: artifact.old_identity(),
                destination_parent_identity: artifact.destination_parent_identity(),
                digest: artifact.new_digest(),
                bytes: artifact.bytes(),
            })
            .collect::<Vec<_>>();
        let containment_root = layout.parent().to_str().unwrap();
        let transaction_for = |version| {
            transaction_id_from_seed(
                &JournalTransactionSeed {
                    version,
                    workspace: manifest.workspace_id,
                    base_revision: manifest.base_revision,
                    committed_revision: manifest.committed_revision,
                    base_installation: manifest.base_installation,
                    committed_installation: manifest.committed_installation,
                    plan_digest: manifest.plan_digest,
                    atomicity: manifest.atomicity,
                    containment_root,
                    containment_root_identity: layout.root_identity(),
                    outputs: &outputs,
                    changed_sources: manifest.result.changes.changed_sources(),
                    changed_objects: manifest.result.changes.changed_objects(),
                    identity_remaps: manifest.result.changes.identity_remaps(),
                    baseline: &manifest.baseline,
                },
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
        };

        let legacy = transaction_for(1);
        let current = transaction_for(2);
        assert_ne!(legacy, current);
        assert_eq!(current, manifest.transaction());
    }

    fn install_fixture_preparation(
        layout: &JournalLayout,
        report: &CommitReport,
        manifest: &JournalManifest,
    ) {
        let mut budget = AssetLoadBudget::default();
        let outputs = manifest
            .artifacts()
            .iter()
            .enumerate()
            .map(|(ordinal, artifact)| {
                JournalPreparationOutput::new(
                    u32::try_from(ordinal).expect("fixture ordinal"),
                    artifact.logical_name(),
                    artifact.source(),
                    artifact.target().clone(),
                    if artifact.old_digest().is_some() {
                        JournalExpectedDestination::Existing
                    } else {
                        JournalExpectedDestination::Absent
                    },
                    artifact.old_digest(),
                    artifact.old_identity().cloned(),
                    artifact.destination_parent_identity().clone(),
                    artifact.new_digest(),
                    artifact.bytes(),
                    &mut budget,
                )
                .expect("fixture preparation output")
            })
            .collect::<Vec<_>>();
        let preparation_parent = layout
            .preparation_path()
            .parent()
            .map(observe_directory_identity)
            .expect("preparation parent")
            .expect("preparation parent identity");
        let mut temporary = layout.preparation_path().to_path_buf();
        temporary.set_extension("prepare.v2.fixture.tmp");
        JournalPreparation::install(
            layout,
            report,
            &outputs,
            manifest.baseline(),
            &temporary,
            &preparation_parent,
            &mut budget,
        )
        .expect("fixture preparation");
    }

    fn baseline(
        workspace: WorkspaceId,
        source: SourceId,
        artifact_digest: DigestV1,
    ) -> JournalBaseline {
        JournalBaseline::from_sorted(
            vec![JournalBaselineSource::new(
                source,
                SourceFingerprint::new(SourceKind::Yaml, artifact_digest),
                JournalCatalogAction::Existing {
                    base_fingerprint: SourceFingerprint::new(
                        SourceKind::Yaml,
                        DigestV1::hash_bytes(b"base"),
                    ),
                },
                JournalBaselineImage::Published { artifact: 0 },
            )],
            workspace,
        )
        .expect("baseline")
    }

    fn preparation_fixture() -> (tempfile::TempDir, JournalLayout, Vec<u8>) {
        let directory = tempdir().expect("temporary directory");
        let (_report, _manifest, layout) = journal_fixture(directory.path(), true);
        let encoded = fs::read(layout.preparation_path()).expect("preparation bytes");
        (directory, layout, encoded)
    }

    fn write_preparation_value(
        layout: &JournalLayout,
        original: &[u8],
        mutate: impl FnOnce(&mut serde_json::Value),
    ) {
        let mut value: serde_json::Value =
            serde_json::from_slice(original).expect("preparation JSON");
        mutate(&mut value);
        fs::write(
            layout.preparation_path(),
            serde_json::to_vec(&value).expect("mutated preparation JSON"),
        )
        .expect("write mutated preparation");
    }

    #[test]
    fn preparation_rejects_unknown_and_duplicate_fields_without_deleting_evidence() {
        let (_directory, layout, original) = preparation_fixture();
        write_preparation_value(&layout, &original, |value| {
            value
                .as_object_mut()
                .expect("preparation object")
                .insert("unexpected".to_owned(), serde_json::Value::Bool(true));
        });
        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::Json(_))
        ));
        assert!(layout.preparation_path().is_file());

        let text = std::str::from_utf8(&original).expect("UTF-8 preparation");
        let duplicate = format!("{{\"version\":{JOURNAL_VERSION},{}", &text[1..]);
        fs::write(layout.preparation_path(), duplicate).expect("duplicate preparation field");
        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::Json(_))
        ));
        assert!(layout.preparation_path().is_file());
    }

    #[test]
    fn preparation_rejects_depth_overflow_before_schema_deserialization() {
        let (_directory, layout, _original) = preparation_fixture();
        let nesting = usize::try_from(MAX_JOURNAL_DEPTH).expect("depth") + 1;
        let encoded = format!(
            "{{\"unknown\":{}null{}}}",
            "[".repeat(nesting),
            "]".repeat(nesting)
        );
        fs::write(layout.preparation_path(), encoded).expect("deep preparation");

        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::NestingDepthExceeded { .. })
        ));
        assert!(layout.preparation_path().is_file());
    }

    #[test]
    fn preparation_rejects_oversized_input_before_reading_it() {
        let (_directory, layout, _original) = preparation_fixture();
        let path = layout.preparation_path();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open preparation for resize");
        file.set_len(MAX_MANIFEST_BYTES + 1)
            .expect("extend preparation beyond limit");
        drop(file);

        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::DocumentTooLarge { .. })
        ));
        assert_eq!(
            fs::metadata(path).expect("preparation metadata").len(),
            MAX_MANIFEST_BYTES + 1
        );
    }

    #[test]
    fn preparation_open_obeys_exact_and_one_short_budgets_without_writing() {
        let (_directory, layout, original) = preparation_fixture();
        let mut measured = AssetLoadBudget::default();
        JournalPreparation::open(&layout, &mut measured).expect("measure preparation open");
        let usage = measured.usage();
        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };

        let mut exact = AssetLoadBudget::new(exact_limits).expect("exact preparation budget");
        JournalPreparation::open(&layout, &mut exact).expect("exact preparation open");
        assert_eq!(exact.usage(), usage);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .expect("one-short preparation budget");
        assert!(matches!(
            JournalPreparation::open(&layout, &mut one_short),
            Err(JournalError::Budget(_))
        ));
        assert_eq!(
            fs::read(layout.preparation_path()).expect("retained preparation"),
            original
        );
    }

    #[test]
    fn preparation_rejects_noncanonical_ordinals_and_transaction_seed_changes() {
        let (_directory, layout, original) = preparation_fixture();
        write_preparation_value(&layout, &original, |value| {
            value["outputs"][0]["ordinal"] = serde_json::Value::from(1_u64);
        });
        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::InvalidManifest(_))
        ));

        write_preparation_value(&layout, &original, |value| {
            value["workspace_id"] =
                serde_json::to_value(WorkspaceId::from_u128(99).expect("different workspace id"))
                    .expect("workspace JSON");
        });
        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::InvalidManifest(_))
        ));

        write_preparation_value(&layout, &original, |value| {
            value["plan_digest"] =
                serde_json::to_value(DigestV1::hash_bytes(b"other plan")).expect("digest JSON");
        });
        assert!(matches!(
            JournalPreparation::open(&layout, &mut AssetLoadBudget::default()),
            Err(JournalError::TransactionMismatch { .. })
        ));
        assert!(layout.preparation_path().is_file());
    }

    #[test]
    fn relative_paths_reject_escape_and_devices() {
        for value in ["../escape", "/absolute", "C:ads", "a\\b", "CON", "a/../b"] {
            assert!(JournalPath::new(value).is_err(), "{value}");
        }
        assert_eq!(
            JournalPath::new("nested/file").unwrap().as_str(),
            "nested/file"
        );
    }

    #[test]
    fn budgeted_journal_path_join_rejects_one_short_byte_budget() {
        let directory = tempdir().expect("temporary journal root");
        let path = JournalPath::new("nested/recovery.image").expect("journal path");

        let mut measured = AssetLoadBudget::default();
        let joined = path
            .join_root_budgeted(directory.path(), "test journal path", &mut measured)
            .expect("measure joined path");
        let usage = measured.usage();
        assert_eq!(joined, directory.path().join("nested/recovery.image"));
        assert!(usage.bytes > 0);

        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        })
        .expect("exact path budget");
        path.join_root_budgeted(directory.path(), "test journal path", &mut exact)
            .expect("exact budgeted path");
        assert_eq!(exact.usage(), usage);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        })
        .expect("one-short path budget");
        assert!(matches!(
            path.join_root_budgeted(directory.path(), "test journal path", &mut one_short),
            Err(JournalError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn event_chain_detects_digest_and_link_changes() {
        let mut budget = AssetLoadBudget::default();
        let first = JournalEvent::new(0, None, JournalEventKind::Journaled, &mut budget).unwrap();
        let second = JournalEvent::new(
            1,
            Some(first.digest()),
            JournalEventKind::Published,
            &mut budget,
        )
        .unwrap();
        assert!(EventChain::from_events(vec![first.clone(), second], &mut budget).is_ok());
        let broken = JournalEvent::new(1, None, JournalEventKind::Published, &mut budget).unwrap();
        assert!(matches!(
            EventChain::from_events(vec![first, broken], &mut budget),
            Err(JournalError::PreviousDigestMismatch { .. })
        ));
    }

    #[test]
    fn version_three_marker_remains_read_compatible() {
        const LEGACY_MARKER_JSON: &[u8] = br#"{"version":3,"sequence":0,"previous":null,"kind":{"type":"marker","data":{"name":"legacy-diagnostic"}},"digest":"blake3-v1:f9442532a217d3bc5abeba6b5ab8fd531e2b64d619e206fafbbc0a7062c66879"}"#;
        const LEGACY_MARKER_FILENAME: &str = "00000000000000000000-f9442532a217d3bc5abeba6b5ab8fd531e2b64d619e206fafbbc0a7062c66879.json";

        let decoded: JournalEvent =
            serde_json::from_slice(LEGACY_MARKER_JSON).expect("decode legacy marker");
        decoded
            .validate(&mut AssetLoadBudget::default())
            .expect("validate legacy marker");
        assert_eq!(
            decoded.kind(),
            &JournalEventKind::Marker {
                name: "legacy-diagnostic".to_owned(),
            }
        );
        assert_eq!(
            budgeted_event_filename(&decoded, &mut AssetLoadBudget::default())
                .expect("legacy marker filename"),
            LEGACY_MARKER_FILENAME
        );
        assert_eq!(
            serde_json::to_vec(&decoded).expect("re-encode legacy marker"),
            LEGACY_MARKER_JSON
        );
    }

    fn assert_legacy_event_chain(records: &[(&[u8], &str)]) {
        let mut events = Vec::with_capacity(records.len());
        for (json, filename) in records {
            let event: JournalEvent = serde_json::from_slice(json).expect("decode legacy event");
            event
                .validate(&mut AssetLoadBudget::default())
                .expect("validate legacy event");
            assert_eq!(
                budgeted_event_filename(&event, &mut AssetLoadBudget::default())
                    .expect("legacy event filename"),
                *filename
            );
            assert_eq!(
                serde_json::to_vec(&event).expect("re-encode legacy event"),
                *json
            );
            events.push(event);
        }
        EventChain::from_events(events, &mut AssetLoadBudget::default())
            .expect("legacy event digest chain");
    }

    #[test]
    fn version_three_action_chains_remain_read_compatible() {
        // Captured from the pre-protocol v3 encoder at d6bef0b. Do not regenerate
        // these bytes from the current encoder: they are a compatibility fixture.
        const FORWARD: &[(&[u8], &str)] = &[
            (br#"{"version":3,"sequence":0,"previous":null,"kind":{"type":"staging_verified"},"digest":"blake3-v1:c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177"}"#, "00000000000000000000-c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177.json"),
            (br#"{"version":3,"sequence":1,"previous":"blake3-v1:c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177","kind":{"type":"journaled"},"digest":"blake3-v1:79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b"}"#, "00000000000000000001-79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b.json"),
            (br#"{"version":3,"sequence":2,"previous":"blake3-v1:79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b","kind":{"type":"recovery_decision","data":{"direction":"forward"}},"digest":"blake3-v1:5720436d11216f7422b1ec717323a4ebd759ec653d74908bd8dc67548bc79894"}"#, "00000000000000000002-5720436d11216f7422b1ec717323a4ebd759ec653d74908bd8dc67548bc79894.json"),
            (br#"{"version":3,"sequence":3,"previous":"blake3-v1:5720436d11216f7422b1ec717323a4ebd759ec653d74908bd8dc67548bc79894","kind":{"type":"backup_intent","data":{"artifact":"existing.asset"}},"digest":"blake3-v1:ef90df5d38fc580e0db06c716153337793942e86a41e92d27d1c92957e870a96"}"#, "00000000000000000003-ef90df5d38fc580e0db06c716153337793942e86a41e92d27d1c92957e870a96.json"),
            (br#"{"version":3,"sequence":4,"previous":"blake3-v1:ef90df5d38fc580e0db06c716153337793942e86a41e92d27d1c92957e870a96","kind":{"type":"backup_captured","data":{"artifact":"existing.asset"}},"digest":"blake3-v1:fbcc39a06b0d2dc4adde1137dec624d47f6ffa7f2996a4652d4806b7883dfac0"}"#, "00000000000000000004-fbcc39a06b0d2dc4adde1137dec624d47f6ffa7f2996a4652d4806b7883dfac0.json"),
            (br#"{"version":3,"sequence":5,"previous":"blake3-v1:fbcc39a06b0d2dc4adde1137dec624d47f6ffa7f2996a4652d4806b7883dfac0","kind":{"type":"promotion_intent","data":{"artifact":"existing.asset"}},"digest":"blake3-v1:e43c7bfb34447cfb585747b8fd1fb630907bdab666d9ed085d954b095c6ecfd0"}"#, "00000000000000000005-e43c7bfb34447cfb585747b8fd1fb630907bdab666d9ed085d954b095c6ecfd0.json"),
            (br#"{"version":3,"sequence":6,"previous":"blake3-v1:e43c7bfb34447cfb585747b8fd1fb630907bdab666d9ed085d954b095c6ecfd0","kind":{"type":"promoted","data":{"artifact":"existing.asset"}},"digest":"blake3-v1:5229fde61dfd9802c1c74af0d957f7fbf0fe2fca3133d9b747037cc6a4ccbb86"}"#, "00000000000000000006-5229fde61dfd9802c1c74af0d957f7fbf0fe2fca3133d9b747037cc6a4ccbb86.json"),
            (br#"{"version":3,"sequence":7,"previous":"blake3-v1:5229fde61dfd9802c1c74af0d957f7fbf0fe2fca3133d9b747037cc6a4ccbb86","kind":{"type":"promotion_intent","data":{"artifact":"absent.asset"}},"digest":"blake3-v1:4bd917884e609b4f5fc71e2f2fa02bde6b88b3f71cf454aa374b6e7f8df80514"}"#, "00000000000000000007-4bd917884e609b4f5fc71e2f2fa02bde6b88b3f71cf454aa374b6e7f8df80514.json"),
            (br#"{"version":3,"sequence":8,"previous":"blake3-v1:4bd917884e609b4f5fc71e2f2fa02bde6b88b3f71cf454aa374b6e7f8df80514","kind":{"type":"promoted","data":{"artifact":"absent.asset"}},"digest":"blake3-v1:89f5248047be01e2d14214cc0f1652c15f63e92374afe78d546476de5f57aa4e"}"#, "00000000000000000008-89f5248047be01e2d14214cc0f1652c15f63e92374afe78d546476de5f57aa4e.json"),
            (br#"{"version":3,"sequence":9,"previous":"blake3-v1:89f5248047be01e2d14214cc0f1652c15f63e92374afe78d546476de5f57aa4e","kind":{"type":"published"},"digest":"blake3-v1:c563fb648d3f7f96cc30a72b292d0229da0fa47456961e004588b396c2505cf2"}"#, "00000000000000000009-c563fb648d3f7f96cc30a72b292d0229da0fa47456961e004588b396c2505cf2.json"),
            (br#"{"version":3,"sequence":10,"previous":"blake3-v1:c563fb648d3f7f96cc30a72b292d0229da0fa47456961e004588b396c2505cf2","kind":{"type":"baseline_installed"},"digest":"blake3-v1:9f592f63a9f5f6789f344832609c753ce5143c71b4631e8c43e7397928c43f3b"}"#, "00000000000000000010-9f592f63a9f5f6789f344832609c753ce5143c71b4631e8c43e7397928c43f3b.json"),
            (br#"{"version":3,"sequence":11,"previous":"blake3-v1:9f592f63a9f5f6789f344832609c753ce5143c71b4631e8c43e7397928c43f3b","kind":{"type":"finalized"},"digest":"blake3-v1:5d57cfe3f184d8e5edb1aee30e7455cfa6a467f6a0aa0ed6507c0660f0d7b3c6"}"#, "00000000000000000011-5d57cfe3f184d8e5edb1aee30e7455cfa6a467f6a0aa0ed6507c0660f0d7b3c6.json"),
        ];
        const ROLLBACK: &[(&[u8], &str)] = &[
            (br#"{"version":3,"sequence":0,"previous":null,"kind":{"type":"staging_verified"},"digest":"blake3-v1:c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177"}"#, "00000000000000000000-c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177.json"),
            (br#"{"version":3,"sequence":1,"previous":"blake3-v1:c7a8dc3ee1d08bc8ec526cb35e2ba1f55233afad5e600b4f0dd47dcbc4135177","kind":{"type":"journaled"},"digest":"blake3-v1:79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b"}"#, "00000000000000000001-79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b.json"),
            (br#"{"version":3,"sequence":2,"previous":"blake3-v1:79e8a849efeb26849075b7fd1867a0a99e8f5ab1dc5ce45709bad5d803de9d7b","kind":{"type":"recovery_decision","data":{"direction":"rollback"}},"digest":"blake3-v1:b2cb8c45a068bd2cc7419b35b59e341f59f1db864ee134e822da1187949fcb1f"}"#, "00000000000000000002-b2cb8c45a068bd2cc7419b35b59e341f59f1db864ee134e822da1187949fcb1f.json"),
            (br#"{"version":3,"sequence":3,"previous":"blake3-v1:b2cb8c45a068bd2cc7419b35b59e341f59f1db864ee134e822da1187949fcb1f","kind":{"type":"abandoned"},"digest":"blake3-v1:da8c96c24e6fa52c1c4851ac35db57f2dbd1a83aaabbb6c736cac281dda7682e"}"#, "00000000000000000003-da8c96c24e6fa52c1c4851ac35db57f2dbd1a83aaabbb6c736cac281dda7682e.json"),
            (br#"{"version":3,"sequence":4,"previous":"blake3-v1:da8c96c24e6fa52c1c4851ac35db57f2dbd1a83aaabbb6c736cac281dda7682e","kind":{"type":"finalized"},"digest":"blake3-v1:9458ef26ff4b7b448778bbcfc250fa83e1b54c8d7ce91452246dadcf38913402"}"#, "00000000000000000004-9458ef26ff4b7b448778bbcfc250fa83e1b54c8d7ce91452246dadcf38913402.json"),
        ];

        assert_legacy_event_chain(FORWARD);
        assert_legacy_event_chain(ROLLBACK);
    }

    #[test]
    fn manifest_event_capacity_accepts_exact_limit_and_rejects_overflow() {
        let existing = (MAX_EVENT_COUNT - TRANSACTION_EVENT_RESERVE - ABSENT_TARGET_EVENT_COUNT)
            / EXISTING_TARGET_EVENT_COUNT;
        assert_eq!(
            existing * EXISTING_TARGET_EVENT_COUNT
                + ABSENT_TARGET_EVENT_COUNT
                + TRANSACTION_EVENT_RESERVE,
            MAX_EVENT_COUNT
        );
        validate_event_capacity(existing, 1).expect("exact event capacity");
        assert!(matches!(
            validate_event_capacity(existing, 2),
            Err(JournalError::InvalidManifest(_))
        ));
        assert!(matches!(
            validate_event_capacity(usize::MAX, usize::MAX),
            Err(JournalError::InvalidManifest(_))
        ));
    }

    #[test]
    fn ordinal_journal_path_matcher_requires_the_canonical_fixed_width_name() {
        let canonical = JournalPath::new("stage/00000042.stage").expect("canonical path");
        assert!(matches_ordinal_journal_path(
            &canonical, "stage/", 42, ".stage"
        ));
        assert!(!matches_ordinal_journal_path(
            &canonical, "stage/", 41, ".stage"
        ));

        let short = JournalPath::new("stage/42.stage").expect("short path");
        let non_numeric = JournalPath::new("stage/abcdefgh.stage").expect("non-numeric path");
        assert!(!matches_ordinal_journal_path(
            &short, "stage/", 42, ".stage"
        ));
        assert!(!matches_ordinal_journal_path(
            &non_numeric,
            "stage/",
            42,
            ".stage"
        ));
    }

    #[test]
    fn file_identity_wire_is_fixed_width_and_round_trips() {
        let short = FileIdentity::test_identity(1, 1);
        let long = FileIdentity::test_identity(u64::MAX, u64::MAX - 1);
        let short_wire = serde_json::to_vec(&short).expect("short identity wire");
        let long_wire = serde_json::to_vec(&long).expect("long identity wire");

        assert_eq!(short_wire.len(), long_wire.len());
        assert_eq!(
            serde_json::from_slice::<FileIdentity>(&short_wire).expect("short identity round trip"),
            short
        );
        assert_eq!(
            serde_json::from_slice::<FileIdentity>(&long_wire).expect("long identity round trip"),
            long
        );
    }

    #[test]
    fn identity_bound_json_read_rejects_replaced_events_parent_before_budget_use() {
        let directory = tempdir().expect("temporary directory");
        let events = directory.path().join("events");
        fs::create_dir(&events).expect("events directory");
        let expected_parent = observe_directory_identity(&events).expect("events identity");
        let event = events.join("event.json");
        fs::write(&event, b"null").expect("event fixture");

        let displaced = directory.path().join("events-displaced");
        fs::rename(&events, &displaced).expect("displace events directory");
        fs::create_dir(&events).expect("replacement events directory");
        fs::write(&event, b"null").expect("replacement event fixture");

        let mut budget = AssetLoadBudget::default();
        let before = budget.usage();
        let error = read_json_bounded_in_parent::<serde_json::Value>(
            &event,
            &expected_parent,
            EVENT_JSON_LIMITS,
            &mut budget,
        )
        .expect_err("replacement event parent must be rejected");

        assert!(matches!(error, JournalError::Io(_)));
        assert_eq!(budget.usage(), before);
    }

    #[test]
    fn journal_round_trips_manifest_and_events() {
        let directory = tempdir().unwrap();
        let (report, manifest, layout) = journal_fixture(directory.path(), true);
        let transaction = report.transaction();
        let mut journal =
            Journal::create(layout.clone(), manifest, &mut AssetLoadBudget::default()).unwrap();
        journal
            .append(JournalEventKind::Journaled, &mut AssetLoadBudget::default())
            .unwrap();
        journal
            .append(JournalEventKind::Published, &mut AssetLoadBudget::default())
            .unwrap();
        drop(journal);

        let reopened = Journal::open(layout, &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(reopened.events().len(), 2);
        assert_eq!(reopened.manifest().transaction(), transaction);
        assert_eq!(
            reopened
                .manifest()
                .report(
                    directory.path(),
                    reopened.layout().root_identity(),
                    &mut AssetLoadBudget::default(),
                )
                .unwrap()
                .transaction(),
            transaction
        );
    }

    #[test]
    fn journal_open_preserves_interrupted_write_and_uses_a_fresh_attempt() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        let mut journal =
            Journal::create(layout.clone(), manifest, &mut AssetLoadBudget::default()).unwrap();
        journal
            .append(JournalEventKind::Journaled, &mut AssetLoadBudget::default())
            .unwrap();
        let interrupted = JournalEvent::new(
            1,
            Some(journal.events()[0].digest()),
            JournalEventKind::Published,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let temporary = layout
            .events_directory()
            .join(event_temporary_filename(&interrupted, 0));
        fs::write(&temporary, serde_json::to_vec(&interrupted).unwrap()).unwrap();
        drop(journal);

        let mut reopened = Journal::open(layout, &mut AssetLoadBudget::default()).unwrap();
        reopened
            .append(JournalEventKind::Published, &mut AssetLoadBudget::default())
            .unwrap();
        assert!(temporary.exists());
        assert_eq!(reopened.events().len(), 2);
    }

    #[test]
    fn journal_open_accepts_multiple_canonical_attempts_and_rejects_noncanonical_names() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        let journal =
            Journal::create(layout.clone(), manifest, &mut AssetLoadBudget::default()).unwrap();
        let next = JournalEvent::new(
            0,
            None,
            JournalEventKind::Journaled,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let temporary = layout
            .events_directory()
            .join(event_temporary_filename(&next, 0));
        fs::write(&temporary, b"partial").unwrap();
        let second = layout.events_directory().join(format!(
            ".{:020}-{}.json.attempt-00000001.tmp",
            0,
            "0".repeat(DigestV1::BYTE_LEN * 2)
        ));
        fs::write(&second, b"partial").unwrap();
        drop(journal);

        let reopened = Journal::open(layout.clone(), &mut AssetLoadBudget::default()).unwrap();
        assert!(reopened.events().is_empty());
        fs::remove_file(second).unwrap();
        let noncanonical = layout.events_directory().join(format!(
            ".0-{}.json.attempt-00000002.tmp",
            "0".repeat(DigestV1::BYTE_LEN * 2)
        ));
        fs::write(&noncanonical, b"partial").unwrap();
        assert!(matches!(
            Journal::open(layout, &mut AssetLoadBudget::default()),
            Err(JournalError::InvalidEvent(_))
        ));
    }

    #[test]
    fn journal_open_obeys_exact_and_one_short_byte_budgets_without_writing() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        let mut journal =
            Journal::create(layout.clone(), manifest, &mut AssetLoadBudget::default()).unwrap();
        journal
            .append(JournalEventKind::Journaled, &mut AssetLoadBudget::default())
            .unwrap();
        journal
            .append(JournalEventKind::Published, &mut AssetLoadBudget::default())
            .unwrap();
        drop(journal);

        let mut measured = AssetLoadBudget::default();
        Journal::open(layout.clone(), &mut measured).unwrap();
        let usage = measured.usage();
        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };
        let event_count = fs::read_dir(layout.events_directory()).unwrap().count();

        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        Journal::open(layout.clone(), &mut exact).unwrap();
        assert_eq!(exact.usage(), usage);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .unwrap();
        assert!(matches!(
            Journal::open(layout.clone(), &mut one_short),
            Err(JournalError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(
            fs::read_dir(layout.events_directory()).unwrap().count(),
            event_count
        );
    }

    #[test]
    fn journal_rejects_tampered_event() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), false);
        let mut journal =
            Journal::create(layout.clone(), manifest, &mut AssetLoadBudget::default()).unwrap();
        journal
            .append(JournalEventKind::Journaled, &mut AssetLoadBudget::default())
            .unwrap();
        let event_path = fs::read_dir(layout.events_directory())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(
            event_path,
            br#"{"version":1,"sequence":0,"previous":null,"kind":{"type":"published"},"digest":"blake3-v1:0000000000000000000000000000000000000000000000000000000000000000"}"#,
        )
        .unwrap();
        assert!(matches!(
            Journal::open(layout, &mut AssetLoadBudget::default()),
            Err(JournalError::DigestMismatch { .. } | JournalError::InvalidEvent(_))
        ));
    }

    #[test]
    fn journal_rejects_a_self_consistent_manifest_with_a_stale_transaction_identity() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        let original_transaction = manifest.transaction();
        let original_target = manifest.artifacts[0].target.clone();
        let mut tampered = manifest;
        tampered.artifacts[0].target = JournalPath::new("other-target").unwrap();
        tampered.validate().unwrap();

        let error = Journal::create(layout, tampered, &mut AssetLoadBudget::default())
            .expect_err("stale transaction identity");
        assert!(!error.manifest_installed());
        assert!(matches!(
            error.journal_error(),
            JournalError::TransactionMismatch { expected, actual }
                if *expected == original_transaction && expected != actual
        ));
        assert_eq!(original_target.as_str(), "target");
    }

    #[test]
    fn transaction_identity_binds_the_recovery_baseline() {
        let directory = tempdir().unwrap();
        let (_report, mut manifest, layout) = journal_fixture(directory.path(), true);
        let original_transaction = manifest.transaction();
        manifest.baseline.sources[0].catalog = JournalCatalogAction::Existing {
            base_fingerprint: SourceFingerprint::new(
                SourceKind::Yaml,
                DigestV1::hash_bytes(b"tampered base"),
            ),
        };
        manifest.validate().unwrap();

        let error = Journal::create(layout, manifest, &mut AssetLoadBudget::default())
            .expect_err("baseline must be bound to the transaction");

        assert!(!error.manifest_installed());
        assert!(matches!(
            error.journal_error(),
            JournalError::TransactionMismatch { expected, actual }
                if *expected == original_transaction && expected != actual
        ));
    }

    #[test]
    fn transaction_identity_binds_the_canonical_change_set() {
        let directory = tempdir().unwrap();
        let (_report, mut manifest, layout) = journal_fixture(directory.path(), true);
        let original_transaction = manifest.transaction();
        let original_source = manifest.artifacts[0].source();
        let injected_source = SourceId::new(manifest.workspace_id, SourceKind::Yaml, 99).unwrap();
        manifest.result.changes = ChangeSet::new(
            original_transaction,
            manifest.workspace_id,
            manifest.base_revision,
            manifest.committed_revision,
            vec![original_source, injected_source],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        manifest.validate().unwrap();

        let error = Journal::create(layout, manifest, &mut AssetLoadBudget::default())
            .expect_err("change set must be bound to the transaction");

        assert!(!error.manifest_installed());
        assert!(matches!(
            error.journal_error(),
            JournalError::TransactionMismatch { expected, actual }
                if *expected == original_transaction && expected != actual
        ));
    }

    #[test]
    fn transaction_identity_binds_the_committed_revision() {
        let directory = tempdir().unwrap();
        let (_report, mut manifest, layout) = journal_fixture(directory.path(), true);
        let original_transaction = manifest.transaction();
        let changed_revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"tampered revision"));
        let changed_sources = manifest.result.changes.changed_sources().to_vec();
        manifest.committed_revision = changed_revision;
        manifest.result.committed_revision = changed_revision;
        manifest.result.changes = ChangeSet::new(
            original_transaction,
            manifest.workspace_id,
            manifest.base_revision,
            changed_revision,
            changed_sources,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        manifest.validate().unwrap();

        let error = Journal::create(layout, manifest, &mut AssetLoadBudget::default())
            .expect_err("committed revision must be bound to the transaction");

        assert!(!error.manifest_installed());
        assert!(matches!(
            error.journal_error(),
            JournalError::TransactionMismatch { expected, actual }
                if *expected == original_transaction && expected != actual
        ));
    }

    #[test]
    fn existing_canonical_manifest_is_classified_as_published_evidence() {
        let directory = tempdir().unwrap();
        let (_report, manifest, layout) = journal_fixture(directory.path(), true);
        Journal::create(
            layout.clone(),
            manifest.clone(),
            &mut AssetLoadBudget::default(),
        )
        .expect("initial journal");

        let error = Journal::create(layout, manifest, &mut AssetLoadBudget::default())
            .expect_err("manifest must be no-replace");

        assert!(error.manifest_installed());
        assert!(matches!(
            error.journal_error(),
            JournalError::Io(source) if source.kind() == io::ErrorKind::AlreadyExists
        ));
    }
}
