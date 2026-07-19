use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::asset::SerializedFile;
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_core::{
    AssetLoadBudget, BudgetError, DigestV1, SourceId, SourceKind, VerifiedSourceImage,
    VerifiedSourceRebinding, WorkspaceId, arc_value_allocation_bytes, vec_allocation_bytes,
};
use unity_asset_yaml::YamlDocument;

/// One immutable source entry and the parse state proven before publication.
#[derive(Debug)]
pub(crate) struct SourceEntry {
    source: SourceId,
    image: VerifiedSourceImage,
    parse: FrozenSourceParse,
}

/// Parse state frozen before a source entry is published into a workspace snapshot.
#[derive(Debug, Clone)]
pub(crate) enum FrozenSourceParse {
    None,
    Serialized(Arc<SerializedFile>),
    Yaml(Arc<YamlDocument>),
}

impl FrozenSourceParse {
    const fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Serialized(_) => "serialized",
            Self::Yaml(_) => "yaml",
        }
    }

    const fn matches(&self, kind: SourceKind) -> bool {
        matches!(
            (kind, self),
            (SourceKind::SerializedFile, Self::Serialized(_))
                | (SourceKind::Yaml, Self::Yaml(_))
                | (
                    SourceKind::AssetBundle
                        | SourceKind::WebFile
                        | SourceKind::Archive
                        | SourceKind::StreamedResource,
                    Self::None
                )
        )
    }

    fn rebind_verified_image(
        &mut self,
        source: SourceId,
        rebinding: VerifiedSourceRebinding,
    ) -> Result<VerifiedSourceImage, SourceStoreError> {
        match self {
            Self::Serialized(parsed) => Arc::get_mut(parsed)
                .ok_or(SourceStoreError::FrozenSerializedParseShared { source_id: source })?
                .rebind_verified_source(rebinding)
                .map_err(|_| SourceStoreError::FrozenSerializedBackingMismatch {
                    source_id: source,
                }),
            Self::None | Self::Yaml(_) => Ok(rebinding.into_image()),
        }
    }
}

impl SourceEntry {
    fn validate_parts(
        source: SourceId,
        image: &VerifiedSourceImage,
        parse: &FrozenSourceParse,
    ) -> Result<(), SourceStoreError> {
        if source.kind() != image.kind() {
            return Err(SourceStoreError::SourceKindMismatch {
                source_id: source,
                expected: source.kind(),
                actual: image.kind(),
            });
        }
        if !parse.matches(image.kind()) {
            return Err(SourceStoreError::FrozenParseKindMismatch {
                source_id: source,
                source_kind: image.kind(),
                parse_kind: parse.label(),
            });
        }
        if let FrozenSourceParse::Serialized(parsed) = parse {
            let complete_backing = parsed.data_base_offset() == 0
                && parsed.data().len() == image.as_bytes().len()
                && match parsed.data_shared() {
                    SharedBytes::Arc(backing) => Arc::ptr_eq(&backing, image.backing()),
                    #[cfg(feature = "mmap")]
                    SharedBytes::Mmap(_) => false,
                };
            if !complete_backing {
                return Err(SourceStoreError::FrozenSerializedBackingMismatch {
                    source_id: source,
                });
            }
        }
        Ok(())
    }

    fn new(source: SourceId, image: VerifiedSourceImage, parse: FrozenSourceParse) -> Self {
        Self {
            source,
            image,
            parse,
        }
    }

    #[must_use]
    pub(crate) const fn source(&self) -> SourceId {
        self.source
    }

    #[must_use]
    pub(crate) fn image(&self) -> &VerifiedSourceImage {
        &self.image
    }

    #[must_use]
    pub(crate) fn cached_serialized(&self) -> Option<&Arc<SerializedFile>> {
        match &self.parse {
            FrozenSourceParse::Serialized(parsed) => Some(parsed),
            FrozenSourceParse::None | FrozenSourceParse::Yaml(_) => None,
        }
    }

    #[must_use]
    pub(crate) fn cached_yaml(&self) -> Option<&Arc<YamlDocument>> {
        match &self.parse {
            FrozenSourceParse::Yaml(parsed) => Some(parsed),
            FrozenSourceParse::None | FrozenSourceParse::Serialized(_) => None,
        }
    }
}

/// Workspace-bound source images with content-addressed backing reuse.
#[derive(Debug)]
pub(crate) struct SourceStore {
    workspace: WorkspaceId,
    by_id: BTreeMap<SourceId, Arc<SourceEntry>>,
    by_digest: BTreeMap<DigestV1, ContentBacking>,
}

/// Canonical bytes and the exact number of source entries that reference them.
#[derive(Debug, Clone)]
struct ContentBacking {
    bytes: Arc<[u8]>,
    source_count: usize,
}

impl SourceStore {
    #[must_use]
    pub(crate) fn new(workspace: WorkspaceId) -> Self {
        Self {
            workspace,
            by_id: BTreeMap::new(),
            by_digest: BTreeMap::new(),
        }
    }

    pub(crate) fn insert(
        &mut self,
        source: SourceId,
        image: VerifiedSourceImage,
        mut parse: FrozenSourceParse,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<SourceEntry>, SourceStoreError> {
        self.ensure_workspace(source)?;
        SourceEntry::validate_parts(source, &image, &parse)?;

        let previous = self.by_id.get(&source);
        if let Some(existing) = previous
            && existing.image.fingerprint() == image.fingerprint()
        {
            let digest = image.fingerprint().digest();
            let _rebinding = image
                .rebind_equivalent_with_proof(Arc::clone(existing.image.backing()))
                .map_err(|_| SourceStoreError::DigestCollision { digest })?;
            self.validate_content_reference(source, existing)?;
            return Ok(Arc::clone(existing));
        }

        let digest = image.fingerprint().digest();
        let (image, next_source_count) = match self.by_digest.get(&digest) {
            Some(existing) => {
                let canonical = Arc::clone(&existing.bytes);
                let needs_parse_rebind = !Arc::ptr_eq(image.backing(), &canonical);
                let rebinding = image
                    .rebind_equivalent_with_proof(canonical)
                    .map_err(|_| SourceStoreError::DigestCollision { digest })?;
                let image = if needs_parse_rebind {
                    parse.rebind_verified_image(source, rebinding)?
                } else {
                    rebinding.into_image()
                };
                let next_source_count = existing
                    .source_count
                    .checked_add(1)
                    .ok_or(SourceStoreError::ContentReferenceCountOverflow { digest })?;
                (image, next_source_count)
            }
            None => (image, 1),
        };
        SourceEntry::validate_parts(source, &image, &parse)?;
        let previous_digest = previous
            .map(|entry| self.validate_content_reference(source, entry))
            .transpose()?;
        let new_source = previous.is_none();
        let new_digest = !self.by_digest.contains_key(&digest);
        let retained_bytes = retained_insert_bytes(new_digest)?;

        if new_source {
            budget.check_entries(1)?;
        }
        budget.check_bytes(retained_bytes)?;
        let entry = Arc::new(SourceEntry::new(source, image, parse));
        if new_source {
            budget.consume_entries(1)?;
        }
        budget.consume_bytes(retained_bytes)?;

        if let Some(previous_digest) = previous_digest {
            self.release_content_reference(previous_digest);
        }
        if let Some(backing) = self.by_digest.get_mut(&digest) {
            backing.source_count = next_source_count;
        } else {
            self.by_digest.insert(
                digest,
                ContentBacking {
                    bytes: Arc::clone(entry.image.backing()),
                    source_count: next_source_count,
                },
            );
        }
        self.by_id.insert(source, Arc::clone(&entry));
        Ok(entry)
    }

    pub(crate) fn clone_for_update(
        &self,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SourceStoreError> {
        let entry_count =
            u64::try_from(self.by_id.len()).map_err(|_| SourceStoreError::RetainedSizeOverflow)?;
        let retained_bytes = self.checked_clone_bytes()?;
        budget.check_entries(entry_count)?;
        budget.check_bytes(retained_bytes)?;
        budget.consume_entries(entry_count)?;
        budget.consume_bytes(retained_bytes)?;

        let candidate = Self {
            workspace: self.workspace,
            by_id: self
                .by_id
                .iter()
                .map(|(source, entry)| (*source, Arc::clone(entry)))
                .collect(),
            by_digest: self
                .by_digest
                .iter()
                .map(|(digest, backing)| (*digest, backing.clone()))
                .collect(),
        };
        Ok(candidate)
    }

    fn checked_clone_bytes(&self) -> Result<u64, SourceStoreError> {
        let entry_bytes =
            checked_btree_entries_bytes::<SourceId, Arc<SourceEntry>>(self.by_id.len())?;
        let digest_bytes =
            checked_btree_entries_bytes::<DigestV1, ContentBacking>(self.by_digest.len())?;
        checked_byte_add(entry_bytes, digest_bytes)
    }

    #[cfg(test)]
    fn remove(&mut self, source: SourceId) -> Result<Arc<SourceEntry>, SourceStoreError> {
        self.ensure_workspace(source)?;
        let removed = Arc::clone(
            self.by_id
                .get(&source)
                .ok_or(SourceStoreError::UnknownSource(source))?,
        );
        let digest = self.validate_content_reference(source, &removed)?;
        self.release_content_reference(digest);
        self.by_id.remove(&source);
        Ok(removed)
    }

    pub(crate) fn remove_all(
        &mut self,
        sources: &[SourceId],
        budget: &mut AssetLoadBudget,
    ) -> Result<(), SourceStoreError> {
        let scratch_bytes = checked_vec_bytes::<(SourceId, DigestV1)>(sources.len())?;
        budget.consume_bytes(scratch_bytes)?;
        let mut removals = Vec::new();
        removals.try_reserve_exact(sources.len()).map_err(|_| {
            SourceStoreError::AllocationFailed {
                resource: "source store removal validation",
                requested: sources.len(),
            }
        })?;

        for source in sources {
            self.ensure_workspace(*source)?;
            let entry = self
                .by_id
                .get(source)
                .ok_or(SourceStoreError::UnknownSource(*source))?;
            let digest = self.validate_content_reference(*source, entry)?;
            removals.push((*source, digest));
        }
        removals.sort_unstable_by_key(|(source, _)| *source);
        if let Some(source) = removals
            .windows(2)
            .find_map(|pair| (pair[0].0 == pair[1].0).then_some(pair[0].0))
        {
            return Err(SourceStoreError::DuplicateRemovalSource(source));
        }

        for (source, digest) in removals {
            self.release_content_reference(digest);
            let removed = self.by_id.remove(&source);
            debug_assert!(
                removed.is_some(),
                "prevalidated source disappeared during removal"
            );
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn get(&self, source: SourceId) -> Option<&Arc<SourceEntry>> {
        (source.workspace() == self.workspace)
            .then(|| self.by_id.get(&source))
            .flatten()
    }

    #[must_use]
    pub(crate) fn contains(&self, source: SourceId) -> bool {
        self.get(source).is_some()
    }

    #[must_use]
    pub(crate) const fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (SourceId, &Arc<SourceEntry>)> {
        self.by_id.iter().map(|(source, entry)| (*source, entry))
    }

    pub(crate) fn validate(&self, budget: &mut AssetLoadBudget) -> Result<(), SourceStoreError> {
        let scratch_bytes = checked_vec_bytes::<DigestV1>(self.by_id.len())?;
        budget.consume_bytes(scratch_bytes)?;
        let mut referenced_digests = Vec::new();
        referenced_digests
            .try_reserve_exact(self.by_id.len())
            .map_err(|_| SourceStoreError::AllocationFailed {
                resource: "source store validation digest list",
                requested: self.by_id.len(),
            })?;

        for (source, entry) in &self.by_id {
            self.ensure_workspace(*source)?;
            if *source != entry.source() {
                return Err(SourceStoreError::EntryIdentityMismatch {
                    key: *source,
                    entry: entry.source(),
                });
            }
            SourceEntry::validate_parts(*source, &entry.image, &entry.parse)?;
            let digest = entry.image.fingerprint().digest();
            self.validate_content_reference(*source, entry)?;
            referenced_digests.push(digest);
        }

        referenced_digests.sort_unstable();
        let mut referenced_position = 0;
        for (digest, backing) in &self.by_digest {
            let Some(actual_digest) = referenced_digests.get(referenced_position).copied() else {
                return Err(SourceStoreError::UnreferencedContentIndex { digest: *digest });
            };
            if actual_digest != *digest {
                return Err(SourceStoreError::UnreferencedContentIndex { digest: *digest });
            }

            let start = referenced_position;
            while referenced_digests.get(referenced_position) == Some(digest) {
                referenced_position += 1;
            }
            let actual = referenced_position - start;
            if backing.source_count != actual {
                return Err(SourceStoreError::ContentReferenceCountMismatch {
                    digest: *digest,
                    indexed: backing.source_count,
                    actual,
                });
            }
        }
        if referenced_position != referenced_digests.len() {
            return Err(SourceStoreError::UnmatchedContentReferences {
                matched: referenced_position,
                total: referenced_digests.len(),
            });
        }
        Ok(())
    }

    fn ensure_workspace(&self, source: SourceId) -> Result<(), SourceStoreError> {
        if source.workspace() != self.workspace {
            return Err(SourceStoreError::WorkspaceMismatch {
                source_id: source,
                expected: self.workspace,
                actual: source.workspace(),
            });
        }
        Ok(())
    }

    fn validate_content_reference(
        &self,
        source: SourceId,
        entry: &SourceEntry,
    ) -> Result<DigestV1, SourceStoreError> {
        let digest = entry.image.fingerprint().digest();
        let indexed = self
            .by_digest
            .get(&digest)
            .ok_or(SourceStoreError::MissingContentIndex {
                source_id: source,
                digest,
            })?;
        if !Arc::ptr_eq(&indexed.bytes, entry.image.backing()) {
            return Err(SourceStoreError::BackingNotCanonical {
                source_id: source,
                digest,
            });
        }
        if indexed.source_count == 0 {
            return Err(SourceStoreError::ContentReferenceCountUnderflow { digest });
        }
        Ok(digest)
    }

    fn release_content_reference(&mut self, digest: DigestV1) {
        let remove = self
            .by_digest
            .get(&digest)
            .is_some_and(|backing| backing.source_count == 1);
        if remove {
            self.by_digest.remove(&digest);
        } else if let Some(backing) = self.by_digest.get_mut(&digest) {
            backing.source_count -= 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum SourceStoreError {
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error("source {source_id:?} belongs to workspace {actual}, not {expected}")]
    WorkspaceMismatch {
        source_id: SourceId,
        expected: WorkspaceId,
        actual: WorkspaceId,
    },
    #[error("source {source_id:?} requires kind {expected:?}, got {actual:?}")]
    SourceKindMismatch {
        source_id: SourceId,
        expected: SourceKind,
        actual: SourceKind,
    },
    #[error(
        "source {source_id:?} of kind {source_kind:?} cannot retain frozen parse kind {parse_kind}"
    )]
    FrozenParseKindMismatch {
        source_id: SourceId,
        source_kind: SourceKind,
        parse_kind: &'static str,
    },
    #[error("serialized parse for source {source_id:?} does not use its verified complete backing")]
    FrozenSerializedBackingMismatch { source_id: SourceId },
    #[error("serialized parse for source {source_id:?} is shared and cannot be rebound atomically")]
    FrozenSerializedParseShared { source_id: SourceId },
    #[error("unknown source image: {0:?}")]
    UnknownSource(SourceId),
    #[error("source removal batch contains duplicate source {0:?}")]
    DuplicateRemovalSource(SourceId),
    #[error("digest collision for distinct source bytes: {digest}")]
    DigestCollision { digest: DigestV1 },
    #[error("content reference count overflow for {digest}")]
    ContentReferenceCountOverflow { digest: DigestV1 },
    #[error("content index {digest} records zero references while a source uses it")]
    ContentReferenceCountUnderflow { digest: DigestV1 },
    #[error("source store retained-size arithmetic overflow")]
    RetainedSizeOverflow,
    #[error("failed to reserve {requested} entries for {resource}")]
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    #[error("source store key {key:?} contains entry {entry:?}")]
    EntryIdentityMismatch { key: SourceId, entry: SourceId },
    #[error("source {source_id:?} has no content index for {digest}")]
    MissingContentIndex {
        source_id: SourceId,
        digest: DigestV1,
    },
    #[error("source {source_id:?} does not use the canonical backing for {digest}")]
    BackingNotCanonical {
        source_id: SourceId,
        digest: DigestV1,
    },
    #[error("content index {digest} has no source entry")]
    UnreferencedContentIndex { digest: DigestV1 },
    #[error("content index {digest} records {indexed} references, but {actual} sources use it")]
    ContentReferenceCountMismatch {
        digest: DigestV1,
        indexed: usize,
        actual: usize,
    },
    #[error("content index matched {matched} of {total} source references")]
    UnmatchedContentReferences { matched: usize, total: usize },
}

fn retained_insert_bytes(new_digest: bool) -> Result<u64, SourceStoreError> {
    let mut bytes = checked_byte_add(
        checked_arc_allocation_bytes::<SourceEntry>()?,
        checked_btree_entry_bytes::<SourceId, Arc<SourceEntry>>()?,
    )?;
    if new_digest {
        bytes = checked_byte_add(
            bytes,
            checked_btree_entry_bytes::<DigestV1, ContentBacking>()?,
        )?;
    }
    Ok(bytes)
}

fn checked_arc_allocation_bytes<T>() -> Result<u64, SourceStoreError> {
    arc_value_allocation_bytes::<T>().map_err(|_| SourceStoreError::RetainedSizeOverflow)
}

fn checked_btree_entry_bytes<K, V>() -> Result<u64, SourceStoreError> {
    // A newly allocated BTree node is sparse. Charge a complete conservative node for every
    // logical entry so the first insertion cannot hide the unused key/value slots.
    const MAX_NODE_SLOTS: usize = 32;
    const NODE_METADATA_WORDS: usize = 8;
    let slot_bytes = size_of::<(K, V)>()
        .checked_add(size_of::<usize>().saturating_mul(2))
        .ok_or(SourceStoreError::RetainedSizeOverflow)?;
    let bytes = slot_bytes
        .checked_mul(MAX_NODE_SLOTS)
        .and_then(|value| value.checked_add(size_of::<usize>().saturating_mul(NODE_METADATA_WORDS)))
        .ok_or(SourceStoreError::RetainedSizeOverflow)?;
    usize_to_u64(bytes)
}

fn checked_btree_entries_bytes<K, V>(count: usize) -> Result<u64, SourceStoreError> {
    checked_btree_entry_bytes::<K, V>()?
        .checked_mul(usize_to_u64(count)?)
        .ok_or(SourceStoreError::RetainedSizeOverflow)
}

fn checked_vec_bytes<T>(count: usize) -> Result<u64, SourceStoreError> {
    vec_allocation_bytes::<T>(count).map_err(|_| SourceStoreError::RetainedSizeOverflow)
}

fn checked_byte_add(left: u64, right: u64) -> Result<u64, SourceStoreError> {
    left.checked_add(right)
        .ok_or(SourceStoreError::RetainedSizeOverflow)
}

fn usize_to_u64(value: usize) -> Result<u64, SourceStoreError> {
    u64::try_from(value).map_err(|_| SourceStoreError::RetainedSizeOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_binary::asset::SerializedFileParser;
    use unity_asset_binary::shared_bytes::SharedBytes;
    use unity_asset_core::AssetLoadLimits;

    const V22_FIXTURE: &[u8] = include_bytes!(
        "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
    );

    fn source(workspace: WorkspaceId, local: u128) -> SourceId {
        SourceId::new(workspace, SourceKind::Archive, local).unwrap()
    }

    fn image(bytes: &[u8]) -> VerifiedSourceImage {
        VerifiedSourceImage::verify(SourceKind::Archive, bytes.to_vec().into())
    }

    fn serialized_source(workspace: WorkspaceId, local: u128) -> SourceId {
        SourceId::new(workspace, SourceKind::SerializedFile, local).unwrap()
    }

    fn serialized_image(backing: Arc<[u8]>) -> VerifiedSourceImage {
        VerifiedSourceImage::verify(SourceKind::SerializedFile, backing)
    }

    fn serialized_parse(backing: Arc<[u8]>) -> Arc<SerializedFile> {
        let len = backing.len();
        Arc::new(
            SerializedFileParser::from_shared_range(SharedBytes::from_arc(backing), 0..len)
                .expect("SerializedFile fixture parses"),
        )
    }

    fn insert(
        store: &mut SourceStore,
        source: SourceId,
        bytes: &[u8],
        budget: &mut AssetLoadBudget,
    ) -> Arc<SourceEntry> {
        store
            .insert(source, image(bytes), FrozenSourceParse::None, budget)
            .unwrap()
    }

    fn validate(store: &SourceStore) -> Result<(), SourceStoreError> {
        store.validate(&mut AssetLoadBudget::default())
    }

    fn budget_with(max_bytes: u64, max_entries: u64) -> AssetLoadBudget {
        AssetLoadBudget::new(AssetLoadLimits {
            max_bytes,
            max_entries,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn identical_images_reuse_one_arc_backing() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let first = insert(&mut store, source(workspace, 1), b"same", &mut budget);
        let second = insert(&mut store, source(workspace, 2), b"same", &mut budget);

        assert!(Arc::ptr_eq(
            first.image().backing(),
            second.image().backing()
        ));
        assert_eq!(store.by_digest.len(), 1);
        assert_eq!(
            store
                .by_digest
                .get(&first.image().fingerprint().digest())
                .unwrap()
                .source_count,
            2
        );
        validate(&store).unwrap();
    }

    #[test]
    fn serialized_parse_must_share_the_verified_image_backing() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = serialized_source(workspace, 1);
        let image_backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let parsed_backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let usage = budget.usage();

        let error = store
            .insert(
                source,
                serialized_image(image_backing),
                FrozenSourceParse::Serialized(serialized_parse(parsed_backing)),
                &mut budget,
            )
            .expect_err("equal bytes in different allocations are not one proven source");

        assert_eq!(
            error,
            SourceStoreError::FrozenSerializedBackingMismatch { source_id: source }
        );
        assert_eq!(budget.usage(), usage);
        assert!(store.is_empty());
        assert!(store.by_digest.is_empty());
    }

    #[test]
    fn serialized_parse_rebinds_to_the_canonical_content_backing() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let first_source = serialized_source(workspace, 1);
        let second_source = serialized_source(workspace, 2);
        let first_backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let second_backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();

        let first = store
            .insert(
                first_source,
                serialized_image(Arc::clone(&first_backing)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&first_backing))),
                &mut budget,
            )
            .unwrap();
        let second = store
            .insert(
                second_source,
                serialized_image(Arc::clone(&second_backing)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&second_backing))),
                &mut budget,
            )
            .unwrap();

        assert!(Arc::ptr_eq(
            first.image().backing(),
            second.image().backing()
        ));
        assert!(Arc::ptr_eq(first.image().backing(), &first_backing));
        let parsed = second
            .cached_serialized()
            .expect("serialized parse is cached");
        assert_eq!(
            parsed.data_identity_key(),
            (first_backing.as_ptr() as usize, 0, first_backing.len())
        );
        match parsed.data_shared() {
            SharedBytes::Arc(parsed_backing) => {
                assert!(Arc::ptr_eq(&parsed_backing, &first_backing));
            }
            #[cfg(feature = "mmap")]
            SharedBytes::Mmap(_) => panic!("the cached parse must use the canonical Arc"),
        }
        assert_eq!(Arc::strong_count(&second_backing), 1);
        validate(&store).unwrap();
    }

    #[test]
    fn shared_serialized_parse_rebind_failure_is_atomic_and_uncharged() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let first_source = serialized_source(workspace, 1);
        let second_source = serialized_source(workspace, 2);
        let canonical: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let candidate: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let first = store
            .insert(
                first_source,
                serialized_image(Arc::clone(&canonical)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&canonical))),
                &mut budget,
            )
            .unwrap();
        let parsed = serialized_parse(Arc::clone(&candidate));
        let parsed_observer = Arc::clone(&parsed);
        let usage = budget.usage();

        let error = store
            .insert(
                second_source,
                serialized_image(Arc::clone(&candidate)),
                FrozenSourceParse::Serialized(parsed),
                &mut budget,
            )
            .expect_err("a shared parsed object cannot be mutated for canonicalization");

        assert_eq!(
            error,
            SourceStoreError::FrozenSerializedParseShared {
                source_id: second_source,
            }
        );
        assert_eq!(budget.usage(), usage);
        assert_eq!(store.len(), 1);
        assert!(store.get(second_source).is_none());
        assert_eq!(
            store.by_digest[&first.image().fingerprint().digest()].source_count,
            1
        );
        assert_eq!(
            parsed_observer.data_identity_key(),
            (candidate.as_ptr() as usize, 0, candidate.len())
        );
        validate(&store).unwrap();
    }

    #[test]
    fn post_rebind_budget_failure_preserves_store_and_backing_lifetimes() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let first_source = serialized_source(workspace, 1);
        let second_source = serialized_source(workspace, 2);
        let canonical: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let candidate: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mut store = SourceStore::new(workspace);
        let first = store
            .insert(
                first_source,
                serialized_image(Arc::clone(&canonical)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&canonical))),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let digest = first.image().fingerprint().digest();
        let canonical_strong_count = Arc::strong_count(&canonical);
        let retained_bytes = retained_insert_bytes(false).unwrap();
        let mut budget = budget_with(retained_bytes - 1, 1);

        let error = store
            .insert(
                second_source,
                serialized_image(Arc::clone(&candidate)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&candidate))),
                &mut budget,
            )
            .expect_err("retained entry backing must fit before publication");

        assert!(matches!(
            error,
            SourceStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == retained_bytes - 1 && requested == retained_bytes
        ));
        assert_eq!(budget.usage(), Default::default());
        assert_eq!(store.len(), 1);
        assert!(store.get(second_source).is_none());
        assert_eq!(store.by_digest[&digest].source_count, 1);
        assert_eq!(Arc::strong_count(&canonical), canonical_strong_count);
        assert_eq!(Arc::strong_count(&candidate), 1);
        validate(&store).unwrap();
    }

    #[test]
    fn same_source_fast_path_validates_backing_and_does_not_charge_budget() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let source = serialized_source(workspace, 1);
        let canonical: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let candidate: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mismatched_parse_backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let existing = store
            .insert(
                source,
                serialized_image(Arc::clone(&canonical)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&canonical))),
                &mut budget,
            )
            .unwrap();
        let usage = budget.usage();

        let error = store
            .insert(
                source,
                serialized_image(Arc::clone(&candidate)),
                FrozenSourceParse::Serialized(serialized_parse(mismatched_parse_backing)),
                &mut budget,
            )
            .expect_err("the fast path must validate the incoming parsed backing");
        assert_eq!(
            error,
            SourceStoreError::FrozenSerializedBackingMismatch { source_id: source }
        );
        assert_eq!(budget.usage(), usage);
        assert!(Arc::ptr_eq(store.get(source).unwrap(), &existing));

        let unchanged = store
            .insert(
                source,
                serialized_image(Arc::clone(&candidate)),
                FrozenSourceParse::Serialized(serialized_parse(Arc::clone(&candidate))),
                &mut budget,
            )
            .unwrap();
        assert!(Arc::ptr_eq(&unchanged, &existing));
        assert_eq!(budget.usage(), usage);
        assert_eq!(Arc::strong_count(&candidate), 1);
        validate(&store).unwrap();
    }

    #[test]
    fn replacement_and_removal_update_content_reference_counts() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let first_source = source(workspace, 1);
        let second_source = source(workspace, 2);
        let shared = image(b"shared");
        let shared_digest = shared.fingerprint().digest();

        store
            .insert(
                first_source,
                shared.clone(),
                FrozenSourceParse::None,
                &mut budget,
            )
            .unwrap();
        store
            .insert(second_source, shared, FrozenSourceParse::None, &mut budget)
            .unwrap();

        let replacement = image(b"replacement");
        let replacement_digest = replacement.fingerprint().digest();
        store
            .insert(
                first_source,
                replacement,
                FrozenSourceParse::None,
                &mut budget,
            )
            .unwrap();

        assert_eq!(store.by_digest[&shared_digest].source_count, 1);
        assert_eq!(store.by_digest[&replacement_digest].source_count, 1);
        store.remove(second_source).unwrap();
        assert!(!store.by_digest.contains_key(&shared_digest));
        assert_eq!(store.by_digest[&replacement_digest].source_count, 1);
        validate(&store).unwrap();
    }

    #[test]
    fn replacement_with_existing_digest_updates_both_reference_counts() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let first_source = source(workspace, 1);
        let second_source = source(workspace, 2);
        let first = insert(&mut store, first_source, b"first", &mut budget);
        let second = insert(&mut store, second_source, b"second", &mut budget);
        let first_digest = first.image().fingerprint().digest();
        let second_digest = second.image().fingerprint().digest();

        let replaced = insert(&mut store, first_source, b"second", &mut budget);

        assert!(!store.by_digest.contains_key(&first_digest));
        assert_eq!(store.by_digest[&second_digest].source_count, 2);
        assert!(Arc::ptr_eq(
            replaced.image().backing(),
            second.image().backing()
        ));
        validate(&store).unwrap();
    }

    #[test]
    fn validation_detects_incorrect_content_reference_count() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        let entry = insert(&mut store, source(workspace, 1), b"bytes", &mut budget);
        let digest = entry.image().fingerprint().digest();
        store.by_digest.get_mut(&digest).unwrap().source_count = 2;

        assert_eq!(
            validate(&store),
            Err(SourceStoreError::ContentReferenceCountMismatch {
                digest,
                indexed: 2,
                actual: 1,
            })
        );
    }

    #[test]
    fn large_unique_store_validates_and_removes_without_quadratic_scans() {
        const SOURCE_COUNT: u128 = 4_096;

        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut budget = AssetLoadBudget::default();
        for local in 1..=SOURCE_COUNT {
            insert(
                &mut store,
                source(workspace, local),
                &local.to_le_bytes(),
                &mut budget,
            );
        }

        validate(&store).unwrap();
        assert_eq!(store.by_digest.len(), SOURCE_COUNT as usize);
        let sources = (1..=SOURCE_COUNT)
            .map(|local| source(workspace, local))
            .collect::<Vec<_>>();
        store
            .remove_all(&sources, &mut AssetLoadBudget::default())
            .unwrap();
        assert!(store.is_empty());
        assert!(store.by_digest.is_empty());
        validate(&store).unwrap();
    }

    #[test]
    fn failed_insert_does_not_publish_an_entry() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let limits = AssetLoadLimits {
            max_bytes: 1,
            ..Default::default()
        };
        let mut budget = AssetLoadBudget::new(limits).unwrap();
        let mut store = SourceStore::new(workspace);

        assert!(
            store
                .insert(
                    source(workspace, 1),
                    image(b"bytes"),
                    FrozenSourceParse::None,
                    &mut budget,
                )
                .is_err()
        );
        assert!(store.is_empty());
        assert!(store.by_digest.is_empty());
    }

    #[test]
    fn failed_replacement_preserves_entry_and_reference_counts() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let mut initial_budget = AssetLoadBudget::default();
        let source = source(workspace, 1);
        let original = insert(&mut store, source, b"original", &mut initial_budget);
        let original_digest = original.image().fingerprint().digest();
        let mut constrained_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..Default::default()
        })
        .unwrap();

        assert!(
            store
                .insert(
                    source,
                    image(b"replacement"),
                    FrozenSourceParse::None,
                    &mut constrained_budget,
                )
                .is_err()
        );
        assert!(Arc::ptr_eq(store.get(source).unwrap(), &original));
        assert_eq!(store.by_digest.len(), 1);
        assert_eq!(store.by_digest[&original_digest].source_count, 1);
        validate(&store).unwrap();
    }

    #[test]
    fn update_clone_reuses_entries_and_backings() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut budget = AssetLoadBudget::default();
        let mut store = SourceStore::new(workspace);
        let source = source(workspace, 1);
        let entry = insert(&mut store, source, b"bytes", &mut budget);

        let candidate = store.clone_for_update(&mut budget).unwrap();
        let cloned = candidate.get(source).unwrap();
        assert!(Arc::ptr_eq(&entry, cloned));
        assert!(Arc::ptr_eq(
            entry.image().backing(),
            cloned.image().backing()
        ));
    }

    #[test]
    fn frozen_parse_kind_is_checked_before_a_same_image_no_op() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let source = source(workspace, 1);
        let mut budget = AssetLoadBudget::default();
        let existing = insert(&mut store, source, b"bytes", &mut budget);
        let usage = budget.usage();

        let error = store
            .insert(
                source,
                image(b"bytes"),
                FrozenSourceParse::Yaml(Arc::new(YamlDocument::new())),
                &mut budget,
            )
            .unwrap_err();

        assert!(matches!(
            error,
            SourceStoreError::FrozenParseKindMismatch {
                source_id,
                source_kind: SourceKind::Archive,
                parse_kind: "yaml",
            } if source_id == source
        ));
        assert_eq!(budget.usage(), usage);
        assert!(Arc::ptr_eq(store.get(source).unwrap(), &existing));
    }

    #[test]
    fn remove_all_prevalidates_unknown_and_duplicate_sources_atomically() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        let first = source(workspace, 1);
        let second = source(workspace, 2);
        let unknown = source(workspace, 3);
        let mut load_budget = AssetLoadBudget::default();
        let first_entry = insert(&mut store, first, b"shared", &mut load_budget);
        insert(&mut store, second, b"shared", &mut load_budget);
        let digest = first_entry.image().fingerprint().digest();

        let mut removal_budget = AssetLoadBudget::default();
        assert_eq!(
            store.remove_all(&[first, unknown], &mut removal_budget),
            Err(SourceStoreError::UnknownSource(unknown))
        );
        assert!(store.contains(first));
        assert!(store.contains(second));
        assert_eq!(store.by_digest[&digest].source_count, 2);

        assert_eq!(
            store.remove_all(&[first, first], &mut removal_budget),
            Err(SourceStoreError::DuplicateRemovalSource(first))
        );
        assert!(store.contains(first));
        assert!(store.contains(second));
        assert_eq!(store.by_digest[&digest].source_count, 2);
        validate(&store).unwrap();
    }

    #[test]
    fn validation_scratch_is_rejected_before_reservation() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        insert(
            &mut store,
            source(workspace, 1),
            b"bytes",
            &mut AssetLoadBudget::default(),
        );
        let scratch_bytes = checked_vec_bytes::<DigestV1>(store.len()).unwrap();

        let mut rejected = budget_with(scratch_bytes - 1, 1);
        assert!(matches!(
            store.validate(&mut rejected),
            Err(SourceStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(rejected.usage().bytes, 0);

        let mut exact = budget_with(scratch_bytes, 1);
        store.validate(&mut exact).unwrap();
        assert_eq!(exact.usage().bytes, scratch_bytes);
    }

    #[test]
    fn first_insert_charges_arc_and_conservative_btree_backing() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let retained_bytes = retained_insert_bytes(true).unwrap();
        let naive_payload = usize_to_u64(
            size_of::<SourceEntry>()
                + size_of::<(SourceId, Arc<SourceEntry>)>()
                + size_of::<(DigestV1, ContentBacking)>(),
        )
        .unwrap();
        assert!(retained_bytes > naive_payload);

        let mut store = SourceStore::new(workspace);
        let mut rejected = budget_with(retained_bytes - 1, 1);
        assert!(matches!(
            store.insert(
                source(workspace, 1),
                image(b"bytes"),
                FrozenSourceParse::None,
                &mut rejected,
            ),
            Err(SourceStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert!(store.is_empty());
        assert!(store.by_digest.is_empty());
        assert_eq!(rejected.usage().bytes, 0);

        let mut exact = budget_with(retained_bytes, 1);
        insert(&mut store, source(workspace, 1), b"bytes", &mut exact);
        assert_eq!(exact.usage().bytes, retained_bytes);
    }

    #[test]
    fn update_clone_charges_conservative_btree_backing_before_allocation() {
        let workspace = WorkspaceId::from_u128(1).unwrap();
        let mut store = SourceStore::new(workspace);
        insert(
            &mut store,
            source(workspace, 1),
            b"bytes",
            &mut AssetLoadBudget::default(),
        );
        let retained_bytes = store.checked_clone_bytes().unwrap();

        let mut rejected = budget_with(retained_bytes - 1, 1);
        assert!(matches!(
            store.clone_for_update(&mut rejected),
            Err(SourceStoreError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                ..
            }))
        ));
        assert_eq!(rejected.usage().bytes, 0);
        assert_eq!(rejected.usage().entries, 0);

        let mut exact = budget_with(retained_bytes, 1);
        let candidate = store.clone_for_update(&mut exact).unwrap();
        assert_eq!(candidate.len(), 1);
        assert_eq!(exact.usage().bytes, retained_bytes);
        assert_eq!(exact.usage().entries, 1);
    }
}
