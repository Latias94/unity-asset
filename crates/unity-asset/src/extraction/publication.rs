use std::fs::File;
use std::path::Path;

use serde::{Deserialize, Serialize};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, BudgetedJsonError, DigestV1, WorkspaceId, WorkspaceRevision,
    vec_allocation_bytes,
};

use super::CheckedByteCounter;
use super::artifact::{
    EvidenceReadBudget, OutputArtifactError, OutputLayout, PreparedOutputPath, StagedOutput,
    StagedPublishError,
};
use super::contract::ExtractionPath;
use super::executor::{
    ExistingOutputPolicy, ExtractionExecutionError, ExtractionExecutionOptions,
    ExtractionFailurePolicy,
};
use super::json_contract::{large_contract_limits, read_json_bounded};
use super::manifest::{
    ExtractionArtifactStatus, ExtractionCanonicalError, ExtractionManifest,
    ExtractionManifestArtifact, ExtractionReport, canonical_digest, write_canonical_json,
};
use super::model::ExtractionPlan;

#[cfg(all(test, feature = "decode"))]
use std::cell::Cell;

pub(super) const PUBLICATION_JOURNAL_PATH: &str = ".unity-asset-extraction-publication.v1.json";
pub(super) const RECEIPT_SEGMENT_DIRECTORY: &str =
    ".unity-asset-extraction-publication.v1.receipts";

const PUBLICATION_JOURNAL_CONTRACT: &str = "unity_asset.extraction_publication";
const PUBLICATION_JOURNAL_VERSION: u8 = 3;
const RECEIPT_SEGMENT_CONTRACT: &str = "unity_asset.extraction_publication_receipt_segment";
const RECEIPT_SEGMENT_VERSION: u8 = 3;
const RECEIPTS_PER_SEGMENT_U32: u32 = 64;
const RECEIPTS_PER_SEGMENT: usize = RECEIPTS_PER_SEGMENT_U32 as usize;
const PUBLICATION_JOURNAL_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(PUBLICATION_JOURNAL_CONTRACT);
const RECEIPT_SEGMENT_JSON_LIMITS: unity_asset_core::ContractJsonLimits =
    large_contract_limits(RECEIPT_SEGMENT_CONTRACT);
const MAX_PUBLICATION_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn publication_workspace_id(
    output_root: &Path,
    budget: &mut AssetLoadBudget,
) -> Result<Option<WorkspaceId>, ExtractionExecutionError> {
    let Some(file) = OutputLayout::open_existing_at(output_root, PUBLICATION_JOURNAL_PATH)
        .map_err(ExtractionExecutionError::output_layout)?
    else {
        return Ok(None);
    };
    let journal_length = file
        .metadata()
        .map_err(
            |error| ExtractionExecutionError::PublicationJournalInvalid {
                message: error.to_string(),
            },
        )?
        .len();
    if journal_length > MAX_PUBLICATION_JOURNAL_BYTES {
        return Err(ExtractionExecutionError::PublicationJournalLimitExceeded {
            required: journal_length,
            limit: MAX_PUBLICATION_JOURNAL_BYTES,
        });
    }
    let wire = read_journal(file, budget)?;
    validate_envelope(&wire)?;
    Ok(Some(wire.workspace_id))
}

#[cfg(all(test, feature = "decode"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationCrashPoint {
    ReceiptPersisted(u32),
    ArtifactMoved(u32),
    SegmentMoved(u32),
    ManifestMoved,
    CommittedPersisted,
}

#[cfg(all(test, feature = "decode"))]
thread_local! {
    static PUBLICATION_CRASH_POINT: Cell<Option<PublicationCrashPoint>> = const { Cell::new(None) };
}

#[cfg(all(test, feature = "decode"))]
pub(super) struct PublicationCrashGuard;

#[cfg(all(test, feature = "decode"))]
impl Drop for PublicationCrashGuard {
    fn drop(&mut self) {
        PUBLICATION_CRASH_POINT.set(None);
    }
}

#[cfg(all(test, feature = "decode"))]
pub(super) fn crash_once(point: PublicationCrashPoint) -> PublicationCrashGuard {
    PUBLICATION_CRASH_POINT.set(Some(point));
    PublicationCrashGuard
}

#[cfg(all(test, feature = "decode"))]
fn crash_at(point: PublicationCrashPoint) -> bool {
    PUBLICATION_CRASH_POINT.with(|configured| {
        if configured.get() == Some(point) {
            configured.set(None);
            true
        } else {
            false
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEvidence {
    length: u64,
    digest: DigestV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct PublicationGeneration(DigestV1);

impl PublicationGeneration {
    fn random() -> Self {
        Self(DigestV1::from_bytes(rand::random()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum JournalStage {
    Publishing,
    PublishingReceipt {
        receipt: ExtractionManifestArtifact,
    },
    PublishingArtifact {
        receipt: ExtractionManifestArtifact,
        replace_existing: bool,
    },
    PublishingManifest {
        evidence: ManifestEvidence,
    },
    Committed {
        report_digest: DigestV1,
        manifest: Option<ManifestEvidence>,
    },
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum JournalStageRef<'value> {
    Publishing,
    PublishingReceipt {
        receipt: &'value ExtractionManifestArtifact,
    },
    PublishingArtifact {
        receipt: &'value ExtractionManifestArtifact,
        replace_existing: bool,
    },
    PublishingManifest {
        evidence: ManifestEvidence,
    },
    Committed {
        report_digest: DigestV1,
        manifest: Option<ManifestEvidence>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationJournalWire {
    contract: String,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    execution_digest: DigestV1,
    generation: PublicationGeneration,
    sealed_segments: u32,
    segment_chain: Option<DigestV1>,
    tail_receipts: Vec<ExtractionManifestArtifact>,
    stage: JournalStage,
}

#[derive(Serialize)]
struct PublicationJournalRef<'value> {
    contract: &'static str,
    version: u8,
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    execution_digest: DigestV1,
    generation: PublicationGeneration,
    sealed_segments: u32,
    segment_chain: Option<DigestV1>,
    tail_receipts: &'value [ExtractionManifestArtifact],
    stage: JournalStageRef<'value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptSegmentWire {
    contract: String,
    version: u8,
    generation: PublicationGeneration,
    segment_index: u32,
    first_ordinal: u32,
    previous: Option<DigestV1>,
    receipts: Vec<ExtractionManifestArtifact>,
}

#[derive(Serialize)]
struct ReceiptSegmentRef<'value> {
    contract: &'static str,
    version: u8,
    generation: PublicationGeneration,
    segment_index: u32,
    first_ordinal: u32,
    previous: Option<DigestV1>,
    receipts: &'value [ExtractionManifestArtifact],
}

#[derive(Debug, Clone, Copy)]
struct PublicationIdentity {
    workspace_id: WorkspaceId,
    revision: WorkspaceRevision,
    request_digest: DigestV1,
    plan_digest: DigestV1,
    execution_digest: DigestV1,
}

#[derive(Serialize)]
struct ExecutionBinding<'value> {
    existing_output: &'static str,
    failure: &'static str,
    artifact_output_limit: u64,
    manifest_path: Option<&'value str>,
    resume_manifest: Option<DigestV1>,
}

enum JournalWriteError {
    NotPublished(OutputArtifactError),
    Uncertain,
    Canonical(ExtractionCanonicalError),
    LimitExceeded { required: u64, limit: u64 },
    InvalidState { reason: &'static str },
}

pub(super) enum ArtifactPublication {
    Published,
    NotPublished,
}

#[derive(Clone, Copy)]
pub(super) struct PublicationParameters<'value> {
    options: ExtractionExecutionOptions,
    artifact_output_limit: u64,
    manifest_output_limit: u64,
    manifest_path: Option<&'value ExtractionPath>,
    resume: Option<&'value ExtractionManifest>,
}

impl<'value> PublicationParameters<'value> {
    pub(super) const fn new(
        options: ExtractionExecutionOptions,
        artifact_output_limit: u64,
        manifest_output_limit: u64,
        manifest_path: Option<&'value ExtractionPath>,
        resume: Option<&'value ExtractionManifest>,
    ) -> Self {
        Self {
            options,
            artifact_output_limit,
            manifest_output_limit,
            manifest_path,
            resume,
        }
    }
}

pub(super) struct ExtractionPublication<'layout, 'plan> {
    layout: &'layout OutputLayout,
    journal: &'layout PreparedOutputPath,
    segment_paths: &'layout [String],
    plan: &'plan ExtractionPlan,
    identity: PublicationIdentity,
    generation: PublicationGeneration,
    receipts: Vec<ExtractionManifestArtifact>,
    sealed_segments: u32,
    segment_chain: Option<DigestV1>,
    published_bytes: u64,
    evidence_read_budget: EvidenceReadBudget,
    output_limit: u64,
    manifest_output_limit: u64,
    journal_limit: u64,
    stopped: bool,
    stop_after_failure: bool,
    manifest_evidence: Option<ManifestEvidence>,
    committed_report_digest: Option<DigestV1>,
    committed: bool,
    manifest_output: Option<&'layout PreparedOutputPath>,
}

impl<'layout, 'plan> ExtractionPublication<'layout, 'plan> {
    pub(super) fn open(
        layout: &'layout OutputLayout,
        segment_paths: &'layout [String],
        plan: &'plan ExtractionPlan,
        parameters: PublicationParameters<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<Self>, ExtractionExecutionError> {
        validate_segment_path_count(plan.artifacts().len(), segment_paths.len())?;
        let identity = publication_identity(plan, parameters)?;
        let journal = layout
            .path(PUBLICATION_JOURNAL_PATH)
            .map_err(ExtractionExecutionError::output_layout)?;
        let journal_limit = parameters
            .options
            .limits()
            .max_report_bytes()
            .min(MAX_PUBLICATION_JOURNAL_BYTES);
        let Some(file) = journal
            .open_existing()
            .map_err(ExtractionExecutionError::output_layout)?
        else {
            return Ok(None);
        };

        let journal_length = file
            .metadata()
            .map_err(
                |error| ExtractionExecutionError::PublicationJournalInvalid {
                    message: error.to_string(),
                },
            )?
            .len();
        if journal_length > journal_limit {
            return Err(ExtractionExecutionError::PublicationJournalLimitExceeded {
                required: journal_length,
                limit: journal_limit,
            });
        }
        let wire = read_journal(file, budget)?;
        validate_envelope(&wire)?;
        if let Some(reason) = identity_conflict(&wire, identity) {
            if matches!(&wire.stage, JournalStage::Committed { .. }) {
                return Ok(None);
            }
            return Err(ExtractionExecutionError::PublicationJournalConflict { reason });
        }
        let sealed_segments = usize::try_from(wire.sealed_segments).map_err(|_| {
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment count exceeds the platform range",
            }
        })?;
        if (sealed_segments == 0) != wire.segment_chain.is_none() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment count and hash chain disagree",
            });
        }
        if sealed_segments > segment_paths.len() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication journal contains too many sealed receipt segments",
            });
        }
        if wire.tail_receipts.len() > RECEIPTS_PER_SEGMENT {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication journal tail exceeds the segment capacity",
            });
        }
        let sealed_receipts = sealed_segments.checked_mul(RECEIPTS_PER_SEGMENT).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication sealed receipt count overflowed",
            },
        )?;
        let completed_receipts = sealed_receipts
            .checked_add(wire.tail_receipts.len())
            .ok_or(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication receipt count overflowed",
            })?;
        if completed_receipts > plan.artifacts().len() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication journal contains too many artifact receipts",
            });
        }
        let generation = wire.generation;
        let segment_chain = wire.segment_chain;
        let tail_len = wire.tail_receipts.len();
        let stage = wire.stage;
        let receipts = load_receipts(
            ReceiptLoad {
                layout,
                segment_paths,
                generation,
                sealed_segments,
                expected_chain: segment_chain,
                capacity: plan.artifacts().len(),
                segment_limit: journal_limit,
            },
            wire.tail_receipts,
            budget,
        )?;
        let manifest_output = parameters
            .manifest_path
            .map(|path| layout.path(path.as_str()))
            .transpose()
            .map_err(ExtractionExecutionError::output_layout)?;
        let mut publication = Self {
            layout,
            journal,
            segment_paths,
            plan,
            identity,
            generation,
            receipts,
            sealed_segments: wire.sealed_segments,
            segment_chain,
            published_bytes: 0,
            evidence_read_budget: EvidenceReadBudget::new(
                parameters
                    .options
                    .limits()
                    .max_evidence_verification_bytes(),
            ),
            output_limit: parameters.artifact_output_limit,
            manifest_output_limit: parameters.manifest_output_limit,
            journal_limit,
            stopped: false,
            stop_after_failure: parameters.options.failure()
                == ExtractionFailurePolicy::StopInPlanOrder,
            manifest_evidence: None,
            committed_report_digest: None,
            committed: false,
            manifest_output,
        };
        publication.validate_receipt_prefix(parameters.options)?;
        if tail_len == RECEIPTS_PER_SEGMENT {
            publication.seal_full_tail()?;
        }
        publication.reconcile(stage)?;
        Ok(Some(publication))
    }

    pub(super) fn create(
        layout: &'layout OutputLayout,
        segment_paths: &'layout [String],
        plan: &'plan ExtractionPlan,
        parameters: PublicationParameters<'_>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, ExtractionExecutionError> {
        validate_segment_path_count(plan.artifacts().len(), segment_paths.len())?;
        let identity = publication_identity(plan, parameters)?;
        let journal = layout
            .path(PUBLICATION_JOURNAL_PATH)
            .map_err(ExtractionExecutionError::output_layout)?;
        let manifest_output = parameters
            .manifest_path
            .map(|path| layout.path(path.as_str()))
            .transpose()
            .map_err(ExtractionExecutionError::output_layout)?;
        let receipts = allocate_receipts(plan.artifacts().len(), budget)?;
        let publication = Self {
            layout,
            journal,
            segment_paths,
            plan,
            identity,
            generation: PublicationGeneration::random(),
            receipts,
            sealed_segments: 0,
            segment_chain: None,
            published_bytes: 0,
            evidence_read_budget: EvidenceReadBudget::new(
                parameters
                    .options
                    .limits()
                    .max_evidence_verification_bytes(),
            ),
            output_limit: parameters.artifact_output_limit,
            manifest_output_limit: parameters.manifest_output_limit,
            journal_limit: parameters
                .options
                .limits()
                .max_report_bytes()
                .min(MAX_PUBLICATION_JOURNAL_BYTES),
            stopped: false,
            stop_after_failure: parameters.options.failure()
                == ExtractionFailurePolicy::StopInPlanOrder,
            manifest_evidence: None,
            committed_report_digest: None,
            committed: false,
            manifest_output,
        };
        publication
            .write(JournalStageRef::Publishing)
            .map_err(map_initial_journal_write)?;
        Ok(publication)
    }

    pub(super) const fn remaining_output(&self) -> u64 {
        self.output_limit.saturating_sub(self.published_bytes)
    }

    pub(super) const fn stopped(&self) -> bool {
        self.stopped
    }

    pub(super) const fn completed_artifacts(&self) -> usize {
        self.receipts.len()
    }

    pub(super) const fn evidence_read_budget_mut(&mut self) -> &mut EvidenceReadBudget {
        &mut self.evidence_read_budget
    }

    pub(super) fn record(
        &mut self,
        receipt: ExtractionManifestArtifact,
    ) -> Result<(), ExtractionExecutionError> {
        self.validate_next(&receipt)?;
        if receipt.status() == ExtractionArtifactStatus::Written {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "written artifact receipts require the artifact publication transition",
            });
        }
        self.write(JournalStageRef::PublishingReceipt { receipt: &receipt })
            .map_err(map_safe_journal_write)?;
        #[cfg(all(test, feature = "decode"))]
        if crash_at(PublicationCrashPoint::ReceiptPersisted(receipt.ordinal())) {
            return Err(ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "receipt_commit",
            });
        }
        self.accept_recorded_receipt(receipt, false)
    }

    pub(super) fn publish(
        &mut self,
        receipt: ExtractionManifestArtifact,
        staged: StagedOutput,
        replace_existing: bool,
    ) -> Result<ArtifactPublication, ExtractionExecutionError> {
        self.validate_next(&receipt)?;
        if receipt.status() != ExtractionArtifactStatus::Written
            || receipt.length() != Some(staged.length())
            || receipt.digest() != Some(staged.digest())
        {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "pending artifact receipt does not describe its staged output",
            });
        }
        self.write(JournalStageRef::PublishingArtifact {
            receipt: &receipt,
            replace_existing,
        })
        .map_err(map_safe_journal_write)?;

        match staged.publish(replace_existing) {
            Ok(()) => {
                #[cfg(all(test, feature = "decode"))]
                if crash_at(PublicationCrashPoint::ArtifactMoved(receipt.ordinal())) {
                    return Err(ExtractionExecutionError::PublicationRecoveryRequired {
                        stage: "artifact_publication",
                    });
                }
                self.published_bytes = self
                    .published_bytes
                    .checked_add(receipt.length().expect("written receipt has a length"))
                    .ok_or(ExtractionExecutionError::OutputLengthOverflow)?;
                self.commit_receipt(receipt)
                    .map_err(map_artifact_completion_error)?;
                Ok(ArtifactPublication::Published)
            }
            Err(StagedPublishError::NotPublished(_)) => {
                self.write(JournalStageRef::Publishing).map_err(|_| {
                    ExtractionExecutionError::PublicationRecoveryRequired {
                        stage: "artifact_not_published",
                    }
                })?;
                Ok(ArtifactPublication::NotPublished)
            }
            Err(StagedPublishError::Uncertain) => {
                Err(ExtractionExecutionError::PublicationRecoveryRequired {
                    stage: "artifact_publication",
                })
            }
        }
    }

    fn commit_receipt(
        &mut self,
        receipt: ExtractionManifestArtifact,
    ) -> Result<(), ExtractionExecutionError> {
        self.validate_next(&receipt)?;
        self.receipts.push(receipt);
        self.write(JournalStageRef::Publishing)
            .map_err(map_recovery_journal_write)?;
        if self.tail_receipt_count()? == RECEIPTS_PER_SEGMENT {
            self.seal_full_tail()?;
        }
        Ok(())
    }

    fn accept_recorded_receipt(
        &mut self,
        receipt: ExtractionManifestArtifact,
        verify_physical_evidence: bool,
    ) -> Result<(), ExtractionExecutionError> {
        if receipt.status() == ExtractionArtifactStatus::Written {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "written artifact receipt bypassed the artifact publication transition",
            });
        }
        if verify_physical_evidence {
            validate_physical_receipt(self.layout, &receipt, &mut self.evidence_read_budget)?;
        }
        let failed = receipt.status() == ExtractionArtifactStatus::Failed;
        self.commit_receipt(receipt)?;
        if failed && self.stop_after_failure {
            self.stopped = true;
        }
        Ok(())
    }

    fn seal_full_tail(&mut self) -> Result<(), ExtractionExecutionError> {
        if self.tail_receipt_count()? != RECEIPTS_PER_SEGMENT {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication attempted to seal an incomplete receipt segment",
            });
        }
        let segment_index = self.sealed_segments;
        let path = self
            .segment_paths
            .get(usize::try_from(segment_index).map_err(|_| {
                ExtractionExecutionError::PublicationJournalConflict {
                    reason: "publication segment index exceeds the platform range",
                }
            })?)
            .ok_or(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment exceeds the planned segment paths",
            })?;
        let first_ordinal = segment_index.checked_mul(RECEIPTS_PER_SEGMENT_U32).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment ordinal overflowed",
            },
        )?;
        let output = self
            .layout
            .path(path)
            .map_err(ExtractionExecutionError::output_layout)?;
        let segment_digest = {
            let segment = ReceiptSegmentRef {
                contract: RECEIPT_SEGMENT_CONTRACT,
                version: RECEIPT_SEGMENT_VERSION,
                generation: self.generation,
                segment_index,
                first_ordinal,
                previous: self.segment_chain,
                receipts: self.tail_receipts()?,
            };
            let digest = canonical_digest(&segment)?;
            write_atomic_json(output, &segment, self.journal_limit)
                .map_err(map_recovery_journal_write)?;
            digest
        };
        #[cfg(all(test, feature = "decode"))]
        if crash_at(PublicationCrashPoint::SegmentMoved(segment_index)) {
            return Err(ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "receipt_segment_seal",
            });
        }
        self.sealed_segments = self.sealed_segments.checked_add(1).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment count overflowed",
            },
        )?;
        self.segment_chain = Some(segment_digest);
        self.write(JournalStageRef::Publishing)
            .map_err(map_recovery_journal_write)
    }

    fn tail_receipt_count(&self) -> Result<usize, ExtractionExecutionError> {
        Ok(self.tail_receipts()?.len())
    }

    fn tail_receipts(&self) -> Result<&[ExtractionManifestArtifact], ExtractionExecutionError> {
        let sealed = usize::try_from(self.sealed_segments).map_err(|_| {
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment count exceeds the platform range",
            }
        })?;
        let start = sealed.checked_mul(RECEIPTS_PER_SEGMENT).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication sealed receipt count overflowed",
            },
        )?;
        self.receipts
            .get(start..)
            .ok_or(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication sealed receipt count exceeds its receipt prefix",
            })
    }

    pub(super) fn finish(self) -> Result<FinalPublication<'layout>, ExtractionExecutionError> {
        let manifest = ExtractionManifest::new(self.plan, self.receipts)?;
        let report = ExtractionReport::new(manifest)?;
        if let Some(actual) = self.manifest_evidence {
            let expected = manifest_evidence(&report)?;
            if actual != expected {
                return Err(ExtractionExecutionError::PublicationJournalConflict {
                    reason: "persisted manifest intent does not match the recovered report",
                });
            }
        }
        if let Some(actual) = self.committed_report_digest {
            let expected = report.digest()?;
            if actual != expected {
                return Err(ExtractionExecutionError::PublicationJournalConflict {
                    reason: "committed journal digest does not match the recovered report",
                });
            }
        }
        Ok(FinalPublication {
            layout: self.layout,
            journal: self.journal,
            identity: self.identity,
            generation: self.generation,
            journal_limit: self.journal_limit,
            report,
            evidence_read_budget: self.evidence_read_budget,
            sealed_segments: self.sealed_segments,
            segment_chain: self.segment_chain,
            manifest_evidence: self.manifest_evidence,
            committed: self.committed,
            manifest_output: self.manifest_output,
        })
    }

    fn validate_receipt_prefix(
        &mut self,
        options: ExtractionExecutionOptions,
    ) -> Result<(), ExtractionExecutionError> {
        if self.receipts.len() > self.plan.artifacts().len() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication journal contains too many artifact receipts",
            });
        }
        let mut remaining_written = self.output_limit;
        let mut evidence_read_budget = self.evidence_read_budget;
        for (index, receipt) in self.receipts.iter().enumerate() {
            self.validate_at(index, receipt)?;
            if receipt.status() == ExtractionArtifactStatus::Written {
                let length = receipt.length().ok_or(
                    ExtractionExecutionError::PublicationJournalConflict {
                        reason: "written artifact receipt has no length",
                    },
                )?;
                remaining_written = remaining_written.checked_sub(length).ok_or(
                    ExtractionExecutionError::PublicationJournalConflict {
                        reason: "recovered artifact bytes exceed the bound execution limit",
                    },
                )?;
            }
            validate_physical_receipt(self.layout, receipt, &mut evidence_read_budget)?;
        }
        self.published_bytes = self.output_limit.saturating_sub(remaining_written);
        self.evidence_read_budget = evidence_read_budget;
        self.stopped = options.failure() == ExtractionFailurePolicy::StopInPlanOrder
            && self
                .receipts
                .iter()
                .any(|receipt| receipt.status() == ExtractionArtifactStatus::Failed);
        Ok(())
    }

    fn reconcile(&mut self, stage: JournalStage) -> Result<(), ExtractionExecutionError> {
        match stage {
            JournalStage::Publishing => Ok(()),
            JournalStage::PublishingReceipt { receipt } => {
                self.validate_next(&receipt)?;
                self.accept_recorded_receipt(receipt, true)
            }
            JournalStage::PublishingArtifact {
                receipt,
                replace_existing,
            } => {
                self.validate_next(&receipt)?;
                if receipt.status() != ExtractionArtifactStatus::Written {
                    return Err(ExtractionExecutionError::PublicationJournalConflict {
                        reason: "pending artifact receipt is not a written output",
                    });
                }
                let pending_length = receipt.length().expect("written receipt has a length");
                if pending_length > self.remaining_output() {
                    return Err(ExtractionExecutionError::PublicationJournalConflict {
                        reason: "pending artifact evidence exceeds the bound execution limit",
                    });
                }
                match inspect_evidence_bounded(
                    self.layout,
                    receipt.path(),
                    ManifestEvidence {
                        length: pending_length,
                        digest: receipt.digest().expect("written receipt has a digest"),
                    },
                    &mut self.evidence_read_budget,
                )? {
                    EvidenceState::Missing => self
                        .write(JournalStageRef::Publishing)
                        .map_err(map_recovery_journal_write),
                    EvidenceState::Matching => {
                        self.published_bytes = self
                            .published_bytes
                            .checked_add(receipt.length().expect("written receipt has a length"))
                            .filter(|bytes| *bytes <= self.output_limit)
                            .ok_or(ExtractionExecutionError::PublicationJournalConflict {
                                reason: "recovered artifact bytes exceed the bound execution limit",
                            })?;
                        self.commit_receipt(receipt)
                    }
                    EvidenceState::Different if replace_existing => self
                        .write(JournalStageRef::Publishing)
                        .map_err(map_recovery_journal_write),
                    EvidenceState::Different => {
                        Err(ExtractionExecutionError::PublicationJournalConflict {
                            reason: "pending artifact target contains different bytes",
                        })
                    }
                }
            }
            JournalStage::PublishingManifest { evidence } => {
                self.require_complete_receipts()?;
                let Some(output) = self.manifest_output else {
                    return Err(ExtractionExecutionError::PublicationJournalConflict {
                        reason: "publication journal expects a manifest but this execution does not",
                    });
                };
                if evidence.length > self.manifest_output_limit {
                    return Err(ExtractionExecutionError::PublicationJournalConflict {
                        reason: "pending manifest evidence exceeds the bound manifest limit",
                    });
                }
                match inspect_prepared_evidence_bounded(
                    output,
                    evidence,
                    &mut self.evidence_read_budget,
                )? {
                    EvidenceState::Missing => self
                        .write(JournalStageRef::Publishing)
                        .map_err(map_recovery_journal_write),
                    EvidenceState::Matching => {
                        self.manifest_evidence = Some(evidence);
                        Ok(())
                    }
                    EvidenceState::Different => self
                        .write(JournalStageRef::Publishing)
                        .map_err(map_recovery_journal_write),
                }
            }
            JournalStage::Committed {
                report_digest,
                manifest,
            } => {
                self.require_complete_receipts()?;
                match (self.manifest_output, manifest) {
                    (None, None) => {}
                    (Some(output), Some(evidence)) => {
                        if evidence.length > self.manifest_output_limit {
                            return Err(ExtractionExecutionError::PublicationJournalConflict {
                                reason: "committed manifest evidence exceeds the bound manifest limit",
                            });
                        }
                        if inspect_prepared_evidence_bounded(
                            output,
                            evidence,
                            &mut self.evidence_read_budget,
                        )? != EvidenceState::Matching
                        {
                            return Err(ExtractionExecutionError::PublicationJournalConflict {
                                reason: "committed manifest is missing or contains different bytes",
                            });
                        }
                        self.manifest_evidence = Some(evidence);
                    }
                    _ => {
                        return Err(ExtractionExecutionError::PublicationJournalConflict {
                            reason: "committed journal manifest intent does not match this execution",
                        });
                    }
                }
                self.committed_report_digest = Some(report_digest);
                self.committed = true;
                Ok(())
            }
        }
    }

    fn require_complete_receipts(&self) -> Result<(), ExtractionExecutionError> {
        if self.receipts.len() != self.plan.artifacts().len() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "manifest publication began before every artifact reached a terminal state",
            });
        }
        Ok(())
    }

    fn validate_next(
        &self,
        receipt: &ExtractionManifestArtifact,
    ) -> Result<(), ExtractionExecutionError> {
        self.validate_at(self.receipts.len(), receipt)
    }

    fn validate_at(
        &self,
        index: usize,
        receipt: &ExtractionManifestArtifact,
    ) -> Result<(), ExtractionExecutionError> {
        let Some(planned) = self.plan.artifacts().get(index) else {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "artifact receipt exceeds the extraction plan",
            });
        };
        let expected = u32::try_from(index).map_err(|_| {
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "artifact receipt ordinal exceeds the wire range",
            }
        })?;
        if receipt.ordinal() != expected
            || receipt.ordinal() != planned.ordinal()
            || receipt.address() != planned.address()
            || !planned.matches_output(receipt.kind(), receipt.path())
        {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "artifact receipt does not match the extraction plan",
            });
        }
        Ok(())
    }

    fn write(&self, stage: JournalStageRef<'_>) -> Result<(), JournalWriteError> {
        let tail_receipts = self
            .tail_receipts()
            .map_err(|_| JournalWriteError::InvalidState {
                reason: "publication receipt tail is inconsistent with sealed segments",
            })?;
        write_journal(
            self.journal,
            self.identity,
            self.generation,
            self.sealed_segments,
            self.segment_chain,
            tail_receipts,
            stage,
            self.journal_limit,
        )
    }
}

pub(super) struct FinalPublication<'layout> {
    layout: &'layout OutputLayout,
    journal: &'layout PreparedOutputPath,
    identity: PublicationIdentity,
    generation: PublicationGeneration,
    journal_limit: u64,
    report: ExtractionReport,
    evidence_read_budget: EvidenceReadBudget,
    sealed_segments: u32,
    segment_chain: Option<DigestV1>,
    manifest_evidence: Option<ManifestEvidence>,
    committed: bool,
    manifest_output: Option<&'layout PreparedOutputPath>,
}

impl<'layout> FinalPublication<'layout> {
    pub(super) const fn report(&self) -> &ExtractionReport {
        &self.report
    }

    pub(super) const fn needs_manifest_publication(&self) -> bool {
        self.manifest_output.is_some() && self.manifest_evidence.is_none()
    }

    fn validate_commit_snapshot(&mut self) -> Result<(), ExtractionExecutionError> {
        for receipt in self.report.manifest().artifacts() {
            validate_physical_receipt(self.layout, receipt, &mut self.evidence_read_budget)?;
        }
        if let (Some(output), Some(evidence)) = (self.manifest_output, self.manifest_evidence)
            && inspect_prepared_evidence_bounded(output, evidence, &mut self.evidence_read_budget)?
                != EvidenceState::Matching
        {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "published manifest is missing or contains different bytes",
            });
        }
        Ok(())
    }

    pub(super) fn publish_manifest(
        &mut self,
        staged: StagedOutput,
    ) -> Result<(), ExtractionExecutionError> {
        if self.manifest_output.is_none() || self.manifest_evidence.is_some() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "manifest publication was not expected in the current journal state",
            });
        }
        let evidence = ManifestEvidence {
            length: staged.length(),
            digest: staged.digest(),
        };
        if evidence != manifest_evidence(&self.report)? {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "staged manifest does not match the finalized extraction report",
            });
        }
        self.write(JournalStageRef::PublishingManifest { evidence })
            .map_err(map_recovery_journal_write)?;
        match staged.publish(true) {
            Ok(()) => {
                #[cfg(all(test, feature = "decode"))]
                if crash_at(PublicationCrashPoint::ManifestMoved) {
                    return Err(ExtractionExecutionError::PublicationRecoveryRequired {
                        stage: "manifest_publication",
                    });
                }
                self.manifest_evidence = Some(evidence);
                Ok(())
            }
            Err(StagedPublishError::NotPublished(_)) => {
                self.write(JournalStageRef::Publishing)
                    .map_err(map_recovery_journal_write)?;
                Err(ExtractionExecutionError::PublicationRecoveryRequired {
                    stage: "manifest_not_published",
                })
            }
            Err(StagedPublishError::Uncertain) => {
                Err(ExtractionExecutionError::PublicationRecoveryRequired {
                    stage: "manifest_publication",
                })
            }
        }
    }

    pub(super) fn commit(mut self) -> Result<ExtractionReport, ExtractionExecutionError> {
        if self.manifest_output.is_some() && self.manifest_evidence.is_none() {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication cannot commit before its manifest",
            });
        }
        self.validate_commit_snapshot()?;
        if !self.committed {
            let report_digest = self.report.digest()?;
            self.write(JournalStageRef::Committed {
                report_digest,
                manifest: self.manifest_evidence,
            })
            .map_err(map_recovery_journal_write)?;
            self.committed = true;
            #[cfg(all(test, feature = "decode"))]
            if crash_at(PublicationCrashPoint::CommittedPersisted) {
                return Err(ExtractionExecutionError::PublicationRecoveryRequired {
                    stage: "committed_return",
                });
            }
        }
        Ok(self.report)
    }

    fn write(&self, stage: JournalStageRef<'_>) -> Result<(), JournalWriteError> {
        let sealed =
            usize::try_from(self.sealed_segments).map_err(|_| JournalWriteError::InvalidState {
                reason: "publication segment count exceeds the platform range",
            })?;
        let tail_start =
            sealed
                .checked_mul(RECEIPTS_PER_SEGMENT)
                .ok_or(JournalWriteError::InvalidState {
                    reason: "publication sealed receipt count overflowed",
                })?;
        let tail_receipts = self.report.manifest().artifacts().get(tail_start..).ok_or(
            JournalWriteError::InvalidState {
                reason: "publication sealed receipt count exceeds its final report",
            },
        )?;
        write_journal(
            self.journal,
            self.identity,
            self.generation,
            self.sealed_segments,
            self.segment_chain,
            tail_receipts,
            stage,
            self.journal_limit,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceState {
    Missing,
    Matching,
    Different,
}

fn inspect_evidence_bounded(
    layout: &OutputLayout,
    path: &ExtractionPath,
    evidence: ManifestEvidence,
    budget: &mut EvidenceReadBudget,
) -> Result<EvidenceState, ExtractionExecutionError> {
    let output = layout
        .path(path.as_str())
        .map_err(ExtractionExecutionError::output_layout)?;
    inspect_prepared_evidence_bounded(output, evidence, budget)
}

fn inspect_prepared_evidence_bounded(
    output: &PreparedOutputPath,
    evidence: ManifestEvidence,
    budget: &mut EvidenceReadBudget,
) -> Result<EvidenceState, ExtractionExecutionError> {
    match output.hash_existing_bounded(budget) {
        Ok(None) => Ok(EvidenceState::Missing),
        Ok(Some((length, digest))) => {
            if length == evidence.length && digest == evidence.digest {
                Ok(EvidenceState::Matching)
            } else {
                Ok(EvidenceState::Different)
            }
        }
        Err(OutputArtifactError::ExistingHashLimitExceeded { length, limit, .. }) => Err(
            ExtractionExecutionError::EvidenceVerificationLimitExceeded {
                required: length,
                remaining: limit,
            },
        ),
        Err(error) => Err(ExtractionExecutionError::output_layout(error)),
    }
}

fn validate_physical_receipt(
    layout: &OutputLayout,
    receipt: &ExtractionManifestArtifact,
    budget: &mut EvidenceReadBudget,
) -> Result<(), ExtractionExecutionError> {
    let (Some(length), Some(digest)) = (receipt.length(), receipt.digest()) else {
        return Ok(());
    };
    if inspect_evidence_bounded(
        layout,
        receipt.path(),
        ManifestEvidence { length, digest },
        budget,
    )? != EvidenceState::Matching
    {
        return Err(ExtractionExecutionError::PublicationJournalConflict {
            reason: "completed artifact target is missing or contains different bytes",
        });
    }
    Ok(())
}

fn publication_identity(
    plan: &ExtractionPlan,
    parameters: PublicationParameters<'_>,
) -> Result<PublicationIdentity, ExtractionExecutionError> {
    plan.validate_current_representation_semantics()
        .map_err(super::manifest::ExtractionManifestError::from)?;
    Ok(PublicationIdentity {
        workspace_id: plan.workspace_id(),
        revision: plan.revision(),
        request_digest: plan.request_digest(),
        plan_digest: plan.digest()?,
        execution_digest: execution_digest(
            parameters.options,
            parameters.artifact_output_limit,
            parameters.manifest_path,
            parameters.resume,
        )?,
    })
}

fn execution_digest(
    options: ExtractionExecutionOptions,
    artifact_output_limit: u64,
    manifest_path: Option<&ExtractionPath>,
    resume: Option<&ExtractionManifest>,
) -> Result<DigestV1, ExtractionExecutionError> {
    canonical_digest(&ExecutionBinding {
        existing_output: match options.existing_output() {
            ExistingOutputPolicy::Error => "error",
            ExistingOutputPolicy::Skip => "skip",
            ExistingOutputPolicy::Replace => "replace",
        },
        failure: match options.failure() {
            ExtractionFailurePolicy::CollectAll => "collect_all",
            ExtractionFailurePolicy::StopInPlanOrder => "stop_in_plan_order",
        },
        artifact_output_limit,
        manifest_path: manifest_path.map(ExtractionPath::as_str),
        resume_manifest: resume.map(ExtractionManifest::digest).transpose()?,
    })
    .map_err(Into::into)
}

fn validate_envelope(wire: &PublicationJournalWire) -> Result<(), ExtractionExecutionError> {
    let reason = if wire.contract != PUBLICATION_JOURNAL_CONTRACT {
        Some("publication journal has an unexpected contract")
    } else if wire.version != PUBLICATION_JOURNAL_VERSION {
        Some("publication journal has an unsupported version")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(ExtractionExecutionError::PublicationJournalConflict { reason });
    }
    Ok(())
}

fn identity_conflict(
    wire: &PublicationJournalWire,
    expected: PublicationIdentity,
) -> Option<&'static str> {
    if wire.workspace_id != expected.workspace_id || wire.revision != expected.revision {
        Some("publication journal belongs to a different workspace state")
    } else if wire.request_digest != expected.request_digest
        || wire.plan_digest != expected.plan_digest
    {
        Some("publication journal belongs to a different extraction plan")
    } else if wire.execution_digest != expected.execution_digest {
        Some("publication journal belongs to different execution options")
    } else {
        None
    }
}

fn read_journal(
    file: File,
    budget: &mut AssetLoadBudget,
) -> Result<PublicationJournalWire, ExtractionExecutionError> {
    read_json_bounded(file, budget, PUBLICATION_JOURNAL_JSON_LIMITS).map_err(map_json_error)
}

pub(super) fn receipt_segment_paths(
    artifact_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<String>, ExtractionExecutionError> {
    let count = artifact_count / RECEIPTS_PER_SEGMENT;
    let entries = u64::try_from(count).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "extraction publication segment paths",
    })?;
    let vector_bytes =
        vec_allocation_bytes::<String>(count).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "extraction publication segment paths",
        })?;
    let path_bytes = u64::try_from(RECEIPT_SEGMENT_DIRECTORY.len() + 1 + 8 + ".json".len())
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "extraction publication segment paths",
        })?
        .checked_mul(entries)
        .ok_or(BudgetError::ArithmeticOverflow {
            resource: "extraction publication segment paths",
        })?;
    let minimum_bytes =
        vector_bytes
            .checked_add(path_bytes)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "extraction publication segment paths",
            })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;

    let mut paths = Vec::new();
    paths
        .try_reserve_exact(count)
        .map_err(|_| ExtractionExecutionError::Allocation {
            resource: "extraction publication segment paths",
            requested: count,
            unit: super::contract::ExtractionAllocationUnit::CapacityUnits,
        })?;
    for index in 0..count {
        let index = u32::try_from(index).map_err(|_| {
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment index exceeds the wire range",
            }
        })?;
        paths.push(receipt_segment_path(index));
    }
    let retained_vector = vec_allocation_bytes::<String>(paths.capacity()).map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "extraction publication segment paths",
        }
    })?;
    let retained_strings = paths.iter().try_fold(0_u64, |total, path| {
        let capacity =
            u64::try_from(path.capacity()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "extraction publication segment paths",
            })?;
        total
            .checked_add(capacity)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "extraction publication segment paths",
            })
    })?;
    let retained_bytes =
        retained_vector
            .checked_add(retained_strings)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "extraction publication segment paths",
            })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(paths)
}

fn validate_segment_path_count(
    artifact_count: usize,
    actual: usize,
) -> Result<(), ExtractionExecutionError> {
    if actual != artifact_count / RECEIPTS_PER_SEGMENT {
        return Err(ExtractionExecutionError::PublicationJournalConflict {
            reason: "publication segment path inventory does not match the extraction plan",
        });
    }
    Ok(())
}

fn allocate_receipts(
    capacity: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ExtractionManifestArtifact>, ExtractionExecutionError> {
    let entries = u64::try_from(capacity).map_err(|_| BudgetError::ArithmeticOverflow {
        resource: "extraction publication receipts",
    })?;
    let minimum_bytes =
        vec_allocation_bytes::<ExtractionManifestArtifact>(capacity).map_err(|_| {
            BudgetError::ArithmeticOverflow {
                resource: "extraction publication receipts",
            }
        })?;
    budget.check_entries(entries)?;
    budget.check_bytes(minimum_bytes)?;
    let mut receipts = Vec::new();
    receipts
        .try_reserve_exact(capacity)
        .map_err(|_| ExtractionExecutionError::Allocation {
            resource: "extraction publication receipts",
            requested: capacity,
            unit: super::contract::ExtractionAllocationUnit::CapacityUnits,
        })?;
    let retained_bytes = vec_allocation_bytes::<ExtractionManifestArtifact>(receipts.capacity())
        .map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "extraction publication receipts",
        })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(entries)?;
    budget.consume_bytes(retained_bytes)?;
    Ok(receipts)
}

struct ReceiptLoad<'value> {
    layout: &'value OutputLayout,
    segment_paths: &'value [String],
    generation: PublicationGeneration,
    sealed_segments: usize,
    expected_chain: Option<DigestV1>,
    capacity: usize,
    segment_limit: u64,
}

fn load_receipts(
    context: ReceiptLoad<'_>,
    tail_receipts: Vec<ExtractionManifestArtifact>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<ExtractionManifestArtifact>, ExtractionExecutionError> {
    let mut receipts = allocate_receipts(context.capacity, budget)?;

    let mut chain = None;
    for index in 0..context.sealed_segments {
        let segment_index = u32::try_from(index).map_err(|_| {
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment index exceeds the wire range",
            }
        })?;
        let first_ordinal = segment_index.checked_mul(RECEIPTS_PER_SEGMENT_U32).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication segment ordinal overflowed",
            },
        )?;
        let path = context.segment_paths.get(index).ok_or(
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication receipt segment path is missing",
            },
        )?;
        let output = context
            .layout
            .path(path)
            .map_err(ExtractionExecutionError::output_layout)?;
        let Some(file) = output
            .open_existing()
            .map_err(ExtractionExecutionError::output_layout)?
        else {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication receipt segment is missing",
            });
        };
        let length = file
            .metadata()
            .map_err(
                |error| ExtractionExecutionError::PublicationJournalInvalid {
                    message: error.to_string(),
                },
            )?
            .len();
        if length > context.segment_limit {
            return Err(ExtractionExecutionError::PublicationJournalLimitExceeded {
                required: length,
                limit: context.segment_limit,
            });
        }
        let segment: ReceiptSegmentWire =
            read_json_bounded(file, budget, RECEIPT_SEGMENT_JSON_LIMITS).map_err(map_json_error)?;
        if segment.contract != RECEIPT_SEGMENT_CONTRACT
            || segment.version != RECEIPT_SEGMENT_VERSION
            || segment.generation != context.generation
            || segment.segment_index != segment_index
            || segment.first_ordinal != first_ordinal
            || segment.previous != chain
            || segment.receipts.len() != RECEIPTS_PER_SEGMENT
        {
            return Err(ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication receipt segment does not match its hash chain",
            });
        }
        let digest = canonical_digest(&ReceiptSegmentRef {
            contract: RECEIPT_SEGMENT_CONTRACT,
            version: RECEIPT_SEGMENT_VERSION,
            generation: context.generation,
            segment_index,
            first_ordinal,
            previous: chain,
            receipts: &segment.receipts,
        })?;
        receipts.extend(segment.receipts);
        chain = Some(digest);
    }
    if chain != context.expected_chain {
        return Err(ExtractionExecutionError::PublicationJournalConflict {
            reason: "publication receipt segment chain does not match the journal",
        });
    }
    receipts.extend(tail_receipts);
    Ok(receipts)
}

fn receipt_segment_path(index: u32) -> String {
    format!("{RECEIPT_SEGMENT_DIRECTORY}/{index:08x}.json")
}

fn map_json_error(error: BudgetedJsonError) -> ExtractionExecutionError {
    match error {
        BudgetedJsonError::Budget(error) => error.into(),
        BudgetedJsonError::InvalidLimit { resource, .. } => {
            BudgetError::InvalidLimit { resource }.into()
        }
        BudgetedJsonError::StructureLimitExceeded {
            resource,
            limit,
            requested,
            ..
        } => BudgetError::Exceeded {
            resource,
            limit,
            requested,
        }
        .into(),
        BudgetedJsonError::AllocationFailed { requested } => ExtractionExecutionError::Allocation {
            resource: "extraction publication journal JSON",
            requested,
            unit: super::contract::ExtractionAllocationUnit::Bytes,
        },
        other => ExtractionExecutionError::PublicationJournalInvalid {
            message: other.to_string(),
        },
    }
}

fn write_journal(
    journal: &PreparedOutputPath,
    identity: PublicationIdentity,
    generation: PublicationGeneration,
    sealed_segments: u32,
    segment_chain: Option<DigestV1>,
    tail_receipts: &[ExtractionManifestArtifact],
    stage: JournalStageRef<'_>,
    limit: u64,
) -> Result<(), JournalWriteError> {
    let value = PublicationJournalRef {
        contract: PUBLICATION_JOURNAL_CONTRACT,
        version: PUBLICATION_JOURNAL_VERSION,
        workspace_id: identity.workspace_id,
        revision: identity.revision,
        request_digest: identity.request_digest,
        plan_digest: identity.plan_digest,
        execution_digest: identity.execution_digest,
        generation,
        sealed_segments,
        segment_chain,
        tail_receipts,
        stage,
    };
    write_atomic_json(journal, &value, limit)
}

fn write_atomic_json<T: Serialize>(
    output: &PreparedOutputPath,
    value: &T,
    limit: u64,
) -> Result<(), JournalWriteError> {
    let mut counter = CheckedByteCounter::new("extraction publication JSON length overflow");
    write_canonical_json(&mut counter, value).map_err(JournalWriteError::Canonical)?;
    if counter.bytes() > limit {
        return Err(JournalWriteError::LimitExceeded {
            required: counter.bytes(),
            limit,
        });
    }
    let mut staging = output
        .create_staging()
        .map_err(JournalWriteError::NotPublished)?;
    write_canonical_json(staging.writer(), value).map_err(JournalWriteError::Canonical)?;
    let staged = staging.finish().map_err(JournalWriteError::NotPublished)?;
    match staged.publish(true) {
        Ok(()) => Ok(()),
        Err(StagedPublishError::NotPublished(error)) => Err(JournalWriteError::NotPublished(error)),
        Err(StagedPublishError::Uncertain) => Err(JournalWriteError::Uncertain),
    }
}

fn map_initial_journal_write(error: JournalWriteError) -> ExtractionExecutionError {
    match error {
        JournalWriteError::NotPublished(error) => ExtractionExecutionError::output_layout(error),
        JournalWriteError::Uncertain => ExtractionExecutionError::PublicationRecoveryRequired {
            stage: "journal_initialization",
        },
        JournalWriteError::Canonical(error) => error.into(),
        JournalWriteError::LimitExceeded { required, limit } => {
            ExtractionExecutionError::PublicationJournalLimitExceeded { required, limit }
        }
        JournalWriteError::InvalidState { reason } => {
            ExtractionExecutionError::PublicationJournalConflict { reason }
        }
    }
}

fn map_safe_journal_write(error: JournalWriteError) -> ExtractionExecutionError {
    match error {
        JournalWriteError::NotPublished(error) => ExtractionExecutionError::output_layout(error),
        JournalWriteError::Uncertain => ExtractionExecutionError::PublicationRecoveryRequired {
            stage: "journal_transition",
        },
        JournalWriteError::Canonical(error) => error.into(),
        JournalWriteError::LimitExceeded { required, limit } => {
            ExtractionExecutionError::PublicationJournalLimitExceeded { required, limit }
        }
        JournalWriteError::InvalidState { reason } => {
            ExtractionExecutionError::PublicationJournalConflict { reason }
        }
    }
}

fn map_recovery_journal_write(error: JournalWriteError) -> ExtractionExecutionError {
    match error {
        JournalWriteError::Canonical(error) => error.into(),
        JournalWriteError::LimitExceeded { required, limit } => {
            ExtractionExecutionError::PublicationJournalLimitExceeded { required, limit }
        }
        JournalWriteError::NotPublished(_) | JournalWriteError::Uncertain => {
            ExtractionExecutionError::PublicationRecoveryRequired {
                stage: "journal_transition",
            }
        }
        JournalWriteError::InvalidState { reason } => {
            ExtractionExecutionError::PublicationJournalConflict { reason }
        }
    }
}

fn map_artifact_completion_error(error: ExtractionExecutionError) -> ExtractionExecutionError {
    match error {
        error @ ExtractionExecutionError::PublicationRecoveryRequired { .. } => error,
        _ => ExtractionExecutionError::PublicationRecoveryRequired {
            stage: "artifact_completion",
        },
    }
}

fn manifest_evidence(
    report: &ExtractionReport,
) -> Result<ManifestEvidence, ExtractionExecutionError> {
    let mut counter = CheckedByteCounter::new("canonical extraction manifest length overflow");
    report.write_canonical_manifest_json(&mut counter)?;
    Ok(ManifestEvidence {
        length: counter.bytes(),
        digest: report.manifest().digest()?,
    })
}

#[cfg(test)]
mod tests {
    use unity_asset_core::{AssetLoadLimits, ObjectAddress, SourceLocator};

    use super::*;
    use crate::extraction::ExtractionArtifactKind;

    #[test]
    fn legacy_v2_journal_is_rejected_before_recovery() {
        let wire = PublicationJournalWire {
            contract: PUBLICATION_JOURNAL_CONTRACT.to_owned(),
            version: 2,
            workspace_id: WorkspaceId::from_u128(1).unwrap(),
            revision: WorkspaceRevision::new(DigestV1::hash_bytes(b"revision")),
            request_digest: DigestV1::hash_bytes(b"request"),
            plan_digest: DigestV1::hash_bytes(b"legacy-plan"),
            execution_digest: DigestV1::hash_bytes(b"execution"),
            generation: PublicationGeneration(DigestV1::hash_bytes(b"generation")),
            sealed_segments: 0,
            segment_chain: None,
            tail_receipts: Vec::new(),
            stage: JournalStage::Publishing,
        };

        let error = validate_envelope(&wire).expect_err("v2 journals must not enter recovery");
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalConflict {
                reason: "publication journal has an unsupported version",
            }
        ));
    }

    #[test]
    fn segment_path_inventory_has_exact_boundary_and_budget_accounting() {
        assert!(
            receipt_segment_paths(63, &mut AssetLoadBudget::default())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            receipt_segment_paths(64, &mut AssetLoadBudget::default())
                .unwrap()
                .len(),
            1
        );

        let mut measured = AssetLoadBudget::default();
        let expected = receipt_segment_paths(65, &mut measured).unwrap();
        let usage = measured.usage();
        assert_eq!(expected.len(), 1);
        assert!(usage.bytes > 1);
        assert!(usage.entries > 0);

        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        assert_eq!(receipt_segment_paths(65, &mut exact).unwrap(), expected);

        let one_short_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes - 1,
            ..AssetLoadLimits::default()
        };
        let error = receipt_segment_paths(65, &mut AssetLoadBudget::new(one_short_limits).unwrap())
            .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[test]
    fn receipt_segment_loading_is_budgeted_generation_bound_and_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let segment_paths = vec![receipt_segment_path(0)];
        let layout = OutputLayout::prepare_with_internal_paths(
            directory.path(),
            std::iter::empty::<&str>(),
            segment_paths.iter().map(String::as_str),
            &[RECEIPT_SEGMENT_DIRECTORY],
        )
        .unwrap();
        let generation = PublicationGeneration(DigestV1::from_bytes([7; DigestV1::BYTE_LEN]));
        let receipts = failed_receipts(RECEIPTS_PER_SEGMENT_U32);
        let segment = ReceiptSegmentRef {
            contract: RECEIPT_SEGMENT_CONTRACT,
            version: RECEIPT_SEGMENT_VERSION,
            generation,
            segment_index: 0,
            first_ordinal: 0,
            previous: None,
            receipts: &receipts,
        };
        let segment_digest = canonical_digest(&segment).unwrap();
        let output = layout.path(&segment_paths[0]).unwrap();
        assert!(write_atomic_json(output, &segment, u64::MAX).is_ok());

        let mut measured = AssetLoadBudget::default();
        let loaded = load_receipts(
            ReceiptLoad {
                layout: &layout,
                segment_paths: &segment_paths,
                generation,
                sealed_segments: 1,
                expected_chain: Some(segment_digest),
                capacity: RECEIPTS_PER_SEGMENT,
                segment_limit: u64::MAX,
            },
            Vec::new(),
            &mut measured,
        )
        .unwrap();
        assert_eq!(loaded, receipts);
        let usage = measured.usage();
        assert!(usage.bytes > 1);

        let exact_limits = load_limits_from_usage(usage, usage.bytes);
        load_receipts(
            ReceiptLoad {
                layout: &layout,
                segment_paths: &segment_paths,
                generation,
                sealed_segments: 1,
                expected_chain: Some(segment_digest),
                capacity: RECEIPTS_PER_SEGMENT,
                segment_limit: u64::MAX,
            },
            Vec::new(),
            &mut AssetLoadBudget::new(exact_limits).unwrap(),
        )
        .unwrap();

        let one_short = load_limits_from_usage(usage, usage.bytes - 1);
        let error = load_receipts(
            ReceiptLoad {
                layout: &layout,
                segment_paths: &segment_paths,
                generation,
                sealed_segments: 1,
                expected_chain: Some(segment_digest),
                capacity: RECEIPTS_PER_SEGMENT,
                segment_limit: u64::MAX,
            },
            Vec::new(),
            &mut AssetLoadBudget::new(one_short).unwrap(),
        )
        .unwrap_err();
        assert!(matches!(error, ExtractionExecutionError::Budget(_)));

        let wrong_generation = PublicationGeneration(DigestV1::from_bytes([8; DigestV1::BYTE_LEN]));
        let error = load_receipts(
            ReceiptLoad {
                layout: &layout,
                segment_paths: &segment_paths,
                generation: wrong_generation,
                sealed_segments: 1,
                expected_chain: Some(segment_digest),
                capacity: RECEIPTS_PER_SEGMENT,
                segment_limit: u64::MAX,
            },
            Vec::new(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ExtractionExecutionError::PublicationJournalConflict { .. }
        ));

        let ignored = load_receipts(
            ReceiptLoad {
                layout: &layout,
                segment_paths: &segment_paths,
                generation: wrong_generation,
                sealed_segments: 0,
                expected_chain: None,
                capacity: 0,
                segment_limit: u64::MAX,
            },
            Vec::new(),
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert!(ignored.is_empty());
    }

    fn failed_receipts(count: u32) -> Vec<ExtractionManifestArtifact> {
        let source = SourceLocator::path("source.assets").unwrap();
        (0..count)
            .map(|ordinal| {
                ExtractionManifestArtifact::new(
                    ordinal,
                    ObjectAddress::binary_at(source.clone(), i64::from(ordinal) + 1).unwrap(),
                    ExtractionArtifactKind::BinaryRaw,
                    ExtractionPath::new(format!("objects/{ordinal}.bin")).unwrap(),
                    ExtractionArtifactStatus::Failed,
                    None,
                    None,
                    Vec::new(),
                )
                .unwrap()
            })
            .collect()
    }

    fn load_limits_from_usage(
        usage: unity_asset_core::AssetLoadUsage,
        max_bytes: u64,
    ) -> AssetLoadLimits {
        AssetLoadLimits {
            max_entries: usage.entries.max(1),
            max_bytes,
            max_depth: usage.max_observed_depth.max(1),
            max_members: usage.members.max(1),
            max_compressed_bytes: usage.compressed_bytes.max(1),
            max_decompressed_bytes: usage.decompressed_bytes.max(1),
            ..AssetLoadLimits::default()
        }
    }
}
