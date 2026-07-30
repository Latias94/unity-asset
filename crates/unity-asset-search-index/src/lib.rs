//! Generation-bound search and reference indexing for Unity projects.
//!
//! [`SearchIndex`] is the public concurrency boundary. Reindexing is serialized behind the
//! internal generation pipeline, while every query pins an immutable active generation without
//! waiting for a build.
//! Public response and error payloads are owned by `unity-asset-search-protocol`; this crate only
//! owns the in-process index inputs and persisted generation model.

use std::error::Error as StdError;
use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

mod analysis;
mod analyzer;
mod config;
mod generation;
mod pipeline;
mod projection;
mod query;
mod reference_payload;
mod reference_query;
mod scan;
mod state;
mod store;
mod wire;

pub use config::{IndexPaths, SearchIndexOptions};
pub use generation::{FilesystemReindexIntent, FilesystemReindexScope};
pub use unity_asset::workspace::WorkspaceView;
pub use unity_asset_core::{AssetLoadBudget, ChangeSet, DigestV1, DigestV1Builder};
pub use unity_asset_search_core::{SearchKind, SearchRequest};
#[cfg(test)]
use unity_asset_search_protocol::GenerationMaintenanceState;
use unity_asset_search_protocol::{
    ApiError, ApiErrorCode, DaemonLifecycleStatus, GenerationFailure, GenerationMaintenanceStatus,
    GenerationStatus, ReferenceRequest, ReferencesResponse, ReindexReceipt,
    SEARCH_PROTOCOL_REVISION, SearchCapabilities, SearchResponse, StatusResponse, SuggestResponse,
    WireProjectionError,
};

use generation::GenerationStamp as InternalGenerationStamp;
use pipeline::{ActiveGeneration, PipelineBuildOutput, PipelineError, SearchGenerationPipeline};
use reference_query::ReferenceQueryError;
#[cfg(test)]
use state::GenerationFailpoint;

/// Maximum UTF-8 byte length accepted by [`SearchIndex::suggest`].
pub const MAX_SUGGEST_PREFIX_BYTES: usize = unity_asset_search_protocol::MAX_SUGGEST_PREFIX_BYTES;

/// Maximum number of suggestions accepted by [`SearchIndex::suggest`].
pub const MAX_SUGGEST_LIMIT: usize = unity_asset_search_protocol::MAX_SUGGEST_RESULTS as usize;

/// Maximum UTF-8 byte length accepted by [`SearchIndex::search`].
pub const MAX_SEARCH_QUERY_BYTES: usize = unity_asset_search_protocol::MAX_SEARCH_QUERY_BYTES;

/// Maximum result limit accepted by [`SearchIndex::search`].
pub const MAX_SEARCH_LIMIT: usize = unity_asset_search_protocol::MAX_SEARCH_RESULTS as usize;

/// A cloneable handle to one project search index.
///
/// Builds hold the pipeline mutex but never the active-generation lock. Search, suggestion, and
/// reference calls therefore continue reading the previous complete generation until publication
/// succeeds.
#[derive(Clone)]
pub struct SearchIndex {
    inner: Arc<SearchIndexInner>,
}

struct SearchIndexInner {
    paths: IndexPaths,
    options: SearchIndexOptions,
    pipeline: Mutex<SearchGenerationPipeline>,
    status: RwLock<StatusObservation>,
    #[cfg(test)]
    status_commit_hook: Mutex<Option<StatusCommitHook>>,
}

#[derive(Clone)]
struct StatusObservation {
    active: Option<Arc<ActiveGeneration>>,
    runtime: RuntimeStatus,
}

#[derive(Debug, Clone, Default)]
struct RuntimeStatus {
    indexing: bool,
    building_revision: Option<unity_asset_core::WorkspaceRevision>,
    last_failure: Option<GenerationFailure>,
    generation_maintenance: GenerationMaintenanceStatus,
    last_build_duration_ms: Option<u128>,
    last_build_unix_ms: Option<u64>,
}

#[cfg(test)]
struct StatusCommitHook {
    entered: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl StatusCommitHook {
    fn wait(self) {
        let _ = self.entered.send(());
        let _ = self.resume.recv();
    }
}

struct RuntimeBuildGuard<'status> {
    status: &'status RwLock<StatusObservation>,
    armed: bool,
}

impl<'status> RuntimeBuildGuard<'status> {
    fn start(
        status: &'status RwLock<StatusObservation>,
        target_revision: Option<unity_asset_core::WorkspaceRevision>,
    ) -> Result<Self, SearchIndexError> {
        {
            let mut observation = status
                .write()
                .map_err(|_| SearchIndexError::internal("runtime status lock is poisoned"))?;
            observation.runtime.indexing = true;
            observation.runtime.building_revision = target_revision;
        }
        Ok(Self {
            status,
            armed: true,
        })
    }

    fn finish(
        mut self,
        active: Option<Arc<ActiveGeneration>>,
        update: impl FnOnce(&mut RuntimeStatus),
        before_runtime_update: impl FnOnce(),
    ) -> Result<(), SearchIndexError> {
        {
            let mut observation = self
                .status
                .write()
                .map_err(|_| SearchIndexError::internal("runtime status lock is poisoned"))?;
            observation.active = active;
            before_runtime_update();
            clear_runtime_build(&mut observation.runtime);
            update(&mut observation.runtime);
        }
        self.armed = false;
        Ok(())
    }
}

impl Drop for RuntimeBuildGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut observation = match self.status.write() {
            Ok(observation) => observation,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_runtime_build(&mut observation.runtime);
    }
}

fn clear_runtime_build(runtime: &mut RuntimeStatus) {
    runtime.indexing = false;
    runtime.building_revision = None;
}

impl fmt::Debug for SearchIndex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchIndex")
            .field("paths", &self.inner.paths)
            .field("options", &self.inner.options)
            .finish_non_exhaustive()
    }
}

impl SearchIndex {
    /// Opens an existing generation store or creates an empty one with default options.
    pub fn open_or_create(
        paths: IndexPaths,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SearchIndexError> {
        Self::open_or_create_with_options(paths, SearchIndexOptions::default(), budget)
    }

    /// Opens an existing generation store or creates an empty one.
    ///
    /// Discovering persisted generations and loading their source state share the caller's
    /// budget. The store acquires an exclusive process-scoped writer lease for the lifetime of
    /// this handle.
    pub fn open_or_create_with_options(
        paths: IndexPaths,
        options: SearchIndexOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SearchIndexError> {
        let pipeline = SearchGenerationPipeline::open(paths.clone(), options, budget)
            .map_err(|error| SearchIndexError::from_pipeline(error, None))?;
        let active = pipeline.active();
        let generation_maintenance = pipeline.generation_maintenance();
        Ok(Self {
            inner: Arc::new(SearchIndexInner {
                paths,
                options,
                pipeline: Mutex::new(pipeline),
                status: RwLock::new(StatusObservation {
                    active,
                    runtime: RuntimeStatus {
                        generation_maintenance,
                        ..RuntimeStatus::default()
                    },
                }),
                #[cfg(test)]
                status_commit_hook: Mutex::new(None),
            }),
        })
    }

    #[must_use]
    pub fn paths(&self) -> &IndexPaths {
        &self.inner.paths
    }

    #[must_use]
    pub fn options(&self) -> SearchIndexOptions {
        self.inner.options
    }

    #[cfg(test)]
    fn inject_generation_failpoint(
        &self,
        failpoint: GenerationFailpoint,
    ) -> Result<(), SearchIndexError> {
        self.inner
            .pipeline
            .lock()
            .map_err(|_| SearchIndexError::internal("generation pipeline lock is poisoned"))?
            .inject_publish_failpoint(failpoint);
        Ok(())
    }

    #[cfg(test)]
    fn inject_status_commit_hook(
        &self,
        entered: std::sync::mpsc::SyncSender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *self.inner.status_commit_hook.lock().unwrap() = Some(StatusCommitHook { entered, resume });
    }

    #[cfg(test)]
    fn wait_at_status_commit_hook(&self) {
        let hook = self.inner.status_commit_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook.wait();
        }
    }

    /// Runs a filesystem-backed full, reconciliation, or changed-path build.
    pub fn reindex(
        &self,
        intent: FilesystemReindexIntent,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReindexReceipt, SearchIndexError> {
        self.execute_build(None, |pipeline| pipeline.reindex_filesystem(intent, budget))
    }

    /// Applies a transaction-keyed Change Set against its authoritative target view.
    pub fn reindex_workspace(
        &self,
        changes: ChangeSet,
        view: &dyn WorkspaceView,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReindexReceipt, SearchIndexError> {
        let target_revision = Some(changes.to_revision());
        self.execute_build(target_revision, |pipeline| {
            pipeline.reindex_workspace(changes, view, budget)
        })
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchResponse, SearchIndexError> {
        validate_search_request(&request)?;
        let active = self.active_generation()?;
        active
            .search(request)
            .map_err(|error| SearchIndexError::from_query(error, active.stamp()))
    }

    /// Returns type and path suggestions for a bounded prefix and result limit.
    ///
    /// An empty prefix is valid and returns no suggestions. Limits must be in
    /// `1..=MAX_SUGGEST_LIMIT`, and prefixes must not exceed
    /// [`MAX_SUGGEST_PREFIX_BYTES`] UTF-8 bytes.
    pub fn suggest(&self, prefix: &str, limit: usize) -> Result<SuggestResponse, SearchIndexError> {
        validate_suggest_request(prefix, limit)?;
        let active = self.active_generation()?;
        active
            .suggest(prefix, limit)
            .map_err(|error| SearchIndexError::from_query(error, active.stamp()))
    }

    /// Queries the active reference projection while charging every persisted
    /// JSON field decoded for the response page to the caller-owned budget.
    pub fn references(
        &self,
        request: ReferenceRequest,
        budget: &mut AssetLoadBudget,
    ) -> Result<ReferencesResponse, SearchIndexError> {
        let active = self.active_generation()?;
        active
            .references(request, budget)
            .map_err(|error| SearchIndexError::from_reference(error, Some(active.stamp().clone())))
    }

    pub fn status(&self) -> Result<StatusResponse, SearchIndexError> {
        let (active, runtime) = self
            .inner
            .status
            .read()
            .map_err(|_| SearchIndexError::internal("status observation lock is poisoned"))
            .map(|observation| (observation.active.clone(), observation.runtime.clone()))?;

        let project_root = wire::portable_path(self.inner.paths.project_root())
            .map_err(SearchIndexError::from_wire_projection)?;
        let generation_root = wire::portable_path(self.inner.paths.index_root())
            .map_err(SearchIndexError::from_wire_projection)?;
        let scan_roots = self
            .inner
            .paths
            .scan_roots()
            .iter()
            .map(|path| wire::portable_path(path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(SearchIndexError::from_wire_projection)?;
        let last_build_duration_ms = runtime
            .last_build_duration_ms
            .map(|duration| wire::fixed_millis(duration, "status build duration"))
            .transpose()
            .map_err(SearchIndexError::from_wire_projection)?;

        let generation = GenerationStatus {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            active: active
                .as_ref()
                .map(|generation| wire::generation_stamp(generation.stamp())),
            building_revision: runtime.building_revision,
            last_failure: runtime.last_failure,
        };
        let mut daemon = DaemonLifecycleStatus::unmanaged(&generation, runtime.indexing);
        daemon.generation_maintenance = runtime.generation_maintenance;
        Ok(StatusResponse {
            protocol_revision: SEARCH_PROTOCOL_REVISION,
            daemon,
            generation,
            query_policy_id: wire::query_policy_id(),
            capabilities: SearchCapabilities::current(),
            project_root,
            generation_root,
            scan_roots,
            indexed_assets: active
                .as_ref()
                .map_or(0, |generation| generation.indexed_assets()),
            indexed_search_documents: active
                .as_ref()
                .map_or(0, |generation| generation.indexed_search_documents()),
            indexed_reference_facts: active
                .as_ref()
                .map_or(0, |generation| generation.indexed_reference_facts()),
            incomplete_assets: active
                .as_ref()
                .map_or(0, |generation| generation.incomplete_assets()),
            projection_truncations: active
                .as_ref()
                .map_or(0, |generation| generation.projection_truncations()),
            last_build_duration_ms,
            last_build_unix_ms: runtime.last_build_unix_ms,
            indexing: runtime.indexing,
        })
    }

    fn execute_build(
        &self,
        target_revision: Option<unity_asset_core::WorkspaceRevision>,
        build: impl FnOnce(&mut SearchGenerationPipeline) -> Result<PipelineBuildOutput, PipelineError>,
    ) -> Result<ReindexReceipt, SearchIndexError> {
        let mut pipeline = self
            .inner
            .pipeline
            .lock()
            .map_err(|_| SearchIndexError::internal("generation pipeline lock is poisoned"))?;
        let runtime = RuntimeBuildGuard::start(&self.inner.status, target_revision)?;

        let result = build(&mut pipeline);
        let pipeline_active = pipeline.active();
        let generation_maintenance = pipeline.generation_maintenance();

        let outcome = match result {
            Ok(output) => {
                let receipt = wire::reindex_receipt(&output);
                runtime.finish(
                    pipeline_active.clone(),
                    |status| {
                        status.last_failure = None;
                        status.generation_maintenance = generation_maintenance;
                        status.last_build_duration_ms = Some(output.duration_ms);
                        status.last_build_unix_ms = Some(unix_ms_now());
                    },
                    || {
                        #[cfg(test)]
                        self.wait_at_status_commit_hook();
                    },
                )?;
                Ok(receipt)
            }
            Err(error) => {
                let failure = SearchIndexError::from_pipeline(
                    error,
                    pipeline_active
                        .as_ref()
                        .map(|generation| generation.stamp().clone()),
                );
                let recorded = GenerationFailure {
                    code: api_error_code_name(failure.code()).to_owned(),
                    message: failure.api_error().message.clone(),
                    retryable: failure.retryable(),
                    failed_unix_ms: unix_ms_now(),
                    desired_revision: target_revision,
                };
                runtime.finish(
                    pipeline_active.clone(),
                    |status| {
                        status.last_failure = Some(recorded);
                        status.generation_maintenance = generation_maintenance;
                    },
                    || {
                        #[cfg(test)]
                        self.wait_at_status_commit_hook();
                    },
                )?;
                Err(failure)
            }
        };
        drop(pipeline);
        outcome
    }

    fn active_generation(&self) -> Result<Arc<ActiveGeneration>, SearchIndexError> {
        self.inner
            .status
            .read()
            .map_err(|_| SearchIndexError::internal("status observation lock is poisoned"))?
            .active
            .clone()
            .ok_or_else(SearchIndexError::generation_unavailable)
    }
}

fn validate_search_request(request: &SearchRequest) -> Result<(), SearchIndexError> {
    if request.query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(SearchIndexError::invalid_request(
            format!(
                "search query is {} bytes, exceeding the {MAX_SEARCH_QUERY_BYTES}-byte limit",
                request.query.len()
            ),
            "query_bytes",
            request.query.len(),
            MAX_SEARCH_QUERY_BYTES,
        ));
    }
    if request.limit > MAX_SEARCH_LIMIT {
        return Err(SearchIndexError::invalid_request(
            format!(
                "search limit {} exceeds the {MAX_SEARCH_LIMIT}-result limit",
                request.limit
            ),
            "limit",
            request.limit,
            MAX_SEARCH_LIMIT,
        ));
    }
    Ok(())
}

fn validate_suggest_request(prefix: &str, limit: usize) -> Result<(), SearchIndexError> {
    if prefix.len() > MAX_SUGGEST_PREFIX_BYTES {
        return Err(SearchIndexError::invalid_request(
            format!(
                "suggest prefix is {} bytes, exceeding the {MAX_SUGGEST_PREFIX_BYTES}-byte limit",
                prefix.len()
            ),
            "prefix_bytes",
            prefix.len(),
            MAX_SUGGEST_PREFIX_BYTES,
        ));
    }
    if !(1..=MAX_SUGGEST_LIMIT).contains(&limit) {
        return Err(SearchIndexError::invalid_request(
            format!("suggest limit {limit} is outside 1..={MAX_SUGGEST_LIMIT}"),
            "limit",
            limit,
            MAX_SUGGEST_LIMIT,
        ));
    }
    Ok(())
}

/// Stable public error wrapper used by Rust, CLI, and daemon adapters.
#[derive(Debug)]
pub struct SearchIndexError {
    api: Box<ApiError>,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl SearchIndexError {
    #[must_use]
    pub const fn api_error(&self) -> &ApiError {
        &self.api
    }

    #[must_use]
    pub const fn code(&self) -> ApiErrorCode {
        self.api.code
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        self.api.retryable
    }

    #[must_use]
    pub fn into_api_error(self) -> ApiError {
        *self.api
    }

    fn from_pipeline(error: PipelineError, generation: Option<InternalGenerationStamp>) -> Self {
        let mut api = ApiError::new(
            error.api_code(),
            wire::bounded_error_message(error.to_string()),
            error.retryable(),
        )
        .with_query_policy(wire::query_policy_id());
        if let Some(generation) = generation {
            api = api.with_generation(wire::generation_stamp(&generation));
        }
        Self::with_source(api, error)
    }

    fn from_reference(
        error: ReferenceQueryError,
        generation: Option<InternalGenerationStamp>,
    ) -> Self {
        let mut api = ApiError::new(
            error.api_code(),
            wire::bounded_error_message(error.to_string()),
            false,
        )
        .with_query_policy(wire::query_policy_id());
        if let Some(generation) = generation {
            api = api.with_generation(wire::generation_stamp(&generation));
        }
        Self::with_source(api, error)
    }

    fn from_query(error: anyhow::Error, generation: &InternalGenerationStamp) -> Self {
        let api = ApiError::new(
            ApiErrorCode::Internal,
            wire::bounded_error_message(error.to_string()),
            false,
        )
        .with_generation(wire::generation_stamp(generation))
        .with_query_policy(wire::query_policy_id());
        Self::with_boxed_source(api, error.into_boxed_dyn_error())
    }

    fn from_wire_projection(error: WireProjectionError) -> Self {
        let api = ApiError::new(
            ApiErrorCode::Internal,
            wire::bounded_error_message(error.to_string()),
            false,
        )
        .with_query_policy(wire::query_policy_id());
        Self::with_source(api, error)
    }

    fn generation_unavailable() -> Self {
        Self {
            api: Box::new(
                ApiError::new(
                    ApiErrorCode::NotReady,
                    "no complete search generation is active",
                    true,
                )
                .with_query_policy(wire::query_policy_id()),
            ),
            source: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            api: Box::new(
                ApiError::new(
                    ApiErrorCode::Internal,
                    wire::bounded_error_message(message.into()),
                    false,
                )
                .with_query_policy(wire::query_policy_id()),
            ),
            source: None,
        }
    }

    fn invalid_request(
        message: impl Into<String>,
        field: &'static str,
        actual: usize,
        maximum: usize,
    ) -> Self {
        Self {
            api: Box::new(
                ApiError::new(
                    ApiErrorCode::InvalidRequest,
                    wire::bounded_error_message(message.into()),
                    false,
                )
                .with_query_policy(wire::query_policy_id())
                .with_detail("field", field)
                .with_detail("actual", actual.to_string())
                .with_detail("maximum", maximum.to_string()),
            ),
            source: None,
        }
    }

    fn with_source(api: ApiError, source: impl StdError + Send + Sync + 'static) -> Self {
        Self::with_boxed_source(api, Box::new(source))
    }

    fn with_boxed_source(api: ApiError, source: Box<dyn StdError + Send + Sync + 'static>) -> Self {
        Self {
            api: Box::new(api),
            source: Some(source),
        }
    }
}

impl fmt::Display for SearchIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.api.message)
    }
}

impl StdError for SearchIndexError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

const fn api_error_code_name(code: ApiErrorCode) -> &'static str {
    match code {
        ApiErrorCode::InvalidRequest => "invalid_request",
        ApiErrorCode::InvalidCursor => "invalid_cursor",
        ApiErrorCode::StaleCursor => "stale_cursor",
        ApiErrorCode::IncompatibleProtocol => "incompatible_protocol",
        ApiErrorCode::PeerRejected => "peer_rejected",
        ApiErrorCode::Busy => "busy",
        ApiErrorCode::NotReady => "not_ready",
        ApiErrorCode::RevisionMismatch => "revision_mismatch",
        ApiErrorCode::IndexBuildFailed => "index_build_failed",
        ApiErrorCode::IdempotencyConflict => "idempotency_conflict",
        ApiErrorCode::OperationNotFound => "operation_not_found",
        ApiErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use unity_asset::workspace::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
    use unity_asset::{SourceAlias, SourceKind};
    use unity_asset_core::{SourceId, TransactionId, WorkspaceId, WorkspaceRevision};
    use unity_asset_search_protocol::{
        GenerationIdV1, GenerationStamp, MAX_ERROR_MESSAGE_BYTES, ValidateContract,
    };

    use crate::generation::SearchGenerationId;

    const OWNER_PATH: &str = "Assets/owner.prefab";
    const TARGET_PATH: &str = "Assets/target.prefab";
    const TARGET_GUID: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_GUID: &str = "fedcba9876543210fedcba9876543210";
    const OWNER_BEFORE: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: Before
  m_Target: {fileID: 100, guid: 0123456789abcdef0123456789abcdef, type: 3}
"#;
    const OWNER_AFTER: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &1
GameObject:
  m_Name: After
  m_Target: {fileID: 100, guid: fedcba9876543210fedcba9876543210, type: 3}
"#;
    const TARGET: &str = r#"%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1 &100
GameObject:
  m_Name: Target
"#;

    fn write_generation_fixture(root: &Path) {
        let assets = root.join("Assets");
        fs::create_dir_all(&assets).unwrap();
        fs::write(assets.join("owner.prefab"), OWNER_BEFORE).unwrap();
        fs::write(
            assets.join("owner.prefab.meta"),
            "fileFormatVersion: 2\nguid: 11111111111111111111111111111111\n",
        )
        .unwrap();
        fs::write(assets.join("target.prefab"), TARGET).unwrap();
        fs::write(
            assets.join("target.prefab.meta"),
            format!("fileFormatVersion: 2\nguid: {TARGET_GUID}\n"),
        )
        .unwrap();
    }

    fn search_paths(index: &SearchIndex, query: &str) -> Vec<String> {
        index
            .search(SearchRequest::new(query, 20))
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.path.to_string())
            .collect()
    }

    fn incoming_paths(index: &SearchIndex, guid: &str) -> Vec<String> {
        index
            .references(
                ReferenceRequest::incoming_guid(guid, Some(100), 20),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.source_path.to_string())
            .collect()
    }

    fn assert_baseline_generation(index: &SearchIndex, expected: &GenerationStamp) {
        assert_eq!(
            index.status().unwrap().generation.active.as_ref(),
            Some(expected)
        );
        assert_eq!(search_paths(index, "Before"), vec![OWNER_PATH.to_owned()]);
        assert!(search_paths(index, "After").is_empty());
        assert_eq!(
            incoming_paths(index, TARGET_GUID),
            vec![OWNER_PATH.to_owned()]
        );
        assert!(incoming_paths(index, OTHER_GUID).is_empty());
    }

    fn rewrite_reference_marker_as_legacy(paths: &IndexPaths, generation: GenerationIdV1) {
        let generation_directory = paths
            .index_root()
            .join("generations")
            .join(SearchGenerationId::new(generation.digest()).directory_name());
        let reference_directory = generation_directory.join("references");
        let marker_path = reference_directory.join("schema-contract.json");
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        marker["schema_version"] = serde_json::Value::from(2);
        fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();

        let manifest_path = generation_directory.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["artifacts"]["references"] = serde_json::to_value(
            crate::state::measure_artifact_tree(&reference_directory).unwrap(),
        )
        .unwrap();
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        fs::write(&manifest_path, &manifest_bytes).unwrap();

        let generation_value = serde_json::to_value(generation).unwrap();
        let manifest_digest = serde_json::to_value(DigestV1::hash_bytes(&manifest_bytes)).unwrap();
        let mut matching_activations = 0;
        for entry in fs::read_dir(paths.index_root().join("activations")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let mut activation: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if activation["generation"] != generation_value {
                continue;
            }
            activation["manifest_digest"] = manifest_digest.clone();
            fs::write(path, serde_json::to_vec(&activation).unwrap()).unwrap();
            matching_activations += 1;
        }
        assert!(matching_activations > 0);
    }

    fn assert_publish_failpoint_is_atomic(failpoint: GenerationFailpoint) {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project.clone(), Some(temporary.path().join("index")), None)
                .unwrap();
        let index =
            SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
        let baseline = index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .generation
            .unwrap();
        assert_baseline_generation(&index, &baseline);

        fs::write(project.join(OWNER_PATH), OWNER_AFTER).unwrap();
        index.inject_generation_failpoint(failpoint).unwrap();
        let failed = index.reindex(
            FilesystemReindexIntent::changed_paths(vec![PathBuf::from(OWNER_PATH)]),
            &mut AssetLoadBudget::default(),
        );
        assert!(failed.is_err(), "{failpoint:?} must stop publication");
        let failed_active = index
            .status()
            .unwrap()
            .generation
            .active
            .expect("failed publication must keep an active generation");
        assert_eq!(failed_active.generation, baseline.generation);
        assert_eq!(failed_active.actual_revision, baseline.actual_revision);
        assert_ne!(failed_active.desired_revision, baseline.actual_revision);
        assert!(failed_active.stale);
        assert_eq!(search_paths(&index, "Before"), vec![OWNER_PATH.to_owned()]);
        assert!(search_paths(&index, "After").is_empty());
        assert_eq!(
            incoming_paths(&index, TARGET_GUID),
            vec![OWNER_PATH.to_owned()]
        );
        assert!(incoming_paths(&index, OTHER_GUID).is_empty());
        assert!(index.status().unwrap().generation.last_failure.is_some());

        drop(index);
        let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        assert_baseline_generation(&reopened, &failed_active);
        assert!(reopened.status().unwrap().generation.last_failure.is_none());
        let recovered = reopened
            .reindex(
                FilesystemReindexIntent::changed_paths(vec![PathBuf::from(OWNER_PATH)]),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .generation
            .unwrap();
        assert_ne!(recovered.generation, baseline.generation);
        assert_eq!(
            search_paths(&reopened, "After"),
            vec![OWNER_PATH.to_owned()]
        );
        assert!(search_paths(&reopened, "Before").is_empty());
        assert_eq!(
            incoming_paths(&reopened, OTHER_GUID),
            vec![OWNER_PATH.to_owned()]
        );
        assert!(incoming_paths(&reopened, TARGET_GUID).is_empty());
    }

    #[test]
    fn staging_cleanup_failure_is_distinct_and_reconcile_clears_it() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project, Some(temporary.path().join("index")), None).unwrap();
        let index = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        index
            .inject_generation_failpoint(GenerationFailpoint::ActivationCleanup)
            .unwrap();

        let published = index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(!published.evidence.publish_warnings.is_empty());
        let status = index.status().unwrap();
        assert!(status.generation.last_failure.is_none());
        assert_eq!(
            status.daemon.generation_maintenance.state,
            GenerationMaintenanceState::RecoveryRequired
        );
        assert!(
            status
                .daemon
                .generation_maintenance
                .last_cleanup_failure
                .is_some()
        );

        index
            .reindex(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let recovered = index.status().unwrap();
        assert_eq!(
            recovered.daemon.generation_maintenance.state,
            GenerationMaintenanceState::Clean
        );
        assert!(
            recovered
                .daemon
                .generation_maintenance
                .last_recovered_entries
                >= 1
        );
        assert!(
            recovered
                .daemon
                .generation_maintenance
                .last_cleanup_failure
                .is_none()
        );
        assert!(recovered.generation.last_failure.is_none());
    }

    #[test]
    fn runtime_build_guard_clears_building_state_during_unwind() {
        let status = RwLock::new(StatusObservation {
            active: None,
            runtime: RuntimeStatus::default(),
        });

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _guard = RuntimeBuildGuard::start(&status, None).unwrap();
            panic!("injected build panic");
        }));

        assert!(panic.is_err());
        let status = status.read().unwrap();
        assert!(!status.runtime.indexing);
        assert_eq!(status.runtime.building_revision, None);
    }

    #[test]
    fn every_generation_publish_phase_preserves_the_previous_active_generation() {
        for failpoint in [
            GenerationFailpoint::Search,
            GenerationFailpoint::References,
            GenerationFailpoint::SourceState,
            GenerationFailpoint::Activation,
        ] {
            assert_publish_failpoint_is_atomic(failpoint);
        }
    }

    #[test]
    fn older_reference_projection_rebuilds_without_reusing_its_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project, Some(temporary.path().join("index")), None).unwrap();
        let index =
            SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
        let baseline = index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .generation
            .unwrap();
        assert_baseline_generation(&index, &baseline);
        drop(index);

        rewrite_reference_marker_as_legacy(&paths, baseline.generation);

        let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        assert!(reopened.status().unwrap().generation.active.is_none());

        let receipt = reopened
            .reindex(
                FilesystemReindexIntent::reconcile(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert!(receipt.evidence.forced_full_scan);
        let rebuilt = receipt.generation.unwrap();
        assert_ne!(rebuilt.generation, baseline.generation);
        assert_baseline_generation(&reopened, &rebuilt);
    }

    #[test]
    fn late_receipt_rebuild_from_legacy_projection_preserves_store_desired_revision() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project.clone(), Some(temporary.path().join("index")), None)
                .unwrap();
        let index =
            SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
        let baseline = index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .generation
            .unwrap();

        let mut workspace =
            AssetWorkspace::with_workspace_id(baseline.workspace, WorkspaceOptions::lenient())
                .unwrap();
        let owner = workspace
            .load_source(
                SourceOpenRequest::new(
                    project.join(OWNER_PATH),
                    SourceAlias::new(OWNER_PATH.to_owned()).unwrap(),
                )
                .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        workspace
            .load_source(
                SourceOpenRequest::new(
                    project.join(TARGET_PATH),
                    SourceAlias::new(TARGET_PATH.to_owned()).unwrap(),
                )
                .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(workspace.revision(), baseline.actual_revision);

        let future_revision = WorkspaceRevision::new(DigestV1::hash_bytes(b"future-revision"));
        let future = ChangeSet::new(
            TransactionId::new(DigestV1::hash_bytes(b"future-transaction")),
            baseline.workspace,
            baseline.actual_revision,
            future_revision,
            vec![owner],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let error = index
            .reindex_workspace(
                future,
                &workspace.snapshot(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap_err();
        assert_eq!(error.code(), ApiErrorCode::RevisionMismatch);
        let stale = index.status().unwrap().generation.active.unwrap();
        assert_eq!(stale.actual_revision, baseline.actual_revision);
        assert_eq!(stale.desired_revision, future_revision);
        assert!(stale.stale);
        drop(index);

        rewrite_reference_marker_as_legacy(&paths, baseline.generation);
        let reopened =
            SearchIndex::open_or_create(paths.clone(), &mut AssetLoadBudget::default()).unwrap();
        assert!(reopened.status().unwrap().generation.active.is_none());

        let late = ChangeSet::new(
            TransactionId::new(DigestV1::hash_bytes(b"late-transaction")),
            baseline.workspace,
            WorkspaceRevision::new(DigestV1::hash_bytes(b"prior-revision")),
            baseline.actual_revision,
            vec![owner],
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let receipt = reopened
            .reindex_workspace(late, &workspace.snapshot(), &mut AssetLoadBudget::default())
            .unwrap();
        let rebuilt = receipt.generation.unwrap();
        assert_ne!(rebuilt.generation, baseline.generation);
        assert_eq!(rebuilt.actual_revision, baseline.actual_revision);
        assert_eq!(rebuilt.desired_revision, future_revision);
        assert!(rebuilt.stale);
        assert_eq!(
            reopened.status().unwrap().generation.active.as_ref(),
            Some(&rebuilt)
        );
        drop(reopened);

        let reopened = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(
            reopened.status().unwrap().generation.active.as_ref(),
            Some(&rebuilt)
        );
    }

    #[test]
    fn public_suggest_validates_prefix_and_limit_boundaries() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project, Some(temporary.path().join("index")), None).unwrap();
        let index = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        let empty = index.suggest("", 1).unwrap();
        assert!(empty.suggestions.is_empty());

        let normal = index.suggest("t:g", 1).unwrap();
        assert!(normal.suggestions.len() <= 1);

        let maximum_prefix = "x".repeat(MAX_SUGGEST_PREFIX_BYTES);
        assert_eq!(
            index
                .suggest(&maximum_prefix, MAX_SUGGEST_LIMIT)
                .unwrap()
                .prefix,
            maximum_prefix
        );

        let one_over_prefix = "x".repeat(MAX_SUGGEST_PREFIX_BYTES + 1);
        let prefix_error = index.suggest(&one_over_prefix, 1).unwrap_err();
        assert_eq!(prefix_error.code(), ApiErrorCode::InvalidRequest);
        assert_eq!(
            prefix_error.api_error().details.get("field"),
            Some(&"prefix_bytes".to_owned())
        );

        for limit in [0, MAX_SUGGEST_LIMIT + 1, usize::MAX] {
            let error = index.suggest("t:", limit).unwrap_err();
            assert_eq!(error.code(), ApiErrorCode::InvalidRequest);
            assert_eq!(
                error.api_error().details.get("field"),
                Some(&"limit".to_owned())
            );
        }
    }

    #[test]
    fn public_search_validates_the_raw_query_and_limit_boundaries() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project, Some(temporary.path().join("index")), None).unwrap();
        let index = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();

        let exact = "x".repeat(MAX_SEARCH_QUERY_BYTES);
        assert_eq!(
            index
                .search(SearchRequest::new(exact.clone(), MAX_SEARCH_LIMIT))
                .unwrap()
                .query,
            exact
        );

        let one_over = format!("{} ", "x".repeat(MAX_SEARCH_QUERY_BYTES));
        let query_error = index.search(SearchRequest::new(one_over, 1)).unwrap_err();
        assert_eq!(query_error.code(), ApiErrorCode::InvalidRequest);
        assert_eq!(
            query_error.api_error().details.get("field"),
            Some(&"query_bytes".to_owned())
        );

        let limit_error = index
            .search(SearchRequest::new("x", MAX_SEARCH_LIMIT + 1))
            .unwrap_err();
        assert_eq!(limit_error.code(), ApiErrorCode::InvalidRequest);
        assert_eq!(
            limit_error.api_error().details.get("field"),
            Some(&"limit".to_owned())
        );
    }

    #[test]
    fn public_errors_and_recorded_failures_bound_long_utf8_sources() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let error = PipelineError::RelativePathCollision {
            relative_path: "界".repeat(MAX_ERROR_MESSAGE_BYTES),
            first: SourceId::new(workspace, SourceKind::Yaml, 1).unwrap(),
            second: SourceId::new(workspace, SourceKind::Yaml, 2).unwrap(),
        };
        let source_message = error.to_string();
        let wrapped = SearchIndexError::from_pipeline(error, None);

        assert!(source_message.len() > MAX_ERROR_MESSAGE_BYTES);
        assert!(wrapped.to_string().len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(wrapped.to_string().ends_with("... [truncated]"));
        wrapped.api_error().validate().unwrap();
        assert_eq!(
            wrapped.source().unwrap().to_string(),
            source_message,
            "the full diagnostic remains available through the source chain"
        );

        GenerationFailure {
            code: "index_build_failed".to_owned(),
            message: wrapped.to_string(),
            retryable: false,
            failed_unix_ms: 1,
            desired_revision: None,
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn status_commit_is_an_atomic_observation_boundary() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        write_generation_fixture(&project);
        let paths =
            IndexPaths::for_project(project.clone(), Some(temporary.path().join("index")), None)
                .unwrap();
        let index = SearchIndex::open_or_create(paths, &mut AssetLoadBudget::default()).unwrap();
        let baseline = index
            .reindex(
                FilesystemReindexIntent::full(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap()
            .generation
            .unwrap();

        fs::write(project.join(OWNER_PATH), OWNER_AFTER).unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = mpsc::sync_channel(0);
        index.inject_status_commit_hook(entered_tx, resume_rx);

        let build_index = index.clone();
        let build = thread::spawn(move || {
            build_index.reindex(
                FilesystemReindexIntent::changed_paths(vec![PathBuf::from(OWNER_PATH)]),
                &mut AssetLoadBudget::default(),
            )
        });
        entered_rx.recv().unwrap();

        let (attempted_tx, attempted_rx) = mpsc::sync_channel(0);
        let (status_tx, status_rx) = mpsc::sync_channel(0);
        let status_index = index.clone();
        let reader = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            status_tx.send(status_index.status()).unwrap();
        });
        attempted_rx.recv().unwrap();

        assert!(matches!(
            status_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        resume_tx.send(()).unwrap();

        let generation = build.join().unwrap().unwrap().generation.unwrap();
        let status = status_rx.recv().unwrap().unwrap();
        reader.join().unwrap();

        assert_ne!(generation, baseline);
        assert_eq!(status.generation.active, Some(generation));
        assert!(!status.indexing);
        assert!(status.generation.building_revision.is_none());
        assert!(status.last_build_unix_ms.is_some());
        assert!(status.generation.last_failure.is_none());
    }
}
