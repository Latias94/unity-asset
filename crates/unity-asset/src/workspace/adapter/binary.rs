use std::collections::TryReserveError;
use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;
use unity_asset_binary::asset::{SerializedFile, SerializedFileParser};
use unity_asset_binary::bundle::{AssetBundle, BundleLoadOptions, BundleParser};
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::file::{UnityFileKind, sniff_unity_file_kind_prefix};
use unity_asset_binary::shared_bytes::SharedBytes;
use unity_asset_binary::webfile::{WebFile, WebFileProbeError};
use unity_asset_core::{
    AssetLoadBudget, BudgetError, ContractError, SourceMemberId, arc_slice_allocation_bytes,
};

const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const BROTLI_MARKER_OFFSET: usize = 0x20;
const BROTLI_MARKER: &[u8] = b"brotli";

/// Binary parsing boundary used by immutable workspace snapshots.
///
/// Container recursion remains a caller concern. [`Self::members`] expands exactly one level;
/// the workspace mutation boundary freezes any external TypeTree registry before publication.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct BinaryWorkspaceAdapter;

impl BinaryWorkspaceAdapter {
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self
    }

    #[must_use]
    pub(crate) fn has_members(&self, payload: &BinaryPayload) -> bool {
        match payload {
            BinaryPayload::SerializedFile(_) => false,
            BinaryPayload::AssetBundle(bundle) => bundle.nodes.iter().any(|node| node.is_file()),
            BinaryPayload::WebFile(web_file) => !web_file.files().is_empty(),
        }
    }

    /// Parses one owned logical source image without copying its input bytes.
    pub(crate) fn parse(
        &self,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryPayload, BinaryAdapterError> {
        self.parse_root_image(image, budget)
    }

    /// Expands one container level in wire order.
    ///
    /// Serialized files are leaves and therefore return an empty member list. Duplicate member
    /// names remain distinct through [`SourceMemberId::same_name_occurrence`].
    pub(crate) fn members(
        &self,
        payload: &BinaryPayload,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<BinaryMember>, BinaryAdapterError> {
        match payload {
            BinaryPayload::SerializedFile(_) => Ok(Vec::new()),
            BinaryPayload::AssetBundle(bundle) => self.bundle_members(bundle, budget),
            BinaryPayload::WebFile(web_file) => self.webfile_members(web_file, budget),
        }
    }

    fn bundle_members(
        &self,
        bundle: &AssetBundle,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<BinaryMember>, BinaryAdapterError> {
        let member_count = bundle.nodes.iter().filter(|node| node.is_file()).count();
        let mut ordinals = reserve_member_ordinals(member_count, budget)?;
        ordinals.extend(
            bundle
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(wire_index, node)| {
                    node.is_file().then_some(MemberOrdinal {
                        wire_index,
                        same_name_occurrence: 0,
                    })
                }),
        );
        assign_same_name_occurrences(&mut ordinals, |wire_index| {
            bundle.nodes[wire_index].name.as_str()
        })?;

        let mut members = reserve_members(member_count, budget)?;
        for ordinal in ordinals {
            let node = &bundle.nodes[ordinal.wire_index];
            let wire_ordinal = wire_ordinal(ordinal.wire_index)?;
            let identity = copy_member_identity(
                &node.name,
                ordinal.same_name_occurrence,
                BinaryContainerKind::AssetBundle,
                wire_ordinal,
                budget,
            )?;

            budget.check_bytes(node.size)?;
            let extracted =
                bundle
                    .extract_node_data_with_budget(node, budget)
                    .map_err(|source| BinaryAdapterError::MemberBinary {
                        container: BinaryContainerKind::AssetBundle,
                        wire_ordinal,
                        source,
                    })?;
            let image = promote_member_image(extracted, budget).map_err(|source| {
                BinaryAdapterError::MemberBinary {
                    container: BinaryContainerKind::AssetBundle,
                    wire_ordinal,
                    source,
                }
            })?;
            let content = self.member_content(
                Arc::clone(&image),
                BinaryContainerKind::AssetBundle,
                wire_ordinal,
                budget,
            )?;
            members.push(BinaryMember {
                wire_ordinal,
                identity,
                image,
                content,
            });
        }
        Ok(members)
    }

    fn webfile_members(
        &self,
        web_file: &WebFile,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<BinaryMember>, BinaryAdapterError> {
        let member_count = web_file.files().len();
        let mut ordinals = reserve_member_ordinals(member_count, budget)?;
        ordinals.extend((0..member_count).map(|wire_index| MemberOrdinal {
            wire_index,
            same_name_occurrence: 0,
        }));
        assign_same_name_occurrences(&mut ordinals, |wire_index| {
            web_file.files()[wire_index].name.as_str()
        })?;

        let mut members = reserve_members(member_count, budget)?;
        for ordinal in ordinals {
            let file = &web_file.files()[ordinal.wire_index];
            let wire_ordinal = wire_ordinal(ordinal.wire_index)?;
            let identity = copy_member_identity(
                &file.name,
                ordinal.same_name_occurrence,
                BinaryContainerKind::WebFile,
                wire_ordinal,
                budget,
            )?;
            let bytes = web_file
                .extract_file_slice_by_info(file)
                .map_err(|source| BinaryAdapterError::MemberBinary {
                    container: BinaryContainerKind::WebFile,
                    wire_ordinal,
                    source,
                })?;
            let image = copy_member_image(bytes, budget).map_err(|source| {
                BinaryAdapterError::MemberBinary {
                    container: BinaryContainerKind::WebFile,
                    wire_ordinal,
                    source,
                }
            })?;
            let content = self.member_content(
                Arc::clone(&image),
                BinaryContainerKind::WebFile,
                wire_ordinal,
                budget,
            )?;
            members.push(BinaryMember {
                wire_ordinal,
                identity,
                image,
                content,
            });
        }
        Ok(members)
    }

    fn member_content(
        &self,
        image: Arc<[u8]>,
        container: BinaryContainerKind,
        wire_ordinal: u64,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryMemberContent, BinaryAdapterError> {
        self.try_parse_member_image(image, budget)
            .map(|payload| match payload {
                Some(payload) => BinaryMemberContent::Parsed(payload),
                None => BinaryMemberContent::RawResource,
            })
            .map_err(|source| BinaryAdapterError::MemberBinary {
                container,
                wire_ordinal,
                source,
            })
    }

    fn parse_root_image(
        &self,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryPayload, BinaryAdapterError> {
        let prefix = image_prefix(&image);
        if let Some(kind) = sniff_unity_file_kind_prefix(prefix) {
            return self
                .parse_recognized_image(image, kind, budget)
                .map_err(root_parse_error);
        }

        if !looks_like_compressed_webfile_candidate(prefix) {
            return Err(BinaryAdapterError::FormatMismatch);
        }
        budget.check_depth(1).map_err(BinaryAdapterError::Budget)?;
        match self.try_parse_webfile_image(image, budget) {
            Ok(Some(payload)) => Ok(payload),
            Ok(None) => Err(BinaryAdapterError::FormatMismatch),
            Err(source) => Err(root_parse_error(source)),
        }
    }

    fn try_parse_member_image(
        &self,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<BinaryPayload>, BinaryError> {
        let prefix = image_prefix(&image);
        if let Some(kind) = sniff_unity_file_kind_prefix(prefix) {
            return self.parse_recognized_image(image, kind, budget).map(Some);
        }
        if !looks_like_compressed_webfile_candidate(prefix) {
            return Ok(None);
        }

        budget.check_depth(1)?;
        self.try_parse_webfile_image(image, budget)
    }

    fn try_parse_webfile_image(
        &self,
        image: Arc<[u8]>,
        budget: &mut AssetLoadBudget,
    ) -> Result<Option<BinaryPayload>, BinaryError> {
        let shared = SharedBytes::from_arc(image);
        let len = shared.len();
        match WebFile::probe_from_shared_range_with_budget(shared, 0..len, budget) {
            Ok(web_file) => boxed_payload(web_file, budget)
                .map(BinaryPayload::WebFile)
                .map(Some),
            Err(WebFileProbeError::Mismatch { .. }) => Ok(None),
            Err(WebFileProbeError::Recognized { source }) => Err(source),
        }
    }

    fn parse_recognized_image(
        &self,
        image: Arc<[u8]>,
        kind: UnityFileKind,
        budget: &mut AssetLoadBudget,
    ) -> Result<BinaryPayload, BinaryError> {
        let shared = SharedBytes::from_arc(image);
        let len = shared.len();
        match kind {
            UnityFileKind::AssetBundle => {
                budget.check_depth(1)?;
                BundleParser::from_shared_range_with_options_and_budget(
                    shared,
                    0..len,
                    BundleLoadOptions::lazy(),
                    budget,
                )
                .and_then(|bundle| boxed_payload(bundle, budget))
                .map(BinaryPayload::AssetBundle)
            }
            UnityFileKind::SerializedFile => {
                let serialized_file =
                    SerializedFileParser::from_shared_range_with_budget(shared, 0..len, budget)?;
                boxed_payload(serialized_file, budget).map(BinaryPayload::SerializedFile)
            }
            UnityFileKind::WebFile => {
                budget.check_depth(1)?;
                WebFile::from_shared_range_with_budget(shared, 0..len, budget)
                    .and_then(|web_file| boxed_payload(web_file, budget))
                    .map(BinaryPayload::WebFile)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum BinaryPayload {
    SerializedFile(Box<SerializedFile>),
    AssetBundle(Box<AssetBundle>),
    WebFile(Box<WebFile>),
}

#[derive(Debug)]
pub(crate) struct BinaryMember {
    wire_ordinal: u64,
    identity: SourceMemberId,
    image: Arc<[u8]>,
    content: BinaryMemberContent,
}

impl BinaryMember {
    pub(crate) fn into_parts(self) -> (u64, SourceMemberId, Arc<[u8]>, BinaryMemberContent) {
        (self.wire_ordinal, self.identity, self.image, self.content)
    }
}

#[derive(Debug)]
pub(crate) enum BinaryMemberContent {
    Parsed(BinaryPayload),
    RawResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryContainerKind {
    AssetBundle,
    WebFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryAdapterAllocationUnit {
    Bytes,
    Elements,
}

impl std::fmt::Display for BinaryAdapterAllocationUnit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Bytes => "bytes",
            Self::Elements => "elements",
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum BinaryAdapterError {
    #[error("input is not a recognized Unity binary source")]
    FormatMismatch,
    #[error("failed to parse Unity binary source")]
    Parse {
        #[source]
        source: BinaryError,
    },
    #[error("failed to read {container:?} member at wire ordinal {wire_ordinal}")]
    MemberBinary {
        container: BinaryContainerKind,
        wire_ordinal: u64,
        #[source]
        source: BinaryError,
    },
    #[error("member {wire_ordinal} has an invalid stable identity")]
    InvalidMemberIdentity {
        container: BinaryContainerKind,
        wire_ordinal: u64,
        #[source]
        source: ContractError,
    },
    #[error("container wire ordinal does not fit in u64")]
    WireOrdinalOverflow,
    #[error("same-name member occurrence exceeds u32 at wire index {wire_index}")]
    SameNameOccurrenceOverflow { wire_index: usize },
    #[error("{resource} retained-size arithmetic overflow")]
    RetainedSizeOverflow { resource: &'static str },
    #[error("failed to reserve {requested} {unit} for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        unit: BinaryAdapterAllocationUnit,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

#[derive(Debug, Clone, Copy)]
struct MemberOrdinal {
    wire_index: usize,
    same_name_occurrence: u32,
}

fn reserve_member_ordinals(
    member_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<MemberOrdinal>, BinaryAdapterError> {
    let mut ordinals = Vec::new();
    reserve_exact_budgeted(
        &mut ordinals,
        member_count,
        budget,
        "binary member ordinal table",
    )?;
    Ok(ordinals)
}

fn reserve_members(
    member_count: usize,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<BinaryMember>, BinaryAdapterError> {
    let mut members = Vec::new();
    reserve_exact_budgeted(
        &mut members,
        member_count,
        budget,
        "binary member output table",
    )?;
    Ok(members)
}

fn reserve_exact_budgeted<T>(
    values: &mut Vec<T>,
    additional: usize,
    budget: &mut AssetLoadBudget,
    resource: &'static str,
) -> Result<(), BinaryAdapterError> {
    let bytes = size_of::<T>()
        .checked_mul(additional)
        .ok_or(BinaryAdapterError::RetainedSizeOverflow { resource })?;
    let bytes =
        u64::try_from(bytes).map_err(|_| BinaryAdapterError::RetainedSizeOverflow { resource })?;
    budget.check_bytes(bytes)?;
    budget.consume_bytes(bytes)?;
    values
        .try_reserve_exact(additional)
        .map_err(|source| BinaryAdapterError::Allocation {
            resource,
            requested: additional,
            unit: BinaryAdapterAllocationUnit::Elements,
            source,
        })
}

fn assign_same_name_occurrences<'a>(
    ordinals: &mut [MemberOrdinal],
    name_at: impl Fn(usize) -> &'a str,
) -> Result<(), BinaryAdapterError> {
    ordinals.sort_unstable_by(|left, right| {
        name_at(left.wire_index)
            .cmp(name_at(right.wire_index))
            .then_with(|| left.wire_index.cmp(&right.wire_index))
    });

    let mut group_start = 0;
    while group_start < ordinals.len() {
        let group_name = name_at(ordinals[group_start].wire_index);
        let mut group_end = group_start + 1;
        while group_end < ordinals.len() && name_at(ordinals[group_end].wire_index) == group_name {
            group_end += 1;
        }
        for (occurrence, ordinal) in ordinals[group_start..group_end].iter_mut().enumerate() {
            ordinal.same_name_occurrence = u32::try_from(occurrence).map_err(|_| {
                BinaryAdapterError::SameNameOccurrenceOverflow {
                    wire_index: ordinal.wire_index,
                }
            })?;
        }
        group_start = group_end;
    }

    ordinals.sort_unstable_by_key(|ordinal| ordinal.wire_index);
    Ok(())
}

fn copy_member_identity(
    name: &str,
    same_name_occurrence: u32,
    container: BinaryContainerKind,
    wire_ordinal: u64,
    budget: &mut AssetLoadBudget,
) -> Result<SourceMemberId, BinaryAdapterError> {
    let byte_count =
        u64::try_from(name.len()).map_err(|_| BinaryAdapterError::RetainedSizeOverflow {
            resource: "binary member identity",
        })?;
    budget.check_bytes(byte_count)?;
    budget.consume_bytes(byte_count)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|source| BinaryAdapterError::Allocation {
            resource: "binary member identity",
            requested: name.len(),
            unit: BinaryAdapterAllocationUnit::Bytes,
            source,
        })?;
    owned.push_str(name);
    SourceMemberId::with_occurrence(owned, same_name_occurrence).map_err(|source| {
        BinaryAdapterError::InvalidMemberIdentity {
            container,
            wire_ordinal,
            source,
        }
    })
}

fn copy_member_image(bytes: &[u8], budget: &mut AssetLoadBudget) -> Result<Arc<[u8]>, BinaryError> {
    let byte_count = checked_arc_slice_allocation_bytes(bytes.len())?;
    budget.check_bytes(byte_count)?;
    budget.consume_bytes(byte_count)?;
    Ok(Arc::from(bytes))
}

fn promote_member_image(
    bytes: Vec<u8>,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<[u8]>, BinaryError> {
    let byte_count = checked_arc_slice_allocation_bytes(bytes.len())?;
    // Bundle extraction owns a temporary Vec. Promoting it to Arc allocates the retained image,
    // so both allocations remain visible in the monotonic budget.
    budget.check_bytes(byte_count)?;
    budget.consume_bytes(byte_count)?;
    Ok(Arc::from(bytes))
}

fn checked_arc_slice_allocation_bytes(length: usize) -> Result<u64, BinaryError> {
    arc_slice_allocation_bytes::<u8>(length)
        .map_err(|error| BinaryError::invalid_data(error.to_string()))
}

fn boxed_payload<T>(value: T, budget: &mut AssetLoadBudget) -> Result<Box<T>, BinaryError> {
    let byte_count = u64::try_from(size_of::<T>())
        .map_err(|_| BinaryError::invalid_data("Binary payload size does not fit in u64"))?;
    budget.check_bytes(byte_count)?;
    budget.consume_bytes(byte_count)?;
    Ok(Box::new(value))
}

fn root_parse_error(source: BinaryError) -> BinaryAdapterError {
    match source {
        BinaryError::Budget(error) => BinaryAdapterError::Budget(error),
        source => BinaryAdapterError::Parse { source },
    }
}

fn wire_ordinal(wire_index: usize) -> Result<u64, BinaryAdapterError> {
    u64::try_from(wire_index).map_err(|_| BinaryAdapterError::WireOrdinalOverflow)
}

fn image_prefix(image: &[u8]) -> &[u8] {
    &image[..image.len().min(64)]
}

fn looks_like_compressed_webfile_candidate(prefix: &[u8]) -> bool {
    prefix.starts_with(GZIP_MAGIC)
        || prefix
            .get(BROTLI_MARKER_OFFSET..BROTLI_MARKER_OFFSET + BROTLI_MARKER.len())
            .is_some_and(|marker| marker == BROTLI_MARKER)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use unity_asset_core::AssetLoadLimits;

    use super::*;

    fn webfile_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let head_length = entries.iter().fold(20_usize, |length, (name, _)| {
            length
                .checked_add(12 + name.len())
                .expect("test WebFile header length does not overflow")
        });
        let mut payload_offset = head_length;
        let mut directory = Vec::new();
        for (name, payload) in entries {
            directory.extend_from_slice(
                &i32::try_from(payload_offset)
                    .expect("test WebFile payload offset fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(
                &i32::try_from(payload.len())
                    .expect("test WebFile payload length fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(
                &i32::try_from(name.len())
                    .expect("test WebFile name length fits in i32")
                    .to_le_bytes(),
            );
            directory.extend_from_slice(name.as_bytes());
            payload_offset = payload_offset
                .checked_add(payload.len())
                .expect("test WebFile payload range does not overflow");
        }

        let mut bytes = b"UnityWebData1.0\0".to_vec();
        bytes.extend_from_slice(
            &i32::try_from(head_length)
                .expect("test WebFile header length fits in i32")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&directory);
        for (_, payload) in entries {
            bytes.extend_from_slice(payload);
        }
        bytes
    }

    fn minimal_unityfs_bundle(revision: &str) -> Vec<u8> {
        let mut blocks_info = vec![0_u8; 16];
        blocks_info.extend_from_slice(&1_i32.to_be_bytes());
        blocks_info.extend_from_slice(&1_u32.to_be_bytes());
        blocks_info.extend_from_slice(&1_u32.to_be_bytes());
        blocks_info.extend_from_slice(&0_u16.to_be_bytes());
        blocks_info.extend_from_slice(&0_i32.to_be_bytes());

        let mut bytes = b"UnityFS\0".to_vec();
        bytes.extend_from_slice(&7_u32.to_be_bytes());
        bytes.extend_from_slice(b"5.x.x\0");
        bytes.extend_from_slice(revision.as_bytes());
        bytes.push(0);
        let size_offset = bytes.len();
        bytes.extend_from_slice(&0_i64.to_be_bytes());
        bytes.extend_from_slice(
            &u32::try_from(blocks_info.len())
                .expect("test blocks info length fits in u32")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(
            &u32::try_from(blocks_info.len())
                .expect("test blocks info length fits in u32")
                .to_be_bytes(),
        );
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        while !bytes.len().is_multiple_of(16) {
            bytes.push(0);
        }
        bytes.extend_from_slice(&blocks_info);
        bytes.push(0);
        let total_len = i64::try_from(bytes.len()).expect("test bundle length fits in i64");
        bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());
        bytes
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn webfile_with_head_length(head_length: i32) -> Vec<u8> {
        let mut bytes = b"UnityWebData1.0\0".to_vec();
        bytes.extend_from_slice(&head_length.to_le_bytes());
        bytes
    }

    fn webfile_with_corrupt_directory() -> Vec<u8> {
        let mut bytes = webfile_with_head_length(33);
        bytes.extend_from_slice(&33_i32.to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&4_i32.to_le_bytes());
        bytes.push(b'x');
        bytes
    }

    #[test]
    fn webfile_members_preserve_wire_order_duplicate_occurrence_and_exact_images() {
        let nested_bundle = minimal_unityfs_bundle("2020.3.0f1");
        let raw = b"raw resource";
        let webfile = webfile_with_entries(&[
            ("duplicate.bin", raw.as_slice()),
            ("duplicate.bin", nested_bundle.as_slice()),
        ]);
        let adapter = BinaryWorkspaceAdapter::new();
        let mut budget = AssetLoadBudget::default();
        let payload = adapter.parse(Arc::from(webfile), &mut budget).unwrap();
        assert_eq!(budget.usage().members, 2);

        let members = adapter.members(&payload, &mut budget).unwrap();

        assert_eq!(members.len(), 2);
        assert_eq!(budget.usage().members, 2);
        assert_eq!(members[0].wire_ordinal, 0);
        assert_eq!(members[1].wire_ordinal, 1);
        assert_eq!(members[0].identity.name(), "duplicate.bin");
        assert_eq!(members[0].identity.same_name_occurrence(), 0);
        assert_eq!(members[1].identity.same_name_occurrence(), 1);
        assert_eq!(members[0].image.as_ref(), raw);
        assert_eq!(members[1].image.as_ref(), nested_bundle.as_slice());
        assert!(matches!(
            &members[0].content,
            BinaryMemberContent::RawResource
        ));
        assert!(matches!(
            &members[1].content,
            BinaryMemberContent::Parsed(BinaryPayload::AssetBundle(_))
        ));
    }

    #[test]
    fn container_depth_is_checked_before_unityfs_or_gzip_decompression() {
        let webfile = webfile_with_entries(&[("payload.resource", b"payload")]);
        let images = [minimal_unityfs_bundle("2020.3.0f1"), gzip(&webfile)];

        for image in images {
            let adapter = BinaryWorkspaceAdapter::new();
            let mut budget = AssetLoadBudget::new(AssetLoadLimits {
                max_depth: 1,
                ..AssetLoadLimits::default()
            })
            .unwrap();

            let error = {
                let mut scoped = budget.enter_depth(1).unwrap();
                adapter.parse(Arc::from(image), &mut scoped).unwrap_err()
            };

            assert!(matches!(
                error,
                BinaryAdapterError::Budget(BudgetError::Exceeded {
                    resource: "depth",
                    limit: 1,
                    requested: 2,
                })
            ));
            assert_eq!(budget.usage().decompressed_bytes, 0);
            assert_eq!(budget.usage().bytes, 0);
        }
    }

    #[test]
    fn compressed_recognized_root_corruption_is_a_parse_error() {
        for decoded in [
            webfile_with_head_length(-1),
            webfile_with_head_length(1024),
            webfile_with_corrupt_directory(),
        ] {
            let adapter = BinaryWorkspaceAdapter::new();
            let mut budget = AssetLoadBudget::default();

            let error = adapter
                .parse(Arc::from(gzip(&decoded)), &mut budget)
                .expect_err("recognized corrupt WebFile must not become a format mismatch");

            assert!(matches!(
                error,
                BinaryAdapterError::Parse {
                    source: BinaryError::InvalidData(_),
                }
            ));
        }
    }

    #[test]
    fn compressed_recognized_member_corruption_is_not_a_raw_resource() {
        for decoded in [
            webfile_with_head_length(-1),
            webfile_with_corrupt_directory(),
        ] {
            let nested = gzip(&decoded);
            let outer = webfile_with_entries(&[("nested.web", nested.as_slice())]);
            let adapter = BinaryWorkspaceAdapter::new();
            let mut budget = AssetLoadBudget::default();
            let payload = adapter.parse(Arc::from(outer), &mut budget).unwrap();

            let error = adapter
                .members(&payload, &mut budget)
                .expect_err("recognized corrupt member must fail container expansion");

            assert!(matches!(
                error,
                BinaryAdapterError::MemberBinary {
                    container: BinaryContainerKind::WebFile,
                    source: BinaryError::InvalidData(_),
                    ..
                }
            ));
        }
    }

    #[test]
    fn compressed_non_webfile_remains_a_mismatch_or_raw_member() {
        let encoded = gzip(b"ordinary gzip payload");
        let adapter = BinaryWorkspaceAdapter::new();
        let mut root_budget = AssetLoadBudget::default();

        let root_error = adapter
            .parse(Arc::from(encoded.clone()), &mut root_budget)
            .expect_err("gzip alone does not establish a root WebFile");
        assert!(matches!(root_error, BinaryAdapterError::FormatMismatch));

        let outer = webfile_with_entries(&[("ordinary.gz", encoded.as_slice())]);
        let mut member_budget = AssetLoadBudget::default();
        let payload = adapter.parse(Arc::from(outer), &mut member_budget).unwrap();
        let members = adapter.members(&payload, &mut member_budget).unwrap();

        assert!(matches!(
            members.as_slice(),
            [BinaryMember {
                content: BinaryMemberContent::RawResource,
                ..
            }]
        ));
    }

    #[test]
    fn member_copy_is_rejected_before_allocation_and_charge() {
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 3,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        let error = copy_member_image(b"four", &mut budget).unwrap_err();

        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 3,
                requested,
            })
            if requested == arc_slice_allocation_bytes::<u8>(4).unwrap()
        ));
        assert_eq!(budget.usage().bytes, 0);
    }

    #[test]
    fn bundle_duplicate_files_keep_wire_ordinals_across_directory_records() {
        let mut bundle = AssetBundle::new(Default::default(), b"firstsecond".to_vec());
        bundle.nodes = vec![
            unity_asset_binary::bundle::DirectoryNode::new("duplicate.bin".to_string(), 0, 5, 0x4),
            unity_asset_binary::bundle::DirectoryNode::new("directory".to_string(), 0, 0, 0),
            unity_asset_binary::bundle::DirectoryNode::new("duplicate.bin".to_string(), 5, 6, 0x4),
        ];
        let payload = BinaryPayload::AssetBundle(Box::new(bundle));
        let adapter = BinaryWorkspaceAdapter::new();
        let mut budget = AssetLoadBudget::default();

        let members = adapter.members(&payload, &mut budget).unwrap();

        assert_eq!(members.len(), 2);
        assert_eq!(members[0].wire_ordinal, 0);
        assert_eq!(members[1].wire_ordinal, 2);
        assert_eq!(members[0].identity.same_name_occurrence(), 0);
        assert_eq!(members[1].identity.same_name_occurrence(), 1);
        assert_eq!(members[0].image.as_ref(), b"first");
        assert_eq!(members[1].image.as_ref(), b"second");
    }

    #[test]
    fn corrupt_recognized_member_keeps_binary_error_in_source_chain() {
        let webfile = webfile_with_entries(&[("broken.bundle", b"UnityFS\0")]);
        let adapter = BinaryWorkspaceAdapter::new();
        let mut budget = AssetLoadBudget::default();
        let payload = adapter.parse(Arc::from(webfile), &mut budget).unwrap();

        let error = adapter.members(&payload, &mut budget).unwrap_err();
        let source = error
            .source()
            .expect("adapter error preserves parser source");

        assert!(matches!(&error, BinaryAdapterError::MemberBinary { .. }));
        assert!(source.downcast_ref::<BinaryError>().is_some());
    }

    #[test]
    fn bundle_members_do_not_attach_a_live_registry_or_build_asset_indexes() {
        let bundle_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../unity-asset-binary/tests/samples/banner_1");
        let bytes = fs::read(bundle_path).expect("read sample AssetBundle");
        let adapter = BinaryWorkspaceAdapter::new();
        let mut budget = AssetLoadBudget::default();

        let payload = adapter.parse(Arc::from(bytes), &mut budget).unwrap();
        let BinaryPayload::AssetBundle(bundle) = &payload else {
            panic!("sample is an AssetBundle");
        };
        assert!(
            bundle.assets.is_empty(),
            "workspace adapter must not preload the asset-index facade"
        );
        let members = adapter.members(&payload, &mut budget).unwrap();
        let serialized = members
            .iter()
            .filter_map(|member| match &member.content {
                BinaryMemberContent::Parsed(BinaryPayload::SerializedFile(file)) => {
                    Some(file.as_ref())
                }
                BinaryMemberContent::Parsed(
                    BinaryPayload::AssetBundle(_) | BinaryPayload::WebFile(_),
                )
                | BinaryMemberContent::RawResource => None,
            })
            .next()
            .expect("sample bundle contains a SerializedFile member");

        assert!(serialized.type_tree_registry().is_none());
    }
}
