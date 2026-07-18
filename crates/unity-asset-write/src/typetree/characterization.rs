use std::hint::black_box;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_binary::typetree::{
    TypeTree, TypeTreeNode, TypeTreeParseMode, TypeTreeParseOptions, TypeTreeSchema,
    TypeTreeTraversalStats,
};
use unity_asset_core::{AssetLoadBudget, AssetLoadUsage, UnityValue};

use super::template::{TemplateRewriteStats, rewrite_object};
use super::test_support::{aligned, map, node, pptr, record, sequence};
use super::writer::encode_object;
use crate::binary_writer::Endian;

const LARGE_BYTE_COUNT: usize = 256 * 1024;
const LARGE_SIGNED_BYTE_COUNT: usize = 64 * 1024;
const LARGE_NUMBER_COUNT: usize = 64 * 1024;
const ADVERSARIAL_SCALAR_FIELDS: usize = 256;
const ADVERSARIAL_EMPTY_SEQUENCES: usize = 64;
const ADVERSARIAL_RECORD_DEPTH: usize = 48;
const BORROWED_TEXT_BYTES: usize = 128 * 1024;
const BORROWED_DATA_BYTES: usize = 128 * 1024;
const BORROWED_RECORD_COUNT: usize = 128;

#[derive(Debug)]
struct Fixture {
    name: &'static str,
    schema: TypeTreeSchema,
    properties: IndexMap<String, UnityValue>,
    expected_pptrs: u64,
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    stats: TypeTreeTraversalStats,
    usage: AssetLoadUsage,
    peak_owned_surrogate_bytes: u64,
}

impl Observation {
    fn new(stats: TypeTreeTraversalStats, usage: AssetLoadUsage) -> Self {
        Self {
            stats,
            usage,
            peak_owned_surrogate_bytes: usage.bytes.saturating_sub(stats.wire_bytes),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RewriteObservation {
    input: TypeTreeTraversalStats,
    output: TypeTreeTraversalStats,
    usage: AssetLoadUsage,
    preserved_bytes: u64,
    peak_owned_surrogate_bytes: u64,
}

impl RewriteObservation {
    fn new(stats: TemplateRewriteStats, usage: AssetLoadUsage) -> Self {
        let traversed_wire = stats
            .input
            .wire_bytes
            .saturating_add(stats.output.wire_bytes);
        Self {
            input: stats.input,
            output: stats.output,
            usage,
            preserved_bytes: stats.preserved_bytes,
            peak_owned_surrogate_bytes: usage.bytes.saturating_sub(traversed_wire),
        }
    }
}

#[derive(Debug)]
struct Characterization {
    fixture: &'static str,
    wire_bytes: u64,
    write: Observation,
    read: Observation,
    skip: Observation,
    scan: Observation,
    rewrite: RewriteObservation,
}

#[derive(Debug, Clone, Copy)]
struct GuardThresholds {
    wire_bytes: u64,
    node_visits: u64,
    members: u64,
    max_depth: u32,
    min_bulk_bytes: u64,
    max_write_bulk_runs: u64,
    max_read_bulk_runs: u64,
    scalar_element_ops: ScalarElementOps,
    max_read_materialized: u64,
    max_rewrite_materialized: u64,
    max_write_owned: u64,
    max_read_owned: u64,
    max_scan_owned: u64,
    max_rewrite_input_owned: u64,
    max_rewrite_output_owned: u64,
    max_write_budget: u64,
    max_read_budget: u64,
    max_scan_budget: u64,
    max_rewrite_budget: u64,
    max_rewrite_owned_surrogate: u64,
}

#[derive(Debug, Clone, Copy)]
struct ScalarElementOps {
    write: u64,
    read: u64,
    skip: u64,
    scan: u64,
    rewrite_input: u64,
}

#[test]
fn typetree_characterization_contract() {
    for fixture in fixtures() {
        let report = characterize(&fixture);
        println!("{report:#?}");
        assert_common_contract(&report, fixture.expected_pptrs);
        assert_guard_thresholds(&report, thresholds(fixture.name));
    }
}

#[test]
fn rewrite_comparison_borrows_large_payloads_and_nested_values() {
    let fixture = borrowed_nested_fixture();
    let mut encode_budget = AssetLoadBudget::default();
    let (wire, _) = encode_object(
        &fixture.schema,
        &fixture.properties,
        Endian::Little,
        &mut encode_budget,
    )
    .expect("borrowed fixture must encode");

    let mut rewrite_budget = AssetLoadBudget::default();
    let (unchanged, unchanged_stats) = rewrite_object(
        &fixture.schema,
        &fixture.properties,
        &wire,
        Endian::Little,
        &mut rewrite_budget,
    )
    .expect("borrowed fixture must rewrite without changes");
    assert_eq!(unchanged, wire);
    assert_eq!(unchanged_stats.preserved_bytes, wire.len() as u64);
    assert_eq!(unchanged_stats.input.wire_bytes, wire.len() as u64);
    assert_eq!(unchanged_stats.input.owned_bytes, 0);
    assert_eq!(unchanged_stats.input.unity_values_materialized, 0);

    let mut changed = fixture.properties.clone();
    let payload = changed
        .get_mut("m_Entries")
        .and_then(|value| match value {
            UnityValue::Array(entries) => entries.get_mut(BORROWED_RECORD_COUNT / 2),
            _ => None,
        })
        .and_then(UnityValue::as_object_mut)
        .and_then(|entry| entry.get_mut("m_Payload"))
        .and_then(|value| match value {
            UnityValue::Bytes(bytes) => bytes.get_mut(17),
            _ => None,
        })
        .expect("fixture must expose the nested payload byte");
    *payload ^= 0xff;

    let mut changed_budget = AssetLoadBudget::default();
    let (changed_wire, changed_stats) = rewrite_object(
        &fixture.schema,
        &changed,
        &wire,
        Endian::Little,
        &mut changed_budget,
    )
    .expect("borrowed fixture must rewrite a nested change");
    assert_ne!(changed_wire, wire);
    assert_eq!(changed_stats.input.wire_bytes, wire.len() as u64);
    assert_eq!(
        changed_stats.input.node_visits,
        unchanged_stats.input.node_visits
    );
    assert_eq!(changed_stats.input.owned_bytes, 0);
    assert_eq!(changed_stats.input.unity_values_materialized, 0);

    let mut read_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&changed_wire, ByteOrder::Little);
    let decoded = fixture
        .schema
        .read_object(
            &mut reader,
            &mut read_budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
        .expect("changed borrowed fixture must decode");
    assert_eq!(decoded.properties, changed);
    assert_eq!(reader.position(), changed_wire.len() as u64);
}

#[test]
#[ignore = "opt-in release characterization; emits timing and process memory observations"]
fn typetree_characterization_sample_read() {
    sample_adapter(Adapter::Read);
}

#[test]
#[ignore = "opt-in release characterization; emits timing and process memory observations"]
fn typetree_characterization_sample_skip() {
    sample_adapter(Adapter::Skip);
}

#[test]
#[ignore = "opt-in release characterization; emits timing and process memory observations"]
fn typetree_characterization_sample_scan() {
    sample_adapter(Adapter::Scan);
}

#[test]
#[ignore = "opt-in release characterization; emits timing and process memory observations"]
fn typetree_characterization_sample_write() {
    sample_adapter(Adapter::Write);
}

#[test]
#[ignore = "opt-in release characterization; emits timing and process memory observations"]
fn typetree_characterization_sample_rewrite() {
    sample_adapter(Adapter::Rewrite);
}

fn characterize(fixture: &Fixture) -> Characterization {
    let mut write_budget = AssetLoadBudget::default();
    let (wire, write_stats) = encode_object(
        &fixture.schema,
        &fixture.properties,
        Endian::Little,
        &mut write_budget,
    )
    .expect("fixture must encode");
    let wire_bytes = u64::try_from(wire.len()).expect("fixture wire extent must fit u64");
    let write = Observation::new(write_stats, write_budget.usage());

    let mut read_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&wire, ByteOrder::Little);
    let read_output = fixture
        .schema
        .read_object(
            &mut reader,
            &mut read_budget,
            TypeTreeParseOptions {
                mode: TypeTreeParseMode::Strict,
            },
        )
        .expect("fixture must read");
    assert_eq!(reader.position(), wire_bytes);
    assert_eq!(read_output.properties, fixture.properties);
    let read = Observation::new(read_output.stats, read_budget.usage());

    let mut skip_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&wire, ByteOrder::Little);
    let skip_stats = fixture
        .schema
        .skip_value(&mut reader, &mut skip_budget, fixture.schema.root())
        .expect("fixture must skip");
    assert_eq!(reader.position(), wire_bytes);
    let skip = Observation::new(skip_stats, skip_budget.usage());

    let mut scan_budget = AssetLoadBudget::default();
    let mut reader = BinaryReader::new(&wire, ByteOrder::Little);
    let scan_output = fixture
        .schema
        .scan_pptrs(&mut reader, &mut scan_budget)
        .expect("fixture must scan");
    assert_eq!(reader.position(), wire_bytes);
    assert_eq!(
        u64::try_from(scan_output.internal.len() + scan_output.external.len())
            .expect("PPtr result count must fit u64"),
        fixture.expected_pptrs
    );
    let scan = Observation::new(scan_output.stats, scan_budget.usage());

    let mut rewrite_budget = AssetLoadBudget::default();
    let (rewritten, rewrite_stats) = rewrite_object(
        &fixture.schema,
        &fixture.properties,
        &wire,
        Endian::Little,
        &mut rewrite_budget,
    )
    .expect("fixture must rewrite");
    assert_eq!(rewritten, wire);
    let rewrite = RewriteObservation::new(rewrite_stats, rewrite_budget.usage());

    Characterization {
        fixture: fixture.name,
        wire_bytes,
        write,
        read,
        skip,
        scan,
        rewrite,
    }
}

fn assert_common_contract(report: &Characterization, expected_pptrs: u64) {
    let wire = report.wire_bytes;
    for (adapter, observation) in [
        ("write", report.write),
        ("read", report.read),
        ("skip", report.skip),
        ("scan", report.scan),
    ] {
        assert_eq!(
            observation.stats.wire_bytes, wire,
            "{} {adapter} changed its wire extent",
            report.fixture
        );
        assert!(
            observation.usage.bytes >= observation.stats.wire_bytes,
            "{} {adapter} under-accounted traversed bytes",
            report.fixture
        );
        assert_eq!(
            observation.usage.entries, observation.stats.node_visits,
            "{} {adapter} node visits escaped the entry budget",
            report.fixture
        );
        assert_eq!(
            observation.usage.members, observation.stats.members,
            "{} {adapter} members escaped the member budget",
            report.fixture
        );
    }

    assert_eq!(report.rewrite.input.wire_bytes, wire);
    assert_eq!(report.rewrite.output.wire_bytes, wire);
    assert_eq!(report.rewrite.preserved_bytes, wire);
    assert_eq!(report.skip.stats.owned_bytes, 0);
    assert_eq!(report.skip.stats.unity_values_materialized, 0);
    assert_eq!(report.skip.peak_owned_surrogate_bytes, 0);
    assert_eq!(report.scan.stats.unity_values_materialized, 0);
    assert_eq!(report.rewrite.input.owned_bytes, 0);
    assert_eq!(report.rewrite.input.unity_values_materialized, 0);
    assert_eq!(report.scan.stats.pptrs_emitted, expected_pptrs);
    assert!(
        report.rewrite.input.unity_values_materialized
            <= report.read.stats.unity_values_materialized
    );
    assert!(report.rewrite.input.owned_bytes <= report.read.stats.owned_bytes);
    assert!(
        report.rewrite.usage.bytes
            >= report
                .rewrite
                .input
                .wire_bytes
                .saturating_add(report.rewrite.output.wire_bytes)
    );
    assert!(report.rewrite.peak_owned_surrogate_bytes <= report.rewrite.usage.bytes);
    assert_eq!(report.write.stats.bulk_bytes, report.read.stats.bulk_bytes);
    assert_eq!(report.read.stats.bulk_runs, report.skip.stats.bulk_runs);
    assert_eq!(report.read.stats.bulk_bytes, report.skip.stats.bulk_bytes);
    assert_eq!(report.skip.stats.bulk_runs, report.scan.stats.bulk_runs);
    assert_eq!(report.skip.stats.bulk_bytes, report.scan.stats.bulk_bytes);
    assert_eq!(report.scan.stats.bulk_runs, report.rewrite.input.bulk_runs);
    assert_eq!(
        report.scan.stats.bulk_bytes,
        report.rewrite.input.bulk_bytes
    );
    assert_eq!(
        report.write.stats.node_visits,
        report.read.stats.node_visits
    );
    assert_eq!(report.read.stats.node_visits, report.skip.stats.node_visits);
    assert_eq!(report.skip.stats.node_visits, report.scan.stats.node_visits);
    assert_eq!(
        report.scan.stats.node_visits,
        report.rewrite.input.node_visits
    );
    assert_eq!(report.write.stats.members, report.read.stats.members);
    assert_eq!(report.read.stats.members, report.skip.stats.members);
    assert_eq!(
        report.scan.stats.members,
        report.skip.stats.members.saturating_add(expected_pptrs)
    );
    assert_eq!(report.skip.stats.members, report.rewrite.input.members);
}

fn assert_guard_thresholds(report: &Characterization, guard: GuardThresholds) {
    assert_eq!(report.wire_bytes, guard.wire_bytes, "wire fixture drifted");
    assert_eq!(
        report.read.stats.node_visits, guard.node_visits,
        "semantic node work drifted"
    );
    assert_eq!(
        report.read.stats.members, guard.members,
        "semantic member work drifted"
    );
    for observation in [report.write, report.read, report.skip, report.scan] {
        assert_eq!(observation.usage.max_observed_depth, guard.max_depth);
    }
    assert_eq!(report.rewrite.usage.max_observed_depth, guard.max_depth);

    assert!(report.write.stats.bulk_bytes >= guard.min_bulk_bytes);
    assert!(report.read.stats.bulk_bytes >= guard.min_bulk_bytes);
    assert!(report.rewrite.input.bulk_bytes >= guard.min_bulk_bytes);
    assert!(report.write.stats.bulk_runs <= guard.max_write_bulk_runs);
    assert!(report.read.stats.bulk_runs <= guard.max_read_bulk_runs);
    assert_eq!(
        report.write.stats.scalar_element_ops, guard.scalar_element_ops.write,
        "write scalar work drifted"
    );
    assert_eq!(
        report.read.stats.scalar_element_ops, guard.scalar_element_ops.read,
        "read scalar work drifted"
    );
    assert_eq!(
        report.skip.stats.scalar_element_ops, guard.scalar_element_ops.skip,
        "skip scalar work drifted"
    );
    assert_eq!(
        report.scan.stats.scalar_element_ops, guard.scalar_element_ops.scan,
        "scan scalar work drifted"
    );
    assert_eq!(
        report.rewrite.input.scalar_element_ops, guard.scalar_element_ops.rewrite_input,
        "rewrite comparison scalar work drifted"
    );
    assert!(
        report.read.stats.unity_values_materialized <= guard.max_read_materialized,
        "read materialized more UnityValue nodes than the baseline ceiling"
    );
    assert!(
        report.rewrite.input.unity_values_materialized <= guard.max_rewrite_materialized,
        "rewrite comparison materialized more UnityValue nodes than the baseline ceiling"
    );

    assert!(report.write.stats.owned_bytes <= guard.max_write_owned);
    assert!(report.read.stats.owned_bytes <= guard.max_read_owned);
    assert!(report.scan.stats.owned_bytes <= guard.max_scan_owned);
    assert!(report.rewrite.input.owned_bytes <= guard.max_rewrite_input_owned);
    assert!(report.rewrite.output.owned_bytes <= guard.max_rewrite_output_owned);
    assert!(report.write.usage.bytes <= guard.max_write_budget);
    assert!(report.read.usage.bytes <= guard.max_read_budget);
    assert!(report.scan.usage.bytes <= guard.max_scan_budget);
    assert!(report.rewrite.usage.bytes <= guard.max_rewrite_budget);
    assert!(
        report.rewrite.peak_owned_surrogate_bytes <= guard.max_rewrite_owned_surrogate,
        "rewrite owned/scratch surrogate exceeded its regression ceiling"
    );

    if report.fixture == "generated-large" {
        assert_eq!(report.skip.stats.unity_values_materialized, 0);
        assert_eq!(report.skip.stats.owned_bytes, 0);
        assert_eq!(report.scan.stats.unity_values_materialized, 0);
        assert_eq!(report.scan.stats.owned_bytes, 0);
        assert_eq!(report.rewrite.input.unity_values_materialized, 0);
        assert_eq!(report.rewrite.input.owned_bytes, 0);
        assert_eq!(
            report.read.stats.scalar_element_ops,
            LARGE_NUMBER_COUNT as u64
        );
        assert_eq!(report.skip.stats.scalar_element_ops, 0);
        assert_eq!(report.scan.stats.scalar_element_ops, 0);
        assert_eq!(
            report.rewrite.input.scalar_element_ops,
            LARGE_NUMBER_COUNT as u64
        );
    }
}

fn thresholds(fixture: &str) -> GuardThresholds {
    match fixture {
        "representative" => GuardThresholds {
            wire_bytes: 324,
            node_visits: 78,
            members: 77,
            max_depth: 3,
            min_bulk_bytes: 256,
            max_write_bulk_runs: 1,
            max_read_bulk_runs: 1,
            scalar_element_ops: ScalarElementOps {
                write: 5,
                read: 69,
                skip: 5,
                scan: 5,
                rewrite_input: 69,
            },
            max_read_materialized: 77,
            max_rewrite_materialized: 0,
            max_write_owned: 640,
            max_read_owned: 6_400,
            max_scan_owned: 32,
            max_rewrite_input_owned: 0,
            max_rewrite_output_owned: 640,
            max_write_budget: 1_024,
            max_read_budget: 6_656,
            max_scan_budget: 384,
            max_rewrite_budget: 2_560,
            max_rewrite_owned_surrogate: 1_856,
        },
        "generated-large" => GuardThresholds {
            wire_bytes: 589_836,
            node_visits: 393_220,
            members: 393_219,
            max_depth: 2,
            min_bulk_bytes: 589_824,
            max_write_bulk_runs: 66,
            max_read_bulk_runs: 3,
            scalar_element_ops: ScalarElementOps {
                write: 0,
                read: 65_536,
                skip: 0,
                scan: 0,
                rewrite_input: 65_536,
            },
            max_read_materialized: 65_539,
            max_rewrite_materialized: 0,
            max_write_owned: 1_100_000,
            max_read_owned: 5_300_000,
            max_scan_owned: 0,
            max_rewrite_input_owned: 0,
            max_rewrite_output_owned: 1_100_000,
            max_write_budget: 1_720_000,
            max_read_budget: 5_920_000,
            max_scan_budget: 589_836,
            max_rewrite_budget: 2_350_000,
            max_rewrite_owned_surrogate: 1_102_000,
        },
        "adversarial-wide-deep" => GuardThresholds {
            wire_bytes: 1_284,
            node_visits: 370,
            members: 369,
            max_depth: 49,
            min_bulk_bytes: 0,
            max_write_bulk_runs: 0,
            max_read_bulk_runs: 64,
            scalar_element_ops: ScalarElementOps {
                write: 257,
                read: 257,
                skip: 257,
                scan: 257,
                rewrite_input: 257,
            },
            max_read_materialized: 369,
            max_rewrite_materialized: 0,
            max_write_owned: 2_304,
            max_read_owned: 48_000,
            max_scan_owned: 0,
            max_rewrite_input_owned: 0,
            max_rewrite_output_owned: 2_304,
            max_write_budget: 3_584,
            max_read_budget: 49_000,
            max_scan_budget: 1_284,
            max_rewrite_budget: 32_000,
            max_rewrite_owned_surrogate: 28_000,
        },
        _ => panic!("missing TypeTree characterization thresholds for {fixture}"),
    }
}

fn fixtures() -> [Fixture; 3] {
    [
        representative_fixture(),
        generated_large_fixture(),
        adversarial_fixture(),
    ]
}

fn representative_fixture() -> Fixture {
    let mut root = record(vec![
        node("UInt64", "m_Id"),
        pptr("m_Target"),
        aligned(node("TypelessData", "m_Data")),
        sequence("m_Numbers", node("int", "data")),
        map("m_Map", node("string", "first"), node("UInt16", "second")),
    ]);
    root.meta_flags = 0x4000;

    let properties = IndexMap::from([
        ("m_Id".to_owned(), UnityValue::Unsigned(u64::MAX)),
        ("m_Target".to_owned(), pptr_value(2, 77)),
        ("m_Data".to_owned(), UnityValue::Bytes(vec![9, 8, 7, 6, 5])),
        (
            "m_Numbers".to_owned(),
            UnityValue::Array(
                (-32_i64..32_i64)
                    .map(UnityValue::Integer)
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "m_Map".to_owned(),
            UnityValue::Array(vec![
                UnityValue::Array(vec![
                    UnityValue::String("answer".to_owned()),
                    UnityValue::Integer(42),
                ]),
                UnityValue::Array(vec![
                    UnityValue::String("limit".to_owned()),
                    UnityValue::Integer(65_535),
                ]),
            ]),
        ),
    ]);

    Fixture {
        name: "representative",
        schema: compile(root),
        properties,
        expected_pptrs: 1,
    }
}

fn generated_large_fixture() -> Fixture {
    let mut root = record(vec![
        aligned(sequence("m_Blob", node("UInt8", "data"))),
        aligned(sequence("m_SignedBlob", node("SInt8", "data"))),
        aligned(sequence("m_Numbers", node("SInt32", "data"))),
    ]);
    root.meta_flags = 0x4000;

    let blob = (0..LARGE_BYTE_COUNT)
        .map(|index| ((index * 31 + 17) & 0xff) as u8)
        .collect::<Vec<_>>();
    let signed_blob = (0..LARGE_SIGNED_BYTE_COUNT)
        .map(|index| ((index * 17 + 29) & 0xff) as u8)
        .collect::<Vec<_>>();
    let numbers = (0..LARGE_NUMBER_COUNT)
        .map(|index| UnityValue::Integer(i64::from(index as i32) - 32_768))
        .collect::<Vec<_>>();

    let properties = IndexMap::from([
        ("m_Blob".to_owned(), UnityValue::Bytes(blob)),
        ("m_SignedBlob".to_owned(), UnityValue::Bytes(signed_blob)),
        ("m_Numbers".to_owned(), UnityValue::Array(numbers)),
    ]);

    Fixture {
        name: "generated-large",
        schema: compile(root),
        properties,
        expected_pptrs: 0,
    }
}

fn adversarial_fixture() -> Fixture {
    let mut children =
        Vec::with_capacity(ADVERSARIAL_SCALAR_FIELDS + ADVERSARIAL_EMPTY_SEQUENCES + 1);
    let mut properties = IndexMap::with_capacity(children.capacity());

    for index in 0..ADVERSARIAL_SCALAR_FIELDS {
        let name = format!("m_Scalar_{index:03}");
        let field = if index % 2 == 0 {
            node("UInt32", &name)
        } else {
            aligned(node("UInt8", &name))
        };
        children.push(field);
        properties.insert(name, UnityValue::Integer(index as i64));
    }

    for index in 0..ADVERSARIAL_EMPTY_SEQUENCES {
        let name = format!("m_Empty_{index:03}");
        children.push(sequence(&name, node("UInt32", "data")));
        properties.insert(name, UnityValue::Array(Vec::new()));
    }

    let (deep_node, deep_value) = deeply_nested_record();
    properties.insert(deep_node.name.clone(), deep_value);
    children.push(deep_node);

    let mut root = record(children);
    root.meta_flags = 0x4000;
    Fixture {
        name: "adversarial-wide-deep",
        schema: compile(root),
        properties,
        expected_pptrs: 0,
    }
}

fn borrowed_nested_fixture() -> Fixture {
    let mut entry = node("NestedEntry", "data");
    entry.children = vec![
        node("string", "m_Name"),
        aligned(node("TypelessData", "m_Payload")),
        map(
            "m_Metadata",
            node("string", "first"),
            node("UInt32", "second"),
        ),
    ];
    let root = record(vec![
        aligned(node("string", "m_Text")),
        aligned(node("TypelessData", "m_Data")),
        sequence("m_Entries", entry),
    ]);

    let text = (0..BORROWED_TEXT_BYTES)
        .map(|index| char::from(b'a' + (index % 26) as u8))
        .collect::<String>();
    let data = (0..BORROWED_DATA_BYTES)
        .map(|index| ((index * 29 + 7) & 0xff) as u8)
        .collect::<Vec<_>>();
    let entries = (0..BORROWED_RECORD_COUNT)
        .map(|index| {
            UnityValue::Object(IndexMap::from([
                (
                    "m_Name".to_owned(),
                    UnityValue::String(format!("entry-{index:03}")),
                ),
                (
                    "m_Payload".to_owned(),
                    UnityValue::Bytes(
                        (0..257)
                            .map(|offset| ((index * 13 + offset * 17) & 0xff) as u8)
                            .collect(),
                    ),
                ),
                (
                    "m_Metadata".to_owned(),
                    UnityValue::Array(vec![UnityValue::Array(vec![
                        UnityValue::String("index".to_owned()),
                        UnityValue::Integer(index as i64),
                    ])]),
                ),
            ]))
        })
        .collect();
    let properties = IndexMap::from([
        ("m_Text".to_owned(), UnityValue::String(text)),
        ("m_Data".to_owned(), UnityValue::Bytes(data)),
        ("m_Entries".to_owned(), UnityValue::Array(entries)),
    ]);

    Fixture {
        name: "borrowed-nested",
        schema: compile(root),
        properties,
        expected_pptrs: 0,
    }
}

fn deeply_nested_record() -> (TypeTreeNode, UnityValue) {
    let mut child = node("UInt32", "m_Value");
    let mut value = UnityValue::Integer(0x1234_5678);
    for depth in (0..ADVERSARIAL_RECORD_DEPTH).rev() {
        let child_name = child.name.clone();
        let mut parent = node("Nested", &format!("m_Level_{depth:02}"));
        parent.children.push(child);
        value = UnityValue::Object(IndexMap::from([(child_name, value)]));
        child = parent;
    }
    (child, value)
}

fn pptr_value(file_id: i64, path_id: i64) -> UnityValue {
    UnityValue::Object(IndexMap::from([
        ("m_FileID".to_owned(), UnityValue::Integer(file_id)),
        ("m_PathID".to_owned(), UnityValue::Integer(path_id)),
    ]))
}

fn compile(root: TypeTreeNode) -> TypeTreeSchema {
    let mut tree = TypeTree::new();
    tree.add_node(root);
    TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default())
        .expect("characterization schema must compile")
}

#[derive(Debug, Clone, Copy)]
enum Adapter {
    Read,
    Skip,
    Scan,
    Write,
    Rewrite,
}

impl Adapter {
    const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Skip => "skip",
            Self::Scan => "scan",
            Self::Write => "write",
            Self::Rewrite => "rewrite",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SampleIteration {
    traversed_wire_bytes: u64,
    stats: TypeTreeTraversalStats,
    usage: AssetLoadUsage,
    peak_owned_surrogate_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct SampleMeasurement {
    iterations: u64,
    last: SampleIteration,
    elapsed: Duration,
    process_before: ProcessSample,
    process_after: ProcessSample,
}

fn sample_adapter(adapter: Adapter) {
    let iterations = sampling_iterations();
    for fixture in fixtures() {
        let wire = if matches!(adapter, Adapter::Write) {
            Vec::new()
        } else {
            encoded_fixture(&fixture)
        };
        let process_before = ProcessSample::capture();
        let started = Instant::now();
        let mut last = None;
        for _ in 0..iterations {
            last = Some(run_sample_iteration(adapter, &fixture, &wire));
        }
        let elapsed = started.elapsed();
        let process_after = ProcessSample::capture();
        let last = last.expect("sampling requires at least one iteration");
        emit_sample(
            adapter,
            &fixture,
            SampleMeasurement {
                iterations,
                last,
                elapsed,
                process_before,
                process_after,
            },
        );
    }
}

fn sampling_iterations() -> u64 {
    let value = std::env::var("UNITY_ASSET_TYPETREE_SAMPLE_ITERATIONS")
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .expect("UNITY_ASSET_TYPETREE_SAMPLE_ITERATIONS must be an integer")
        })
        .unwrap_or(20);
    assert!(
        (1..=10_000).contains(&value),
        "UNITY_ASSET_TYPETREE_SAMPLE_ITERATIONS must be between 1 and 10000"
    );
    value
}

fn encoded_fixture(fixture: &Fixture) -> Vec<u8> {
    let mut budget = AssetLoadBudget::default();
    encode_object(
        &fixture.schema,
        &fixture.properties,
        Endian::Little,
        &mut budget,
    )
    .expect("fixture must encode")
    .0
}

fn run_sample_iteration(adapter: Adapter, fixture: &Fixture, wire: &[u8]) -> SampleIteration {
    let mut budget = AssetLoadBudget::default();
    let (traversed_wire_bytes, stats) = match adapter {
        Adapter::Read => {
            let mut reader = BinaryReader::new(wire, ByteOrder::Little);
            let output = fixture
                .schema
                .read_object(
                    &mut reader,
                    &mut budget,
                    TypeTreeParseOptions {
                        mode: TypeTreeParseMode::Strict,
                    },
                )
                .expect("sample read must succeed");
            black_box(output.properties);
            (output.stats.wire_bytes, output.stats)
        }
        Adapter::Skip => {
            let mut reader = BinaryReader::new(wire, ByteOrder::Little);
            let stats = fixture
                .schema
                .skip_value(&mut reader, &mut budget, fixture.schema.root())
                .expect("sample skip must succeed");
            black_box(reader.position());
            (stats.wire_bytes, stats)
        }
        Adapter::Scan => {
            let mut reader = BinaryReader::new(wire, ByteOrder::Little);
            let output = fixture
                .schema
                .scan_pptrs(&mut reader, &mut budget)
                .expect("sample scan must succeed");
            black_box((output.internal, output.external));
            (output.stats.wire_bytes, output.stats)
        }
        Adapter::Write => {
            let (output, stats) = encode_object(
                &fixture.schema,
                &fixture.properties,
                Endian::Little,
                &mut budget,
            )
            .expect("sample write must succeed");
            black_box(output);
            (stats.wire_bytes, stats)
        }
        Adapter::Rewrite => {
            let (output, rewrite) = rewrite_object(
                &fixture.schema,
                &fixture.properties,
                wire,
                Endian::Little,
                &mut budget,
            )
            .expect("sample rewrite must succeed");
            black_box(output);
            let stats = saturating_add_stats(rewrite.input, rewrite.output);
            (stats.wire_bytes, stats)
        }
    };
    let usage = budget.usage();
    SampleIteration {
        traversed_wire_bytes,
        stats,
        usage,
        peak_owned_surrogate_bytes: usage.bytes.saturating_sub(traversed_wire_bytes),
    }
}

fn saturating_add_stats(
    left: TypeTreeTraversalStats,
    right: TypeTreeTraversalStats,
) -> TypeTreeTraversalStats {
    macro_rules! saturating {
        ($field:ident) => {
            left.$field.saturating_add(right.$field)
        };
    }

    TypeTreeTraversalStats {
        wire_bytes: saturating!(wire_bytes),
        owned_bytes: saturating!(owned_bytes),
        node_visits: saturating!(node_visits),
        members: saturating!(members),
        bulk_runs: saturating!(bulk_runs),
        bulk_bytes: saturating!(bulk_bytes),
        scalar_element_ops: saturating!(scalar_element_ops),
        unity_values_materialized: saturating!(unity_values_materialized),
        pptrs_emitted: saturating!(pptrs_emitted),
    }
}

#[test]
fn sample_stats_merge_preserves_fields_and_saturates() {
    let left = TypeTreeTraversalStats {
        wire_bytes: 1,
        owned_bytes: 2,
        node_visits: 3,
        members: 4,
        bulk_runs: 5,
        bulk_bytes: 6,
        scalar_element_ops: 7,
        unity_values_materialized: 8,
        pptrs_emitted: 9,
    };
    let right = TypeTreeTraversalStats {
        wire_bytes: 10,
        owned_bytes: 20,
        node_visits: 30,
        members: 40,
        bulk_runs: 50,
        bulk_bytes: 60,
        scalar_element_ops: 70,
        unity_values_materialized: 80,
        pptrs_emitted: 90,
    };
    assert_eq!(
        saturating_add_stats(left, right),
        TypeTreeTraversalStats {
            wire_bytes: 11,
            owned_bytes: 22,
            node_visits: 33,
            members: 44,
            bulk_runs: 55,
            bulk_bytes: 66,
            scalar_element_ops: 77,
            unity_values_materialized: 88,
            pptrs_emitted: 99,
        }
    );

    let maximum = TypeTreeTraversalStats {
        wire_bytes: u64::MAX,
        owned_bytes: u64::MAX,
        node_visits: u64::MAX,
        members: u64::MAX,
        bulk_runs: u64::MAX,
        bulk_bytes: u64::MAX,
        scalar_element_ops: u64::MAX,
        unity_values_materialized: u64::MAX,
        pptrs_emitted: u64::MAX,
    };
    assert_eq!(saturating_add_stats(maximum, right), maximum);
}

fn emit_sample(adapter: Adapter, fixture: &Fixture, measurement: SampleMeasurement) {
    let SampleMeasurement {
        iterations,
        last,
        elapsed,
        process_before,
        process_after,
    } = measurement;
    let total_wire = last.traversed_wire_bytes.saturating_mul(iterations);
    let elapsed_seconds = elapsed.as_secs_f64();
    let throughput_mib_s = total_wire as f64 / (1024.0 * 1024.0) / elapsed_seconds;
    let cpu = process_after
        .cpu_time
        .zip(process_before.cpu_time)
        .and_then(|(after, before)| after.checked_sub(before));
    let cpu_throughput_mib_s = cpu
        .filter(|duration| !duration.is_zero())
        .map(|duration| total_wire as f64 / (1024.0 * 1024.0) / duration.as_secs_f64());
    let peak_rss_growth = process_after
        .peak_rss_bytes
        .zip(process_before.peak_rss_bytes)
        .map(|(after, before)| after.saturating_sub(before));

    let output = serde_json::json!({
        "schema": "unity-asset.typetree-characterization.v1",
        "fixture": fixture.name,
        "adapter": adapter.name(),
        "iterations": iterations,
        "wire_bytes_per_iteration": last.traversed_wire_bytes,
        "total_wire_bytes": total_wire,
        "elapsed_ns": duration_ns(elapsed),
        "cpu_ns": cpu.map(duration_ns),
        "throughput_mib_s": throughput_mib_s,
        "cpu_throughput_mib_s": cpu_throughput_mib_s,
        "peak_rss_before_bytes": process_before.peak_rss_bytes,
        "peak_rss_after_bytes": process_after.peak_rss_bytes,
        "peak_rss_growth_bytes": peak_rss_growth,
        "last_iteration": {
            "budget_bytes": last.usage.bytes,
            "budget_entries": last.usage.entries,
            "budget_members": last.usage.members,
            "max_observed_depth": last.usage.max_observed_depth,
            "owned_bytes": last.stats.owned_bytes,
            "peak_owned_surrogate_bytes": last.peak_owned_surrogate_bytes,
            "node_visits": last.stats.node_visits,
            "members": last.stats.members,
            "bulk_runs": last.stats.bulk_runs,
            "bulk_bytes": last.stats.bulk_bytes,
            "scalar_element_ops": last.stats.scalar_element_ops,
            "unity_values_materialized": last.stats.unity_values_materialized,
            "pptrs_emitted": last.stats.pptrs_emitted,
        },
    });
    println!("{output}");
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcessSample {
    cpu_time: Option<Duration>,
    peak_rss_bytes: Option<u64>,
}

impl ProcessSample {
    fn capture() -> Self {
        platform_process_sample().unwrap_or_default()
    }
}

#[cfg(target_os = "windows")]
fn platform_process_sample() -> Option<ProcessSample> {
    use std::ffi::c_void;
    use std::mem::size_of;

    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetCurrentProcess"]
        fn get_current_process() -> Handle;
        #[link_name = "GetProcessTimes"]
        fn get_process_times(
            process: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        #[link_name = "GetProcessMemoryInfo"]
        fn get_process_memory_info(
            process: Handle,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let mut memory = ProcessMemoryCounters {
        cb: u32::try_from(size_of::<ProcessMemoryCounters>()).ok()?,
        ..ProcessMemoryCounters::default()
    };
    let memory_size = memory.cb;

    // SAFETY: GetCurrentProcess returns a non-owning pseudo-handle valid in this process. Every
    // output pointer refers to a correctly sized, initialized C-layout structure for the duration
    // of the calls, and no handle needs to be closed.
    let success = unsafe {
        let process = get_current_process();
        get_process_times(process, &mut creation, &mut exit, &mut kernel, &mut user) != 0
            && get_process_memory_info(process, &mut memory, memory_size) != 0
    };
    if !success {
        return None;
    }

    let kernel_ticks = (u64::from(kernel.high) << 32) | u64::from(kernel.low);
    let user_ticks = (u64::from(user.high) << 32) | u64::from(user.low);
    Some(ProcessSample {
        cpu_time: Some(Duration::from_nanos(
            kernel_ticks.saturating_add(user_ticks).saturating_mul(100),
        )),
        peak_rss_bytes: u64::try_from(memory.peak_working_set_size).ok(),
    })
}

#[cfg(target_os = "linux")]
fn platform_process_sample() -> Option<ProcessSample> {
    use std::process::Command;
    use std::sync::OnceLock;

    static TICKS_PER_SECOND: OnceLock<Option<u64>> = OnceLock::new();

    let ticks_per_second = TICKS_PER_SECOND
        .get_or_init(|| {
            Command::new("getconf")
                .arg("CLK_TCK")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|value| value.trim().parse::<u64>().ok())
        })
        .as_ref()
        .copied()?;

    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let ticks = fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .saturating_add(fields.get(12)?.parse::<u64>().ok()?);
    let cpu_ns = u128::from(ticks)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(ticks_per_second))?;

    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let peak_rss_kib = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    Some(ProcessSample {
        cpu_time: Some(Duration::from_nanos(
            u64::try_from(cpu_ns).unwrap_or(u64::MAX),
        )),
        peak_rss_bytes: peak_rss_kib.checked_mul(1024),
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_process_sample() -> Option<ProcessSample> {
    None
}
