use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
    SearchGenerationId, SearchGenerationManifestV1,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, ContractJsonLimits, ContractJsonResourceModel,
    DigestBuildError, DigestV1, DigestV1Builder, WorkspaceId, WorkspaceRevision,
    read_contract_json,
};
use unity_asset_search_local::{PrivateIndexRootV1, PrivateRootsError};

use crate::anchored_fs::{
    AnchoredFsError as SecureReadError, EntryKindHint, OpenPolicy,
    ReadDirectory as SecureReadDirectory, RegularFile as SecureRegularFile,
    StableDirectoryIdentity,
};

const GENERATIONS_DIRECTORY: &str = "generations";
const STAGING_DIRECTORY: &str = ".staging";
const ACTIVATIONS_DIRECTORY: &str = "activations";
const SEARCH_ARTIFACT_DIRECTORY: &str = "search";
const REFERENCE_ARTIFACT_DIRECTORY: &str = "references";
const SOURCE_STATE_ARTIFACT_DIRECTORY: &str = "state";
const SOURCE_STATE_FILE: &str = "source-state-v3.json";
const MANIFEST_FILE: &str = "manifest.json";
const LEGACY_ACTIVATION_CONTRACT_VERSION: u16 = 1;
const REVISIONED_ACTIVATION_CONTRACT_VERSION: u16 = 2;
const GENERATION_HEAD_CONTRACT_VERSION: u16 = 3;
const ACTIVATION_FILE_DIGITS: usize = 20;
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES_U64: u64 = 8 * 1024 * 1024;
const MAX_ACTIVATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACTIVATION_BYTES_U64: u64 = 4 * 1024 * 1024;
const MAX_ACTIVATION_CANDIDATES: usize = 65_536;
const ACTIVATION_CANDIDATE_GROWTH: usize = 256;
const CONTRACT_JSON_PARSER_WORK_MULTIPLIER: u64 = 6;
const CONTRACT_JSON_PARSER_FIXED_WORK_BYTES: u64 = 4 * 1024;
// A manifest retains at most 4,096 transaction digests. The remaining 128 values cover every
// scalar and nested evidence object in the fixed v1 envelope with room for contract evolution.
const MAX_MANIFEST_JSON_VALUES: u64 = 4_096 + 128;
const MAX_ACTIVATION_JSON_VALUES: u64 = 4_096 * 8 + 64;
// Activation owns the bounded transaction receipt window. The fixed reserve covers the envelope;
// 512 bytes per observed value covers receipt digests, identifiers, and Serde temporaries.
const ACTIVATION_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    64 * 1024,
    512,
);
const ACTIVATION_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.activation",
    MAX_ACTIVATION_BYTES,
    4,
    MAX_ACTIVATION_JSON_VALUES,
    MAX_ACTIVATION_JSON_VALUES,
    ACTIVATION_JSON_RESOURCES,
);
const MANIFEST_JSON_RESOURCES: ContractJsonResourceModel = ContractJsonResourceModel::new(
    CONTRACT_JSON_PARSER_WORK_MULTIPLIER,
    CONTRACT_JSON_PARSER_FIXED_WORK_BYTES,
    // The fixed reserve covers the manifest envelope; 512 bytes per value is deliberately above
    // a retained 32-byte transaction digest and all nested scalar evidence representations.
    64 * 1024,
    512,
);
const MANIFEST_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.generation_manifest",
    MAX_MANIFEST_BYTES,
    8,
    MAX_MANIFEST_JSON_VALUES,
    MAX_MANIFEST_JSON_VALUES,
    MANIFEST_JSON_RESOURCES,
);
const MAX_ARTIFACT_RELATIVE_PATH_BYTES: usize = 64 * 1024;
const ARTIFACT_TREE_DOMAIN: &[u8] = b"unity-asset:search-generation:artifact-tree:v1\0";
const MAX_PERSISTED_ARTIFACT_TREE_FILES: u64 = 1_000_000;
const MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES: u64 = 1_000_000;
const MAX_PERSISTED_ARTIFACT_TREE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

mod source_state;

use source_state::{
    ByteCounter, SizeLimitedWriter, scan_json_structure, source_state_entry_count,
    source_state_owned_allocation_bound, validate_source_state_manifest,
};
pub(crate) use source_state::{
    SourceScanHint, SourceStateError, SourceStateLimits, SourceStateSnapshot,
    TransactionReceiptMembership, TransactionReceiptWindow,
};

const WRITER_LEASE_FILE: &str = ".writer.lock";
const QUARANTINE_DIRECTORY_PREFIX: &str = "quarantine-";
const OBSOLETE_ACTIVATION_DIRECTORY_PREFIX: &str = "obsolete-activations-";
const OBSOLETE_GENERATION_DIRECTORY_PREFIX: &str = "generation-v1-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationStoreOptions {
    pub retain_previous_generations: usize,
}

impl Default for GenerationStoreOptions {
    fn default() -> Self {
        Self {
            retain_previous_generations: 2,
        }
    }
}

#[derive(Debug, Default)]
struct LiveBuildClaim {
    held: AtomicBool,
}

impl LiveBuildClaim {
    fn acquire(self: &Arc<Self>) -> Result<LiveBuildToken, GenerationStoreError> {
        self.held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| GenerationStoreError::BuildAlreadyActive)?;
        Ok(LiveBuildToken {
            claim: Arc::clone(self),
            held: true,
        })
    }

    fn is_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct LiveBuildToken {
    claim: Arc<LiveBuildClaim>,
    held: bool,
}

impl LiveBuildToken {
    fn belongs_to(&self, claim: &Arc<LiveBuildClaim>) -> bool {
        Arc::ptr_eq(&self.claim, claim)
    }

    fn release(&mut self) {
        if self.held {
            self.claim.held.store(false, Ordering::Release);
            self.held = false;
        }
    }
}

impl Drop for LiveBuildToken {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationBuildState {
    Armed,
    Completed,
    Relinquished,
}

/// A store-owned staging directory. All writable paths are derived from its ordinal.
#[derive(Debug)]
pub(crate) struct GenerationBuild {
    store_root: PathBuf,
    ordinal: u64,
    directory: PathBuf,
    lease: Arc<WriterLease>,
    claim: LiveBuildToken,
    state: GenerationBuildState,
}

impl GenerationBuild {
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[cfg(test)]
    #[must_use]
    pub fn search_directory(&self) -> PathBuf {
        self.directory.join(SEARCH_ARTIFACT_DIRECTORY)
    }

    #[cfg(test)]
    #[must_use]
    pub fn reference_directory(&self) -> PathBuf {
        self.directory.join(REFERENCE_ARTIFACT_DIRECTORY)
    }

    #[must_use]
    pub fn source_state_directory(&self) -> PathBuf {
        self.directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY)
    }

    pub(crate) fn abort_with_budget(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        if self.state != GenerationBuildState::Armed {
            return Ok(());
        }
        let result = self.cleanup_directory_with_budget(budget);
        self.state = GenerationBuildState::Relinquished;
        self.claim.release();
        result
    }

    pub(crate) fn write_source_state(
        &self,
        snapshot: &SourceStateSnapshot,
    ) -> Result<(), GenerationStoreError> {
        let limits = SourceStateLimits::default();
        snapshot
            .validate_limits(limits)
            .map_err(|source| invalid_source_state(&self.directory, source))?;
        let directory = self.source_state_directory();
        ensure_existing_directory_no_follow(&directory)?;
        let path = directory.join(SOURCE_STATE_FILE);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                GenerationStoreError::io("create source state", path.clone(), source)
            })?;
        let mut writer = SizeLimitedWriter::new(BufWriter::new(file), limits.max_encoded_bytes);
        let encoded = serde_json::to_writer(&mut writer, snapshot);
        if let Some(actual) = writer.rejected_bytes() {
            return Err(invalid_source_state(
                &self.directory,
                SourceStateError::EncodedTooLarge {
                    actual,
                    maximum: limits.max_encoded_bytes,
                },
            ));
        }
        encoded.map_err(|source| {
            invalid_source_state(&self.directory, SourceStateError::Json(source))
        })?;
        writer.flush().map_err(|source| {
            GenerationStoreError::io("flush source state", path.clone(), source)
        })?;
        writer
            .inner()
            .get_ref()
            .sync_all()
            .map_err(|source| GenerationStoreError::io("sync source state", path, source))
    }

    fn cleanup_directory_with_budget(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        if path_exists_no_follow(&self.directory)? {
            remove_tree_no_follow(&self.directory, budget)?;
            sync_directory(
                self.directory
                    .parent()
                    .ok_or(GenerationStoreError::ForeignBuild)?,
            )?;
        }
        Ok(())
    }

    fn mark_completed(&mut self) {
        debug_assert_eq!(self.state, GenerationBuildState::Armed);
        self.state = GenerationBuildState::Completed;
        self.claim.release();
    }
}

impl Drop for GenerationBuild {
    fn drop(&mut self) {
        if self.state == GenerationBuildState::Armed {
            let _ = self.cleanup_directory_with_budget(&mut AssetLoadBudget::default());
            self.state = GenerationBuildState::Relinquished;
        }
        self.claim.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivationStagingState {
    Armed,
    Committed,
    Relinquished,
}

/// Owns one activation staging file from creation through commit or explicit cleanup.
#[derive(Debug)]
struct ActivationStagingFile {
    path: PathBuf,
    parent: PathBuf,
    state: ActivationStagingState,
}

impl ActivationStagingFile {
    fn create(path: PathBuf, bytes: &[u8]) -> Result<Self, GenerationStoreError> {
        let parent = path
            .parent()
            .ok_or(GenerationStoreError::ForeignBuild)?
            .to_path_buf();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| {
                GenerationStoreError::io("create activation staging file", path.clone(), source)
            })?;
        let mut staging = Self {
            path,
            parent,
            state: ActivationStagingState::Armed,
        };
        let write_result = file
            .write_all(bytes)
            .map_err(|source| {
                GenerationStoreError::io(
                    "write activation staging file",
                    staging.path.clone(),
                    source,
                )
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    GenerationStoreError::io(
                        "sync activation staging file",
                        staging.path.clone(),
                        source,
                    )
                })
            });
        drop(file);
        match write_result {
            Ok(()) => Ok(staging),
            Err(primary) => Err(staging.precommit_failure(primary)),
        }
    }

    fn publish(
        &mut self,
        final_path: &Path,
        failpoint: Option<GenerationFailpoint>,
    ) -> Result<(), GenerationStoreError> {
        debug_assert_eq!(self.state, ActivationStagingState::Armed);
        let publish_result = inject_failure(failpoint, GenerationFailpoint::ActivationPreCommit)
            .and_then(|()| {
                fs::hard_link(&self.path, final_path).map_err(|source| {
                    GenerationStoreError::io(
                        "activate generation",
                        final_path.to_path_buf(),
                        source,
                    )
                })
            });
        match publish_result {
            Ok(()) => {
                self.state = ActivationStagingState::Committed;
                Ok(())
            }
            Err(primary) => Err(self.precommit_failure(primary)),
        }
    }

    fn cleanup_after_commit(&mut self) -> Result<(), GenerationStoreError> {
        debug_assert_eq!(self.state, ActivationStagingState::Committed);
        self.cleanup_and_relinquish("remove committed activation staging file")
    }

    fn precommit_failure(&mut self, primary: GenerationStoreError) -> GenerationStoreError {
        match self.cleanup_and_relinquish("remove uncommitted activation staging file") {
            Ok(()) => primary,
            Err(cleanup) => GenerationStoreError::ActivationPreCommitCleanupFailed {
                primary: Box::new(primary),
                cleanup: Box::new(cleanup),
            },
        }
    }

    fn cleanup_and_relinquish(
        &mut self,
        operation: &'static str,
    ) -> Result<(), GenerationStoreError> {
        if self.state == ActivationStagingState::Relinquished {
            return Ok(());
        }
        let result = fs::remove_file(&self.path)
            .map_err(|source| GenerationStoreError::io(operation, self.path.clone(), source))
            .and_then(|()| sync_directory(&self.parent));
        self.state = ActivationStagingState::Relinquished;
        result
    }
}

impl Drop for ActivationStagingFile {
    fn drop(&mut self) {
        if self.state != ActivationStagingState::Relinquished {
            let _ = self.cleanup_and_relinquish("remove dropped activation staging file");
        }
    }
}

/// Immutable view of one activated generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationActivationEvidence {
    parent_generation: Option<SearchGenerationId>,
    transaction_receipts: TransactionReceiptWindow,
}

impl GenerationActivationEvidence {
    #[must_use]
    pub(crate) const fn new(
        parent_generation: Option<SearchGenerationId>,
        transaction_receipts: TransactionReceiptWindow,
    ) -> Self {
        Self {
            parent_generation,
            transaction_receipts,
        }
    }

    #[must_use]
    pub(crate) const fn parent_generation(&self) -> Option<SearchGenerationId> {
        self.parent_generation
    }

    #[must_use]
    pub(crate) const fn transaction_receipts(&self) -> &TransactionReceiptWindow {
        &self.transaction_receipts
    }
}

/// Immutable view of one activated generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationSnapshot {
    activation_ordinal: u64,
    generation: SearchGenerationId,
    manifest_digest: DigestV1,
    manifest: SearchGenerationManifestV1,
    desired_revision: WorkspaceRevision,
    activation: GenerationActivationEvidence,
    directory: PathBuf,
}

impl GenerationSnapshot {
    #[must_use]
    pub const fn activation_ordinal(&self) -> u64 {
        self.activation_ordinal
    }

    #[must_use]
    pub const fn generation(&self) -> SearchGenerationId {
        self.generation
    }

    #[must_use]
    pub const fn desired_revision(&self) -> WorkspaceRevision {
        self.desired_revision
    }

    #[must_use]
    pub const fn manifest(&self) -> &SearchGenerationManifestV1 {
        &self.manifest
    }

    #[must_use]
    pub(crate) const fn parent_generation(&self) -> Option<SearchGenerationId> {
        self.activation.parent_generation()
    }

    #[must_use]
    pub(crate) const fn transaction_receipts(&self) -> &TransactionReceiptWindow {
        self.activation.transaction_receipts()
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[cfg(test)]
    #[must_use]
    pub fn search_directory(&self) -> PathBuf {
        self.directory.join(SEARCH_ARTIFACT_DIRECTORY)
    }

    #[must_use]
    pub fn source_state_directory(&self) -> PathBuf {
        self.directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY)
    }

    pub(crate) fn load_source_state(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<SourceStateSnapshot, GenerationStoreError> {
        self.load_source_state_with_limits(budget, SourceStateLimits::default())
    }

    fn load_source_state_with_limits(
        &self,
        budget: &mut AssetLoadBudget,
        limits: SourceStateLimits,
    ) -> Result<SourceStateSnapshot, GenerationStoreError> {
        let directory = self.source_state_directory();
        let generation = SecureReadDirectory::open(&self.directory, OpenPolicy::PersistedState)
            .map_err(|source| {
                persisted_read_error(
                    "open generation for source-state load",
                    self.directory.clone(),
                    source,
                )
            })?;
        let generation_identity = generation.stable_identity().map_err(|source| {
            persisted_read_error(
                "capture generation identity for source-state load",
                self.directory.clone(),
                source,
            )
        })?;
        let measured_directory = generation
            .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
            .map_err(|source| {
                persisted_read_error(
                    "open source-state directory for evidence measurement",
                    directory.clone(),
                    source,
                )
            })?;
        let source_state_identity = measured_directory.stable_identity().map_err(|source| {
            persisted_read_error(
                "capture source-state directory identity",
                directory.clone(),
                source,
            )
        })?;
        let actual = measure_anchored_artifact_tree(&directory, measured_directory, budget)?;
        let expected = self.manifest.artifacts().source_state();
        if actual != expected {
            return Err(invalid_source_state(
                &self.directory,
                SourceStateError::PhysicalEvidenceMismatch { expected, actual },
            ));
        }
        let opened_directory = generation
            .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
            .map_err(|source| {
                persisted_read_error(
                    "reopen source-state directory for parsing",
                    directory.clone(),
                    source,
                )
            })?;
        opened_directory
            .ensure_identity(source_state_identity)
            .map_err(|source| {
                persisted_read_error(
                    "revalidate source-state directory before parsing",
                    directory.clone(),
                    source,
                )
            })?;
        let snapshot =
            read_source_state_snapshot_in(&opened_directory, &directory, budget, limits)?;
        validate_source_state_manifest(&snapshot, &self.manifest)
            .map_err(|source| invalid_source_state(&self.directory, source))?;
        opened_directory
            .ensure_identity(source_state_identity)
            .map_err(|source| {
                persisted_read_error(
                    "revalidate source-state directory after parsing",
                    directory.clone(),
                    source,
                )
            })?;
        generation
            .ensure_identity(generation_identity)
            .map_err(|source| {
                persisted_read_error(
                    "revalidate generation after source-state load",
                    self.directory.clone(),
                    source,
                )
            })?;
        let rebound_generation =
            SecureReadDirectory::open(&self.directory, OpenPolicy::PersistedState).map_err(
                |source| {
                    persisted_read_error(
                        "reopen generation after source-state load",
                        self.directory.clone(),
                        source,
                    )
                },
            )?;
        rebound_generation
            .ensure_identity(generation_identity)
            .map_err(|source| {
                persisted_read_error(
                    "rebind generation after source-state load",
                    self.directory.clone(),
                    source,
                )
            })?;
        Ok(snapshot)
    }
}

#[cfg(test)]
fn read_source_state_snapshot(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    limits: SourceStateLimits,
) -> Result<SourceStateSnapshot, GenerationStoreError> {
    let opened_directory = SecureReadDirectory::open(directory, OpenPolicy::PersistedState)
        .map_err(|source| {
            persisted_read_error(
                "open source-state directory",
                directory.to_path_buf(),
                source,
            )
        })?;
    read_source_state_snapshot_in(&opened_directory, directory, budget, limits)
}

fn read_source_state_snapshot_in(
    directory: &SecureReadDirectory,
    directory_path: &Path,
    budget: &mut AssetLoadBudget,
    limits: SourceStateLimits,
) -> Result<SourceStateSnapshot, GenerationStoreError> {
    read_source_state_contract_in(
        directory,
        directory_path,
        SOURCE_STATE_FILE,
        budget,
        limits,
        SourceStateSnapshot::validate_limits,
        source_state_entry_count,
    )
}

fn read_source_state_contract_in<T>(
    directory: &SecureReadDirectory,
    directory_path: &Path,
    file_name: &'static str,
    budget: &mut AssetLoadBudget,
    limits: SourceStateLimits,
    validate_limits: impl FnOnce(&T, SourceStateLimits) -> Result<(), SourceStateError>,
    semantic_entry_count: impl FnOnce(&T) -> Result<u64, SourceStateError>,
) -> Result<T, GenerationStoreError>
where
    T: DeserializeOwned,
{
    let path = directory_path.join(file_name);
    let mut file = directory
        .open_regular(OsStr::new(file_name))
        .map_err(|source| persisted_read_error("open source state", path.clone(), source))?;
    let encoded_length = file.length();
    if encoded_length > limits.max_encoded_bytes {
        return Err(classify_source_state_file_error(
            &path,
            SourceStateError::EncodedTooLarge {
                actual: encoded_length,
                maximum: limits.max_encoded_bytes,
            },
        ));
    }

    let read_limit = encoded_length
        .checked_add(1)
        .ok_or(SourceStateError::SizeOverflow {
            resource: "source state read limit",
        })
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    budget
        .check_bytes(read_limit)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .consume_bytes(read_limit)
        .map_err(GenerationStoreError::Budget)?;
    let capacity = usize::try_from(read_limit)
        .map_err(|_| SourceStateError::SizeOverflow {
            resource: "source state read buffer",
        })
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "source state encoded bytes",
            requested: capacity,
        })?;
    Read::by_ref(file.file_mut())
        .take(read_limit)
        .read_to_end(&mut encoded)
        .map_err(|source| GenerationStoreError::io("read source state", path.clone(), source))?;
    file.ensure_unchanged()
        .map_err(|source| persisted_read_error("revalidate source state", path.clone(), source))?;
    let actual = u64::try_from(encoded.len())
        .map_err(|_| SourceStateError::SizeOverflow {
            resource: "source state encoded length",
        })
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    if actual > limits.max_encoded_bytes {
        return Err(classify_source_state_file_error(
            &path,
            SourceStateError::EncodedTooLarge {
                actual,
                maximum: limits.max_encoded_bytes,
            },
        ));
    }
    if actual != encoded_length {
        return Err(classify_source_state_file_error(
            &path,
            SourceStateError::EncodedLengthChanged {
                expected: encoded_length,
                actual,
            },
        ));
    }

    let structure = scan_json_structure(&encoded)
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    let owned_allocation = source_state_owned_allocation_bound(structure)
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    budget
        .check_entries(structure.array_entries)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .check_members(structure.object_members)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .check_depth(structure.max_depth)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .check_bytes(owned_allocation)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .consume_entries(structure.array_entries)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .consume_members(structure.object_members)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .observe_depth(structure.max_depth)
        .map_err(GenerationStoreError::Budget)?;
    budget
        .consume_bytes(owned_allocation)
        .map_err(GenerationStoreError::Budget)?;
    let snapshot: T = serde_json::from_slice(&encoded)
        .map_err(SourceStateError::Json)
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    validate_limits(&snapshot, limits)
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    let semantic_entries = semantic_entry_count(&snapshot)
        .map_err(|source| classify_source_state_file_error(&path, source))?;
    if semantic_entries > structure.array_entries {
        return Err(classify_source_state_file_error(
            &path,
            SourceStateError::StructuralEntryUnderestimate {
                structural: structure.array_entries,
                semantic: semantic_entries,
            },
        ));
    }
    Ok(snapshot)
}

fn validate_persisted_source_state(
    directory: &Path,
    manifest: &SearchGenerationManifestV1,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state =
        SecureReadDirectory::open(&source_state_directory, OpenPolicy::PersistedState).map_err(
            |source| {
                persisted_read_error(
                    "open completed source-state directory",
                    source_state_directory.clone(),
                    source,
                )
            },
        )?;
    validate_persisted_source_state_in(
        directory,
        &source_state_directory,
        &opened_source_state,
        manifest,
        budget,
    )
}

fn validate_persisted_source_state_in(
    generation_directory: &Path,
    source_state_directory: &Path,
    opened_source_state: &SecureReadDirectory,
    manifest: &SearchGenerationManifestV1,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let source_limits = SourceStateLimits::default();
    let snapshot = read_source_state_snapshot_in(
        opened_source_state,
        source_state_directory,
        budget,
        source_limits,
    )?;
    validate_source_state_manifest(&snapshot, manifest)
        .map_err(|error| classify_persisted_source_state_error(generation_directory, error))
}

fn invalid_source_state(directory: &Path, source: SourceStateError) -> GenerationStoreError {
    invalid_source_state_file(directory, SOURCE_STATE_FILE, source)
}

fn invalid_source_state_file(
    directory: &Path,
    file_name: &'static str,
    source: SourceStateError,
) -> GenerationStoreError {
    GenerationStoreError::SourceState {
        path: directory
            .join(SOURCE_STATE_ARTIFACT_DIRECTORY)
            .join(file_name),
        source: Box::new(source),
    }
}

fn classify_source_state_file_error(path: &Path, source: SourceStateError) -> GenerationStoreError {
    match source {
        SourceStateError::Budget(BudgetedJsonError::Budget(source)) => {
            GenerationStoreError::Budget(source)
        }
        SourceStateError::Budget(source) => GenerationStoreError::ContractJson {
            artifact: "source state",
            path: path.to_path_buf(),
            source,
        },
        SourceStateError::AllocationFailed { requested, .. } => {
            GenerationStoreError::AllocationFailed {
                resource: "source state materialization",
                requested,
            }
        }
        SourceStateError::SizeOverflow { resource } => {
            GenerationStoreError::SizeOverflow { resource }
        }
        source => GenerationStoreError::SourceState {
            path: path.to_path_buf(),
            source: Box::new(source),
        },
    }
}

fn classify_persisted_source_state_error(
    directory: &Path,
    source: SourceStateError,
) -> GenerationStoreError {
    classify_source_state_file_error(
        &directory
            .join(SOURCE_STATE_ARTIFACT_DIRECTORY)
            .join(SOURCE_STATE_FILE),
        source,
    )
}

/// Classifies non-fatal work that could not be completed around generation publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationPublishWarningKind {
    /// Cleanup of an inactive completed-generation preparation could not be finished.
    PreparationCleanup,
    /// The activation is visible, but crash durability could not be confirmed.
    PostCommitDurability,
    /// The activation is visible, but its staging cleanup could not be finished durably.
    PostCommitCleanup,
    /// Best-effort retention after selecting the active generation could not be finished.
    Retention,
}

/// Typed evidence for non-fatal generation publication work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationPublishWarning {
    kind: GenerationPublishWarningKind,
    message: String,
}

impl GenerationPublishWarning {
    pub(crate) fn new(kind: GenerationPublishWarningKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> GenerationPublishWarningKind {
        self.kind
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for GenerationPublishWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn platform_durability_warnings() -> Vec<GenerationPublishWarning> {
    #[cfg(unix)]
    {
        Vec::new()
    }
    #[cfg(not(unix))]
    {
        vec![GenerationPublishWarning::new(
            GenerationPublishWarningKind::PostCommitDurability,
            "directory namespace durability is platform-dependent; file contents and create-new activation were synced, but the standard library cannot fsync directories on this platform",
        )]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GenerationPublishReport {
    pub active: GenerationSnapshot,
    pub warnings: Vec<GenerationPublishWarning>,
}

/// A completed generation whose readers must be validated before activation.
///
/// Holding this capability keeps the store mutably borrowed, so no second generation can be
/// prepared or activated out of order. Dropping it leaves a valid, inactive completed generation
/// that recovery can safely reuse.
#[derive(Debug)]
pub(crate) struct PreparedGenerationPublish<'store> {
    store: &'store mut GenerationStore,
    snapshot: GenerationSnapshot,
    activation: PreparedActivation,
    warnings: Vec<GenerationPublishWarning>,
    failpoint: Option<GenerationFailpoint>,
}

impl PreparedGenerationPublish<'_> {
    #[must_use]
    pub const fn snapshot(&self) -> &GenerationSnapshot {
        &self.snapshot
    }

    /// Durably activates the generation after the caller has opened and validated every reader
    /// represented by [`Self::snapshot`].
    ///
    /// Reader validation is intentionally absent here. Once the activation hard link is visible,
    /// later durability, cleanup, and retention failures are returned as typed warnings so a
    /// successful result always agrees with both in-memory and reopened active state.
    #[cfg(test)]
    pub fn activate(self) -> Result<GenerationPublishReport, GenerationStoreError> {
        self.activate_with_budget(&mut AssetLoadBudget::default())
    }

    pub fn activate_with_budget(
        self,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublishReport, GenerationStoreError> {
        let Self {
            store,
            snapshot,
            activation,
            mut warnings,
            failpoint,
        } = self;
        store.revalidate_private_root(
            "revalidate private index root before activation publication",
        )?;
        match activation {
            PreparedActivation::AlreadyActive => {
                let mut maintenance_budget = AssetLoadBudget::default();
                if let Err(error) = store.prune_retention(&mut maintenance_budget) {
                    warnings.push(GenerationPublishWarning::new(
                        GenerationPublishWarningKind::Retention,
                        error.to_string(),
                    ));
                }
                Ok(GenerationPublishReport {
                    active: snapshot,
                    warnings,
                })
            }
            PreparedActivation::Activate { manifest_digest } => {
                inject_failure(failpoint, GenerationFailpoint::Activation)?;
                store.activate_prepared(snapshot, manifest_digest, warnings, failpoint, budget)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedActivation {
    AlreadyActive,
    Activate { manifest_digest: DigestV1 },
}

/// Preflight estimate for the period where the old and new generations coexist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationDiskEstimate {
    pub existing_generation_bytes: u64,
    pub old_active_generation_bytes: u64,
    pub new_generation_bytes: u64,
    pub publish_peak_bytes: u64,
    pub retained_bytes_after_publish: u64,
    pub reclaimable_bytes_after_publish: u64,
}

/// Deterministic failure injection checkpoints used by state-machine tests.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationFailpoint {
    Search,
    References,
    SourceState,
    #[cfg(test)]
    StartupStagingCleanup,
    Activation,
    ActivationPreCommit,
    ActivationDirectorySync,
    ActivationCleanup,
}

#[derive(Debug)]
struct WriterLease {
    file: File,
}

impl WriterLease {
    fn acquire(root: &Path) -> Result<Self, GenerationStoreError> {
        let path = root.join(WRITER_LEASE_FILE);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path).map_err(|source| {
                    GenerationStoreError::io(
                        "inspect generation writer lease",
                        path.clone(),
                        source,
                    )
                })?;
                reject_link_or_reparse(&path, &metadata)?;
                if !metadata.is_file() {
                    return Err(GenerationStoreError::UnsupportedFileType { path });
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| {
                        GenerationStoreError::io(
                            "open generation writer lease",
                            path.clone(),
                            source,
                        )
                    })?
            }
            Err(source) => {
                return Err(GenerationStoreError::io(
                    "create generation writer lease",
                    path,
                    source,
                ));
            }
        };
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            GenerationStoreError::io("reinspect generation writer lease", path.clone(), source)
        })?;
        reject_link_or_reparse(&path, &metadata)?;
        if !metadata.is_file() {
            return Err(GenerationStoreError::UnsupportedFileType { path });
        }
        file.try_lock_exclusive()
            .map_err(|source| GenerationStoreError::WriterLeaseUnavailable { path, source })?;
        Ok(Self { file })
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Bounded evidence produced by one abandoned-staging reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GenerationStagingRecoveryReport {
    removed_entries: u64,
}

impl GenerationStagingRecoveryReport {
    #[must_use]
    pub(crate) const fn removed_entries(self) -> u64 {
        self.removed_entries
    }
}

fn merge_recovery_results(
    first: Result<GenerationStagingRecoveryReport, GenerationStoreError>,
    second: Result<GenerationStagingRecoveryReport, GenerationStoreError>,
) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
    match (first, second) {
        (Ok(first), Ok(second)) => Ok(GenerationStagingRecoveryReport {
            removed_entries: first
                .removed_entries
                .checked_add(second.removed_entries)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "startup recovery entries",
                })?,
        }),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

/// Typed evidence that the authoritative generation belongs to a recognized obsolete cache
/// contract and must be rebuilt instead of queried or converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexRebuildRequired {
    reason: IndexRebuildReason,
    activation_ordinal: u64,
    generation: SearchGenerationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexRebuildReason {
    ObsoleteActivationContract { actual: u16 },
    ObsoleteGenerationStorage { actual: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationStartupDisposition {
    Ready,
    RebuildRequired(IndexRebuildRequired),
}

impl GenerationStartupDisposition {
    #[must_use]
    pub(crate) const fn rebuild_required(self) -> Option<IndexRebuildRequired> {
        match self {
            Self::Ready => None,
            Self::RebuildRequired(required) => Some(required),
        }
    }
}

/// A validated generation store paired with the independent startup-maintenance outcome.
///
/// Staging is not authoritative for the active generation. Its cleanup result is therefore
/// reported separately so callers can keep a validated active generation queryable while
/// scheduling another bounded reconciliation pass.
#[derive(Debug)]
pub(crate) struct OpenedGenerationStore {
    store: GenerationStore,
    staging_recovery: Result<GenerationStagingRecoveryReport, GenerationStoreError>,
    startup_disposition: GenerationStartupDisposition,
}

impl OpenedGenerationStore {
    pub(crate) fn into_parts(
        self,
    ) -> (
        GenerationStore,
        Result<GenerationStagingRecoveryReport, GenerationStoreError>,
        GenerationStartupDisposition,
    ) {
        (self.store, self.staging_recovery, self.startup_disposition)
    }
}

#[derive(Debug)]
enum GenerationStoreRootAuthority {
    Private(PrivateIndexRootV1),
    #[cfg(test)]
    Fixture,
}

impl GenerationStoreRootAuthority {
    fn revalidate(&self, operation: &'static str) -> Result<(), GenerationStoreError> {
        match self {
            Self::Private(private_root) => revalidate_private_index_root(private_root, operation),
            #[cfg(test)]
            Self::Fixture => Ok(()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct GenerationStore {
    root: PathBuf,
    root_authority: GenerationStoreRootAuthority,
    generations: PathBuf,
    staging: PathBuf,
    activations: PathBuf,
    options: GenerationStoreOptions,
    active: Option<GenerationSnapshot>,
    next_staging_ordinal: u64,
    next_activation_ordinal: u64,
    lease: Arc<WriterLease>,
    live_build: Arc<LiveBuildClaim>,
}

impl GenerationStore {
    /// Opens the durable store and selects the generation named by the latest committed head.
    ///
    /// The highest head is the sole authority for actual and desired revision state. Corruption in
    /// that head or its immutable generation therefore fails closed instead of silently rolling
    /// freshness back to an older activation. Authoritative directory discovery and validation
    /// share the caller's ledger; abandoned-staging cleanup uses an independent bounded ledger and
    /// is returned as maintenance evidence rather than invalidating a verified active generation.
    pub(crate) fn open_private(
        private_root: PrivateIndexRootV1,
        options: GenerationStoreOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<OpenedGenerationStore, GenerationStoreError> {
        revalidate_private_index_root(
            &private_root,
            "revalidate private index root before reopening generation store",
        )?;
        let root = private_root.path().to_path_buf();
        Self::open_at_root(
            root,
            GenerationStoreRootAuthority::Private(private_root),
            options,
            budget,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn open_private_with_startup_recovery_failpoint(
        private_root: PrivateIndexRootV1,
        options: GenerationStoreOptions,
        budget: &mut AssetLoadBudget,
        failpoint: GenerationFailpoint,
    ) -> Result<OpenedGenerationStore, GenerationStoreError> {
        revalidate_private_index_root(
            &private_root,
            "revalidate private index root before reopening generation store",
        )?;
        let root = private_root.path().to_path_buf();
        Self::open_at_root(
            root,
            GenerationStoreRootAuthority::Private(private_root),
            options,
            budget,
            Some(failpoint),
        )
    }

    #[cfg(test)]
    pub fn open(
        root: impl AsRef<Path>,
        options: GenerationStoreOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, GenerationStoreError> {
        let root = initialize_root(root.as_ref())?;
        let opened = Self::open_at_root(
            root,
            GenerationStoreRootAuthority::Fixture,
            options,
            budget,
            None,
        )?;
        let (store, staging_recovery, _) = opened.into_parts();
        staging_recovery?;
        Ok(store)
    }

    fn open_at_root(
        root: PathBuf,
        root_authority: GenerationStoreRootAuthority,
        options: GenerationStoreOptions,
        budget: &mut AssetLoadBudget,
        #[cfg(test)] startup_recovery_failpoint: Option<GenerationFailpoint>,
    ) -> Result<OpenedGenerationStore, GenerationStoreError> {
        root_authority
            .revalidate("revalidate private index root before opening generation store")?;
        ensure_existing_directory_no_follow(&root)?;
        let lease = Arc::new(WriterLease::acquire(&root)?);
        let generations = ensure_managed_directory(&root, GENERATIONS_DIRECTORY)?;
        let staging = ensure_managed_directory(&root, STAGING_DIRECTORY)?;
        let activations = ensure_managed_directory(&root, ACTIVATIONS_DIRECTORY)?;
        let opened_generations =
            SecureReadDirectory::open(&generations, OpenPolicy::PersistedState).map_err(
                |source| {
                    persisted_read_error("open generations directory", generations.clone(), source)
                },
            )?;
        let opened_activations =
            SecureReadDirectory::open(&activations, OpenPolicy::PersistedState).map_err(
                |source| {
                    persisted_read_error("open activations directory", activations.clone(), source)
                },
            )?;
        let mut recovery_budget = AssetLoadBudget::default();
        let mut staging_recovery = recover_owned_staging(
            &staging,
            &generations,
            true,
            &mut recovery_budget,
            #[cfg(test)]
            startup_recovery_failpoint,
        );
        let opened_staging = SecureReadDirectory::open(&staging, OpenPolicy::PersistedState)
            .map_err(|source| {
                persisted_read_error("open staging directory", staging.clone(), source)
            })?;

        let next_staging_ordinal = next_staging_ordinal(&staging, &opened_staging, budget)?;
        let activation_snapshot = activation_candidates_for_open(
            &activations,
            &opened_activations,
            &staging,
            &opened_staging,
            budget,
        )?;
        let selected = select_active_generation(
            &activations,
            &generations,
            &opened_generations,
            &opened_activations,
            &activation_snapshot,
            budget,
        );
        let next_activation_ordinal = activation_snapshot.next_ordinal;
        drop(opened_generations);
        drop(opened_activations);
        drop(opened_staging);

        let (active, startup_disposition) = match selected {
            Ok(active) => (active, GenerationStartupDisposition::Ready),
            Err(GenerationStoreError::IndexRebuildRequired(required)) => {
                root_authority.revalidate(
                    "revalidate private index root before retiring obsolete activations",
                )?;
                retire_obsolete_activation_authority(&root, &activations, &staging, required)?;
                let retired_recovery = recover_owned_staging(
                    &staging,
                    &generations,
                    true,
                    &mut recovery_budget,
                    #[cfg(test)]
                    startup_recovery_failpoint,
                );
                staging_recovery = merge_recovery_results(staging_recovery, retired_recovery);
                (
                    None,
                    GenerationStartupDisposition::RebuildRequired(required),
                )
            }
            Err(error) => return Err(error),
        };
        let generation_recovery = recover_unreferenced_generation_directories(
            &generations,
            startup_disposition.rebuild_required().is_some(),
            &mut recovery_budget,
        );
        staging_recovery = merge_recovery_results(staging_recovery, generation_recovery);

        let store = Self {
            root,
            root_authority,
            generations,
            staging,
            activations,
            options,
            active,
            next_staging_ordinal,
            next_activation_ordinal,
            lease,
            live_build: Arc::new(LiveBuildClaim::default()),
        };
        store.revalidate_private_root(
            "revalidate private index root after reopening generation store",
        )?;
        Ok(OpenedGenerationStore {
            store,
            staging_recovery,
            startup_disposition,
        })
    }

    #[must_use]
    pub const fn active(&self) -> Option<&GenerationSnapshot> {
        self.active.as_ref()
    }

    /// Removes only store-owned abandoned entries from the private staging namespace.
    ///
    /// A live build is an exclusive capability. Recovery refuses to run while that capability is
    /// armed, so it cannot mistake an in-flight build for crash residue.
    pub(crate) fn reconcile_abandoned_staging(
        &mut self,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
        if self.live_build.is_held() {
            return Err(GenerationStoreError::BuildAlreadyActive);
        }
        recover_owned_staging(
            &self.staging,
            &self.generations,
            self.active.is_none(),
            budget,
            #[cfg(test)]
            None,
        )
    }

    /// Appends a durable head for the current immutable generation before derived work starts.
    ///
    /// The hard-linked head is the commit point. A later generation activation records
    /// `desired_revision == actual_revision`, so no mutable sidecar or cross-file clearing
    /// protocol is required.
    pub fn record_desired_revision(
        &mut self,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationPublishWarning>, GenerationStoreError> {
        let Some(active) = self.active.clone() else {
            return Ok(Vec::new());
        };
        if active.desired_revision == desired_revision {
            return Ok(Vec::new());
        }

        let mut updated = active;
        updated.activation_ordinal = self.allocate_activation_ordinal()?;
        updated.desired_revision = desired_revision;
        let report = self.activate_prepared(
            updated.clone(),
            updated.manifest_digest,
            platform_durability_warnings(),
            None,
            budget,
        )?;
        Ok(report.warnings)
    }

    #[must_use]
    pub fn generation_directory(&self, generation: SearchGenerationId) -> PathBuf {
        self.generations.join(generation.directory_name())
    }

    pub fn begin(&mut self) -> Result<GenerationBuild, GenerationStoreError> {
        self.revalidate_private_root("revalidate private index root before creating generation")?;
        let claim = self.live_build.acquire()?;
        loop {
            let ordinal = self.next_staging_ordinal;
            self.next_staging_ordinal = ordinal
                .checked_add(1)
                .ok_or(GenerationStoreError::OrdinalOverflow)?;
            let directory = self.staging.join(staging_directory_name(ordinal));

            match fs::create_dir(&directory) {
                Ok(()) => {
                    let build = GenerationBuild {
                        store_root: self.root.clone(),
                        ordinal,
                        directory,
                        lease: Arc::clone(&self.lease),
                        claim,
                        state: GenerationBuildState::Armed,
                    };
                    create_build_directories(&build.directory)?;
                    return Ok(build);
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => {
                    return Err(GenerationStoreError::io(
                        "create generation staging directory",
                        directory,
                        source,
                    ));
                }
            }
        }
    }

    #[cfg(test)]
    pub fn measure_artifacts(
        &self,
        build: &GenerationBuild,
    ) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        self.measure_artifacts_with_budget(build, &mut budget)
    }

    pub fn measure_artifacts_with_budget(
        &self,
        build: &GenerationBuild,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
        self.validate_build(build)?;
        measure_generation_artifacts(&build.directory, budget, None)
    }

    #[cfg(test)]
    pub fn prepare_publish(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        let desired_revision = manifest.revision();
        let activation = GenerationActivationEvidence::new(
            self.active.as_ref().map(GenerationSnapshot::generation),
            TransactionReceiptWindow::empty(),
        );
        self.prepare_publish_inner(
            build,
            manifest,
            activation,
            desired_revision,
            &mut budget,
            None,
        )
    }

    pub(crate) fn prepare_publish_with_desired_revision_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        activation: GenerationActivationEvidence,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.prepare_publish_inner(build, manifest, activation, desired_revision, budget, None)
    }

    #[cfg(test)]
    pub fn prepare_publish_with_failpoint(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let mut budget = AssetLoadBudget::default();
        let activation = GenerationActivationEvidence::new(
            self.active.as_ref().map(GenerationSnapshot::generation),
            TransactionReceiptWindow::empty(),
        );
        self.prepare_publish_with_failpoint_and_budget(
            build,
            manifest,
            activation,
            &mut budget,
            failpoint,
        )
    }

    #[cfg(test)]
    pub fn prepare_publish_with_failpoint_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        activation: GenerationActivationEvidence,
        budget: &mut AssetLoadBudget,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        let desired_revision = manifest.revision();
        self.prepare_publish_inner(
            build,
            manifest,
            activation,
            desired_revision,
            budget,
            Some(failpoint),
        )
    }

    #[cfg(test)]
    pub(crate) fn prepare_publish_with_desired_revision_failpoint_and_budget(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        activation: GenerationActivationEvidence,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
        failpoint: GenerationFailpoint,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.prepare_publish_inner(
            build,
            manifest,
            activation,
            desired_revision,
            budget,
            Some(failpoint),
        )
    }

    pub fn estimate_publish(
        &self,
        new_generation_bytes: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationDiskEstimate, GenerationStoreError> {
        let existing_generations = completed_generation_sizes(&self.generations, budget)?;
        let existing_generation_bytes = checked_sum(
            existing_generations.iter().map(|(_, bytes)| *bytes),
            "existing generation bytes",
        )?;
        let old_active_generation_bytes = self
            .active
            .as_ref()
            .and_then(|active| {
                existing_generations
                    .iter()
                    .find(|(generation, _)| *generation == active.generation())
                    .map(|(_, bytes)| *bytes)
            })
            .unwrap_or(0);

        let historical = self.retained_historical_snapshots(budget)?;
        let mut retained_after_publish = Vec::new();
        if self.options.retain_previous_generations != 0 {
            if let Some(active) = &self.active {
                retained_after_publish.push(active.generation());
            }
            retained_after_publish.extend(
                historical
                    .into_iter()
                    .map(|generation| generation.generation())
                    .take(self.options.retain_previous_generations.saturating_sub(1)),
            );
        }
        let retained_historical_bytes = checked_sum(
            retained_after_publish.iter().filter_map(|generation| {
                existing_generations
                    .iter()
                    .find(|(candidate, _)| candidate == generation)
                    .map(|(_, bytes)| *bytes)
            }),
            "retained historical generation bytes",
        )?;
        let retained_bytes_after_publish = new_generation_bytes
            .checked_add(retained_historical_bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "retained bytes after publish",
            })?;
        let publish_peak_bytes = existing_generation_bytes
            .checked_add(new_generation_bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "publish peak bytes",
            })?;
        let reclaimable_bytes_after_publish =
            publish_peak_bytes.saturating_sub(retained_bytes_after_publish);

        Ok(GenerationDiskEstimate {
            existing_generation_bytes,
            old_active_generation_bytes,
            new_generation_bytes,
            publish_peak_bytes,
            retained_bytes_after_publish,
            reclaimable_bytes_after_publish,
        })
    }

    pub fn estimate_manifest_publish(
        &self,
        manifest: &SearchGenerationManifestV1,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationDiskEstimate, GenerationStoreError> {
        let manifest_path = self.staging.join(MANIFEST_FILE);
        let manifest_bytes = generation_manifest_json_length(manifest, &manifest_path)?;
        budget
            .check_bytes(manifest_bytes)
            .and_then(|()| budget.consume_bytes(manifest_bytes))
            .map_err(GenerationStoreError::Budget)?;
        let artifact_bytes =
            manifest
                .artifacts()
                .total_bytes()
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "incoming generation artifact bytes",
                })?;
        let new_generation_bytes = artifact_bytes.checked_add(manifest_bytes).ok_or(
            GenerationStoreError::SizeOverflow {
                resource: "incoming generation bytes",
            },
        )?;
        self.estimate_publish(new_generation_bytes, budget)
    }

    fn prepare_publish_inner(
        &mut self,
        build: &mut GenerationBuild,
        manifest: SearchGenerationManifestV1,
        activation: GenerationActivationEvidence,
        desired_revision: WorkspaceRevision,
        budget: &mut AssetLoadBudget,
        failpoint: Option<GenerationFailpoint>,
    ) -> Result<PreparedGenerationPublish<'_>, GenerationStoreError> {
        self.validate_build(build)?;
        activation
            .transaction_receipts()
            .validate_for_workspace(manifest.workspace())
            .map_err(|source| GenerationStoreError::ActivationProvenance {
                path: build.directory.clone(),
                source: Box::new(source),
            })?;
        self.validate_activation_parent(&manifest, &activation)?;

        let observed = measure_generation_artifacts(&build.directory, budget, failpoint)?;
        if observed != manifest.artifacts() {
            return Err(GenerationStoreError::ArtifactEvidenceMismatch {
                expected: Box::new(manifest.artifacts()),
                actual: Box::new(observed),
            });
        }
        validate_persisted_source_state(&build.directory, &manifest, budget)?;

        let generation = manifest.generation_id();
        if let Some(active) = self
            .active
            .as_ref()
            .filter(|active| active.generation == generation)
            .cloned()
        {
            let completed = inspect_completed_generation(&active.directory, generation, budget)?;
            build.abort_with_budget(budget)?;
            if active.desired_revision != desired_revision || active.activation != activation {
                let mut refreshed = active;
                refreshed.activation_ordinal = self.allocate_activation_ordinal()?;
                refreshed.manifest_digest = completed.manifest_digest;
                refreshed.desired_revision = desired_revision;
                refreshed.activation = activation;
                return Ok(PreparedGenerationPublish {
                    store: self,
                    snapshot: refreshed,
                    activation: PreparedActivation::Activate {
                        manifest_digest: completed.manifest_digest,
                    },
                    warnings: platform_durability_warnings(),
                    failpoint,
                });
            }
            return Ok(PreparedGenerationPublish {
                store: self,
                snapshot: active,
                activation: PreparedActivation::AlreadyActive,
                warnings: Vec::new(),
                failpoint,
            });
        }

        let completed_directory = self.generation_directory(generation);
        let replace_invalid_completed = if path_exists_no_follow(&completed_directory)? {
            match inspect_completed_generation(&completed_directory, generation, budget) {
                Ok(completed) => {
                    build.abort_with_budget(budget)?;
                    let activation_ordinal = self.allocate_activation_ordinal()?;
                    let snapshot = GenerationSnapshot {
                        activation_ordinal,
                        generation,
                        manifest_digest: completed.manifest_digest,
                        desired_revision,
                        manifest: completed.manifest,
                        activation,
                        directory: completed_directory,
                    };
                    return Ok(PreparedGenerationPublish {
                        store: self,
                        snapshot,
                        activation: PreparedActivation::Activate {
                            manifest_digest: completed.manifest_digest,
                        },
                        warnings: platform_durability_warnings(),
                        failpoint,
                    });
                }
                Err(error) if error.is_repairable_completed_generation() => true,
                Err(error) => return Err(error),
            }
        } else {
            false
        };

        let manifest_path = build.directory.join(MANIFEST_FILE);
        let manifest_bytes = encode_generation_manifest_json(&manifest, &manifest_path, budget)?;
        write_new_file(&manifest_path, &manifest_bytes)?;
        sync_tree_no_follow(&build.directory, budget)?;
        let durable_observed = measure_generation_artifacts(&build.directory, budget, None)?;
        if durable_observed != manifest.artifacts() {
            return Err(GenerationStoreError::ArtifactEvidenceMismatch {
                expected: Box::new(manifest.artifacts()),
                actual: Box::new(durable_observed),
            });
        }

        self.revalidate_private_root(
            "revalidate private index root before completing generation publication",
        )?;
        let quarantine = if replace_invalid_completed {
            let quarantine = self
                .staging
                .join(quarantine_directory_name(build.ordinal, generation));
            if path_exists_no_follow(&quarantine)? {
                return Err(GenerationStoreError::QuarantineCollision { path: quarantine });
            }
            fs::rename(&completed_directory, &quarantine).map_err(|source| {
                GenerationStoreError::io(
                    "quarantine invalid completed generation",
                    completed_directory.clone(),
                    source,
                )
            })?;
            if let Err(primary) =
                sync_directory(&self.generations).and_then(|()| sync_directory(&self.staging))
            {
                return Err(self.rollback_quarantine(&quarantine, &completed_directory, primary));
            }
            Some(quarantine)
        } else {
            None
        };

        if let Err(source) = fs::rename(&build.directory, &completed_directory) {
            let primary = GenerationStoreError::io(
                "complete generation directory",
                completed_directory.clone(),
                source,
            );
            if let Some(quarantine) = &quarantine {
                return Err(self.rollback_quarantine(quarantine, &completed_directory, primary));
            }
            return Err(primary);
        }
        build.mark_completed();
        sync_directory(&self.generations)?;

        let mut warnings = Vec::new();
        if let Some(quarantine) = quarantine {
            if let Err(error) = remove_tree_no_follow(&quarantine, budget) {
                warnings.push(GenerationPublishWarning::new(
                    GenerationPublishWarningKind::PreparationCleanup,
                    error.to_string(),
                ));
            } else if let Err(error) = sync_directory(&self.staging) {
                warnings.push(GenerationPublishWarning::new(
                    GenerationPublishWarningKind::PreparationCleanup,
                    error.to_string(),
                ));
            }
        }
        let manifest_digest = DigestV1::hash_bytes(&manifest_bytes);
        warnings.extend(platform_durability_warnings());
        let activation_ordinal = self.allocate_activation_ordinal()?;
        let snapshot = GenerationSnapshot {
            activation_ordinal,
            generation,
            manifest_digest,
            desired_revision,
            manifest,
            activation,
            directory: completed_directory,
        };
        Ok(PreparedGenerationPublish {
            store: self,
            snapshot,
            activation: PreparedActivation::Activate { manifest_digest },
            warnings,
            failpoint,
        })
    }

    fn rollback_quarantine(
        &self,
        quarantine: &Path,
        completed_directory: &Path,
        primary: GenerationStoreError,
    ) -> GenerationStoreError {
        let rollback = fs::rename(quarantine, completed_directory)
            .map_err(|source| {
                GenerationStoreError::io(
                    "restore quarantined generation",
                    completed_directory.to_path_buf(),
                    source,
                )
            })
            .and_then(|()| sync_directory(&self.generations))
            .and_then(|()| sync_directory(&self.staging));
        match rollback {
            Ok(()) => primary,
            Err(rollback) => GenerationStoreError::QuarantineRollbackFailed {
                primary: Box::new(primary),
                rollback: Box::new(rollback),
            },
        }
    }

    fn activate_prepared(
        &mut self,
        snapshot: GenerationSnapshot,
        manifest_digest: DigestV1,
        mut warnings: Vec<GenerationPublishWarning>,
        failpoint: Option<GenerationFailpoint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<GenerationPublishReport, GenerationStoreError> {
        let record = GenerationHeadRecord {
            contract_version: GENERATION_HEAD_CONTRACT_VERSION,
            ordinal: snapshot.activation_ordinal,
            generation: snapshot.generation(),
            generation_storage_contract: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
            manifest_digest,
            workspace: snapshot.manifest.workspace(),
            revision: snapshot.manifest.revision(),
            desired_revision: Some(snapshot.desired_revision),
            parent_generation: snapshot.parent_generation(),
            transaction_receipts: snapshot.transaction_receipts().clone(),
        };
        self.revalidate_private_root("revalidate private index root before writing activation")?;
        warnings.extend(self.write_activation(&record, failpoint, budget)?);

        self.active = Some(snapshot.clone());

        // Retention is post-commit maintenance. A security violation cannot turn this committed
        // activation into a failed publication; reopening rescans managed directories without
        // following links and fails closed if the unsafe entry remains.
        let mut maintenance_budget = AssetLoadBudget::default();
        if let Err(error) = self.prune_retention(&mut maintenance_budget) {
            warnings.push(GenerationPublishWarning::new(
                GenerationPublishWarningKind::Retention,
                error.to_string(),
            ));
        }
        Ok(GenerationPublishReport {
            active: snapshot,
            warnings,
        })
    }

    fn revalidate_private_root(&self, operation: &'static str) -> Result<(), GenerationStoreError> {
        self.root_authority.revalidate(operation)
    }

    fn validate_build(&self, build: &GenerationBuild) -> Result<(), GenerationStoreError> {
        self.revalidate_private_root(
            "revalidate private index root before generation publication",
        )?;
        if build.store_root != self.root
            || !Arc::ptr_eq(&build.lease, &self.lease)
            || !build.claim.belongs_to(&self.live_build)
            || build.state != GenerationBuildState::Armed
            || build.directory.parent() != Some(self.staging.as_path())
            || build.directory.file_name()
                != Some(OsStr::new(&staging_directory_name(build.ordinal)))
        {
            return Err(GenerationStoreError::ForeignBuild);
        }
        ensure_existing_directory_no_follow(&build.directory)?;
        Ok(())
    }

    fn validate_activation_parent(
        &self,
        manifest: &SearchGenerationManifestV1,
        activation: &GenerationActivationEvidence,
    ) -> Result<(), GenerationStoreError> {
        if let Some(active) = &self.active
            && active.manifest.workspace() != manifest.workspace()
        {
            return Err(GenerationStoreError::WorkspaceMismatch {
                expected: active.manifest.workspace(),
                actual: manifest.workspace(),
            });
        }
        if let Some(expected_parent) = activation.parent_generation() {
            let actual_parent = self.active.as_ref().map(GenerationSnapshot::generation);
            if actual_parent != Some(expected_parent) {
                return Err(GenerationStoreError::ParentGenerationMismatch {
                    expected: expected_parent,
                    actual: actual_parent,
                });
            }
        }
        Ok(())
    }

    fn allocate_activation_ordinal(&mut self) -> Result<u64, GenerationStoreError> {
        loop {
            let ordinal = self.next_activation_ordinal;
            self.next_activation_ordinal = ordinal
                .checked_add(1)
                .ok_or(GenerationStoreError::OrdinalOverflow)?;
            let final_path = self.activations.join(activation_file_name(ordinal));
            let temporary_path = self.staging.join(activation_staging_file_name(ordinal));
            if !path_exists_no_follow(&final_path)? && !path_exists_no_follow(&temporary_path)? {
                return Ok(ordinal);
            }
        }
    }

    fn write_activation(
        &self,
        record: &GenerationHeadRecord,
        failpoint: Option<GenerationFailpoint>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationPublishWarning>, GenerationStoreError> {
        let temporary_path = self
            .staging
            .join(activation_staging_file_name(record.ordinal));
        let final_path = self.activations.join(activation_file_name(record.ordinal));
        let bytes = encode_generation_head_json(record, &final_path, budget)?;
        let mut staging = ActivationStagingFile::create(temporary_path, &bytes)?;
        // A hard link publishes the complete file atomically and never replaces an existing ordinal.
        staging.publish(&final_path, failpoint)?;

        // The final hard link is the commit point: reopen selects this highest valid activation
        // immediately after it becomes visible. Every later failure is reported as evidence rather
        // than returning an error that would disagree with the persisted active generation.
        let mut warnings = Vec::new();
        if let Err(error) = inject_failure(failpoint, GenerationFailpoint::ActivationDirectorySync)
            .and_then(|()| sync_directory(&self.activations))
        {
            warnings.push(GenerationPublishWarning::new(
                GenerationPublishWarningKind::PostCommitDurability,
                error.to_string(),
            ));
        }

        if let Err(error) = inject_failure(failpoint, GenerationFailpoint::ActivationCleanup)
            .and_then(|()| staging.cleanup_after_commit())
        {
            staging.state = ActivationStagingState::Relinquished;
            warnings.push(GenerationPublishWarning::new(
                GenerationPublishWarningKind::PostCommitCleanup,
                error.to_string(),
            ));
        }
        Ok(warnings)
    }

    fn retained_historical_snapshots(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<GenerationSnapshot>, GenerationStoreError> {
        let Some(active) = &self.active else {
            return Ok(Vec::new());
        };
        let opened_activations =
            SecureReadDirectory::open(&self.activations, OpenPolicy::PersistedState).map_err(
                |source| {
                    persisted_read_error(
                        "open activations directory",
                        self.activations.clone(),
                        source,
                    )
                },
            )?;
        let opened_generations =
            SecureReadDirectory::open(&self.generations, OpenPolicy::PersistedState).map_err(
                |source| {
                    persisted_read_error(
                        "open generations directory",
                        self.generations.clone(),
                        source,
                    )
                },
            )?;
        let opened_staging = SecureReadDirectory::open(&self.staging, OpenPolicy::PersistedState)
            .map_err(|source| {
            persisted_read_error("open staging directory", self.staging.clone(), source)
        })?;
        let mut activation_snapshot = activation_candidates_for_open(
            &self.activations,
            &opened_activations,
            &self.staging,
            &opened_staging,
            budget,
        )?;
        let mut candidates = std::mem::take(&mut activation_snapshot.candidates);
        candidates.sort_unstable_by_key(|candidate| std::cmp::Reverse(candidate.ordinal));

        let retained = (|| {
            let mut seen = BTreeSet::new();
            seen.insert(active.generation());
            let mut retained = Vec::new();
            for candidate in candidates {
                if retained.len() >= self.options.retain_previous_generations {
                    break;
                }
                let record = match read_activation_record(
                    &opened_activations,
                    &candidate.display_path,
                    &candidate.file_name,
                    candidate.ordinal,
                    budget,
                ) {
                    Ok(record) => record,
                    Err(error) if error.is_candidate_scan_fatal() => return Err(error),
                    Err(_) => continue,
                };
                if seen.contains(&record.generation) {
                    continue;
                }
                let generation = match load_completed_generation(
                    &self.generations,
                    &opened_generations,
                    record,
                    budget,
                ) {
                    Ok(generation) => generation,
                    Err(error) if error.is_candidate_scan_fatal() => return Err(error),
                    Err(_) => continue,
                };
                if seen.insert(generation.generation()) {
                    retained.push(generation);
                }
            }
            Ok(retained)
        })();
        revalidate_opened_directory_snapshot(
            &self.activations,
            &opened_activations,
            activation_snapshot.directory_identity,
            "revalidate activation directory after reading retained history",
        )?;
        retained
    }

    fn prune_retention(&self, budget: &mut AssetLoadBudget) -> Result<(), GenerationStoreError> {
        let Some(active) = &self.active else {
            return Ok(());
        };
        let historical = self.retained_historical_snapshots(budget)?;
        let retained_directories = historical
            .iter()
            .map(GenerationSnapshot::generation)
            .chain(std::iter::once(active.generation()))
            .map(SearchGenerationId::directory_name)
            .collect::<BTreeSet<_>>();
        let retained_activations = historical
            .iter()
            .map(GenerationSnapshot::activation_ordinal)
            .chain(std::iter::once(active.activation_ordinal))
            .collect::<BTreeSet<_>>();

        let mut pruned_any = false;
        visit_directory_entries_budgeted(&self.generations, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if !metadata.is_dir() {
                return Ok(());
            }
            let Some(name) = entry.file_name.to_str() else {
                return Ok(());
            };
            if SearchGenerationId::from_directory_name(name).is_none() {
                return Ok(());
            }
            if retained_directories.contains(name) {
                return Ok(());
            }
            remove_tree_no_follow(&entry.path, budget)?;
            pruned_any = true;
            Ok(())
        })?;
        if pruned_any {
            sync_directory(&self.generations)?;
        }
        prune_activation_history(&self.activations, &retained_activations, budget)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationHeadRecord {
    contract_version: u16,
    ordinal: u64,
    generation: SearchGenerationId,
    generation_storage_contract: u16,
    manifest_digest: DigestV1,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    desired_revision: Option<WorkspaceRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_generation: Option<SearchGenerationId>,
    #[serde(default)]
    transaction_receipts: TransactionReceiptWindow,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationHeadRecordWire {
    contract_version: u16,
    ordinal: u64,
    generation: SearchGenerationId,
    #[serde(default)]
    generation_storage_contract: WirePresence<Option<u16>>,
    manifest_digest: DigestV1,
    workspace: WorkspaceId,
    revision: WorkspaceRevision,
    #[serde(default)]
    desired_revision: WirePresence<Option<WorkspaceRevision>>,
    #[serde(default)]
    parent_generation: WirePresence<Option<SearchGenerationId>>,
    #[serde(default)]
    transaction_receipts: WirePresence<Option<TransactionReceiptWindow>>,
}

#[derive(Debug, Default)]
enum WirePresence<T> {
    #[default]
    Missing,
    Present(T),
}

impl<'de, T> Deserialize<'de> for WirePresence<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

impl GenerationHeadRecord {
    fn desired_revision(&self) -> WorkspaceRevision {
        self.desired_revision.unwrap_or(self.revision)
    }
}

#[derive(Debug)]
struct ActivationCandidate {
    ordinal: u64,
    display_path: PathBuf,
    file_name: OsString,
}

#[derive(Debug)]
struct ActivationDirectorySnapshot {
    candidates: Vec<ActivationCandidate>,
    next_ordinal: u64,
    directory_identity: StableDirectoryIdentity,
}

#[derive(Debug)]
struct CompletedGeneration {
    manifest: SearchGenerationManifestV1,
    manifest_digest: DigestV1,
}

fn revalidate_private_index_root(
    private_root: &PrivateIndexRootV1,
    operation: &'static str,
) -> Result<(), GenerationStoreError> {
    private_root
        .revalidate()
        .map_err(|source| GenerationStoreError::PrivateIndexRoot {
            operation,
            path: private_root.path().to_path_buf(),
            source,
        })
}

#[cfg(test)]
fn initialize_root(root: &Path) -> Result<PathBuf, GenerationStoreError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            reject_link_or_reparse(root, &metadata)?;
            if !metadata.is_dir() {
                return Err(GenerationStoreError::NotDirectory {
                    path: root.to_path_buf(),
                });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|source| {
                GenerationStoreError::io("create generation store root", root.to_path_buf(), source)
            })?;
        }
        Err(source) => {
            return Err(GenerationStoreError::io(
                "inspect generation store root",
                root.to_path_buf(),
                source,
            ));
        }
    }
    fs::canonicalize(root).map_err(|source| {
        GenerationStoreError::io(
            "canonicalize generation store root",
            root.to_path_buf(),
            source,
        )
    })
}

fn ensure_managed_directory(
    root: &Path,
    name: &'static str,
) -> Result<PathBuf, GenerationStoreError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            reject_link_or_reparse(&path, &metadata)?;
            if !metadata.is_dir() {
                return Err(GenerationStoreError::NotDirectory { path });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&path).map_err(|source| {
                GenerationStoreError::io(
                    "create managed generation directory",
                    path.clone(),
                    source,
                )
            })?;
        }
        Err(source) => {
            return Err(GenerationStoreError::io(
                "inspect managed generation directory",
                path,
                source,
            ));
        }
    }
    Ok(path)
}

fn ensure_existing_directory_no_follow(path: &Path) -> Result<(), GenerationStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        GenerationStoreError::io("inspect generation directory", path.to_path_buf(), source)
    })?;
    reject_link_or_reparse(path, &metadata)?;
    if !metadata.is_dir() {
        return Err(GenerationStoreError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn create_build_directories(directory: &Path) -> Result<(), GenerationStoreError> {
    for name in [
        SEARCH_ARTIFACT_DIRECTORY,
        REFERENCE_ARTIFACT_DIRECTORY,
        SOURCE_STATE_ARTIFACT_DIRECTORY,
    ] {
        let path = directory.join(name);
        fs::create_dir(&path).map_err(|source| {
            GenerationStoreError::io("create generation artifact directory", path, source)
        })?;
    }
    Ok(())
}

struct BudgetedDirectoryEntry {
    path: PathBuf,
    file_name: OsString,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenedDirectoryEntryKind {
    Directory,
    Regular,
}

struct OpenedBudgetedDirectoryEntry {
    display_path: PathBuf,
    file_name: OsString,
    kind: OpenedDirectoryEntryKind,
}

fn charge_budgeted_directory_entry(
    path: &Path,
    file_name: &OsStr,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let fixed_bytes = size_of::<fs::DirEntry>()
        .checked_add(size_of::<PathBuf>())
        .and_then(|bytes| bytes.checked_add(size_of::<OsString>()))
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "generation directory entry",
        })?;
    let accounted_bytes = u64::try_from(fixed_bytes)
        .ok()
        .and_then(|bytes| bytes.checked_add(u64::try_from(path.as_os_str().len()).ok()?))
        .and_then(|bytes| bytes.checked_add(u64::try_from(file_name.len()).ok()?))
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "generation directory entry",
        })?;
    budget
        .check_entries(1)
        .and_then(|()| budget.check_bytes(accounted_bytes))
        .map_err(GenerationStoreError::Budget)?;
    budget
        .consume_entries(1)
        .and_then(|()| budget.consume_bytes(accounted_bytes))
        .map_err(GenerationStoreError::Budget)
}

fn visit_directory_entries_budgeted(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    mut visitor: impl FnMut(
        BudgetedDirectoryEntry,
        &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError>,
) -> Result<(), GenerationStoreError> {
    let entries = fs::read_dir(directory).map_err(|source| {
        GenerationStoreError::io("read generation directory", directory.to_path_buf(), source)
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            GenerationStoreError::io(
                "read generation directory entry",
                directory.to_path_buf(),
                source,
            )
        })?;
        let path = entry.path();
        let file_name = entry.file_name();
        charge_budgeted_directory_entry(&path, &file_name, budget)?;
        visitor(BudgetedDirectoryEntry { path, file_name }, budget)?;
    }
    Ok(())
}

fn visit_opened_directory_entries_budgeted(
    display_directory: &Path,
    opened_directory: &SecureReadDirectory,
    budget: &mut AssetLoadBudget,
    mut visitor: impl FnMut(
        OpenedBudgetedDirectoryEntry,
        &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError>,
) -> Result<(), GenerationStoreError> {
    let identity = opened_directory.stable_identity().map_err(|source| {
        persisted_read_error(
            "capture persisted directory identity",
            display_directory.to_path_buf(),
            source,
        )
    })?;
    let entries = opened_directory.entries().map_err(|source| {
        persisted_read_error(
            "enumerate persisted directory",
            display_directory.to_path_buf(),
            source,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            persisted_read_error(
                "read persisted directory entry",
                display_directory.to_path_buf(),
                source,
            )
        })?;
        let hint = entry.kind();
        let file_name = entry.into_name();
        let display_path = display_directory.join(&file_name);
        charge_budgeted_directory_entry(&display_path, &file_name, budget)?;
        let kind = match open_anchored_artifact_entry(
            opened_directory,
            &file_name,
            hint,
            &display_path,
        )? {
            OpenedArtifactEntry::Directory(_) => OpenedDirectoryEntryKind::Directory,
            OpenedArtifactEntry::Regular(_) => OpenedDirectoryEntryKind::Regular,
        };
        visitor(
            OpenedBudgetedDirectoryEntry {
                display_path,
                file_name,
                kind,
            },
            budget,
        )?;
    }
    revalidate_opened_directory_snapshot(
        display_directory,
        opened_directory,
        identity,
        "revalidate persisted directory after enumeration",
    )
}

fn revalidate_opened_directory_snapshot(
    display_directory: &Path,
    opened_directory: &SecureReadDirectory,
    identity: StableDirectoryIdentity,
    operation: &'static str,
) -> Result<(), GenerationStoreError> {
    opened_directory
        .ensure_identity(identity)
        .map_err(|source| {
            persisted_read_error(operation, display_directory.to_path_buf(), source)
        })?;
    let rebound = SecureReadDirectory::open(display_directory, OpenPolicy::PersistedState)
        .map_err(|source| {
            persisted_read_error(
                "reopen persisted directory snapshot path",
                display_directory.to_path_buf(),
                source,
            )
        })?;
    rebound.ensure_identity(identity).map_err(|source| {
        persisted_read_error(
            "revalidate persisted directory snapshot path binding",
            display_directory.to_path_buf(),
            source,
        )
    })
}

fn recover_owned_staging(
    staging: &Path,
    generations: &Path,
    allow_obsolete_retirement: bool,
    budget: &mut AssetLoadBudget,
    #[cfg(test)] failpoint: Option<GenerationFailpoint>,
) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
    let mut changed = false;
    let mut removed_entries = 0_u64;
    visit_directory_entries_budgeted(staging, budget, |entry, budget| {
        let metadata = metadata_no_follow(&entry.path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if parse_staging_directory_name(name).is_some() {
            if !metadata.is_dir() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            #[cfg(test)]
            inject_failure(failpoint, GenerationFailpoint::StartupStagingCleanup)?;
            remove_tree_no_follow(&entry.path, budget)?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
            return Ok(());
        }
        if parse_quarantine_directory_name(name).is_some() {
            if !metadata.is_dir() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            #[cfg(test)]
            inject_failure(failpoint, GenerationFailpoint::StartupStagingCleanup)?;
            remove_tree_no_follow(&entry.path, budget)?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
            return Ok(());
        }
        if parse_obsolete_activation_directory_name(name).is_some() {
            if !metadata.is_dir() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            if !allow_obsolete_retirement {
                return Err(
                    GenerationStoreError::ObsoleteRetirementConflictsWithActiveGeneration {
                        path: entry.path,
                    },
                );
            }
            #[cfg(test)]
            inject_failure(failpoint, GenerationFailpoint::StartupStagingCleanup)?;
            let generation_recovery =
                recover_unreferenced_generation_directories(generations, true, budget)?;
            remove_tree_no_follow(&entry.path, budget)?;
            changed = true;
            removed_entries = removed_entries
                .checked_add(generation_recovery.removed_entries)
                .and_then(|removed| removed.checked_add(1))
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "recovered staging entries",
                })?;
            return Ok(());
        }
        if parse_activation_staging_file_name(name).is_some() {
            if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            #[cfg(test)]
            inject_failure(failpoint, GenerationFailpoint::StartupStagingCleanup)?;
            fs::remove_file(&entry.path).map_err(|source| {
                GenerationStoreError::io(
                    "remove abandoned activation staging file",
                    entry.path,
                    source,
                )
            })?;
            changed = true;
            removed_entries =
                removed_entries
                    .checked_add(1)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "recovered staging entries",
                    })?;
        }
        Ok(())
    })?;
    if changed {
        sync_directory(staging)?;
    }
    Ok(GenerationStagingRecoveryReport { removed_entries })
}

fn retire_obsolete_activation_authority(
    root: &Path,
    activations: &Path,
    staging: &Path,
    required: IndexRebuildRequired,
) -> Result<(), GenerationStoreError> {
    let retired = staging.join(obsolete_activation_directory_name(
        required.activation_ordinal,
    ));
    match fs::symlink_metadata(&retired) {
        Ok(_) => return Err(GenerationStoreError::QuarantineCollision { path: retired }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GenerationStoreError::io(
                "inspect obsolete activation retirement path",
                retired,
                source,
            ));
        }
    }

    fs::rename(activations, &retired).map_err(|source| {
        GenerationStoreError::io(
            "retire obsolete activation authority",
            activations.to_path_buf(),
            source,
        )
    })?;
    sync_directory(root)?;
    sync_directory(staging)?;
    ensure_managed_directory(root, ACTIVATIONS_DIRECTORY)?;
    sync_directory(root)
}

fn recover_unreferenced_generation_directories(
    generations: &Path,
    retire_current_generations: bool,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationStagingRecoveryReport, GenerationStoreError> {
    let mut changed = false;
    let mut removed_entries = 0_u64;
    visit_directory_entries_budgeted(generations, budget, |entry, budget| {
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let current = SearchGenerationId::from_directory_name(name);
        let obsolete = is_obsolete_generation_directory_name(name);
        let remove = obsolete || retire_current_generations && current.is_some();
        if !remove {
            return Ok(());
        }
        let metadata = metadata_no_follow(&entry.path)?;
        if !metadata.is_dir() {
            return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
        }
        remove_tree_no_follow(&entry.path, budget)?;
        changed = true;
        removed_entries =
            removed_entries
                .checked_add(1)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "recovered generation entries",
                })?;
        Ok(())
    })?;
    if changed {
        sync_directory(generations)?;
    }
    Ok(GenerationStagingRecoveryReport { removed_entries })
}

fn next_staging_ordinal(
    staging: &Path,
    opened_staging: &SecureReadDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<u64, GenerationStoreError> {
    let mut maximum = 0_u64;
    visit_opened_directory_entries_budgeted(staging, opened_staging, budget, |entry, _budget| {
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if let Some(ordinal) = parse_staging_directory_name(name) {
            if entry.kind != OpenedDirectoryEntryKind::Directory {
                return Err(GenerationStoreError::UnsupportedFileType {
                    path: entry.display_path,
                });
            }
            maximum = maximum.max(ordinal);
        }
        Ok(())
    })?;
    maximum
        .checked_add(1)
        .ok_or(GenerationStoreError::OrdinalOverflow)
}

fn activation_candidates_for_open(
    activations: &Path,
    opened_activations: &SecureReadDirectory,
    staging: &Path,
    opened_staging: &SecureReadDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<ActivationDirectorySnapshot, GenerationStoreError> {
    let directory_identity = opened_activations.stable_identity().map_err(|source| {
        persisted_read_error(
            "capture activation directory snapshot identity",
            activations.to_path_buf(),
            source,
        )
    })?;
    let mut candidates = Vec::new();
    let mut maximum = 0_u64;
    visit_opened_directory_entries_budgeted(
        activations,
        opened_activations,
        budget,
        |entry, budget| {
            let Some(name) = entry.file_name.to_str() else {
                return Ok(());
            };
            let Some(ordinal) = parse_activation_file_name(name) else {
                return Ok(());
            };
            if entry.kind != OpenedDirectoryEntryKind::Regular {
                return Err(GenerationStoreError::UnsupportedFileType {
                    path: entry.display_path,
                });
            }
            maximum = maximum.max(ordinal);
            push_activation_candidate(
                &mut candidates,
                ActivationCandidate {
                    ordinal,
                    display_path: entry.display_path,
                    file_name: entry.file_name,
                },
                Some(budget),
            )
        },
    )?;
    visit_opened_directory_entries_budgeted(staging, opened_staging, budget, |entry, _budget| {
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        if let Some(ordinal) = parse_activation_staging_file_name(name) {
            if entry.kind != OpenedDirectoryEntryKind::Regular {
                return Err(GenerationStoreError::UnsupportedFileType {
                    path: entry.display_path,
                });
            }
            maximum = maximum.max(ordinal);
        }
        Ok(())
    })?;
    candidates.sort_unstable_by_key(|candidate| candidate.ordinal);
    let next = maximum
        .checked_add(1)
        .ok_or(GenerationStoreError::OrdinalOverflow)?;
    revalidate_opened_directory_snapshot(
        activations,
        opened_activations,
        directory_identity,
        "revalidate activation directory after candidate enumeration",
    )?;
    Ok(ActivationDirectorySnapshot {
        candidates,
        next_ordinal: next,
        directory_identity,
    })
}

fn push_activation_candidate(
    candidates: &mut Vec<ActivationCandidate>,
    candidate: ActivationCandidate,
    mut budget: Option<&mut AssetLoadBudget>,
) -> Result<(), GenerationStoreError> {
    if candidates.len() >= MAX_ACTIVATION_CANDIDATES {
        return Err(GenerationStoreError::ActivationCandidateLimitExceeded {
            maximum: MAX_ACTIVATION_CANDIDATES,
        });
    }
    if candidates.len() == candidates.capacity() {
        let additional = ACTIVATION_CANDIDATE_GROWTH.min(
            MAX_ACTIVATION_CANDIDATES
                .checked_sub(candidates.len())
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "activation candidate capacity",
                })?,
        );
        let requested_bytes = additional
            .checked_mul(size_of::<ActivationCandidate>())
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "activation candidate vector",
            })?;
        if let Some(budget) = budget.as_deref_mut() {
            budget
                .check_bytes(u64::try_from(requested_bytes).map_err(|_| {
                    GenerationStoreError::SizeOverflow {
                        resource: "activation candidate vector",
                    }
                })?)
                .map_err(GenerationStoreError::Budget)?;
        }
        let previous_capacity = candidates.capacity();
        candidates.try_reserve_exact(additional).map_err(|_| {
            GenerationStoreError::AllocationFailed {
                resource: "activation candidate vector",
                requested: requested_bytes,
            }
        })?;
        if let Some(budget) = budget {
            let added_capacity = candidates.capacity().checked_sub(previous_capacity).ok_or(
                GenerationStoreError::SizeOverflow {
                    resource: "activation candidate vector",
                },
            )?;
            let allocated_bytes = added_capacity
                .checked_mul(size_of::<ActivationCandidate>())
                .and_then(|bytes| u64::try_from(bytes).ok())
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "activation candidate vector",
                })?;
            budget
                .consume_bytes(allocated_bytes)
                .map_err(GenerationStoreError::Budget)?;
        }
    }
    candidates.push(candidate);
    Ok(())
}

fn prune_activation_history(
    activations: &Path,
    retained_ordinals: &BTreeSet<u64>,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut changed = false;
    visit_directory_entries_budgeted(activations, budget, |entry, _budget| {
        let path = entry.path;
        let metadata = metadata_no_follow(&path)?;
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let Some(ordinal) = parse_activation_file_name(name) else {
            return Ok(());
        };
        if retained_ordinals.contains(&ordinal) {
            return Ok(());
        }
        if !metadata.is_file() {
            return Err(GenerationStoreError::UnsupportedFileType { path });
        }
        fs::remove_file(&path).map_err(|source| {
            GenerationStoreError::io("prune generation activation record", path, source)
        })?;
        changed = true;
        Ok(())
    })?;
    if changed {
        sync_directory(activations)?;
    }
    Ok(())
}

fn select_active_generation(
    activations: &Path,
    generations: &Path,
    opened_generations: &SecureReadDirectory,
    opened_activations: &SecureReadDirectory,
    snapshot: &ActivationDirectorySnapshot,
    budget: &mut AssetLoadBudget,
) -> Result<Option<GenerationSnapshot>, GenerationStoreError> {
    let selected = if let Some(candidate) = snapshot.candidates.last() {
        let record = read_activation_record(
            opened_activations,
            &candidate.display_path,
            &candidate.file_name,
            candidate.ordinal,
            budget,
        );
        record.and_then(|record| {
            load_completed_generation(generations, opened_generations, record, budget).map(Some)
        })
    } else {
        Ok(None)
    };
    revalidate_opened_directory_snapshot(
        activations,
        opened_activations,
        snapshot.directory_identity,
        "revalidate activation directory after selecting the active generation",
    )?;
    selected
}

fn read_activation_record(
    directory: &SecureReadDirectory,
    path: &Path,
    file_name: &OsStr,
    expected_ordinal: u64,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationHeadRecord, GenerationStoreError> {
    let mut file = open_contract_file(
        directory,
        file_name,
        path,
        MAX_ACTIVATION_BYTES_U64,
        "activation record",
    )?;
    let decoded = read_contract_json::<GenerationHeadRecordWire>(
        file.file_mut(),
        budget,
        ACTIVATION_JSON_LIMITS,
    );
    file.ensure_unchanged().map_err(|source| {
        persisted_read_error("revalidate activation record", path.to_path_buf(), source)
    })?;
    let wire = decoded.map_err(|source| GenerationStoreError::ContractJson {
        artifact: "activation record",
        path: path.to_path_buf(),
        source,
    })?;
    let record = decode_generation_head_record(wire, path)?;
    if record.ordinal != expected_ordinal {
        return Err(GenerationStoreError::ActivationOrdinalMismatch {
            path: path.to_path_buf(),
            expected: expected_ordinal,
            actual: record.ordinal,
        });
    }
    record
        .transaction_receipts
        .validate_for_workspace(record.workspace)
        .map_err(|source| GenerationStoreError::ActivationProvenance {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    Ok(record)
}

fn decode_generation_head_record(
    wire: GenerationHeadRecordWire,
    path: &Path,
) -> Result<GenerationHeadRecord, GenerationStoreError> {
    let GenerationHeadRecordWire {
        contract_version,
        ordinal,
        generation,
        generation_storage_contract,
        manifest_digest,
        workspace,
        revision,
        desired_revision,
        parent_generation,
        transaction_receipts,
    } = wire;
    let invalid = |message| GenerationStoreError::InvalidGenerationHead {
        path: path.to_path_buf(),
        message,
    };

    let (desired_revision, parent_generation, transaction_receipts) = match contract_version {
        LEGACY_ACTIVATION_CONTRACT_VERSION | REVISIONED_ACTIVATION_CONTRACT_VERSION => {
            return Err(GenerationStoreError::IndexRebuildRequired(
                IndexRebuildRequired {
                    reason: IndexRebuildReason::ObsoleteActivationContract {
                        actual: contract_version,
                    },
                    activation_ordinal: ordinal,
                    generation,
                },
            ));
        }
        GENERATION_HEAD_CONTRACT_VERSION => {
            let generation_storage_contract = require_wire_value(
                generation_storage_contract,
                &invalid,
                "activation v3 is missing its generation storage contract",
                "activation v3 generation storage contract must not be null",
            )?;
            if generation_storage_contract == 1 {
                return Err(GenerationStoreError::IndexRebuildRequired(
                    IndexRebuildRequired {
                        reason: IndexRebuildReason::ObsoleteGenerationStorage {
                            actual: generation_storage_contract,
                        },
                        activation_ordinal: ordinal,
                        generation,
                    },
                ));
            }
            if generation_storage_contract != SEARCH_GENERATION_STORAGE_CONTRACT_VERSION {
                return Err(GenerationStoreError::UnsupportedVersion {
                    artifact: "generation storage",
                    actual: generation_storage_contract,
                    expected: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
                });
            }
            let desired_revision = require_wire_value(
                desired_revision,
                &invalid,
                "activation v3 is missing its desired revision",
                "activation v3 desired revision must not be null",
            )?;
            let parent_generation = optional_non_null_wire_value(
                parent_generation,
                &invalid,
                "activation v3 parent generation must be omitted instead of null",
            )?;
            let transaction_receipts = require_wire_value(
                transaction_receipts,
                &invalid,
                "activation v3 is missing its transaction receipts",
                "activation v3 transaction receipts must not be null",
            )?;
            (
                Some(desired_revision),
                parent_generation,
                transaction_receipts,
            )
        }
        actual => {
            return Err(GenerationStoreError::UnsupportedVersion {
                artifact: "generation head",
                actual,
                expected: GENERATION_HEAD_CONTRACT_VERSION,
            });
        }
    };

    Ok(GenerationHeadRecord {
        contract_version,
        ordinal,
        generation,
        generation_storage_contract: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
        manifest_digest,
        workspace,
        revision,
        desired_revision,
        parent_generation,
        transaction_receipts,
    })
}

fn require_wire_value<T>(
    presence: WirePresence<Option<T>>,
    invalid: &impl Fn(&'static str) -> GenerationStoreError,
    missing_message: &'static str,
    null_message: &'static str,
) -> Result<T, GenerationStoreError> {
    match presence {
        WirePresence::Missing => Err(invalid(missing_message)),
        WirePresence::Present(None) => Err(invalid(null_message)),
        WirePresence::Present(Some(value)) => Ok(value),
    }
}

fn optional_non_null_wire_value<T>(
    presence: WirePresence<Option<T>>,
    invalid: &impl Fn(&'static str) -> GenerationStoreError,
    null_message: &'static str,
) -> Result<Option<T>, GenerationStoreError> {
    match presence {
        WirePresence::Missing => Ok(None),
        WirePresence::Present(None) => Err(invalid(null_message)),
        WirePresence::Present(Some(value)) => Ok(Some(value)),
    }
}

fn load_completed_generation(
    generations: &Path,
    opened_generations: &SecureReadDirectory,
    record: GenerationHeadRecord,
    budget: &mut AssetLoadBudget,
) -> Result<GenerationSnapshot, GenerationStoreError> {
    let directory_name = record.generation.directory_name();
    let directory = generations.join(&directory_name);
    let opened_directory = opened_generations
        .open_directory(OsStr::new(&directory_name))
        .map_err(|source| {
            persisted_read_error(
                "open completed generation directory",
                directory.clone(),
                source,
            )
        })?;
    let completed =
        inspect_completed_generation_in(&directory, &opened_directory, record.generation, budget)?;
    if completed.manifest_digest != record.manifest_digest {
        return Err(GenerationStoreError::ManifestDigestMismatch {
            generation: record.generation,
            expected: record.manifest_digest,
            actual: completed.manifest_digest,
        });
    }
    if completed.manifest.workspace() != record.workspace
        || completed.manifest.revision() != record.revision
    {
        return Err(GenerationStoreError::ActivationContextMismatch {
            generation: record.generation,
        });
    }
    let desired_revision = record.desired_revision();
    let activation =
        GenerationActivationEvidence::new(record.parent_generation, record.transaction_receipts);
    Ok(GenerationSnapshot {
        activation_ordinal: record.ordinal,
        generation: record.generation,
        manifest_digest: completed.manifest_digest,
        desired_revision,
        manifest: completed.manifest,
        activation,
        directory,
    })
}

fn inspect_completed_generation(
    directory: &Path,
    expected_generation: SearchGenerationId,
    budget: &mut AssetLoadBudget,
) -> Result<CompletedGeneration, GenerationStoreError> {
    let opened_directory = SecureReadDirectory::open(directory, OpenPolicy::PersistedState)
        .map_err(|source| {
            persisted_read_error(
                "open completed generation directory",
                directory.to_path_buf(),
                source,
            )
        })?;
    inspect_completed_generation_in(directory, &opened_directory, expected_generation, budget)
}

fn inspect_completed_generation_in(
    directory: &Path,
    opened_directory: &SecureReadDirectory,
    expected_generation: SearchGenerationId,
    budget: &mut AssetLoadBudget,
) -> Result<CompletedGeneration, GenerationStoreError> {
    let (manifest, manifest_digest) = read_generation_manifest_in::<SearchGenerationManifestV1>(
        opened_directory,
        directory,
        budget,
    )?;
    let actual_generation = manifest.generation_id();
    if actual_generation != expected_generation {
        return Err(GenerationStoreError::ManifestGenerationMismatch {
            expected: expected_generation,
            actual: actual_generation,
        });
    }
    let actual_artifacts =
        measure_generation_artifacts_in(directory, opened_directory, budget, None)?;
    if actual_artifacts != manifest.artifacts() {
        return Err(GenerationStoreError::ArtifactEvidenceMismatch {
            expected: Box::new(manifest.artifacts()),
            actual: Box::new(actual_artifacts),
        });
    }
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state = opened_directory
        .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open completed source-state directory",
                source_state_directory.clone(),
                source,
            )
        })?;
    validate_persisted_source_state_in(
        directory,
        &source_state_directory,
        &opened_source_state,
        &manifest,
        budget,
    )?;
    Ok(CompletedGeneration {
        manifest,
        manifest_digest,
    })
}

fn read_generation_manifest_in<T>(
    opened_directory: &SecureReadDirectory,
    directory: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(T, DigestV1), GenerationStoreError>
where
    T: DeserializeOwned,
{
    let manifest_path = directory.join(MANIFEST_FILE);
    let mut manifest_file = open_contract_file(
        opened_directory,
        OsStr::new(MANIFEST_FILE),
        &manifest_path,
        MAX_MANIFEST_BYTES_U64,
        "generation manifest",
    )?;
    let manifest_length = manifest_file.length();
    let mut reader = DigestingReader::new(manifest_file.file_mut(), manifest_length);
    let decoded = read_contract_json::<T>(&mut reader, budget, MANIFEST_JSON_LIMITS);
    let manifest_digest = reader.finalize();
    manifest_file.ensure_unchanged().map_err(|source| {
        persisted_read_error(
            "revalidate generation manifest",
            manifest_path.clone(),
            source,
        )
    })?;
    let manifest = decoded.map_err(|source| GenerationStoreError::ContractJson {
        artifact: "generation manifest",
        path: manifest_path.clone(),
        source,
    })?;
    let manifest_digest = manifest_digest.map_err(|source| {
        GenerationStoreError::io(
            "digest generation manifest",
            manifest_path,
            io::Error::other(source),
        )
    })?;
    Ok((manifest, manifest_digest))
}

fn measure_generation_artifacts(
    directory: &Path,
    budget: &mut AssetLoadBudget,
    failpoint: Option<GenerationFailpoint>,
) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
    let opened =
        SecureReadDirectory::open(directory, OpenPolicy::PersistedState).map_err(|source| {
            persisted_read_error(
                "open generation artifact root",
                directory.to_path_buf(),
                source,
            )
        })?;
    measure_generation_artifacts_in(directory, &opened, budget, failpoint)
}

fn measure_generation_artifacts_in(
    directory: &Path,
    opened_directory: &SecureReadDirectory,
    budget: &mut AssetLoadBudget,
    failpoint: Option<GenerationFailpoint>,
) -> Result<GenerationArtifactEvidence, GenerationStoreError> {
    inject_failure(failpoint, GenerationFailpoint::Search)?;
    let search_directory = directory.join(SEARCH_ARTIFACT_DIRECTORY);
    let opened_search = opened_directory
        .open_directory(OsStr::new(SEARCH_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open search artifact directory",
                search_directory.clone(),
                source,
            )
        })?;
    let search = measure_anchored_artifact_tree(
        &directory.join(SEARCH_ARTIFACT_DIRECTORY),
        opened_search,
        budget,
    )?;
    inject_failure(failpoint, GenerationFailpoint::References)?;
    let reference_directory = directory.join(REFERENCE_ARTIFACT_DIRECTORY);
    let opened_references = opened_directory
        .open_directory(OsStr::new(REFERENCE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open reference artifact directory",
                reference_directory.clone(),
                source,
            )
        })?;
    let references = measure_anchored_artifact_tree(
        &directory.join(REFERENCE_ARTIFACT_DIRECTORY),
        opened_references,
        budget,
    )?;
    inject_failure(failpoint, GenerationFailpoint::SourceState)?;
    let source_state_directory = directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY);
    let opened_source_state = opened_directory
        .open_directory(OsStr::new(SOURCE_STATE_ARTIFACT_DIRECTORY))
        .map_err(|source| {
            persisted_read_error(
                "open source-state artifact directory",
                source_state_directory.clone(),
                source,
            )
        })?;
    let source_state = measure_anchored_artifact_tree(
        &directory.join(SOURCE_STATE_ARTIFACT_DIRECTORY),
        opened_source_state,
        budget,
    )?;
    Ok(GenerationArtifactEvidence::new(
        search,
        references,
        source_state,
    ))
}

pub(crate) fn measure_artifact_tree(
    root: &Path,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    let mut budget = AssetLoadBudget::default();
    measure_artifact_tree_with_budget(root, &mut budget)
}

fn measure_artifact_tree_with_budget(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    ensure_existing_directory_no_follow(root)?;
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "artifact directory traversal stack",
        budget,
    )?;
    pending.push((root.to_path_buf(), 0_u32));
    let mut entries = Vec::new();
    let mut total_bytes = 0_u64;
    let mut directory_count = 0_u64;

    while let Some((directory, depth)) = pending.pop() {
        directory_count =
            directory_count
                .checked_add(1)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "artifact tree directories",
                })?;
        if directory_count > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "artifact tree directories",
                actual: directory_count,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(depth)
            .map_err(GenerationStoreError::Budget)?;
        let iterator = fs::read_dir(&directory).map_err(|source| {
            GenerationStoreError::io("read artifact tree directory", directory.clone(), source)
        })?;
        for entry in iterator {
            budget
                .check_entries(1)
                .and_then(|()| budget.check_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let entry = entry.map_err(|source| {
                GenerationStoreError::io(
                    "read artifact tree directory entry",
                    directory.clone(),
                    source,
                )
            })?;
            budget
                .consume_entries(1)
                .and_then(|()| budget.consume_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let path = entry.path();
            let metadata = metadata_no_follow(&path)?;
            if metadata.is_dir() {
                let next_depth =
                    depth
                        .checked_add(1)
                        .ok_or(GenerationStoreError::SizeOverflow {
                            resource: "artifact tree depth",
                        })?;
                budget
                    .check_depth(next_depth)
                    .map_err(GenerationStoreError::Budget)?;
                reserve_artifact_vec(
                    &mut pending,
                    1,
                    "artifact directory traversal stack",
                    budget,
                )?;
                pending.push((path, next_depth));
                continue;
            }
            if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path });
            }

            let bytes = metadata.len();
            total_bytes =
                total_bytes
                    .checked_add(bytes)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "artifact tree bytes",
                    })?;
            if total_bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
                return Err(GenerationStoreError::PersistedArtifactTooLarge {
                    artifact: "artifact tree bytes",
                    actual: total_bytes,
                    maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
                });
            }
            budget
                .check_bytes(bytes)
                .and_then(|()| budget.consume_bytes(bytes))
                .map_err(GenerationStoreError::Budget)?;
            let mut file = File::open(&path).map_err(|source| {
                GenerationStoreError::io("open artifact file", path.clone(), source)
            })?;
            let digest = DigestV1::hash_reader(&mut file, bytes).map_err(|source| {
                GenerationStoreError::io("hash artifact file", path.clone(), source)
            })?;
            let relative_path_bytes = portable_relative_path_byte_len(root, &path)?;
            let relative_path_bytes = u64::try_from(relative_path_bytes).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "artifact relative path bytes",
                }
            })?;
            let artifact_entry_bytes = u64::try_from(size_of::<ArtifactEntry>()).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "artifact entry allocation",
                }
            })?;
            budget
                .check_bytes(relative_path_bytes)
                .and_then(|()| budget.check_bytes(artifact_entry_bytes))
                .map_err(GenerationStoreError::Budget)?;
            reserve_artifact_vec(&mut entries, 1, "artifact tree entries", budget)?;
            budget
                .consume_bytes(relative_path_bytes)
                .and_then(|()| budget.consume_bytes(artifact_entry_bytes))
                .map_err(GenerationStoreError::Budget)?;
            let relative_path = portable_relative_path(root, &path, relative_path_bytes)?;
            entries.push(ArtifactEntry {
                relative_path,
                bytes,
                digest,
            });
        }
    }

    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let encoded_length = artifact_tree_encoded_len(&entries)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let encoded = encode_artifact_tree(&entries, encoded_length)?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let file_count =
        u64::try_from(entries.len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact file count",
        })?;
    if file_count > MAX_PERSISTED_ARTIFACT_TREE_FILES {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: "artifact tree files",
            actual: file_count,
            maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
        });
    }
    Ok(ArtifactTreeEvidence::new(
        DigestV1::hash_bytes(&encoded),
        file_count,
        total_bytes,
    ))
}

struct AnchoredArtifactDirectory {
    listing_path: PathBuf,
    relative_path: String,
    depth: u32,
    identity: StableDirectoryIdentity,
}

enum OpenedArtifactEntry {
    Directory(SecureReadDirectory),
    Regular(SecureRegularFile),
}

fn open_anchored_artifact_entry(
    directory: &SecureReadDirectory,
    name: &OsStr,
    hint: EntryKindHint,
    display_path: &Path,
) -> Result<OpenedArtifactEntry, GenerationStoreError> {
    if hint == EntryKindHint::LinkOrReparse {
        return Err(persisted_link_error(display_path.to_path_buf()));
    }
    if hint == EntryKindHint::Directory || hint == EntryKindHint::Unknown {
        match directory.open_directory(name) {
            Ok(opened) => return Ok(OpenedArtifactEntry::Directory(opened)),
            Err(SecureReadError::NotDirectory) if hint == EntryKindHint::Unknown => {}
            Err(source) => {
                return Err(persisted_read_error(
                    "open anchored artifact directory",
                    display_path.to_path_buf(),
                    source,
                ));
            }
        }
    }

    directory
        .open_regular(name)
        .map(OpenedArtifactEntry::Regular)
        .map_err(|source| {
            persisted_read_error(
                "open anchored artifact file",
                display_path.to_path_buf(),
                source,
            )
        })
}

/// Measures a completed artifact through already-opened directory handles.
///
/// Handle-relative enumeration supplies names and untrusted type hints only. Each name is then
/// re-opened relative to the retained descriptor before hashing or recursion occurs, so a
/// replacement of any pathname cannot redirect evidence collection outside the selected tree.
fn measure_anchored_artifact_tree(
    root: &Path,
    opened_root: SecureReadDirectory,
    budget: &mut AssetLoadBudget,
) -> Result<ArtifactTreeEvidence, GenerationStoreError> {
    let root_identity = opened_root.stable_identity().map_err(|source| {
        persisted_read_error(
            "capture anchored artifact root identity",
            root.to_path_buf(),
            source,
        )
    })?;
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "anchored artifact directory traversal stack",
        budget,
    )?;
    pending.push(AnchoredArtifactDirectory {
        listing_path: root.to_path_buf(),
        relative_path: String::new(),
        depth: 0,
        identity: root_identity,
    });
    let mut entries = Vec::new();
    let mut directories = 0_u64;
    let mut total_bytes = 0_u64;

    while let Some(current) = pending.pop() {
        let reopened = if current.relative_path.is_empty() {
            None
        } else {
            Some(
                opened_root
                    .open_directory(Path::new(&current.relative_path))
                    .map_err(|source| {
                        persisted_read_error(
                            "reopen anchored artifact directory",
                            current.listing_path.clone(),
                            source,
                        )
                    })?,
            )
        };
        let directory = reopened.as_ref().unwrap_or(&opened_root);
        directory
            .ensure_identity(current.identity)
            .map_err(|source| {
                persisted_read_error(
                    "revalidate anchored artifact directory before enumeration",
                    current.listing_path.clone(),
                    source,
                )
            })?;
        directories = directories
            .checked_add(1)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "anchored artifact tree directories",
            })?;
        if directories > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "anchored artifact tree directories",
                actual: directories,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(current.depth)
            .map_err(GenerationStoreError::Budget)?;

        let directory_entries = directory.entries().map_err(|source| {
            persisted_read_error(
                "enumerate anchored artifact names",
                current.listing_path.clone(),
                source,
            )
        })?;
        for listing_entry in directory_entries {
            budget
                .check_entries(1)
                .and_then(|()| budget.check_members(1))
                .map_err(GenerationStoreError::Budget)?;
            let listing_entry = listing_entry.map_err(|source| {
                persisted_read_error(
                    "read anchored artifact name",
                    current.listing_path.clone(),
                    source,
                )
            })?;
            budget
                .consume_entries(1)
                .and_then(|()| budget.consume_members(1))
                .map_err(GenerationStoreError::Budget)?;

            let hint = listing_entry.kind();
            let name = listing_entry.into_name();
            let display_path = current.listing_path.join(&name);
            let relative_path =
                anchored_relative_path(&current.relative_path, &name, &display_path, budget)?;
            let opened = open_anchored_artifact_entry(directory, &name, hint, &display_path)?;

            let file =
                match opened {
                    OpenedArtifactEntry::Directory(child_directory) => {
                        let depth = current.depth.checked_add(1).ok_or(
                            GenerationStoreError::SizeOverflow {
                                resource: "anchored artifact tree depth",
                            },
                        )?;
                        budget
                            .check_depth(depth)
                            .map_err(GenerationStoreError::Budget)?;
                        reserve_artifact_vec(
                            &mut pending,
                            1,
                            "anchored artifact directory traversal stack",
                            budget,
                        )?;
                        let identity = child_directory.stable_identity().map_err(|source| {
                            persisted_read_error(
                                "capture anchored artifact child identity",
                                display_path.clone(),
                                source,
                            )
                        })?;
                        pending.push(AnchoredArtifactDirectory {
                            listing_path: display_path,
                            relative_path,
                            depth,
                            identity,
                        });
                        continue;
                    }
                    OpenedArtifactEntry::Regular(file) => file,
                };
            let bytes = file.length();
            total_bytes =
                total_bytes
                    .checked_add(bytes)
                    .ok_or(GenerationStoreError::SizeOverflow {
                        resource: "anchored artifact tree bytes",
                    })?;
            if total_bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
                return Err(GenerationStoreError::PersistedArtifactTooLarge {
                    artifact: "anchored artifact tree bytes",
                    actual: total_bytes,
                    maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
                });
            }
            budget
                .check_bytes(bytes)
                .and_then(|()| budget.consume_bytes(bytes))
                .map_err(GenerationStoreError::Budget)?;
            let digest = DigestV1::hash_reader(
                file.range(0, bytes).map_err(|source| {
                    persisted_read_error(
                        "open anchored artifact hash range",
                        display_path.clone(),
                        source,
                    )
                })?,
                bytes,
            )
            .map_err(|source| {
                GenerationStoreError::io(
                    "hash anchored artifact file",
                    display_path.clone(),
                    source,
                )
            })?;
            file.ensure_unchanged().map_err(|source| {
                persisted_read_error(
                    "revalidate anchored artifact file",
                    display_path.clone(),
                    source,
                )
            })?;

            let entry_bytes = u64::try_from(size_of::<ArtifactEntry>()).map_err(|_| {
                GenerationStoreError::SizeOverflow {
                    resource: "anchored artifact entry allocation",
                }
            })?;
            budget
                .check_bytes(entry_bytes)
                .map_err(GenerationStoreError::Budget)?;
            reserve_artifact_vec(&mut entries, 1, "anchored artifact tree entries", budget)?;
            budget
                .consume_bytes(entry_bytes)
                .map_err(GenerationStoreError::Budget)?;
            entries.push(ArtifactEntry {
                relative_path,
                bytes,
                digest,
            });
        }
        directory
            .ensure_identity(current.identity)
            .map_err(|source| {
                persisted_read_error(
                    "revalidate anchored artifact directory",
                    current.listing_path.clone(),
                    source,
                )
            })?;
    }

    entries.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let encoded_length = artifact_tree_encoded_len(&entries)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let encoded = encode_artifact_tree(&entries, encoded_length)?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let file_count =
        u64::try_from(entries.len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "anchored artifact file count",
        })?;
    if file_count > MAX_PERSISTED_ARTIFACT_TREE_FILES {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: "anchored artifact tree files",
            actual: file_count,
            maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
        });
    }
    Ok(ArtifactTreeEvidence::new(
        DigestV1::hash_bytes(&encoded),
        file_count,
        total_bytes,
    ))
}

fn anchored_relative_path(
    parent: &str,
    name: &OsStr,
    display_path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<String, GenerationStoreError> {
    let name = name
        .to_str()
        .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
            path: display_path.to_path_buf(),
        })?;
    let length = parent
        .len()
        .checked_add(usize::from(!parent.is_empty()))
        .and_then(|length| length.checked_add(name.len()))
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "anchored artifact relative path",
        })?;
    if length == 0 || length > MAX_ARTIFACT_RELATIVE_PATH_BYTES {
        return Err(GenerationStoreError::NonPortableArtifactPath {
            path: display_path.to_path_buf(),
        });
    }
    let bytes = u64::try_from(length).map_err(|_| GenerationStoreError::SizeOverflow {
        resource: "anchored artifact relative path bytes",
    })?;
    budget
        .check_bytes(bytes)
        .map_err(GenerationStoreError::Budget)?;
    let mut result = String::new();
    result
        .try_reserve_exact(length)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "anchored artifact relative path",
            requested: length,
        })?;
    budget
        .consume_bytes(bytes)
        .map_err(GenerationStoreError::Budget)?;
    result.push_str(parent);
    if !parent.is_empty() {
        result.push('/');
    }
    result.push_str(name);
    Ok(result)
}

#[derive(Debug)]
struct ArtifactEntry {
    relative_path: String,
    bytes: u64,
    digest: DigestV1,
}

fn reserve_artifact_vec<T>(
    values: &mut Vec<T>,
    additional: usize,
    resource: &'static str,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    if required <= values.capacity() {
        return Ok(());
    }
    let current = values.capacity();
    let next = current
        .max(1)
        .checked_mul(2)
        .map(|capacity| capacity.max(required))
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let additional_capacity = next
        .checked_sub(current)
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let requested_bytes = additional_capacity
        .checked_mul(size_of::<T>())
        .ok_or(GenerationStoreError::SizeOverflow { resource })?;
    let budget_bytes = u64::try_from(requested_bytes)
        .map_err(|_| GenerationStoreError::SizeOverflow { resource })?;
    budget
        .check_bytes(budget_bytes)
        .map_err(GenerationStoreError::Budget)?;
    values.try_reserve_exact(additional_capacity).map_err(|_| {
        GenerationStoreError::AllocationFailed {
            resource,
            requested: requested_bytes,
        }
    })?;
    budget
        .consume_bytes(budget_bytes)
        .map_err(GenerationStoreError::Budget)
}

fn portable_relative_path_byte_len(
    root: &Path,
    path: &Path,
) -> Result<usize, GenerationStoreError> {
    let relative =
        path.strip_prefix(root)
            .map_err(|_| GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            })?;
    let mut encoded_len = 0_usize;
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            });
        };
        let value =
            value
                .to_str()
                .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
                    path: path.to_path_buf(),
                })?;
        encoded_len = encoded_len
            .checked_add(usize::from(encoded_len != 0))
            .and_then(|length| length.checked_add(value.len()))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact relative path",
            })?;
    }
    if encoded_len == 0 || encoded_len > MAX_ARTIFACT_RELATIVE_PATH_BYTES {
        return Err(GenerationStoreError::NonPortableArtifactPath {
            path: path.to_path_buf(),
        });
    }
    Ok(encoded_len)
}

fn portable_relative_path(
    root: &Path,
    path: &Path,
    encoded_len: u64,
) -> Result<String, GenerationStoreError> {
    let capacity =
        usize::try_from(encoded_len).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact relative path bytes",
        })?;
    let relative =
        path.strip_prefix(root)
            .map_err(|_| GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            })?;
    let mut encoded = String::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "artifact relative path",
            requested: capacity,
        })?;
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(GenerationStoreError::ArtifactEscapedRoot {
                root: root.to_path_buf(),
                path: path.to_path_buf(),
            });
        };
        let value =
            value
                .to_str()
                .ok_or_else(|| GenerationStoreError::NonPortableArtifactPath {
                    path: path.to_path_buf(),
                })?;
        if !encoded.is_empty() {
            encoded.push('/');
        }
        encoded.push_str(value);
    }
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: "artifact relative path changed length",
        });
    }
    Ok(encoded)
}

fn artifact_tree_encoded_len(entries: &[ArtifactEntry]) -> Result<u64, GenerationStoreError> {
    let mut length =
        ARTIFACT_TREE_DOMAIN
            .len()
            .checked_add(8)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact tree evidence",
            })?;
    for entry in entries {
        length = length
            .checked_add(8)
            .and_then(|value| value.checked_add(entry.relative_path.len()))
            .and_then(|value| value.checked_add(8 + DigestV1::BYTE_LEN))
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "artifact tree evidence",
            })?;
    }
    u64::try_from(length).map_err(|_| GenerationStoreError::SizeOverflow {
        resource: "artifact tree evidence",
    })
}

fn encode_artifact_tree(
    entries: &[ArtifactEntry],
    encoded_length: u64,
) -> Result<Vec<u8>, GenerationStoreError> {
    let capacity =
        usize::try_from(encoded_length).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "artifact tree evidence",
        })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: "artifact tree evidence",
            requested: capacity,
        })?;
    encoded.extend_from_slice(ARTIFACT_TREE_DOMAIN);
    encoded.extend_from_slice(
        &u64::try_from(entries.len())
            .map_err(|_| GenerationStoreError::SizeOverflow {
                resource: "artifact tree entry count",
            })?
            .to_le_bytes(),
    );
    for entry in entries {
        encoded.extend_from_slice(
            &u64::try_from(entry.relative_path.len())
                .map_err(|_| GenerationStoreError::SizeOverflow {
                    resource: "artifact relative path",
                })?
                .to_le_bytes(),
        );
        encoded.extend_from_slice(entry.relative_path.as_bytes());
        encoded.extend_from_slice(&entry.bytes.to_le_bytes());
        encoded.extend_from_slice(entry.digest.as_bytes());
    }
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: "artifact tree encoded length changed",
        });
    }
    Ok(encoded)
}

fn completed_generation_sizes(
    generations: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(SearchGenerationId, u64)>, GenerationStoreError> {
    let mut sizes = Vec::new();
    visit_directory_entries_budgeted(generations, budget, |entry, budget| {
        let metadata = metadata_no_follow(&entry.path)?;
        if !metadata.is_dir() {
            return Ok(());
        }
        let Some(name) = entry.file_name.to_str() else {
            return Ok(());
        };
        let Some(generation) = SearchGenerationId::from_directory_name(name) else {
            return Ok(());
        };
        let bytes = tree_size_no_follow(&entry.path, budget)?;
        reserve_artifact_vec(&mut sizes, 1, "completed generation sizes", budget)?;
        sizes.push((generation, bytes));
        Ok(())
    })?;
    Ok(sizes)
}

fn generation_manifest_json_length(
    manifest: &SearchGenerationManifestV1,
    path: &Path,
) -> Result<u64, GenerationStoreError> {
    store_json_length(manifest, path, GENERATION_MANIFEST_JSON_PROFILE)
}

#[derive(Clone, Copy)]
struct StoreJsonProfile {
    artifact: &'static str,
    allocation_resource: &'static str,
    byte_resource: &'static str,
    changed_length_resource: &'static str,
    maximum_bytes: u64,
}

const GENERATION_MANIFEST_JSON_PROFILE: StoreJsonProfile = StoreJsonProfile {
    artifact: "generation manifest",
    allocation_resource: "generation manifest",
    byte_resource: "generation manifest bytes",
    changed_length_resource: "generation manifest encoded length changed",
    maximum_bytes: MAX_MANIFEST_BYTES_U64,
};

const GENERATION_HEAD_JSON_PROFILE: StoreJsonProfile = StoreJsonProfile {
    artifact: "generation head",
    allocation_resource: "generation head",
    byte_resource: "generation head bytes",
    changed_length_resource: "generation head encoded length changed",
    maximum_bytes: MAX_ACTIVATION_BYTES_U64,
};

fn store_json_length(
    value: &impl Serialize,
    path: &Path,
    profile: StoreJsonProfile,
) -> Result<u64, GenerationStoreError> {
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, value).map_err(|source| GenerationStoreError::Json {
        artifact: profile.artifact,
        path: path.to_path_buf(),
        source,
    })?;
    if counter.bytes > profile.maximum_bytes {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact: profile.artifact,
            actual: counter.bytes,
            maximum: profile.maximum_bytes,
        });
    }
    Ok(counter.bytes)
}

fn encode_generation_manifest_json(
    manifest: &SearchGenerationManifestV1,
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    encode_store_json(manifest, path, GENERATION_MANIFEST_JSON_PROFILE, budget)
}

fn encode_generation_head_json(
    record: &GenerationHeadRecord,
    path: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    encode_store_json(record, path, GENERATION_HEAD_JSON_PROFILE, budget)
}

fn encode_store_json(
    value: &impl Serialize,
    path: &Path,
    profile: StoreJsonProfile,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, GenerationStoreError> {
    let encoded_length = store_json_length(value, path, profile)?;
    budget
        .check_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    let capacity =
        usize::try_from(encoded_length).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: profile.byte_resource,
        })?;
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(capacity)
        .map_err(|_| GenerationStoreError::AllocationFailed {
            resource: profile.allocation_resource,
            requested: capacity,
        })?;
    budget
        .consume_bytes(encoded_length)
        .map_err(GenerationStoreError::Budget)?;
    serde_json::to_writer(&mut encoded, value).map_err(|source| GenerationStoreError::Json {
        artifact: profile.artifact,
        path: path.to_path_buf(),
        source,
    })?;
    if encoded.len() != capacity {
        return Err(GenerationStoreError::SizeOverflow {
            resource: profile.changed_length_resource,
        });
    }
    Ok(encoded)
}

fn tree_size_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<u64, GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if metadata.is_file() {
                meter.observe_file(metadata.len())?;
            } else {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            Ok(())
        })?;
    }
    Ok(meter.bytes)
}

fn sync_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut directories = Vec::new();
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if metadata.is_file() {
                meter.observe_file(metadata.len())?;
                sync_regular_file(&entry.path)?;
            } else {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            }
            Ok(())
        })?;
        reserve_artifact_vec(
            &mut directories,
            1,
            "generation directories awaiting sync",
            budget,
        )?;
        directories.push((directory, depth));
    }
    directories.sort_unstable_by_key(|(_, depth)| std::cmp::Reverse(*depth));
    for (directory, _) in directories {
        sync_directory(&directory)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn sync_regular_file(path: &Path) -> Result<(), GenerationStoreError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation artifact", path.to_path_buf(), source)
        })
}

#[cfg(windows)]
fn sync_regular_file(path: &Path) -> Result<(), GenerationStoreError> {
    // FlushFileBuffers requires a writable handle. Generated artifacts remain
    // writable until activation; a read-only staged file is rejected explicitly.
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation artifact", path.to_path_buf(), source)
        })
}

fn remove_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    validate_tree_no_follow(root, budget)?;
    // The standard-library implementation uses handle-relative deletion on supported Unix and
    // Windows targets, preventing a directory-to-link replacement from escaping the managed tree.
    fs::remove_dir_all(root).map_err(|source| {
        GenerationStoreError::io("remove managed generation tree", root.to_path_buf(), source)
    })
}

fn validate_tree_no_follow(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let mut pending = managed_tree_stack(root, budget)?;
    let mut meter = ManagedTreeMeter::default();
    while let Some((directory, depth)) = pending.pop() {
        meter.observe_directory(depth, budget)?;
        ensure_existing_directory_no_follow(&directory)?;
        visit_directory_entries_budgeted(&directory, budget, |entry, budget| {
            let metadata = metadata_no_follow(&entry.path)?;
            if metadata.is_dir() {
                push_managed_tree_directory(&mut pending, entry.path, depth, budget)?;
            } else if !metadata.is_file() {
                return Err(GenerationStoreError::UnsupportedFileType { path: entry.path });
            } else {
                meter.observe_file(metadata.len())?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[derive(Default)]
struct ManagedTreeMeter {
    directories: u64,
    files: u64,
    bytes: u64,
}

impl ManagedTreeMeter {
    fn observe_directory(
        &mut self,
        depth: u32,
        budget: &mut AssetLoadBudget,
    ) -> Result<(), GenerationStoreError> {
        self.directories =
            self.directories
                .checked_add(1)
                .ok_or(GenerationStoreError::SizeOverflow {
                    resource: "generation tree directories",
                })?;
        if self.directories > MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree directories",
                actual: self.directories,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_DIRECTORIES,
            });
        }
        budget
            .observe_depth(depth)
            .map_err(GenerationStoreError::Budget)
    }

    fn observe_file(&mut self, bytes: u64) -> Result<(), GenerationStoreError> {
        self.files = self
            .files
            .checked_add(1)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation tree files",
            })?;
        if self.files > MAX_PERSISTED_ARTIFACT_TREE_FILES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree files",
                actual: self.files,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_FILES,
            });
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(GenerationStoreError::SizeOverflow {
                resource: "generation tree bytes",
            })?;
        if self.bytes > MAX_PERSISTED_ARTIFACT_TREE_BYTES {
            return Err(GenerationStoreError::PersistedArtifactTooLarge {
                artifact: "generation tree bytes",
                actual: self.bytes,
                maximum: MAX_PERSISTED_ARTIFACT_TREE_BYTES,
            });
        }
        Ok(())
    }
}

fn managed_tree_stack(
    root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<(PathBuf, u32)>, GenerationStoreError> {
    ensure_existing_directory_no_follow(root)?;
    let root_bytes =
        u64::try_from(root.as_os_str().len()).map_err(|_| GenerationStoreError::SizeOverflow {
            resource: "generation tree root path",
        })?;
    budget
        .check_bytes(root_bytes)
        .map_err(GenerationStoreError::Budget)?;
    let mut pending = Vec::new();
    reserve_artifact_vec(
        &mut pending,
        1,
        "generation directory traversal stack",
        budget,
    )?;
    budget
        .consume_bytes(root_bytes)
        .map_err(GenerationStoreError::Budget)?;
    pending.push((root.to_path_buf(), 0));
    Ok(pending)
}

fn push_managed_tree_directory(
    pending: &mut Vec<(PathBuf, u32)>,
    path: PathBuf,
    parent_depth: u32,
    budget: &mut AssetLoadBudget,
) -> Result<(), GenerationStoreError> {
    let depth = parent_depth
        .checked_add(1)
        .ok_or(GenerationStoreError::SizeOverflow {
            resource: "generation tree depth",
        })?;
    budget
        .check_depth(depth)
        .map_err(GenerationStoreError::Budget)?;
    reserve_artifact_vec(pending, 1, "generation directory traversal stack", budget)?;
    pending.push((path, depth));
    Ok(())
}

fn metadata_no_follow(path: &Path) -> Result<fs::Metadata, GenerationStoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        GenerationStoreError::io("inspect managed entry", path.to_path_buf(), source)
    })?;
    reject_link_or_reparse(path, &metadata)?;
    Ok(metadata)
}

fn reject_link_or_reparse(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), GenerationStoreError> {
    // Windows reports junctions as symlinks too. Native reparse classification takes priority so
    // every Windows filesystem indirection has one stable error contract.
    if metadata_is_reparse_point(metadata) {
        return Err(GenerationStoreError::ReparsePoint {
            path: path.to_path_buf(),
        });
    }
    if metadata.file_type().is_symlink() {
        return Err(GenerationStoreError::Symlink {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn persisted_read_error(
    operation: &'static str,
    path: PathBuf,
    source: SecureReadError,
) -> GenerationStoreError {
    match source {
        SecureReadError::Io(source) => GenerationStoreError::io(operation, path, source),
        SecureReadError::UnsupportedPlatform => {
            GenerationStoreError::UnsupportedPlatform { operation, path }
        }
        SecureReadError::LinkOrReparse => persisted_link_error(path),
        SecureReadError::NotDirectory => GenerationStoreError::NotDirectory { path },
        SecureReadError::NotRegular => GenerationStoreError::UnsupportedFileType { path },
        SecureReadError::IdentityChanged => GenerationStoreError::PersistedIdentityChanged { path },
    }
}

#[cfg(windows)]
fn persisted_link_error(path: PathBuf) -> GenerationStoreError {
    GenerationStoreError::ReparsePoint { path }
}

#[cfg(not(windows))]
fn persisted_link_error(path: PathBuf) -> GenerationStoreError {
    GenerationStoreError::Symlink { path }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn path_exists_no_follow(path: &Path) -> Result<bool, GenerationStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            reject_link_or_reparse(path, &metadata)?;
            Ok(true)
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(GenerationStoreError::io(
            "inspect managed path",
            path.to_path_buf(),
            source,
        )),
    }
}

fn open_contract_file(
    directory: &SecureReadDirectory,
    file_name: &OsStr,
    path: &Path,
    maximum: u64,
    artifact: &'static str,
) -> Result<SecureRegularFile, GenerationStoreError> {
    let file = directory.open_regular(file_name).map_err(|source| {
        persisted_read_error("open persisted generation file", path.to_path_buf(), source)
    })?;
    let opened_length = file.length();
    if opened_length > maximum {
        return Err(GenerationStoreError::PersistedArtifactTooLarge {
            artifact,
            actual: opened_length,
            maximum,
        });
    }
    Ok(file)
}

struct DigestingReader<'file> {
    file: &'file mut File,
    digest: DigestV1Builder,
}

impl<'file> DigestingReader<'file> {
    fn new(file: &'file mut File, length: u64) -> Self {
        Self {
            file,
            digest: DigestV1Builder::new(length),
        }
    }

    fn finalize(self) -> Result<DigestV1, DigestBuildError> {
        self.digest.finalize()
    }
}

impl Read for DigestingReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let amount = self.file.read(output)?;
        self.digest
            .update(&output[..amount])
            .map_err(io::Error::other)?;
        Ok(amount)
    }
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), GenerationStoreError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| {
            GenerationStoreError::io(
                "create persisted generation file",
                path.to_path_buf(),
                source,
            )
        })?;
    file.write_all(bytes).map_err(|source| {
        GenerationStoreError::io(
            "write persisted generation file",
            path.to_path_buf(),
            source,
        )
    })?;
    file.sync_all().map_err(|source| {
        GenerationStoreError::io("sync persisted generation file", path.to_path_buf(), source)
    })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), GenerationStoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| {
            GenerationStoreError::io("sync generation directory", path.to_path_buf(), source)
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), GenerationStoreError> {
    // Rust's standard library has no portable API for opening and syncing directories.
    // File contents are synced explicitly; namespace durability follows the platform contract.
    Ok(())
}

fn inject_failure(
    configured: Option<GenerationFailpoint>,
    checkpoint: GenerationFailpoint,
) -> Result<(), GenerationStoreError> {
    if configured == Some(checkpoint) {
        return Err(GenerationStoreError::InjectedFailure { checkpoint });
    }
    Ok(())
}

fn checked_sum(
    values: impl IntoIterator<Item = u64>,
    resource: &'static str,
) -> Result<u64, GenerationStoreError> {
    values.into_iter().try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or(GenerationStoreError::SizeOverflow { resource })
    })
}

fn staging_directory_name(ordinal: u64) -> String {
    format!("build-{ordinal:020}")
}

fn parse_staging_directory_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "build-", "")
}

fn obsolete_activation_directory_name(ordinal: u64) -> String {
    format!("{OBSOLETE_ACTIVATION_DIRECTORY_PREFIX}{ordinal:020}")
}

fn parse_obsolete_activation_directory_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, OBSOLETE_ACTIVATION_DIRECTORY_PREFIX, "")
}

fn is_obsolete_generation_directory_name(value: &str) -> bool {
    value
        .strip_prefix(OBSOLETE_GENERATION_DIRECTORY_PREFIX)
        .is_some_and(|encoded| {
            encoded.len() == DigestV1::BYTE_LEN * 2
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn quarantine_directory_name(ordinal: u64, generation: SearchGenerationId) -> String {
    format!(
        "{QUARANTINE_DIRECTORY_PREFIX}{ordinal:020}-{}",
        generation.directory_name()
    )
}

fn parse_quarantine_directory_name(value: &str) -> Option<(u64, SearchGenerationId)> {
    let encoded = value.strip_prefix(QUARANTINE_DIRECTORY_PREFIX)?;
    let ordinal = encoded.get(..ACTIVATION_FILE_DIGITS)?;
    if encoded.as_bytes().get(ACTIVATION_FILE_DIGITS) != Some(&b'-')
        || !ordinal.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let generation = SearchGenerationId::from_directory_name(
        encoded.get(ACTIVATION_FILE_DIGITS.checked_add(1)?..)?,
    )?;
    Some((ordinal.parse().ok()?, generation))
}

fn activation_file_name(ordinal: u64) -> String {
    format!("{ordinal:020}.json")
}

fn parse_activation_file_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "", ".json")
}

fn activation_staging_file_name(ordinal: u64) -> String {
    format!("activation-{ordinal:020}.json")
}

fn parse_activation_staging_file_name(value: &str) -> Option<u64> {
    parse_ordinal_component(value, "activation-", ".json")
}

fn parse_ordinal_component(value: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let digits = value.strip_prefix(prefix)?.strip_suffix(suffix)?;
    if digits.len() != ACTIVATION_FILE_DIGITS || !digits.bytes().all(|value| value.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

#[derive(Debug)]
pub(crate) enum GenerationStoreError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    PrivateIndexRoot {
        operation: &'static str,
        path: PathBuf,
        source: PrivateRootsError,
    },
    UnsupportedPlatform {
        operation: &'static str,
        path: PathBuf,
    },
    WriterLeaseUnavailable {
        path: PathBuf,
        source: io::Error,
    },
    Json {
        artifact: &'static str,
        path: PathBuf,
        source: serde_json::Error,
    },
    ContractJson {
        artifact: &'static str,
        path: PathBuf,
        source: BudgetedJsonError,
    },
    Budget(BudgetError),
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    ActivationCandidateLimitExceeded {
        maximum: usize,
    },
    Symlink {
        path: PathBuf,
    },
    ReparsePoint {
        path: PathBuf,
    },
    NotDirectory {
        path: PathBuf,
    },
    UnsupportedFileType {
        path: PathBuf,
    },
    PersistedIdentityChanged {
        path: PathBuf,
    },
    NonPortableArtifactPath {
        path: PathBuf,
    },
    ArtifactEscapedRoot {
        root: PathBuf,
        path: PathBuf,
    },
    PersistedArtifactTooLarge {
        artifact: &'static str,
        actual: u64,
        maximum: u64,
    },
    UnsupportedVersion {
        artifact: &'static str,
        actual: u16,
        expected: u16,
    },
    IndexRebuildRequired(IndexRebuildRequired),
    ActivationOrdinalMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    ActivationContextMismatch {
        generation: SearchGenerationId,
    },
    InvalidGenerationHead {
        path: PathBuf,
        message: &'static str,
    },
    ManifestDigestMismatch {
        generation: SearchGenerationId,
        expected: DigestV1,
        actual: DigestV1,
    },
    ManifestGenerationMismatch {
        expected: SearchGenerationId,
        actual: SearchGenerationId,
    },
    ArtifactEvidenceMismatch {
        expected: Box<GenerationArtifactEvidence>,
        actual: Box<GenerationArtifactEvidence>,
    },
    SourceState {
        path: PathBuf,
        source: Box<SourceStateError>,
    },
    ActivationProvenance {
        path: PathBuf,
        source: Box<SourceStateError>,
    },
    WorkspaceMismatch {
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    ParentGenerationMismatch {
        expected: SearchGenerationId,
        actual: Option<SearchGenerationId>,
    },
    BuildAlreadyActive,
    ForeignBuild,
    ObsoleteRetirementConflictsWithActiveGeneration {
        path: PathBuf,
    },
    QuarantineCollision {
        path: PathBuf,
    },
    QuarantineRollbackFailed {
        primary: Box<GenerationStoreError>,
        rollback: Box<GenerationStoreError>,
    },
    ActivationPreCommitCleanupFailed {
        primary: Box<GenerationStoreError>,
        cleanup: Box<GenerationStoreError>,
    },
    OrdinalOverflow,
    SizeOverflow {
        resource: &'static str,
    },
    InjectedFailure {
        checkpoint: GenerationFailpoint,
    },
}

impl GenerationStoreError {
    fn io(operation: &'static str, path: PathBuf, source: io::Error) -> GenerationStoreError {
        Self::Io {
            operation,
            path,
            source,
        }
    }

    fn is_security_violation(&self) -> bool {
        matches!(
            self,
            Self::Symlink { .. }
                | Self::ReparsePoint { .. }
                | Self::NotDirectory { .. }
                | Self::UnsupportedFileType { .. }
                | Self::PersistedIdentityChanged { .. }
                | Self::PrivateIndexRoot { .. }
        )
    }

    fn is_candidate_scan_fatal(&self) -> bool {
        if self.is_security_violation()
            || matches!(
                self,
                Self::Budget(_)
                    | Self::AllocationFailed { .. }
                    | Self::ActivationCandidateLimitExceeded { .. }
                    | Self::UnsupportedPlatform { .. }
            )
        {
            return true;
        }
        matches!(
            self,
            Self::ContractJson { source, .. }
                if !matches!(
                    source,
                    BudgetedJsonError::Io(_)
                        | BudgetedJsonError::Json(_)
                        | BudgetedJsonError::EncodedLimitExceeded { .. }
                        | BudgetedJsonError::StructureLimitExceeded { .. }
                )
        )
    }

    fn is_repairable_completed_generation(&self) -> bool {
        matches!(
            self,
            Self::Json { .. }
                | Self::ContractJson {
                    source: BudgetedJsonError::Io(_)
                        | BudgetedJsonError::Json(_)
                        | BudgetedJsonError::EncodedLimitExceeded { .. }
                        | BudgetedJsonError::StructureLimitExceeded { .. },
                    ..
                }
                | Self::PersistedArtifactTooLarge { .. }
                | Self::ManifestGenerationMismatch { .. }
                | Self::ArtifactEvidenceMismatch { .. }
                | Self::SourceState { .. }
        ) || matches!(
            self,
            Self::Io { source, .. }
                if source.kind() == io::ErrorKind::NotFound
        )
    }

    pub(crate) fn is_retryable(&self) -> bool {
        match self {
            Self::Io { .. } | Self::WriterLeaseUnavailable { .. } => true,
            Self::QuarantineRollbackFailed { primary, rollback } => {
                primary.is_retryable() || rollback.is_retryable()
            }
            Self::ActivationPreCommitCleanupFailed { primary, cleanup } => {
                primary.is_retryable() || cleanup.is_retryable()
            }
            _ => false,
        }
    }
}

impl fmt::Display for GenerationStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} failed for {}: {source}",
                path.display()
            ),
            Self::PrivateIndexRoot {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} failed for {}: {source}",
                path.display()
            ),
            Self::UnsupportedPlatform { operation, path } => write!(
                formatter,
                "{operation} is unsupported on this platform for {}",
                path.display()
            ),
            Self::WriterLeaseUnavailable { path, source } => write!(
                formatter,
                "generation writer lease is unavailable at {}: {source}",
                path.display()
            ),
            Self::Json {
                artifact,
                path,
                source,
            } => write!(
                formatter,
                "invalid {artifact} at {}: {source}",
                path.display()
            ),
            Self::ContractJson {
                artifact,
                path,
                source,
            } => write!(
                formatter,
                "invalid {artifact} at {}: {source}",
                path.display()
            ),
            Self::Budget(source) => {
                write!(formatter, "generation store budget exhausted: {source}")
            }
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(
                formatter,
                "failed to reserve {requested} bytes for {resource}"
            ),
            Self::ActivationCandidateLimitExceeded { maximum } => write!(
                formatter,
                "generation store contains more than {maximum} activation candidates"
            ),
            Self::Symlink { path } => write!(
                formatter,
                "generation store refuses symbolic link {}",
                path.display()
            ),
            Self::ReparsePoint { path } => write!(
                formatter,
                "generation store refuses Windows reparse point {}",
                path.display()
            ),
            Self::NotDirectory { path } => {
                write!(formatter, "{} is not a directory", path.display())
            }
            Self::UnsupportedFileType { path } => write!(
                formatter,
                "generation store refuses unsupported file type {}",
                path.display()
            ),
            Self::PersistedIdentityChanged { path } => write!(
                formatter,
                "persisted generation file identity, link count, or length is unsafe at {}",
                path.display()
            ),
            Self::NonPortableArtifactPath { path } => {
                write!(
                    formatter,
                    "artifact path is not portable: {}",
                    path.display()
                )
            }
            Self::ArtifactEscapedRoot { root, path } => write!(
                formatter,
                "artifact {} escaped generation root {}",
                path.display(),
                root.display()
            ),
            Self::PersistedArtifactTooLarge {
                artifact,
                actual,
                maximum,
            } => write!(
                formatter,
                "{artifact} contains {actual} bytes; maximum is {maximum}"
            ),
            Self::UnsupportedVersion {
                artifact,
                actual,
                expected,
            } => write!(
                formatter,
                "{artifact} version {actual} is unsupported; expected {expected}"
            ),
            Self::IndexRebuildRequired(required) => match required.reason {
                IndexRebuildReason::ObsoleteActivationContract { actual } => write!(
                    formatter,
                    "activation {} uses obsolete contract version {actual}; generation {} must be rebuilt",
                    required.activation_ordinal, required.generation
                ),
                IndexRebuildReason::ObsoleteGenerationStorage { actual } => write!(
                    formatter,
                    "activation {} selects obsolete generation storage version {actual}; generation {} must be rebuilt",
                    required.activation_ordinal, required.generation
                ),
            },
            Self::ActivationOrdinalMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "activation {} declares ordinal {actual}; filename requires {expected}",
                path.display()
            ),
            Self::ActivationContextMismatch { generation } => write!(
                formatter,
                "activation context does not match generation {generation}"
            ),
            Self::InvalidGenerationHead { path, message } => write!(
                formatter,
                "invalid generation head at {}: {message}",
                path.display()
            ),
            Self::ManifestDigestMismatch {
                generation,
                expected,
                actual,
            } => write!(
                formatter,
                "generation {generation} manifest digest mismatch: expected {expected}, got {actual}"
            ),
            Self::ManifestGenerationMismatch { expected, actual } => write!(
                formatter,
                "manifest logical generation mismatch: expected {expected}, got {actual}"
            ),
            Self::ArtifactEvidenceMismatch { expected, actual } => write!(
                formatter,
                "generation artifact evidence mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::SourceState { path, source } => write!(
                formatter,
                "invalid generation source state at {}: {source}",
                path.display()
            ),
            Self::ActivationProvenance { path, source } => write!(
                formatter,
                "invalid generation activation provenance at {}: {source}",
                path.display()
            ),
            Self::WorkspaceMismatch { expected, actual } => write!(
                formatter,
                "generation workspace mismatch: expected {expected}, got {actual}"
            ),
            Self::ParentGenerationMismatch { expected, actual } => write!(
                formatter,
                "generation parent mismatch: expected {expected}, got {actual:?}"
            ),
            Self::BuildAlreadyActive => {
                formatter.write_str("generation store already has an armed staging build")
            }
            Self::ForeignBuild => formatter.write_str(
                "generation build does not belong to this store or has an invalid ordinal path",
            ),
            Self::ObsoleteRetirementConflictsWithActiveGeneration { path } => write!(
                formatter,
                "obsolete activation retirement at {} conflicts with an active generation",
                path.display()
            ),
            Self::QuarantineCollision { path } => write!(
                formatter,
                "generation quarantine path already exists: {}",
                path.display()
            ),
            Self::QuarantineRollbackFailed { primary, rollback } => write!(
                formatter,
                "generation publication failed ({primary}); restoring the quarantined generation also failed ({rollback})"
            ),
            Self::ActivationPreCommitCleanupFailed { primary, cleanup } => write!(
                formatter,
                "activation publication failed before commit ({primary}); activation staging cleanup also failed ({cleanup})"
            ),
            Self::OrdinalOverflow => formatter.write_str("generation ordinal overflow"),
            Self::SizeOverflow { resource } => write!(formatter, "{resource} size overflow"),
            Self::InjectedFailure { checkpoint } => {
                write!(formatter, "injected generation failure at {checkpoint:?}")
            }
        }
    }
}

impl Error for GenerationStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::PrivateIndexRoot { source, .. } => Some(source),
            Self::WriterLeaseUnavailable { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::ContractJson { source, .. } => Some(source),
            Self::Budget(source) => Some(source),
            Self::SourceState { source, .. } => Some(source.as_ref()),
            Self::ActivationProvenance { source, .. } => Some(source.as_ref()),
            Self::QuarantineRollbackFailed { primary, .. }
            | Self::ActivationPreCommitCleanupFailed { primary, .. } => Some(primary.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "generation_store/reference_generation_tests.rs"]
mod reference_generation_tests;

#[cfg(test)]
mod generation_store_tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    #[test]
    fn anchored_artifact_measurement_honors_the_exact_caller_budget() {
        let temporary = tempfile::TempDir::new().unwrap();
        let nested = temporary.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("artifact.bin"), b"anchored artifact").unwrap();

        let mut measured = AssetLoadBudget::default();
        let evidence = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap(),
            &mut measured,
        )
        .unwrap();
        let usage = measured.usage();
        assert!(usage.bytes > 0);

        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        let exact_evidence = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap(),
            &mut exact,
        )
        .unwrap();
        assert_eq!(exact_evidence, evidence);
        assert_eq!(exact.usage(), usage);

        let mut one_byte_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..exact_limits
        })
        .unwrap();
        let error = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap(),
            &mut one_byte_short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[test]
    fn anchored_artifact_measurement_keeps_wide_tree_handles_bounded() {
        let temporary = tempfile::TempDir::new().unwrap();
        for ordinal in 0..1_200 {
            fs::create_dir(temporary.path().join(format!("child-{ordinal:04}"))).unwrap();
        }

        let mut budget = AssetLoadBudget::default();
        let evidence = measure_anchored_artifact_tree(
            temporary.path(),
            SecureReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap(),
            &mut budget,
        )
        .unwrap();

        assert_eq!(evidence.files(), 0);
        assert_eq!(budget.usage().entries, 1_200);
    }

    #[test]
    fn activation_materialization_is_budgeted_before_typed_deserialization() {
        let temporary = tempfile::TempDir::new().unwrap();
        let path = temporary.path().join("activation.json");
        fs::write(&path, br#"{"contract_version":"invalid"}"#).unwrap();
        let directory =
            SecureReadDirectory::open(temporary.path(), OpenPolicy::PersistedState).unwrap();

        let mut measured_budget = AssetLoadBudget::default();
        let error = read_activation_record(
            &directory,
            &path,
            OsStr::new("activation.json"),
            0,
            &mut measured_budget,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                source: BudgetedJsonError::Json(_),
                ..
            }
        ));

        let usage = measured_budget.usage();
        let mut one_byte_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes.checked_sub(1).unwrap(),
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = read_activation_record(
            &directory,
            &path,
            OsStr::new("activation.json"),
            0,
            &mut one_byte_short,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            GenerationStoreError::ContractJson {
                source: BudgetedJsonError::Budget(BudgetError::Exceeded {
                    resource: "bytes",
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn activation_candidate_vector_has_a_hard_limit() {
        let mut candidates = Vec::new();
        for ordinal in 0..MAX_ACTIVATION_CANDIDATES {
            push_activation_candidate(
                &mut candidates,
                ActivationCandidate {
                    ordinal: u64::try_from(ordinal).unwrap(),
                    display_path: PathBuf::new(),
                    file_name: OsString::new(),
                },
                None,
            )
            .unwrap();
        }

        assert!(matches!(
            push_activation_candidate(
                &mut candidates,
                ActivationCandidate {
                    ordinal: u64::try_from(MAX_ACTIVATION_CANDIDATES).unwrap(),
                    display_path: PathBuf::new(),
                    file_name: OsString::new(),
                },
                None,
            ),
            Err(GenerationStoreError::ActivationCandidateLimitExceeded {
                maximum: MAX_ACTIVATION_CANDIDATES
            })
        ));
    }

    #[test]
    fn activation_snapshot_rejects_a_higher_head_or_candidate_replacement_before_selection() {
        for replace_existing in [false, true] {
            let temporary = tempfile::TempDir::new().unwrap();
            let activations = temporary.path().join("activations");
            let staging = temporary.path().join("staging");
            let generations = temporary.path().join("generations");
            fs::create_dir(&activations).unwrap();
            fs::create_dir(&staging).unwrap();
            fs::create_dir(&generations).unwrap();
            let first = activations.join(activation_file_name(1));
            if replace_existing {
                fs::write(&first, b"{}").unwrap();
            }
            let opened_activations =
                SecureReadDirectory::open(&activations, OpenPolicy::PersistedState).unwrap();
            let opened_staging =
                SecureReadDirectory::open(&staging, OpenPolicy::PersistedState).unwrap();
            let opened_generations =
                SecureReadDirectory::open(&generations, OpenPolicy::PersistedState).unwrap();
            let mut budget = AssetLoadBudget::default();
            let snapshot = activation_candidates_for_open(
                &activations,
                &opened_activations,
                &staging,
                &opened_staging,
                &mut budget,
            )
            .unwrap();

            if replace_existing {
                fs::remove_file(&first).unwrap();
                fs::write(&first, b"{\"ordinal\":1}").unwrap();
            } else {
                fs::write(activations.join(activation_file_name(2)), b"{}").unwrap();
            }

            let error = select_active_generation(
                &activations,
                &generations,
                &opened_generations,
                &opened_activations,
                &snapshot,
                &mut budget,
            )
            .unwrap_err();
            assert!(matches!(
                error,
                GenerationStoreError::PersistedIdentityChanged { path } if path == activations
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn activation_enumeration_rejects_display_path_rebinding() {
        let temporary = tempfile::TempDir::new().unwrap();
        let activations = temporary.path().join("activations");
        let anchored_activations = temporary.path().join("anchored-activations");
        let staging = temporary.path().join("staging");
        fs::create_dir(&activations).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(activations.join(activation_file_name(7)), b"{}").unwrap();
        let opened_activations =
            SecureReadDirectory::open(&activations, OpenPolicy::PersistedState).unwrap();
        let opened_staging =
            SecureReadDirectory::open(&staging, OpenPolicy::PersistedState).unwrap();
        fs::rename(&activations, &anchored_activations).unwrap();
        fs::create_dir(&activations).unwrap();

        let error = activation_candidates_for_open(
            &activations,
            &opened_activations,
            &staging,
            &opened_staging,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GenerationStoreError::PersistedIdentityChanged { path } if path == activations
        ));
    }

    #[cfg(unix)]
    #[test]
    fn activation_enumeration_rejects_unknown_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::TempDir::new().unwrap();
        let activations = temporary.path().join("activations");
        let staging = temporary.path().join("staging");
        fs::create_dir(&activations).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(temporary.path().join("outside.json"), b"{}").unwrap();
        symlink("../outside.json", activations.join("unknown-link")).unwrap();
        let opened_activations =
            SecureReadDirectory::open(&activations, OpenPolicy::PersistedState).unwrap();
        let opened_staging =
            SecureReadDirectory::open(&staging, OpenPolicy::PersistedState).unwrap();

        let error = activation_candidates_for_open(
            &activations,
            &opened_activations,
            &staging,
            &opened_staging,
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();

        assert!(matches!(error, GenerationStoreError::Symlink { .. }));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn private_root_is_revalidated_on_reopen_and_before_generation_publication() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::generation::{GenerationProjectionDigests, SearchGenerationIdentityV1};

        let temporary = tempfile::TempDir::new().unwrap();
        let root_path = temporary.path().join("index");
        let project_identity =
            unity_asset_search_local::ProjectIdentityV1::for_existing_root(temporary.path())
                .unwrap();
        let private_root =
            PrivateIndexRootV1::open_or_create_for_project_override(project_identity, &root_path)
                .unwrap();
        let opened = GenerationStore::open_private(
            private_root.clone(),
            GenerationStoreOptions::default(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        let (mut store, staging_recovery, startup_disposition) = opened.into_parts();
        staging_recovery.unwrap();
        assert_eq!(startup_disposition, GenerationStartupDisposition::Ready);
        let mut build = store.begin().unwrap();
        fs::write(build.search_directory().join("segments"), b"search").unwrap();
        fs::write(build.reference_directory().join("segments"), b"references").unwrap();

        let workspace = WorkspaceId::from_u128(0x7a).unwrap();
        let revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"revision"));
        let source_state =
            SourceStateSnapshot::new(workspace, revision, Vec::new(), Vec::new()).unwrap();
        build.write_source_state(&source_state).unwrap();
        let evidence = store.measure_artifacts(&build).unwrap();
        let identity = SearchGenerationIdentityV1::new(
            workspace,
            revision,
            GenerationProjectionDigests::new(
                DigestV1::hash_bytes(b"search"),
                DigestV1::hash_bytes(b"references"),
            ),
            Default::default(),
            DigestV1::hash_bytes(b"options"),
            source_state.logical_digest(),
        )
        .unwrap();
        let manifest = SearchGenerationManifestV1::new(identity, evidence);

        fs::set_permissions(&root_path, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            store.prepare_publish(&mut build, manifest),
            Err(GenerationStoreError::PrivateIndexRoot { .. })
        ));
        assert!(matches!(
            GenerationStore::open_private(
                private_root,
                GenerationStoreOptions::default(),
                &mut AssetLoadBudget::default(),
            ),
            Err(GenerationStoreError::PrivateIndexRoot { .. })
        ));
    }
}
