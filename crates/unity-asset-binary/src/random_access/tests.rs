use std::cell::Cell;
use std::ops::Range;
use std::sync::Arc;

use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, AssetLoadUsage, BudgetError};

use super::{ByteCursor, ByteSegment, ByteSource, SegmentedBytes};
use crate::error::{BinaryError, Result};
use crate::reader::ByteOrder;

#[derive(Debug, PartialEq, Eq)]
struct ParseSnapshot {
    value: (u16, u32, String, i64, Vec<u8>),
    positions: Vec<u64>,
    terminal_error: String,
    usage: AssetLoadUsage,
}

fn permissive_budget() -> AssetLoadBudget {
    budget_with_max_bytes(1_024)
}

fn budget_with_max_bytes(max_bytes: u64) -> AssetLoadBudget {
    AssetLoadBudget::new(AssetLoadLimits {
        max_entries: 100,
        max_bytes,
        max_depth: 16,
        max_members: 100,
        max_compressed_bytes: 1_024,
        max_decompressed_bytes: 1_024,
        max_expansion_ratio: 16,
    })
    .unwrap()
}

struct CountingSource {
    bytes: Arc<[u8]>,
    read_calls: Cell<usize>,
    bytes_read: Cell<u64>,
}

impl CountingSource {
    fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
            read_calls: Cell::new(0),
            bytes_read: Cell::new(0),
        }
    }
}

impl ByteSource for CountingSource {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap()
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset).unwrap();
        let end = start.checked_add(output.len()).unwrap();
        output.copy_from_slice(&self.bytes[start..end]);
        self.read_calls.set(self.read_calls.get() + 1);
        self.bytes_read
            .set(self.bytes_read.get() + u64::try_from(output.len()).unwrap());
        Ok(())
    }

    fn contiguous(&self, _range: Range<u64>) -> Option<&[u8]> {
        None
    }
}

struct ContiguousOnlySource {
    bytes: Arc<[u8]>,
    random_reads: Cell<usize>,
}

impl ByteSource for ContiguousOnlySource {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap()
    }

    fn read_exact_at(&self, _offset: u64, _output: &mut [u8]) -> Result<()> {
        self.random_reads.set(self.random_reads.get() + 1);
        Err(BinaryError::invalid_data(
            "contiguous cursor unexpectedly used random-access reads",
        ))
    }

    fn contiguous(&self, range: Range<u64>) -> Option<&[u8]> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        self.bytes.get(start..end)
    }
}

fn fixture() -> Arc<[u8]> {
    Arc::from(
        [
            0xaa, 0xbb, 0xcc, // prefix outside the cursor range
            0x12, 0x34, // u16
            0x01, 0x02, 0x03, 0x04, // u32
            0xee, 0xee, 0xee, // absolute alignment to 12
            b'U', b'n', b'i', b't', b'y', 0, // C string
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe, // i64 = -2
            0x91, 0x92, 0x93, // bytes
        ]
        .as_slice(),
    )
}

fn source_with_split(backing: &Arc<[u8]>, split: usize) -> SegmentedBytes {
    let len = backing.len();
    SegmentedBytes::new(vec![
        ByteSegment::from_arc_range(0, Arc::clone(backing), 0..split).unwrap(),
        ByteSegment::from_arc_range(
            u64::try_from(split).unwrap(),
            Arc::clone(backing),
            split..len,
        )
        .unwrap(),
    ])
    .unwrap()
}

fn one_byte_source(backing: &Arc<[u8]>) -> SegmentedBytes {
    let segments = (0..backing.len())
        .map(|index| {
            ByteSegment::from_arc_range(
                u64::try_from(index).unwrap(),
                Arc::clone(backing),
                index..index + 1,
            )
            .unwrap()
        })
        .collect();
    SegmentedBytes::new(segments).unwrap()
}

fn parse(source: &SegmentedBytes) -> ParseSnapshot {
    let mut budget = permissive_budget();
    let (value, positions, terminal_error) = {
        let mut cursor =
            ByteCursor::with_range(source, 3..source.len(), ByteOrder::Big, &mut budget).unwrap();
        let first = cursor.read_u16().unwrap();
        let mut positions = vec![cursor.position()];
        let second = cursor.read_u32().unwrap();
        positions.push(cursor.position());
        cursor.align_to(4).unwrap();
        positions.push(cursor.position());
        let text = cursor.read_cstring(32).unwrap();
        positions.push(cursor.position());
        let signed = cursor.read_i64().unwrap();
        positions.push(cursor.position());
        let trailing = cursor.read_bytes(3).unwrap();
        positions.push(cursor.position());
        let terminal_error = cursor.read_u8().unwrap_err().to_string();
        (
            (first, second, text, signed, trailing),
            positions,
            terminal_error,
        )
    };

    ParseSnapshot {
        value,
        positions,
        terminal_error,
        usage: budget.usage(),
    }
}

#[test]
fn every_segmentation_produces_identical_values_offsets_errors_and_usage() {
    let backing = fixture();
    let contiguous = SegmentedBytes::from_contiguous(Arc::clone(&backing)).unwrap();
    let expected = parse(&contiguous);
    assert_eq!(expected.value.0, 0x1234);
    assert_eq!(expected.value.1, 0x0102_0304);
    assert_eq!(expected.value.2, "Unity");
    assert_eq!(expected.value.3, -2);
    assert_eq!(expected.value.4, [0x91, 0x92, 0x93]);
    assert_eq!(expected.positions, [5, 9, 12, 18, 26, 29]);
    assert_eq!(expected.usage.bytes, 23);

    for split in 0..=backing.len() {
        assert_eq!(parse(&source_with_split(&backing, split)), expected);
    }
    assert_eq!(parse(&one_byte_source(&backing)), expected);
}

#[test]
fn cstring_crosses_segments_and_truncation_is_stable() {
    let backing: Arc<[u8]> = Arc::from(&b"prefix-without-nul"[..]);
    let source = one_byte_source(&backing);
    let mut budget = permissive_budget();
    {
        let mut cursor = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
        let error = cursor.read_cstring(64).unwrap_err().to_string();
        assert!(error.contains("unterminated C string"));
        assert_eq!(cursor.position(), 0);
    }
    assert_eq!(
        budget.usage(),
        AssetLoadUsage {
            bytes: u64::try_from(backing.len()).unwrap(),
            ..AssetLoadUsage::default()
        }
    );
}

#[test]
fn cstring_scan_is_bounded_by_remaining_budget_and_preserves_usage_on_failure() {
    let source = CountingSource::new(&b"abc\0unreachable"[..]);
    let mut budget = budget_with_max_bytes(3);
    {
        let mut cursor = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
        let error = cursor.read_cstring(64).unwrap_err();
        assert!(matches!(
            error,
            BinaryError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit: 3,
                requested: 4,
            })
        ));
        assert_eq!(cursor.position(), 0);
    }
    assert_eq!(budget.usage().bytes, 3);
    assert_eq!(source.bytes_read.get(), 3);

    let source = CountingSource::new(&b"abc\0unreachable"[..]);
    let mut budget = budget_with_max_bytes(4);
    {
        let mut cursor = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
        assert_eq!(cursor.read_cstring(64).unwrap(), "abc");
        assert_eq!(cursor.position(), 4);
    }
    assert_eq!(budget.usage().bytes, 4);
    assert_eq!(source.bytes_read.get(), 4);
}

#[test]
fn seek_and_alignment_move_without_reading_or_charging_bytes() {
    let source = CountingSource::new(Arc::<[u8]>::from([0_u8; 16]));
    let mut budget = permissive_budget();
    {
        let mut cursor = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
        cursor.set_position(3).unwrap();
        cursor.align_to(8).unwrap();
        assert_eq!(cursor.position(), 8);
        assert_eq!(cursor.read_u8().unwrap(), 0);
        cursor.set_position(1).unwrap();
        assert_eq!(cursor.read_u8().unwrap(), 0);
    }

    assert_eq!(source.read_calls.get(), 2);
    assert_eq!(source.bytes_read.get(), 2);
    assert_eq!(budget.usage().bytes, 2);
}

#[test]
fn cursor_reads_directly_from_a_contiguous_view() {
    let source = ContiguousOnlySource {
        bytes: Arc::from(&b"\x01\x02text\0"[..]),
        random_reads: Cell::new(0),
    };
    let mut budget = permissive_budget();
    {
        let mut cursor = ByteCursor::new(&source, ByteOrder::Big, &mut budget).unwrap();
        assert_eq!(cursor.read_u16().unwrap(), 0x0102);
        assert_eq!(cursor.read_cstring(16).unwrap(), "text");
    }
    assert_eq!(source.random_reads.get(), 0);
    assert_eq!(budget.usage().bytes, 7);
}

#[test]
fn cursor_ranges_are_absolute_and_strictly_bounded() {
    let backing = fixture();
    let source = one_byte_source(&backing);
    let mut budget = permissive_budget();
    let mut cursor = ByteCursor::with_range(&source, 3..18, ByteOrder::Big, &mut budget).unwrap();
    assert_eq!(cursor.position(), 3);
    cursor.set_position(5).unwrap();
    assert_eq!(cursor.read_u32().unwrap(), 0x0102_0304);
    assert_eq!(cursor.position(), 9);
    assert!(cursor.set_position(2).is_err());
    assert!(cursor.set_position(19).is_err());
    cursor.set_position(17).unwrap();
    let error = cursor.read_u16().unwrap_err().to_string();
    assert!(error.contains("leaves bounded range"));
    assert_eq!(cursor.position(), 17);
}

struct VirtualSource {
    len: u64,
    reads: Cell<usize>,
}

impl ByteSource for VirtualSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_exact_at(&self, _offset: u64, output: &mut [u8]) -> Result<()> {
        self.reads.set(self.reads.get() + 1);
        output.fill(0);
        Ok(())
    }

    fn contiguous(&self, _range: Range<u64>) -> Option<&[u8]> {
        None
    }
}

#[test]
fn checked_ranges_reject_u64_overflow_without_source_reads() {
    let byte: Arc<[u8]> = Arc::from(&[1_u8][..]);
    assert!(ByteSegment::new(u64::MAX, byte).is_err());

    let source = VirtualSource {
        len: u64::MAX,
        reads: Cell::new(0),
    };
    let mut budget = permissive_budget();
    {
        let mut cursor = ByteCursor::with_range(
            &source,
            u64::MAX - 2..u64::MAX,
            ByteOrder::Little,
            &mut budget,
        )
        .unwrap();
        cursor.set_position(u64::MAX - 1).unwrap();
        assert!(cursor.align_to(8).is_err());
        assert_eq!(source.reads.get(), 0);
    }
    assert_eq!(budget.usage(), AssetLoadUsage::default());
}

#[test]
fn cursor_construction_does_not_read_or_materialize_the_source() {
    let source = VirtualSource {
        len: 16 * 1024 * 1024,
        reads: Cell::new(0),
    };
    let mut budget = permissive_budget();
    {
        let cursor = ByteCursor::new(&source, ByteOrder::Little, &mut budget).unwrap();
        assert_eq!(cursor.remaining(), source.len);
        assert_eq!(source.reads.get(), 0);
    }
    assert_eq!(budget.usage(), AssetLoadUsage::default());
}
