use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tantivy::HasLen;
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, OwnedBytes, WatchCallback, WatchHandle, WritePtr,
    error::{DeleteError, LockError, OpenReadError, OpenWriteError},
};
use tantivy::schema::{FAST, Field, INDEXED, STORED, STRING, Schema, TEXT};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument, TantivyError};
use unity_asset_core::{
    AssetLoadBudget, ContractJsonLimits, ContractJsonResourceModel, DigestV1, DigestV1Builder,
    ObjectAddress, SourceLocator, read_contract_json,
};
use unity_asset_search_core::normalize_for_match;
use unity_asset_search_protocol::{MAX_PORTABLE_PATH_BYTES, PortablePath};

use crate::analysis::{RawReferenceProjection, ReferenceResolutionProjection};
use crate::anchored_fs::{
    AnchoredFsError as SecureReadError, OpenPolicy, ReadDirectory as SecureReadDirectory,
    RegularFile,
};
use crate::generation::{
    ArtifactTreeEvidence, GenerationArtifactEvidence, GenerationProjectionDigests,
    SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
};
use crate::generation_store::measure_artifact_tree;
use crate::projection::{GenerationProjection, ReferenceDocument, SearchDocument};
use crate::reference_payload::{
    REFERENCE_PAYLOAD_FILE, ReferencePayloadLocation, ReferencePayloadReader,
    ReferencePayloadWriter,
};
use crate::wire;

const SEARCH_ARTIFACT_DIRECTORY: &str = "search";
const REFERENCE_ARTIFACT_DIRECTORY: &str = "references";
const SCHEMA_MARKER_FILE: &str = "schema-contract.json";
const PATH_CATALOG_FILE: &str = "unity-asset-path-catalog-v1.bin";
const PATH_CATALOG_MAGIC: &[u8] = b"unity-asset:path-catalog:v1\0";
const SEARCH_SCHEMA_CONTRACT: &str = "unity-asset.search-projection";
const REFERENCE_SCHEMA_CONTRACT: &str = "unity-asset.reference-projection";
pub(crate) const SEARCH_SCHEMA_VERSION: u16 = 1;
pub(crate) const REFERENCE_SCHEMA_VERSION: u16 = 4;
const MAX_SCHEMA_MARKER_BYTES: u64 = 16 * 1024;
// The fixed reserve covers one maximum-sized contract string, its typed copy, and Serde scratch.
const SCHEMA_MARKER_JSON_RESOURCES: ContractJsonResourceModel =
    ContractJsonResourceModel::new(6, 4 * 1024, 32 * 1024, 256);
const SCHEMA_MARKER_JSON_LIMITS: ContractJsonLimits = ContractJsonLimits::new(
    "search.projection_schema_marker",
    MAX_SCHEMA_MARKER_BYTES as usize,
    2,
    4,
    3,
    SCHEMA_MARKER_JSON_RESOURCES,
);
const SEARCH_LOGICAL_DOMAIN: &[u8] = b"unity-asset:search-generation:search-projection:v1\0";
const REFERENCE_LOGICAL_DOMAIN: &[u8] = b"unity-asset:search-generation:reference-projection:v3\0";
const MIN_WRITER_MEMORY_PER_THREAD: usize = 15_000_000;
const MAX_WRITER_MEMORY_PER_THREAD: usize = u32::MAX as usize - 1_000_000;
const MAX_WRITER_THREADS: usize = 8;
// The path catalog exposes this bound before allocating an entry buffer.
const MAX_PROJECTED_PATH_BYTES: usize = MAX_PORTABLE_PATH_BYTES;

/// A completed projection whose on-disk layout is safe to discard and rebuild.
///
/// This deliberately identifies only an older reference projection schema under the current
/// generation contract. Corruption, I/O, allocation, and security failures remain fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RebuildableProjectionSchemaVersion {
    actual: u16,
    expected: u16,
}

impl std::fmt::Display for RebuildableProjectionSchemaVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "reference projection schema version {} is older than required version {}",
            self.actual, self.expected
        )
    }
}

impl std::error::Error for RebuildableProjectionSchemaVersion {}

pub(crate) fn is_rebuildable_projection_schema_version(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RebuildableProjectionSchemaVersion>()
        .is_some()
}

/// Bounded Tantivy writer settings for immutable projection materialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionStoreOptions {
    pub(crate) search_writer_threads: usize,
    pub(crate) search_writer_memory_bytes: usize,
    pub(crate) reference_writer_threads: usize,
    pub(crate) reference_writer_memory_bytes: usize,
    pub(crate) max_reference_fact_json_bytes: usize,
}

impl Default for ProjectionStoreOptions {
    fn default() -> Self {
        Self {
            search_writer_threads: 1,
            search_writer_memory_bytes: 32 * 1024 * 1024,
            reference_writer_threads: 1,
            reference_writer_memory_bytes: 32 * 1024 * 1024,
            max_reference_fact_json_bytes: 1024 * 1024,
        }
    }
}

impl ProjectionStoreOptions {
    fn validate(self) -> Result<Self> {
        validate_writer_options(
            "search",
            self.search_writer_threads,
            self.search_writer_memory_bytes,
        )?;
        validate_writer_options(
            "reference",
            self.reference_writer_threads,
            self.reference_writer_memory_bytes,
        )?;
        ensure!(
            self.max_reference_fact_json_bytes > 0,
            "reference fact JSON limit must be greater than zero"
        );
        Ok(self)
    }
}

/// Logical projection identity paired with the physical Tantivy artifact evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectionArtifactEvidence {
    logical_digests: GenerationProjectionDigests,
    search_artifact: ArtifactTreeEvidence,
    reference_artifact: ArtifactTreeEvidence,
}

impl ProjectionArtifactEvidence {
    pub(crate) const fn logical_digests(self) -> GenerationProjectionDigests {
        self.logical_digests
    }

    #[cfg(test)]
    pub(crate) const fn search_artifact(self) -> ArtifactTreeEvidence {
        self.search_artifact
    }

    #[cfg(test)]
    pub(crate) const fn reference_artifact(self) -> ArtifactTreeEvidence {
        self.reference_artifact
    }

    pub(crate) const fn generation_artifacts(
        self,
        source_state: ArtifactTreeEvidence,
    ) -> GenerationArtifactEvidence {
        GenerationArtifactEvidence::new(self.search_artifact, self.reference_artifact, source_state)
    }
}

/// Materializes one complete search/reference projection into a caller-owned staging generation.
pub(crate) struct ProjectionStore;

impl ProjectionStore {
    pub(crate) fn build(
        staging_generation_dir: &Path,
        projection: &GenerationProjection,
    ) -> Result<ProjectionArtifactEvidence> {
        Self::build_with_options(
            staging_generation_dir,
            projection,
            ProjectionStoreOptions::default(),
        )
    }

    pub(crate) fn build_with_options(
        staging_generation_dir: &Path,
        projection: &GenerationProjection,
        options: ProjectionStoreOptions,
    ) -> Result<ProjectionArtifactEvidence> {
        let options = options.validate()?;
        let logical_digests = logical_projection_digests(projection)?;
        ensure_directory_no_follow(staging_generation_dir)
            .context("validate projection staging generation directory")?;

        let search_directory = staging_generation_dir.join(SEARCH_ARTIFACT_DIRECTORY);
        let reference_directory = staging_generation_dir.join(REFERENCE_ARTIFACT_DIRECTORY);
        prepare_empty_artifact_directory(&search_directory)
            .context("prepare staged search artifact directory")?;
        prepare_empty_artifact_directory(&reference_directory)
            .context("prepare staged reference artifact directory")?;

        build_search_index(&search_directory, projection, options)
            .context("materialize staged search projection")?;
        build_reference_index(&reference_directory, projection, options)
            .context("materialize staged reference projection")?;

        let search_artifact =
            measure_artifact_tree(&search_directory).context("measure staged search projection")?;
        let reference_artifact = measure_artifact_tree(&reference_directory)
            .context("measure staged reference projection")?;

        Ok(ProjectionArtifactEvidence {
            logical_digests,
            search_artifact,
            reference_artifact,
        })
    }
}

fn validate_projection(projection: &GenerationProjection) -> Result<()> {
    for pair in projection.search_documents.windows(2) {
        ensure!(
            pair[0].stable_id < pair[1].stable_id,
            "search projection stable IDs must be strictly ordered and unique: {:?}, {:?}",
            pair[0].stable_id,
            pair[1].stable_id
        );
    }
    for pair in projection.reference_documents.windows(2) {
        ensure!(
            pair[0].stable_id < pair[1].stable_id,
            "reference projection stable IDs must be strictly ordered and unique: {:?}, {:?}",
            pair[0].stable_id,
            pair[1].stable_id
        );
    }
    for document in &projection.search_documents {
        validate_projected_path(
            "search projection path",
            &document.stable_id,
            &document.path,
        )?;
    }
    for document in &projection.reference_documents {
        validate_reference_paths(document)?;
    }
    Ok(())
}

fn validate_reference_paths(document: &ReferenceDocument) -> Result<()> {
    validate_projected_path(
        "reference projection source path",
        &document.stable_id,
        &document.source_path,
    )?;
    validate_address_path(
        "reference fact source object",
        &document.stable_id,
        &document.fact.source_object,
    )?;
    match &document.fact.raw_target {
        RawReferenceProjection::Binary {
            external: Some(external),
            ..
        } if !external.path.is_empty() => {
            validate_projected_path(
                "reference binary external path",
                &document.stable_id,
                &external.path,
            )?;
        }
        RawReferenceProjection::Binary { .. } | RawReferenceProjection::Yaml { .. } => {}
    }

    match &document.fact.resolution {
        ReferenceResolutionProjection::Resolved { target }
        | ReferenceResolutionProjection::Missing {
            target: Some(target),
        } => validate_address_path("reference resolution target", &document.stable_id, target)?,
        ReferenceResolutionProjection::Unloaded {
            source: Some(source),
        } => validate_locator_path("reference unloaded source", &document.stable_id, source)?,
        ReferenceResolutionProjection::Ambiguous { candidates } => {
            for candidate in candidates {
                validate_address_path(
                    "reference ambiguous candidate",
                    &document.stable_id,
                    candidate,
                )?;
            }
        }
        ReferenceResolutionProjection::Null
        | ReferenceResolutionProjection::Unloaded { source: None }
        | ReferenceResolutionProjection::Missing { target: None }
        | ReferenceResolutionProjection::Invalid => {}
    }
    Ok(())
}

fn validate_projected_path(kind: &str, stable_id: &str, path: &str) -> Result<()> {
    PortablePath::validate(path).with_context(|| {
        format!(
            "{kind} for `{stable_id}` has {} bytes and violates the portable path contract with a \
             {MAX_PROJECTED_PATH_BYTES}-byte maximum",
            path.len()
        )
    })
}

fn validate_address_path(kind: &str, stable_id: &str, address: &ObjectAddress) -> Result<()> {
    validate_locator_path(kind, stable_id, address.source_locator())
}

fn validate_locator_path(kind: &str, stable_id: &str, locator: &SourceLocator) -> Result<()> {
    wire::portable_locator(locator)
        .map(|_| ())
        .with_context(|| format!("{kind} for `{stable_id}` violates the portable path contract"))
}

/// Read-only Tantivy directory rooted at an already-opened generation child directory.
///
/// Tantivy's mmap directory reopens every segment by pathname. Completed generations are
/// untrusted persisted input, so doing that after pinning the generation would reintroduce a
/// replacement race. This adapter accepts only one ordinary leaf name and resolves it through
/// `SecureReadDirectory`; its file handles retain the exact opened descriptor.
#[derive(Clone)]
struct AnchoredTantivyDirectory {
    directory: Arc<SecureReadDirectory>,
}

impl AnchoredTantivyDirectory {
    fn new(directory: Arc<SecureReadDirectory>) -> Self {
        Self { directory }
    }

    fn leaf(path: &Path) -> io::Result<&OsStr> {
        let mut components = path.components();
        let Some(std::path::Component::Normal(name)) = components.next() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tantivy requested an invalid anchored index path",
            ));
        };
        if components.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tantivy requested a nested anchored index path",
            ));
        }
        Ok(name)
    }

    fn open_error(path: &Path, error: SecureReadError) -> OpenReadError {
        match error {
            SecureReadError::Io(source) if source.kind() == io::ErrorKind::NotFound => {
                OpenReadError::FileDoesNotExist(path.to_path_buf())
            }
            source => OpenReadError::wrap_io_error(
                io::Error::other(format!("anchored Tantivy read rejected: {source}")),
                path.to_path_buf(),
            ),
        }
    }

    fn rejected_write(path: &Path) -> OpenWriteError {
        OpenWriteError::wrap_io_error(
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "completed generation Tantivy directories are read-only",
            ),
            path.to_path_buf(),
        )
    }
}

impl std::fmt::Debug for AnchoredTantivyDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AnchoredTantivyDirectory(..)")
    }
}

impl Directory for AnchoredTantivyDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let name = Self::leaf(path)
            .map_err(|error| OpenReadError::wrap_io_error(error, path.to_path_buf()))?;
        let file = self
            .directory
            .open_regular(name)
            .map_err(|error| Self::open_error(path, error))?;
        Ok(Arc::new(AnchoredTantivyFile {
            file: Arc::new(file),
        }))
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "completed generation Tantivy directories are read-only",
            )),
            filepath: path.to_path_buf(),
        })
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        let name = Self::leaf(path)
            .map_err(|error| OpenReadError::wrap_io_error(error, path.to_path_buf()))?;
        match self.directory.open_regular(name) {
            Ok(_) => Ok(true),
            Err(SecureReadError::Io(source)) if source.kind() == io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(error) => Err(Self::open_error(path, error)),
        }
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(Self::rejected_write(path))
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        let handle = self.get_file_handle(path)?;
        handle
            .read_bytes(0..handle.len())
            .map(|bytes| bytes.as_slice().to_vec())
            .map_err(|error| OpenReadError::wrap_io_error(error, path.to_path_buf()))
    }

    fn atomic_write(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "completed generation Tantivy directories are read-only",
        ))
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        // Completed generations are immutable and already protected by the generation-store
        // writer lease. Tantivy readers still request META_LOCK while opening a snapshot, but its
        // default implementation would create a lockfile through this deliberately read-only
        // adapter. A process-local guard preserves the reader protocol without enabling writes.
        Ok(DirectoryLock::from(Box::new(())))
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Err(TantivyError::SystemError(
            "completed generation Tantivy directories do not support watching".to_owned(),
        ))
    }
}

#[derive(Debug)]
struct AnchoredTantivyFile {
    file: Arc<RegularFile>,
}

impl HasLen for AnchoredTantivyFile {
    fn len(&self) -> usize {
        usize::try_from(self.file.length()).unwrap_or(usize::MAX)
    }
}

#[async_trait]
impl FileHandle for AnchoredTantivyFile {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        if range.start > range.end || range.end > self.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tantivy requested an invalid anchored file range",
            ));
        }
        let offset = u64::try_from(range.start)
            .map_err(|_| io::Error::other("Tantivy file range offset exceeds u64"))?;
        let length = u64::try_from(range.end - range.start)
            .map_err(|_| io::Error::other("Tantivy file range length exceeds u64"))?;
        if range.is_empty() {
            return Ok(OwnedBytes::new(Vec::new()));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(range.end - range.start)
            .map_err(|error| {
                io::Error::other(format!("reserve anchored Tantivy range: {error}"))
            })?;
        bytes.resize(range.end - range.start, 0);
        let mut reader = self.file.range(offset, length).map_err(|error| {
            io::Error::other(format!("validate anchored Tantivy range: {error}"))
        })?;
        reader.read_exact(&mut bytes)?;
        self.file.ensure_unchanged().map_err(|error| {
            io::Error::other(format!("revalidate anchored Tantivy file: {error}"))
        })?;
        Ok(OwnedBytes::new(bytes))
    }
}

/// A pair of immutable readers pinned to one completed generation directory.
pub(crate) struct ProjectionReaders {
    search: SearchProjectionReader,
    references: ReferenceProjectionReader,
}

impl ProjectionReaders {
    pub(crate) fn open(
        complete_generation_dir: &Path,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        ensure_directory_no_follow(complete_generation_dir)
            .context("validate completed projection generation directory")?;

        let search_directory = complete_generation_dir.join(SEARCH_ARTIFACT_DIRECTORY);
        let reference_directory = complete_generation_dir.join(REFERENCE_ARTIFACT_DIRECTORY);
        let generation =
            SecureReadDirectory::open(complete_generation_dir, OpenPolicy::PersistedState)
                .context(
                    "open completed projection generation directory without following links",
                )?;
        let opened_search = Arc::new(
            generation
                .open_directory(OsStr::new(SEARCH_ARTIFACT_DIRECTORY))
                .context("open completed search artifact directory without following links")?,
        );
        let opened_references = Arc::new(
            generation
                .open_directory(OsStr::new(REFERENCE_ARTIFACT_DIRECTORY))
                .context("open completed reference artifact directory without following links")?,
        );

        let search = SearchProjectionReader::open(&search_directory, &opened_search, budget)?;
        let references =
            ReferenceProjectionReader::open(&reference_directory, &opened_references, budget)?;
        Ok(Self { search, references })
    }

    pub(crate) const fn search(&self) -> &SearchProjectionReader {
        &self.search
    }

    pub(crate) const fn references(&self) -> &ReferenceProjectionReader {
        &self.references
    }
}

pub(crate) struct SearchProjectionReader {
    index: Index,
    reader: IndexReader,
    path_catalog: Arc<RegularFile>,
}

impl SearchProjectionReader {
    fn open(
        directory: &Path,
        opened_directory: &Arc<SecureReadDirectory>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        validate_schema_marker(
            directory,
            opened_directory,
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            budget,
        )?;
        let index = Index::open(AnchoredTantivyDirectory::new(Arc::clone(opened_directory)))
            .with_context(|| {
                format!(
                    "open search projection index through its anchored directory at {}",
                    directory.display()
                )
            })?;
        let expected = search_schema();
        validate_schema(&index.schema(), &expected, SEARCH_SCHEMA_CONTRACT)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("open immutable search projection reader")?;
        let searcher = reader.searcher();
        ensure!(
            searcher
                .segment_readers()
                .iter()
                .all(|segment| segment.num_docs() == segment.max_doc()),
            "immutable search projection contains deleted documents"
        );
        let (path_catalog, catalog_documents) = open_path_catalog(opened_directory, directory)?;
        ensure!(
            catalog_documents == searcher.num_docs(),
            "path catalog contains {catalog_documents} documents but the search projection has {}",
            searcher.num_docs()
        );
        let mut catalog_validation = path_catalog
            .range(0, path_catalog.length())
            .context("open anchored path catalog validation range")?;
        let _ =
            read_path_catalog_header(&mut catalog_validation, path_catalog.length(), directory)?;
        validate_path_catalog_entries(&mut catalog_validation, catalog_documents)?;
        path_catalog
            .ensure_unchanged()
            .context("revalidate anchored path catalog after validation")?;
        Ok(Self {
            index,
            reader,
            path_catalog: Arc::new(path_catalog),
        })
    }

    pub(crate) const fn index(&self) -> &Index {
        &self.index
    }

    pub(crate) const fn reader(&self) -> &IndexReader {
        &self.reader
    }

    /// Returns the live generation paths using only the persisted search projection.
    ///
    /// The caller's load budget covers traversal entries, output members, and every retained
    /// output allocation. The catalog exposes each path length before allocation, so malformed
    /// persisted data cannot force Tantivy or Serde to materialize an unbounded value first.
    pub(crate) fn stored_paths(&self, budget: &mut AssetLoadBudget) -> Result<Vec<String>> {
        let searcher = self.reader.searcher();
        ensure!(
            searcher
                .segment_readers()
                .iter()
                .all(|segment| segment.num_docs() == segment.max_doc()),
            "immutable search projection contains deleted documents"
        );
        let live_document_count = searcher.num_docs();
        let mut catalog = self
            .path_catalog
            .range(0, self.path_catalog.length())
            .context("open anchored path catalog for stored paths")?;
        let catalog_document_count = read_path_catalog_header(
            &mut catalog,
            self.path_catalog.length(),
            Path::new(PATH_CATALOG_FILE),
        )?;
        ensure!(
            catalog_document_count == live_document_count,
            "path catalog contains {catalog_document_count} documents but the search projection \
             has {live_document_count}"
        );
        let output_capacity = usize::try_from(catalog_document_count)
            .context("live search projection document count exceeds the platform address space")?;
        let output_backing_bytes = size_of::<String>()
            .checked_mul(output_capacity)
            .ok_or_else(|| anyhow!("stored path output backing size overflow"))?;
        let output_backing_bytes = u64::try_from(output_backing_bytes)
            .context("stored path output backing exceeds the load-budget address space")?;

        budget
            .check_entries(catalog_document_count)
            .context("preflight stored path document entries")?;
        budget
            .check_members(catalog_document_count)
            .context("preflight stored path output members")?;
        budget
            .check_bytes(output_backing_bytes)
            .context("preflight stored path output backing")?;
        budget
            .consume_entries(catalog_document_count)
            .context("charge stored path document entries")?;
        budget
            .consume_members(catalog_document_count)
            .context("charge stored path output members")?;
        budget
            .consume_bytes(output_backing_bytes)
            .context("charge stored path output backing")?;

        let mut paths = Vec::new();
        paths.try_reserve_exact(output_capacity).map_err(|error| {
            anyhow!("reserve {output_backing_bytes} bytes for stored path output backing: {error}")
        })?;

        for ordinal in 0..catalog_document_count {
            let path_length = read_u32(&mut catalog, "read path catalog entry length")?;
            let path_length = usize::try_from(path_length)
                .context("path catalog entry length exceeds the platform address space")?;
            ensure!(
                (1..=MAX_PROJECTED_PATH_BYTES).contains(&path_length),
                "path catalog entry {ordinal} has {path_length} bytes; expected 1..=\
                 {MAX_PROJECTED_PATH_BYTES}"
            );
            let path_bytes = u64::try_from(path_length)
                .context("stored path length exceeds the load-budget address space")?;
            budget.consume_bytes(path_bytes).with_context(|| {
                format!("charge stored path string for catalog entry {ordinal}")
            })?;

            let mut encoded = Vec::new();
            encoded.try_reserve_exact(path_length).map_err(|error| {
                anyhow!(
                    "reserve {path_length} bytes for stored path catalog entry {ordinal}: {error}"
                )
            })?;
            encoded.resize(path_length, 0);
            catalog
                .read_exact(&mut encoded)
                .with_context(|| format!("read stored path bytes for catalog entry {ordinal}"))?;
            let path = String::from_utf8(encoded)
                .with_context(|| format!("decode stored path UTF-8 for catalog entry {ordinal}"))?;
            paths.push(path);
        }

        let mut trailing = [0_u8; 1];
        ensure!(
            catalog
                .read(&mut trailing)
                .context("check path catalog end")?
                == 0,
            "path catalog contains trailing bytes after {catalog_document_count} entries"
        );
        self.path_catalog
            .ensure_unchanged()
            .context("revalidate anchored path catalog after stored path read")?;
        paths.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        paths.dedup();
        Ok(paths)
    }
}

pub(crate) struct ReferenceProjectionReader {
    reader: IndexReader,
    fields: ReferenceProjectionFields,
    payloads: ReferencePayloadReader,
}

impl ReferenceProjectionReader {
    fn open(
        directory: &Path,
        opened_directory: &Arc<SecureReadDirectory>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self> {
        validate_schema_marker(
            directory,
            opened_directory,
            REFERENCE_SCHEMA_CONTRACT,
            REFERENCE_SCHEMA_VERSION,
            budget,
        )?;
        let index = Index::open(AnchoredTantivyDirectory::new(Arc::clone(opened_directory)))
            .with_context(|| {
                format!(
                    "open reference projection index through its anchored directory at {}",
                    directory.display()
                )
            })?;
        let expected = reference_schema();
        validate_schema(&index.schema(), &expected, REFERENCE_SCHEMA_CONTRACT)?;
        let fields = ReferenceProjectionFields::from_schema(&expected)?;
        let payload_path = directory.join(REFERENCE_PAYLOAD_FILE);
        let payload = opened_directory
            .open_regular(OsStr::new(REFERENCE_PAYLOAD_FILE))
            .with_context(|| {
                format!(
                    "open reference payload sidecar without following links at {}",
                    payload_path.display()
                )
            })?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .doc_store_cache_num_blocks(0)
            .try_into()
            .context("open immutable reference projection reader")?;
        Ok(Self {
            reader,
            fields,
            payloads: ReferencePayloadReader::new(payload),
        })
    }

    pub(crate) const fn reader(&self) -> &IndexReader {
        &self.reader
    }

    pub(crate) const fn fields(&self) -> &ReferenceProjectionFields {
        &self.fields
    }

    pub(crate) const fn payloads(&self) -> &ReferencePayloadReader {
        &self.payloads
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchProjectionFields {
    schema_version: Field,
    id: Field,
    guid: Field,
    path: Field,
    path_filter: Field,
    path_terms: Field,
    name: Field,
    name_terms: Field,
    kind: Field,
    kind_filter: Field,
    kind_terms: Field,
    content_terms: Field,
    hierarchy_paths: Field,
    script_symbols: Field,
    container_source_path: Field,
}

impl SearchProjectionFields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            schema_version: schema.get_field("schema_version")?,
            id: schema.get_field("id")?,
            guid: schema.get_field("guid")?,
            path: schema.get_field("path")?,
            path_filter: schema.get_field("path_filter")?,
            path_terms: schema.get_field("path_terms")?,
            name: schema.get_field("name")?,
            name_terms: schema.get_field("name_terms")?,
            kind: schema.get_field("kind")?,
            kind_filter: schema.get_field("kind_filter")?,
            kind_terms: schema.get_field("kind_terms")?,
            content_terms: schema.get_field("content_terms")?,
            hierarchy_paths: schema.get_field("hierarchy_paths")?,
            script_symbols: schema.get_field("script_symbols")?,
            container_source_path: schema.get_field("container_source_path")?,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReferenceProjectionFields {
    stable_id: Field,
    incoming_key: Field,
    outgoing_key: Field,
    payload_offset: Field,
    payload_length: Field,
    payload_digest: Field,
}

impl ReferenceProjectionFields {
    fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            stable_id: schema.get_field("stable_id")?,
            incoming_key: schema.get_field("incoming_key")?,
            outgoing_key: schema.get_field("outgoing_key")?,
            payload_offset: schema.get_field("payload_offset")?,
            payload_length: schema.get_field("payload_length")?,
            payload_digest: schema.get_field("payload_digest")?,
        })
    }

    pub(crate) const fn incoming_key(self) -> Field {
        self.incoming_key
    }

    pub(crate) const fn outgoing_key(self) -> Field {
        self.outgoing_key
    }
}

fn build_search_index(
    directory: &Path,
    projection: &GenerationProjection,
    options: ProjectionStoreOptions,
) -> Result<()> {
    let schema = search_schema();
    let fields = SearchProjectionFields::from_schema(&schema)?;
    let index = Index::create_in_dir(directory, schema)
        .with_context(|| format!("create search index at {}", directory.display()))?;
    let mut writer = index
        .writer_with_num_threads::<TantivyDocument>(
            options.search_writer_threads,
            options.search_writer_memory_bytes,
        )
        .context("create bounded search projection writer")?;

    for projected in &projection.search_documents {
        writer
            .add_document(search_document(&fields, projected))
            .with_context(|| format!("add search projection document `{}`", projected.stable_id))?;
    }
    writer.commit().context("commit search projection index")?;
    writer
        .wait_merging_threads()
        .context("finish search projection index workers")?;
    write_path_catalog(directory, projection)?;
    write_schema_marker(directory, SEARCH_SCHEMA_CONTRACT, SEARCH_SCHEMA_VERSION)
}

fn build_reference_index(
    directory: &Path,
    projection: &GenerationProjection,
    options: ProjectionStoreOptions,
) -> Result<()> {
    let schema = reference_schema();
    let fields = ReferenceProjectionFields::from_schema(&schema)?;
    let index = Index::create_in_dir(directory, schema)
        .with_context(|| format!("create reference index at {}", directory.display()))?;
    let mut writer = index
        .writer_with_num_threads::<TantivyDocument>(
            options.reference_writer_threads,
            options.reference_writer_memory_bytes,
        )
        .context("create bounded reference projection writer")?;
    let mut payloads = ReferencePayloadWriter::create(directory).with_context(|| {
        format!(
            "create reference payload sidecar in {}",
            directory.display()
        )
    })?;

    for projected in &projection.reference_documents {
        let payload = payloads
            .append(projected, options.max_reference_fact_json_bytes)
            .with_context(|| {
                format!(
                    "encode reference payload for projection document `{}`",
                    projected.stable_id
                )
            })?;
        let document = reference_document(&fields, projected, payload);
        writer.add_document(document).with_context(|| {
            format!(
                "add reference projection document `{}`",
                projected.stable_id
            )
        })?;
    }
    writer
        .commit()
        .context("commit reference projection index")?;
    writer
        .wait_merging_threads()
        .context("finish reference projection index workers")?;
    payloads
        .finish()
        .context("finish reference payload sidecar")?;
    write_schema_marker(
        directory,
        REFERENCE_SCHEMA_CONTRACT,
        REFERENCE_SCHEMA_VERSION,
    )
}

fn search_document(fields: &SearchProjectionFields, projected: &SearchDocument) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_u64(fields.schema_version, u64::from(SEARCH_SCHEMA_VERSION));
    document.add_text(fields.id, &projected.stable_id);
    if let Some(guid) = &projected.guid {
        document.add_text(fields.guid, guid);
    }
    document.add_text(fields.path, &projected.path);
    document.add_text(fields.path_filter, normalize_for_match(&projected.path));
    document.add_text(fields.path_terms, &projected.path_terms);
    document.add_text(fields.name, &projected.name);
    document.add_text(fields.name_terms, &projected.name_terms);
    document.add_text(fields.kind, &projected.kind);
    document.add_text(fields.kind_filter, normalize_for_match(&projected.kind));
    document.add_text(fields.kind_terms, &projected.kind_terms);
    if !projected.content_terms.is_empty() {
        document.add_text(fields.content_terms, &projected.content_terms);
    }
    for hierarchy_path in &projected.hierarchy_paths {
        document.add_text(fields.hierarchy_paths, hierarchy_path);
    }
    for script_symbol in &projected.script_symbols {
        document.add_text(fields.script_symbols, script_symbol);
    }
    if let Some(container_source_path) = &projected.container_source_path {
        document.add_text(fields.container_source_path, container_source_path);
    }
    document
}

fn reference_document(
    fields: &ReferenceProjectionFields,
    projected: &ReferenceDocument,
    payload: ReferencePayloadLocation,
) -> TantivyDocument {
    let mut document = TantivyDocument::default();
    document.add_text(fields.stable_id, &projected.stable_id);
    for key in &projected.incoming_keys {
        document.add_text(fields.incoming_key, key);
    }
    for key in &projected.outgoing_keys {
        document.add_text(fields.outgoing_key, key);
    }
    document.add_u64(fields.payload_offset, payload.offset());
    document.add_u64(fields.payload_length, payload.length());
    document.add_text(
        fields.payload_digest,
        hex::encode(payload.digest().as_bytes()),
    );
    document
}

fn search_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_u64_field("schema_version", INDEXED | STORED);
    builder.add_text_field("id", STRING | STORED | FAST);
    builder.add_text_field("guid", STRING | STORED);
    builder.add_text_field("path", STORED);
    builder.add_text_field("path_filter", STRING);
    builder.add_text_field("path_terms", TEXT);
    builder.add_text_field("name", STORED);
    builder.add_text_field("name_terms", TEXT);
    builder.add_text_field("kind", STRING | STORED);
    builder.add_text_field("kind_filter", STRING);
    builder.add_text_field("kind_terms", TEXT);
    builder.add_text_field("content_terms", TEXT);
    builder.add_text_field("container_source_path", STRING | STORED);
    builder.add_text_field("hierarchy_paths", STORED);
    builder.add_text_field("script_symbols", STORED);
    builder.build()
}

fn reference_schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("stable_id", STRING | FAST);
    builder.add_text_field("incoming_key", STRING);
    builder.add_text_field("outgoing_key", STRING);
    builder.add_u64_field("payload_offset", FAST);
    builder.add_u64_field("payload_length", FAST);
    builder.add_text_field("payload_digest", STRING | FAST);
    builder.build()
}

fn validate_schema(actual: &Schema, expected: &Schema, contract: &str) -> Result<()> {
    ensure!(
        actual == expected,
        "{contract} Tantivy schema does not match its versioned contract"
    );
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SchemaMarker {
    generation_contract_version: u16,
    schema_contract: String,
    schema_version: u16,
}

impl SchemaMarker {
    fn new(schema_contract: &str, schema_version: u16) -> Self {
        Self {
            generation_contract_version: SEARCH_GENERATION_STORAGE_CONTRACT_VERSION,
            schema_contract: schema_contract.to_owned(),
            schema_version,
        }
    }
}

fn write_schema_marker(directory: &Path, contract: &str, version: u16) -> Result<()> {
    let path = directory.join(SCHEMA_MARKER_FILE);
    let bytes = serde_json::to_vec(&SchemaMarker::new(contract, version))
        .context("serialize projection schema marker")?;
    ensure!(
        bytes.len() <= MAX_SCHEMA_MARKER_BYTES as usize,
        "projection schema marker exceeds its byte limit"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create schema marker {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("write schema marker {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync schema marker {}", path.display()))
}

fn write_path_catalog(directory: &Path, projection: &GenerationProjection) -> Result<()> {
    let path = directory.join(PATH_CATALOG_FILE);
    let document_count = u64::try_from(projection.search_documents.len())
        .context("path catalog document count exceeds u64")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("create path catalog {}", path.display()))?;
    file.write_all(PATH_CATALOG_MAGIC)
        .with_context(|| format!("write path catalog magic {}", path.display()))?;
    file.write_all(&document_count.to_le_bytes())
        .with_context(|| format!("write path catalog count {}", path.display()))?;
    for (ordinal, document) in projection.search_documents.iter().enumerate() {
        let path_length = u32::try_from(document.path.len()).with_context(|| {
            format!("path catalog entry {ordinal} length exceeds the u32 wire format")
        })?;
        file.write_all(&path_length.to_le_bytes())
            .with_context(|| format!("write path catalog entry {ordinal} length"))?;
        file.write_all(document.path.as_bytes())
            .with_context(|| format!("write path catalog entry {ordinal}"))?;
    }
    file.sync_all()
        .with_context(|| format!("sync path catalog {}", path.display()))
}

fn open_path_catalog(
    directory: &SecureReadDirectory,
    directory_path: &Path,
) -> Result<(RegularFile, u64)> {
    let path = directory_path.join(PATH_CATALOG_FILE);
    let file = directory
        .open_regular(OsStr::new(PATH_CATALOG_FILE))
        .with_context(|| format!("open anchored path catalog {}", path.display()))?;
    let file_length = file.length();
    let mut reader = file
        .range(0, file_length)
        .with_context(|| format!("open anchored path catalog range {}", path.display()))?;
    let document_count = read_path_catalog_header(&mut reader, file_length, &path)?;
    file.ensure_unchanged()
        .with_context(|| format!("revalidate anchored path catalog {}", path.display()))?;
    Ok((file, document_count))
}

fn read_path_catalog_header(
    reader: &mut impl Read,
    file_length: u64,
    display_path: &Path,
) -> Result<u64> {
    let header_bytes = u64::try_from(PATH_CATALOG_MAGIC.len())
        .ok()
        .and_then(|length| length.checked_add(8))
        .ok_or_else(|| anyhow!("path catalog header length overflow"))?;
    ensure!(
        file_length >= header_bytes,
        "path catalog {} is {} bytes, shorter than its {header_bytes}-byte header",
        display_path.display(),
        file_length
    );

    let mut magic = [0_u8; PATH_CATALOG_MAGIC.len()];
    reader
        .read_exact(&mut magic)
        .with_context(|| format!("read path catalog magic {}", display_path.display()))?;
    ensure!(
        magic.as_slice() == PATH_CATALOG_MAGIC,
        "path catalog {} has an unsupported magic/version",
        display_path.display()
    );
    let document_count = read_u64(reader, "read path catalog document count")?;
    let minimum_bytes = document_count
        .checked_mul(5)
        .and_then(|bytes| bytes.checked_add(header_bytes))
        .ok_or_else(|| anyhow!("minimum path catalog length overflow"))?;
    let maximum_entry_bytes = u64::try_from(MAX_PROJECTED_PATH_BYTES)
        .ok()
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| anyhow!("maximum path catalog entry length overflow"))?;
    let maximum_bytes = document_count
        .checked_mul(maximum_entry_bytes)
        .and_then(|bytes| bytes.checked_add(header_bytes))
        .ok_or_else(|| anyhow!("maximum path catalog length overflow"))?;
    ensure!(
        (minimum_bytes..=maximum_bytes).contains(&file_length),
        "path catalog {} is {} bytes; count {document_count} requires {}..={} bytes",
        display_path.display(),
        file_length,
        minimum_bytes,
        maximum_bytes
    );
    Ok(document_count)
}

fn read_u32(reader: &mut impl Read, context: &'static str) -> Result<u32> {
    let mut encoded = [0_u8; 4];
    reader.read_exact(&mut encoded).context(context)?;
    Ok(u32::from_le_bytes(encoded))
}

fn read_u64(reader: &mut impl Read, context: &'static str) -> Result<u64> {
    let mut encoded = [0_u8; 8];
    reader.read_exact(&mut encoded).context(context)?;
    Ok(u64::from_le_bytes(encoded))
}

fn validate_path_catalog_entries(reader: &mut impl Read, document_count: u64) -> Result<()> {
    let mut encoded_path = [0_u8; MAX_PROJECTED_PATH_BYTES];
    for ordinal in 0..document_count {
        let path_length = read_u32(reader, "read path catalog entry length")?;
        let path_length = usize::try_from(path_length)
            .context("path catalog entry length exceeds the platform address space")?;
        ensure!(
            (1..=MAX_PROJECTED_PATH_BYTES).contains(&path_length),
            "path catalog entry {ordinal} has {path_length} bytes; expected 1..=\
             {MAX_PROJECTED_PATH_BYTES}"
        );
        reader
            .read_exact(&mut encoded_path[..path_length])
            .with_context(|| format!("read path catalog entry {ordinal}"))?;
        std::str::from_utf8(&encoded_path[..path_length])
            .with_context(|| format!("validate path catalog entry {ordinal} UTF-8"))?;
    }
    let mut trailing = [0_u8; 1];
    ensure!(
        reader
            .read(&mut trailing)
            .context("check path catalog end")?
            == 0,
        "path catalog contains trailing bytes after {document_count} entries"
    );
    Ok(())
}

fn validate_schema_marker(
    directory: &Path,
    opened_directory: &SecureReadDirectory,
    contract: &str,
    version: u16,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let path = directory.join(SCHEMA_MARKER_FILE);
    let mut file = opened_directory
        .open_regular(OsStr::new(SCHEMA_MARKER_FILE))
        .with_context(|| format!("open schema marker {}", path.display()))?;
    ensure!(
        file.length() <= MAX_SCHEMA_MARKER_BYTES,
        "schema marker {} is {} bytes, exceeding the {}-byte limit",
        path.display(),
        file.length(),
        MAX_SCHEMA_MARKER_BYTES
    );

    let decoded: std::result::Result<SchemaMarker, _> =
        read_contract_json(file.file_mut(), budget, SCHEMA_MARKER_JSON_LIMITS);
    file.ensure_unchanged()
        .with_context(|| format!("revalidate schema marker {}", path.display()))?;
    let actual = decoded.with_context(|| format!("decode schema marker {}", path.display()))?;
    let expected = SchemaMarker::new(contract, version);
    if contract == REFERENCE_SCHEMA_CONTRACT
        && version == REFERENCE_SCHEMA_VERSION
        && actual.generation_contract_version == SEARCH_GENERATION_STORAGE_CONTRACT_VERSION
        && actual.schema_contract == contract
        && (1..REFERENCE_SCHEMA_VERSION).contains(&actual.schema_version)
    {
        return Err(RebuildableProjectionSchemaVersion {
            actual: actual.schema_version,
            expected: REFERENCE_SCHEMA_VERSION,
        }
        .into());
    }
    ensure!(
        actual == expected,
        "schema marker for `{contract}` does not match version {version}"
    );
    Ok(())
}

pub(crate) fn logical_projection_digests(
    projection: &GenerationProjection,
) -> Result<GenerationProjectionDigests> {
    validate_projection(projection)?;
    let search = logical_document_digest(SEARCH_LOGICAL_DOMAIN, &projection.search_documents)?;
    let references =
        logical_document_digest(REFERENCE_LOGICAL_DOMAIN, &projection.reference_documents)?;
    Ok(GenerationProjectionDigests::new(search, references))
}

fn logical_document_digest<T>(domain: &[u8], documents: &[T]) -> Result<DigestV1>
where
    T: Serialize,
{
    let document_count = u64::try_from(documents.len())
        .map_err(|_| anyhow!("logical projection document count exceeds u64"))?;
    let digest_bytes = document_count
        .checked_mul(DigestV1::BYTE_LEN as u64)
        .ok_or_else(|| anyhow!("logical projection digest length overflow"))?;
    let declared_length = u64::try_from(domain.len())
        .ok()
        .and_then(|length| length.checked_add(8))
        .and_then(|length| length.checked_add(digest_bytes))
        .ok_or_else(|| anyhow!("logical projection digest length overflow"))?;
    let mut builder = DigestV1Builder::new(declared_length);
    builder
        .update(domain)
        .context("hash logical projection domain")?;
    builder
        .update(&document_count.to_le_bytes())
        .context("hash logical projection document count")?;
    for document in documents {
        let digest = canonical_json_digest(document)
            .context("hash serialized logical projection document")?;
        builder
            .update(digest.as_bytes())
            .context("hash logical projection document identity")?;
    }
    builder
        .finalize()
        .context("finalize logical projection digest")
}

fn canonical_json_digest<T>(value: &T) -> Result<DigestV1>
where
    T: Serialize,
{
    let encoded_len = json_encoded_len(value)?;
    let mut builder = DigestV1Builder::new(encoded_len);
    serde_json::to_writer(DigestWriter(&mut builder), value)
        .context("stream canonical JSON into digest")?;
    builder.finalize().context("finalize canonical JSON digest")
}

fn json_encoded_len<T>(value: &T) -> Result<u64>
where
    T: Serialize,
{
    let mut counter = JsonByteCounter::default();
    serde_json::to_writer(&mut counter, value).context("measure canonical JSON")?;
    Ok(counter.bytes)
}

#[derive(Default)]
struct JsonByteCounter {
    bytes: u64,
}

impl Write for JsonByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(buffer.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| io::Error::other("canonical JSON length overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'a>(&'a mut DigestV1Builder);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .update(buffer)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn prepare_empty_artifact_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("artifact directory is a symlink: {}", path.display());
            }
            ensure!(
                metadata.is_dir(),
                "artifact path is not a directory: {}",
                path.display()
            );
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("create artifact directory {}", path.display()))?;
        }
        Err(source) => {
            return Err(source)
                .with_context(|| format!("inspect artifact directory {}", path.display()));
        }
    }
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read artifact directory {}", path.display()))?;
    ensure!(
        entries.next().transpose()?.is_none(),
        "artifact directory must be empty before materialization: {}",
        path.display()
    );
    Ok(())
}

fn ensure_directory_no_follow(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect directory {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("directory is a symlink: {}", path.display());
    }
    ensure!(
        metadata.is_dir(),
        "path is not a directory: {}",
        path.display()
    );
    Ok(())
}

fn validate_writer_options(kind: &str, threads: usize, memory_bytes: usize) -> Result<()> {
    ensure!(
        (1..=MAX_WRITER_THREADS).contains(&threads),
        "{kind} writer thread count {threads} is outside 1..={MAX_WRITER_THREADS}"
    );
    let minimum = MIN_WRITER_MEMORY_PER_THREAD
        .checked_mul(threads)
        .ok_or_else(|| anyhow!("{kind} writer minimum memory overflow"))?;
    let maximum = MAX_WRITER_MEMORY_PER_THREAD
        .checked_mul(threads)
        .ok_or_else(|| anyhow!("{kind} writer maximum memory overflow"))?;
    ensure!(
        (minimum..=maximum).contains(&memory_bytes),
        "{kind} writer memory {memory_bytes} is outside {minimum}..={maximum} bytes"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom};
    use std::path::PathBuf;

    use tantivy::Term;
    use tantivy::merge_policy::NoMergePolicy;
    use tempfile::tempdir;
    use unity_asset_core::{
        AssetLoadLimits, AssetLoadUsage, BudgetError, BudgetedJsonError, FieldPath,
    };

    use super::*;
    use crate::analysis::{BinaryExternalProjection, ReferenceProjectionFact};
    use crate::projection::ProjectionMetrics;

    fn empty_projection() -> GenerationProjection {
        GenerationProjection {
            search_documents: Vec::new(),
            reference_documents: Vec::new(),
            diagnostics: Vec::new(),
            truncations: Vec::new(),
            metrics: ProjectionMetrics::default(),
        }
    }

    fn projected_search_document(stable_id: &str, path: &str) -> SearchDocument {
        SearchDocument {
            stable_id: stable_id.to_owned(),
            guid: None,
            path: path.to_owned(),
            path_terms: normalize_for_match(path),
            name: path.to_owned(),
            name_terms: normalize_for_match(path),
            kind: "TextAsset".to_owned(),
            kind_terms: "text asset".to_owned(),
            content_terms: String::new(),
            hierarchy_paths: Vec::new(),
            script_symbols: Vec::new(),
            container_source_path: None,
        }
    }

    fn projected_reference_document(stable_id: &str) -> ReferenceDocument {
        let source =
            ObjectAddress::binary_direct(SourceLocator::path("Assets/Source.asset").unwrap(), 1)
                .unwrap();
        ReferenceDocument {
            stable_id: stable_id.to_owned(),
            source_path: "Assets/Source.asset".to_owned(),
            source_kind: "SerializedAsset".to_owned(),
            source_guid: None,
            fact: ReferenceProjectionFact {
                source_object: source,
                source_class_id: Some(114),
                field_path: FieldPath::root(),
                raw_target: RawReferenceProjection::Binary {
                    file_id: 0,
                    path_id: 2,
                    external: Some(BinaryExternalProjection {
                        index: 0,
                        guid: None,
                        type_id: 0,
                        path: "Assets/Target.asset".to_owned(),
                    }),
                },
                resolution: ReferenceResolutionProjection::Invalid,
                diagnostics: Vec::new(),
                dependency_keys: Vec::new(),
            },
            incoming_keys: Vec::new(),
            outgoing_keys: Vec::new(),
        }
    }

    fn generous_load_budget() -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 100,
            max_bytes: 1024 * 1024,
            max_members: 100,
            ..AssetLoadLimits::default()
        })
        .unwrap()
    }

    fn write_schema_marker_for_test(
        directory: &Path,
        contract: &str,
        version: u16,
        generation_contract_version: u16,
    ) {
        let marker = SchemaMarker {
            generation_contract_version,
            schema_contract: contract.to_owned(),
            schema_version: version,
        };
        fs::write(
            directory.join(SCHEMA_MARKER_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn schema_marker_contract_accepts_exact_and_rejects_one_short_budget() {
        let directory = tempdir().unwrap();
        write_schema_marker(
            directory.path(),
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
        )
        .unwrap();
        let opened =
            SecureReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap();

        let mut measured = AssetLoadBudget::default();
        validate_schema_marker(
            directory.path(),
            &opened,
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            &mut measured,
        )
        .unwrap();
        let usage = measured.usage();

        let exact_limits = AssetLoadLimits {
            max_entries: usage.entries,
            max_bytes: usage.bytes,
            max_depth: usage.max_observed_depth,
            max_members: usage.members,
            ..AssetLoadLimits::default()
        };
        let mut exact = AssetLoadBudget::new(exact_limits).unwrap();
        validate_schema_marker(
            directory.path(),
            &opened,
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            &mut exact,
        )
        .unwrap();
        assert_eq!(exact.usage(), usage);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes.checked_sub(1).unwrap(),
            ..exact_limits
        })
        .unwrap();
        let error = validate_schema_marker(
            directory.path(),
            &opened,
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            &mut one_short,
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<BudgetedJsonError>(),
            Some(BudgetedJsonError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
    }

    #[test]
    fn only_older_reference_markers_are_rebuildable() {
        for schema_version in 1..REFERENCE_SCHEMA_VERSION {
            let directory = tempdir().unwrap();
            write_schema_marker(directory.path(), REFERENCE_SCHEMA_CONTRACT, schema_version)
                .unwrap();
            let opened =
                SecureReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap();
            let error = validate_schema_marker(
                directory.path(),
                &opened,
                REFERENCE_SCHEMA_CONTRACT,
                REFERENCE_SCHEMA_VERSION,
                &mut generous_load_budget(),
            )
            .unwrap_err();
            assert!(is_rebuildable_projection_schema_version(&error));
        }

        let future = tempdir().unwrap();
        write_schema_marker(
            future.path(),
            REFERENCE_SCHEMA_CONTRACT,
            REFERENCE_SCHEMA_VERSION + 1,
        )
        .unwrap();
        let opened = SecureReadDirectory::open(future.path(), OpenPolicy::PersistedState).unwrap();
        let error = validate_schema_marker(
            future.path(),
            &opened,
            REFERENCE_SCHEMA_CONTRACT,
            REFERENCE_SCHEMA_VERSION,
            &mut generous_load_budget(),
        )
        .unwrap_err();
        assert!(!is_rebuildable_projection_schema_version(&error));

        let wrong_projection = tempdir().unwrap();
        write_schema_marker(
            wrong_projection.path(),
            REFERENCE_SCHEMA_CONTRACT,
            REFERENCE_SCHEMA_VERSION - 1,
        )
        .unwrap();
        let opened =
            SecureReadDirectory::open(wrong_projection.path(), OpenPolicy::PersistedState).unwrap();
        let error = validate_schema_marker(
            wrong_projection.path(),
            &opened,
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            &mut generous_load_budget(),
        )
        .unwrap_err();
        assert!(!is_rebuildable_projection_schema_version(&error));
    }

    #[test]
    fn current_projection_rejects_an_obsolete_storage_marker() {
        let directory = tempdir().unwrap();
        write_schema_marker_for_test(
            directory.path(),
            SEARCH_SCHEMA_CONTRACT,
            SEARCH_SCHEMA_VERSION,
            1,
        );
        let opened =
            SecureReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap();
        assert!(
            validate_schema_marker(
                directory.path(),
                &opened,
                SEARCH_SCHEMA_CONTRACT,
                SEARCH_SCHEMA_VERSION,
                &mut generous_load_budget(),
            )
            .is_err()
        );
    }

    #[test]
    fn anchored_directory_reader_locks_do_not_enable_writes() {
        let directory = tempdir().unwrap();
        let anchored = AnchoredTantivyDirectory::new(Arc::new(
            SecureReadDirectory::open(directory.path(), OpenPolicy::PersistedState).unwrap(),
        ));
        let lock_path = PathBuf::from("reader.lock");
        let lock = Lock {
            filepath: lock_path.clone(),
            is_blocking: false,
        };

        let _guard = anchored.acquire_lock(&lock).unwrap();

        assert!(!directory.path().join(&lock_path).exists());
        assert!(anchored.open_write(Path::new("forbidden")).is_err());
        assert!(!directory.path().join("forbidden").exists());
    }

    fn validate_catalog_bytes(bytes: &[u8]) -> Result<()> {
        let directory = tempdir().unwrap();
        let path = directory.path().join(PATH_CATALOG_FILE);
        fs::write(&path, bytes).unwrap();
        let opened = SecureReadDirectory::open(directory.path(), OpenPolicy::PersistedState)
            .map_err(|error| anyhow!("open catalog test directory: {error}"))?;
        let (file, document_count) = open_path_catalog(&opened, directory.path())?;
        let mut range = file.range(0, file.length())?;
        let _ = read_path_catalog_header(&mut range, file.length(), &path)?;
        validate_path_catalog_entries(&mut range, document_count)
    }

    fn catalog_header(document_count: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(PATH_CATALOG_MAGIC);
        bytes.extend_from_slice(&document_count.to_le_bytes());
        bytes
    }

    #[test]
    fn stored_paths_are_byte_sorted_and_deduplicated() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![
            projected_search_document("a", "Assets/Zeta.asset"),
            projected_search_document("b", "Assets/Alpha.asset"),
            projected_search_document("c", "Assets/Alpha.asset"),
        ];
        ProjectionStore::build(directory.path(), &projection).unwrap();
        let readers =
            ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default()).unwrap();
        let mut budget = generous_load_budget();

        let paths = readers.search().stored_paths(&mut budget).unwrap();

        assert_eq!(
            paths,
            vec![
                "Assets/Alpha.asset".to_owned(),
                "Assets/Zeta.asset".to_owned(),
            ]
        );
        let usage = budget.usage();
        assert_eq!(usage.entries, 3);
        assert_eq!(usage.members, 3);
        let retained_bytes =
            3 * size_of::<String>() + 2 * "Assets/Alpha.asset".len() + "Assets/Zeta.asset".len();
        assert_eq!(usage.bytes, u64::try_from(retained_bytes).unwrap());
    }

    #[test]
    fn path_catalog_wire_is_versioned_and_follows_document_order() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![
            projected_search_document("a", "Assets/Zeta.asset"),
            projected_search_document("b", "Assets/Alpha.asset"),
        ];
        ProjectionStore::build(directory.path(), &projection).unwrap();

        let actual = fs::read(
            directory
                .path()
                .join(SEARCH_ARTIFACT_DIRECTORY)
                .join(PATH_CATALOG_FILE),
        )
        .unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(PATH_CATALOG_MAGIC);
        expected.extend_from_slice(&2_u64.to_le_bytes());
        for path in ["Assets/Zeta.asset", "Assets/Alpha.asset"] {
            expected.extend_from_slice(&u32::try_from(path.len()).unwrap().to_le_bytes());
            expected.extend_from_slice(path.as_bytes());
        }

        assert_eq!(actual, expected);
    }

    #[test]
    fn path_catalog_validation_rejects_zero_invalid_truncated_and_trailing_entries() {
        let mut zero = catalog_header(1);
        zero.extend_from_slice(&0_u32.to_le_bytes());
        zero.push(b'x');

        let mut invalid_utf8 = catalog_header(1);
        invalid_utf8.extend_from_slice(&1_u32.to_le_bytes());
        invalid_utf8.push(0xff);

        let mut truncated = catalog_header(1);
        truncated.extend_from_slice(&3_u32.to_le_bytes());
        truncated.extend_from_slice(b"ab");

        let mut trailing = catalog_header(1);
        trailing.extend_from_slice(&1_u32.to_le_bytes());
        trailing.extend_from_slice(b"ax");

        assert!(
            validate_catalog_bytes(&zero)
                .unwrap_err()
                .to_string()
                .contains(&format!("expected 1..={MAX_PROJECTED_PATH_BYTES}"))
        );
        assert!(
            validate_catalog_bytes(&invalid_utf8)
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
        assert!(
            validate_catalog_bytes(&truncated)
                .unwrap_err()
                .to_string()
                .contains("read path catalog entry 0")
        );
        assert!(
            validate_catalog_bytes(&trailing)
                .unwrap_err()
                .to_string()
                .contains("trailing bytes")
        );
    }

    #[test]
    fn projection_reader_rejects_path_catalog_count_mismatch() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![projected_search_document("only", "Assets/Only.asset")];
        ProjectionStore::build(directory.path(), &projection).unwrap();
        let catalog_path = directory
            .path()
            .join(SEARCH_ARTIFACT_DIRECTORY)
            .join(PATH_CATALOG_FILE);
        let mut catalog = OpenOptions::new().write(true).open(catalog_path).unwrap();
        catalog
            .seek(SeekFrom::Start(
                u64::try_from(PATH_CATALOG_MAGIC.len()).unwrap(),
            ))
            .unwrap();
        catalog.write_all(&2_u64.to_le_bytes()).unwrap();
        catalog.sync_all().unwrap();
        drop(catalog);

        let error = match ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default())
        {
            Ok(_) => panic!("mismatched path catalog count must be rejected"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("path catalog contains 2 documents")
        );
    }

    #[test]
    fn projection_reader_rejects_deleted_segments_even_when_live_count_matches() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![
            projected_search_document("deleted", "Assets/Deleted.asset"),
            projected_search_document("kept", "Assets/Kept.asset"),
        ];
        ProjectionStore::build(directory.path(), &projection).unwrap();
        let search_directory = directory.path().join(SEARCH_ARTIFACT_DIRECTORY);
        let index = Index::open_in_dir(search_directory).unwrap();
        let fields = SearchProjectionFields::from_schema(&index.schema()).unwrap();
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, MIN_WRITER_MEMORY_PER_THREAD)
            .unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        writer.delete_term(Term::from_field_text(fields.id, "deleted"));
        writer
            .add_document(search_document(
                &fields,
                &projected_search_document("replacement", "Assets/Replacement.asset"),
            ))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(index);

        let error = match ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default())
        {
            Ok(_) => panic!("deleted search segments must be rejected before publication"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("immutable search projection contains deleted documents")
        );
    }

    #[test]
    fn stored_paths_reject_backing_budget_before_allocation() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![projected_search_document("only", "Assets/Only.asset")];
        ProjectionStore::build(directory.path(), &projection).unwrap();
        let readers =
            ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default()).unwrap();
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_bytes: u64::try_from(size_of::<String>() - 1).unwrap(),
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = readers.search().stored_paths(&mut budget).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("preflight stored path output backing")
        );
        assert_eq!(budget.usage(), AssetLoadUsage::default());
    }

    #[test]
    fn projection_reader_rejects_overlong_catalog_entry_before_publication() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![projected_search_document("only", "Assets/Only.asset")];
        ProjectionStore::build(directory.path(), &projection).unwrap();
        let catalog_path = directory
            .path()
            .join(SEARCH_ARTIFACT_DIRECTORY)
            .join(PATH_CATALOG_FILE);
        let mut catalog = OpenOptions::new().write(true).open(catalog_path).unwrap();
        let first_length_offset = u64::try_from(PATH_CATALOG_MAGIC.len()).unwrap() + 8;
        catalog.seek(SeekFrom::Start(first_length_offset)).unwrap();
        catalog
            .write_all(
                &u32::try_from(MAX_PROJECTED_PATH_BYTES + 1)
                    .unwrap()
                    .to_le_bytes(),
            )
            .unwrap();
        catalog.sync_all().unwrap();
        drop(catalog);

        let error = match ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default())
        {
            Ok(_) => panic!("overlong path catalog entries must be rejected before publication"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains(&format!("expected 1..={MAX_PROJECTED_PATH_BYTES}"))
        );
    }

    #[test]
    fn projection_rejects_overlong_paths_before_creating_artifacts() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![projected_search_document(
            "overlong",
            &"x".repeat(MAX_PROJECTED_PATH_BYTES + 1),
        )];

        let error = ProjectionStore::build(directory.path(), &projection).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("violates the portable path contract")
        );
        assert!(!directory.path().join(SEARCH_ARTIFACT_DIRECTORY).exists());
        assert!(!directory.path().join(REFERENCE_ARTIFACT_DIRECTORY).exists());
    }

    #[test]
    fn projection_rejects_empty_paths_before_creating_artifacts() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![projected_search_document("empty", "")];

        let error = ProjectionStore::build(directory.path(), &projection).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("violates the portable path contract")
        );
        assert!(!directory.path().join(SEARCH_ARTIFACT_DIRECTORY).exists());
        assert!(!directory.path().join(REFERENCE_ARTIFACT_DIRECTORY).exists());
    }

    #[test]
    fn projection_rejects_unrepresentable_nested_reference_paths_before_creating_artifacts() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        let mut external = projected_reference_document("external");
        let RawReferenceProjection::Binary {
            external: Some(target),
            ..
        } = &mut external.fact.raw_target
        else {
            unreachable!();
        };
        target.path = "x".repeat(MAX_PROJECTED_PATH_BYTES + 1);
        projection.reference_documents = vec![external];

        let error = ProjectionStore::build(directory.path(), &projection).unwrap_err();

        assert!(error.to_string().contains("binary external path"));
        assert!(!directory.path().join(SEARCH_ARTIFACT_DIRECTORY).exists());
        assert!(!directory.path().join(REFERENCE_ARTIFACT_DIRECTORY).exists());
    }

    #[test]
    fn projection_rejects_unrepresentable_resolution_locators_before_creating_artifacts() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        let mut reference = projected_reference_document("resolution");
        reference.fact.resolution = ReferenceResolutionProjection::Resolved {
            target: ObjectAddress::binary_direct(
                SourceLocator::path("x".repeat(MAX_PROJECTED_PATH_BYTES + 1)).unwrap(),
                2,
            )
            .unwrap(),
        };
        projection.reference_documents = vec![reference];

        let error = ProjectionStore::build(directory.path(), &projection).unwrap_err();

        assert!(error.to_string().contains("resolution target"));
        assert!(!directory.path().join(SEARCH_ARTIFACT_DIRECTORY).exists());
        assert!(!directory.path().join(REFERENCE_ARTIFACT_DIRECTORY).exists());
    }

    #[test]
    fn empty_projection_builds_and_opens_both_versioned_indices() {
        let directory = tempdir().unwrap();
        let evidence = ProjectionStore::build(directory.path(), &empty_projection()).unwrap();
        assert!(evidence.search_artifact().files() > 0);
        assert!(evidence.reference_artifact().files() > 0);

        let readers =
            ProjectionReaders::open(directory.path(), &mut AssetLoadBudget::default()).unwrap();
        assert_eq!(readers.search().reader().searcher().num_docs(), 0);
        assert_eq!(readers.references().reader().searcher().num_docs(), 0);
    }

    #[test]
    fn reference_payload_sidecar_is_part_of_artifact_evidence() {
        let directory = tempdir().unwrap();
        let evidence = ProjectionStore::build(directory.path(), &empty_projection()).unwrap();
        let reference_directory = directory.path().join(REFERENCE_ARTIFACT_DIRECTORY);
        let payload = reference_directory.join(REFERENCE_PAYLOAD_FILE);

        assert!(payload.is_file());
        fs::write(&payload, b"tampered").unwrap();
        let tampered = measure_artifact_tree(&reference_directory).unwrap();

        assert_ne!(tampered, evidence.reference_artifact());
    }

    #[test]
    fn logical_digest_requires_canonical_document_order_without_sort_allocation() {
        let one = projected_search_document("one", "Assets/One.asset");
        let two = projected_search_document("two", "Assets/Two.asset");

        let mut forward = empty_projection();
        forward.search_documents = vec![one.clone(), two.clone()];
        let mut reverse = empty_projection();
        reverse.search_documents = vec![two, one];

        logical_projection_digests(&forward).unwrap();
        let error = logical_projection_digests(&reverse).unwrap_err();
        assert!(error.to_string().contains("strictly ordered and unique"));
    }

    #[test]
    fn duplicate_stable_ids_are_rejected_before_writing_an_index() {
        let directory = tempdir().unwrap();
        let mut projection = empty_projection();
        projection.search_documents = vec![
            projected_search_document("duplicate", "Assets/One.asset"),
            projected_search_document("duplicate", "Assets/Two.asset"),
        ];

        let error = ProjectionStore::build(directory.path(), &projection).unwrap_err();

        assert!(error.to_string().contains("strictly ordered and unique"));
    }

    #[test]
    fn artifact_tree_digest_is_independent_of_creation_order() {
        let first = tempdir().unwrap();
        fs::write(first.path().join("b"), b"two").unwrap();
        fs::write(first.path().join("a"), b"one").unwrap();

        let second = tempdir().unwrap();
        fs::write(second.path().join("a"), b"one").unwrap();
        fs::write(second.path().join("b"), b"two").unwrap();

        assert_eq!(
            measure_artifact_tree(first.path()).unwrap(),
            measure_artifact_tree(second.path()).unwrap()
        );
    }
}
