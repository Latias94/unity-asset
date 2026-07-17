use super::*;
use crate::asset::{ObjectMetadata, ObjectTypeReference, SerializedFileRegions};
use crate::random_access::ByteSegment;
use std::sync::Arc;

const SEGMENTED_CASES: &[(u32, &[u8])] = &[
    (
        8,
        include_bytes!(
            "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v8.assets.bin"
        ),
    ),
    (
        16,
        include_bytes!(
            "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v16.assets.bin"
        ),
    ),
    (
        22,
        include_bytes!(
            "../../../unity-asset-write/tests/fixtures/serialized_file_wire/v22.assets.bin"
        ),
    ),
];

#[derive(Debug, PartialEq, Eq)]
struct ObjectSnapshot {
    path_id: i64,
    byte_start: u64,
    byte_size: u32,
    class_id: i32,
    type_reference: ObjectTypeReference,
    metadata: ObjectMetadata,
    serialized_type_index: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedSnapshot {
    version: u32,
    header: (u32, u64, u64, u8, [u8; 3], i64),
    regions: SerializedFileRegions,
    unity_version: String,
    target_platform: i32,
    enable_type_tree: bool,
    legacy_big_id: Option<i32>,
    types: Vec<(i32, String, String, usize)>,
    objects: Vec<ObjectSnapshot>,
    externals: Vec<(String, [u8; 16], i32)>,
    ref_types: Vec<i32>,
    user_information: String,
}

fn snapshot(parts: &ParsedParts) -> ParsedSnapshot {
    ParsedSnapshot {
        version: parts.format.version(),
        header: (
            parts.header.metadata_size,
            parts.header.file_size,
            parts.header.data_offset,
            parts.header.endian,
            parts.header.reserved,
            parts.header.unknown,
        ),
        regions: parts.regions.clone(),
        unity_version: parts.unity_version.clone(),
        target_platform: parts.target_platform,
        enable_type_tree: parts.enable_type_tree,
        legacy_big_id: parts.legacy_big_id,
        types: parts
            .types
            .iter()
            .map(|serialized_type| {
                let root = &serialized_type.type_tree.nodes[0];
                (
                    serialized_type.class_id,
                    root.type_name.clone(),
                    root.name.clone(),
                    root.children.len(),
                )
            })
            .collect(),
        objects: parts
            .objects
            .iter()
            .map(|object| ObjectSnapshot {
                path_id: object.path_id(),
                byte_start: object.byte_start(),
                byte_size: object.byte_size(),
                class_id: object.class_id(),
                type_reference: object.type_reference(),
                metadata: object.metadata(),
                serialized_type_index: object.serialized_type_index(),
            })
            .collect(),
        externals: parts
            .externals
            .iter()
            .map(|external| (external.path.clone(), external.guid, external.type_))
            .collect(),
        ref_types: parts
            .ref_types
            .iter()
            .map(|serialized_type| serialized_type.class_id)
            .collect(),
        user_information: parts.user_information.clone(),
    }
}

fn parse_snapshot(source: &dyn ByteSource) -> (ParsedSnapshot, unity_asset_core::AssetLoadUsage) {
    let mut budget = AssetLoadBudget::default();
    let parts = SerializedFileParser::parse_source(source, &mut budget).unwrap();
    (snapshot(&parts), budget.usage())
}

fn source_with_split(backing: &Arc<[u8]>, split: usize) -> SegmentedBytes {
    SegmentedBytes::new(vec![
        ByteSegment::from_arc_range(0, Arc::clone(backing), 0..split).unwrap(),
        ByteSegment::from_arc_range(
            u64::try_from(split).unwrap(),
            Arc::clone(backing),
            split..backing.len(),
        )
        .unwrap(),
    ])
    .unwrap()
}

fn one_byte_source(backing: &Arc<[u8]>) -> SegmentedBytes {
    SegmentedBytes::new(
        (0..backing.len())
            .map(|index| {
                ByteSegment::from_arc_range(
                    u64::try_from(index).unwrap(),
                    Arc::clone(backing),
                    index..index + 1,
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap()
}

#[test]
fn every_split_matches_contiguous_values_regions_and_budget_usage() {
    for (version, bytes) in SEGMENTED_CASES {
        let backing: Arc<[u8]> = Arc::from(*bytes);
        let view = DataView::from_shared(SharedBytes::from_arc(Arc::clone(&backing)));
        let expected = parse_snapshot(&view);
        assert_eq!(expected.0.version, *version);

        for split in 0..=backing.len() {
            assert_eq!(
                parse_snapshot(&source_with_split(&backing, split)),
                expected,
                "SerializedFile v{version} split at {split}"
            );
        }
        assert_eq!(parse_snapshot(&one_byte_source(&backing)), expected);
    }
}

#[test]
fn complete_parser_budget_usage_has_fixed_oracles() {
    let cases = [
        (
            8,
            unity_asset_core::AssetLoadUsage {
                entries: 4,
                bytes: 194,
                max_observed_depth: 0,
                members: 0,
                compressed_bytes: 0,
                decompressed_bytes: 0,
            },
        ),
        (
            22,
            unity_asset_core::AssetLoadUsage {
                entries: 8,
                bytes: 430,
                max_observed_depth: 0,
                members: 0,
                compressed_bytes: 0,
                decompressed_bytes: 0,
            },
        ),
    ];

    for (version, expected) in cases {
        let bytes = SEGMENTED_CASES
            .iter()
            .find(|(candidate, _)| *candidate == version)
            .unwrap()
            .1;
        let backing: Arc<[u8]> = Arc::from(bytes);
        let view = DataView::from_shared(SharedBytes::from_arc(backing));
        let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_entries: expected.entries,
            max_bytes: expected.bytes,
            ..Default::default()
        })
        .unwrap();
        SerializedFileParser::parse_source(&view, &mut exact).unwrap();
        assert_eq!(exact.usage(), expected, "SerializedFile v{version}");

        let mut short = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
            max_entries: expected.entries,
            max_bytes: expected.bytes - 1,
            ..Default::default()
        })
        .unwrap();
        let error = SerializedFileParser::parse_source(&view, &mut short).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            }) if limit == expected.bytes - 1 && requested == expected.bytes
        ));
    }
}

#[test]
fn segmented_errors_and_usage_are_independent_of_boundaries() {
    let mut bytes = SEGMENTED_CASES
        .iter()
        .find(|(version, _)| *version == 22)
        .unwrap()
        .1
        .to_vec();
    bytes[180..184].copy_from_slice(&(-1_i32).to_be_bytes());
    let backing: Arc<[u8]> = Arc::from(bytes);

    let parse_error = |source: &dyn ByteSource| {
        let mut budget = AssetLoadBudget::default();
        let error = SerializedFileParser::parse_source(source, &mut budget).unwrap_err();
        (error.to_string(), budget.usage())
    };
    let view = DataView::from_shared(SharedBytes::from_arc(Arc::clone(&backing)));
    let expected = parse_error(&view);
    assert!(expected.0.contains("Negative SerializedType index"));

    for split in 0..=backing.len() {
        assert_eq!(
            parse_error(&source_with_split(&backing, split)),
            expected,
            "invalid v22 split at {split}"
        );
    }
    assert_eq!(parse_error(&one_byte_source(&backing)), expected);
}

struct TrackingSource {
    image: SegmentedBytes,
    maximum_read: std::cell::Cell<usize>,
    reads: std::cell::RefCell<Vec<Range<u64>>>,
    forbidden: Range<u64>,
}

impl ByteSource for TrackingSource {
    fn len(&self) -> u64 {
        self.image.len()
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end =
            offset
                .checked_add(u64::try_from(output.len()).map_err(|_| {
                    BinaryError::invalid_data("tracked read length does not fit u64")
                })?)
                .ok_or_else(|| BinaryError::invalid_data("tracked read range overflow"))?;
        let range = offset..end;
        self.reads.borrow_mut().push(range.clone());
        if range.start < self.forbidden.end && self.forbidden.start < range.end {
            return Err(BinaryError::invalid_data(
                "validation parser touched the SerializedFile payload region",
            ));
        }
        self.maximum_read
            .set(self.maximum_read.get().max(output.len()));
        ByteSource::read_exact_at(&self.image, offset, output)
    }

    fn contiguous(&self, _range: Range<u64>) -> Option<&[u8]> {
        None
    }
}

#[test]
fn segmented_validation_never_materializes_or_reads_the_payload() {
    let backing: Arc<[u8]> = Arc::from(SEGMENTED_CASES[2].1);
    let source = TrackingSource {
        image: one_byte_source(&backing),
        maximum_read: std::cell::Cell::new(0),
        reads: std::cell::RefCell::new(Vec::new()),
        forbidden: 416..420,
    };
    let mut budget = AssetLoadBudget::default();

    SerializedFileParser::parse_source(&source, &mut budget).unwrap();
    assert!(source.maximum_read.get() < backing.len());

    let mut reads = source.reads.borrow().clone();
    reads.sort_by_key(|range| range.start);
    let covered = reads
        .into_iter()
        .fold(Vec::<Range<u64>>::new(), |mut merged, range| {
            if let Some(previous) = merged.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
                return merged;
            }
            merged.push(range);
            merged
        });
    let covered_bytes = covered
        .iter()
        .map(|range| range.end - range.start)
        .sum::<u64>();
    assert!(covered_bytes < u64::try_from(backing.len()).unwrap());
    assert!(
        covered
            .iter()
            .all(|range| range.end <= 416 || range.start >= 420)
    );
}

#[test]
fn table_and_byte_budgets_fail_before_unbounded_work() {
    let backing: Arc<[u8]> = Arc::from(SEGMENTED_CASES[2].1);
    let source = one_byte_source(&backing);
    let mut entry_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_entries: 1,
        ..Default::default()
    })
    .unwrap();
    let entry_error = SerializedFileParser::parse_source(&source, &mut entry_budget).unwrap_err();
    assert!(matches!(
        entry_error,
        BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
            resource: "entries",
            limit: 1,
            requested: 2,
        })
    ));

    let mut byte_budget = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: 15,
        ..Default::default()
    })
    .unwrap();
    let byte_error = SerializedFileParser::parse_source(&source, &mut byte_budget).unwrap_err();
    assert!(matches!(
        byte_error,
        BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
            resource: "bytes",
            limit: 15,
            requested: 16,
        })
    ));
}

#[test]
fn preloaded_payloads_share_the_parse_byte_budget() {
    let bytes = SEGMENTED_CASES[2].1;
    let view = DataView::from_shared(SharedBytes::from_vec(bytes.to_vec()));
    let mut parse_budget = AssetLoadBudget::default();
    SerializedFileParser::parse_source(&view, &mut parse_budget).unwrap();
    let parsed_bytes = parse_budget.usage().bytes;

    let mut insufficient = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: parsed_bytes + 3,
        ..Default::default()
    })
    .unwrap();
    let error = SerializedFileParser::from_bytes_with_options_and_budget(
        bytes.to_vec(),
        true,
        &mut insufficient,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
            resource: "bytes",
            ..
        })
    ));

    let mut exact = AssetLoadBudget::new(unity_asset_core::AssetLoadLimits {
        max_bytes: parsed_bytes + 4,
        ..Default::default()
    })
    .unwrap();
    let file =
        SerializedFileParser::from_bytes_with_options_and_budget(bytes.to_vec(), true, &mut exact)
            .unwrap();
    assert_eq!(file.objects()[0].loaded_data().unwrap().len(), 4);
    assert_eq!(exact.usage().bytes, parsed_bytes + 4);
}
