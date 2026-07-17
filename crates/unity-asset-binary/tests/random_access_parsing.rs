use std::sync::Arc;

use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_binary::{ByteSegment, SegmentedBytes};
use unity_asset_core::AssetLoadBudget;

#[test]
fn segmented_values_validate_offsets_and_retain_original_backings() {
    let left: Arc<[u8]> = Arc::from(&b"abcd"[..]);
    let right: Arc<[u8]> = Arc::from(&b"efgh"[..]);
    let left_pointer = left.as_ptr();
    let right_pointer = right.as_ptr();

    let image = SegmentedBytes::new(vec![
        ByteSegment::new(0, Arc::clone(&left)).unwrap(),
        ByteSegment::new(4, Arc::clone(&right)).unwrap(),
    ])
    .unwrap();

    assert_eq!(image.len(), 8);
    assert_eq!(image.segments().len(), 2);
    assert_eq!(image.segments()[0].as_slice().as_ptr(), left_pointer);
    assert_eq!(image.segments()[1].as_slice().as_ptr(), right_pointer);
    assert!(image.contiguous().is_none());
}

#[test]
fn subrange_is_zero_copy_and_rebased_across_segment_boundaries() {
    let backing: Arc<[u8]> = Arc::from(&b"0123456789"[..]);
    let image = SegmentedBytes::new(vec![
        ByteSegment::from_arc_range(0, Arc::clone(&backing), 0..3).unwrap(),
        ByteSegment::from_arc_range(3, Arc::clone(&backing), 3..7).unwrap(),
        ByteSegment::from_arc_range(7, Arc::clone(&backing), 7..10).unwrap(),
    ])
    .unwrap();

    let view = image.subrange(2..9).unwrap();
    assert_eq!(view.len(), 7);
    assert_eq!(view.segments().len(), 3);
    assert_eq!(view.segments()[0].logical_range(), 0..1);
    assert_eq!(view.segments()[1].logical_range(), 1..5);
    assert_eq!(view.segments()[2].logical_range(), 5..7);
    assert_eq!(view.segments()[0].as_slice(), b"2");
    assert_eq!(view.segments()[1].as_slice(), b"3456");
    assert_eq!(view.segments()[2].as_slice(), b"78");
    assert_eq!(
        view.segments()[0].as_slice().as_ptr(),
        backing[2..].as_ptr()
    );
    assert_eq!(Arc::strong_count(&backing), 7);
}

#[test]
fn empty_contiguous_and_invalid_ranges_have_explicit_behavior() {
    let empty = SegmentedBytes::empty();
    assert!(empty.is_empty());
    assert_eq!(empty.contiguous(), Some(&[][..]));
    assert_eq!(empty.subrange(0..0).unwrap(), empty);

    let bytes: Arc<[u8]> = Arc::from(&b"abc"[..]);
    let contiguous = SegmentedBytes::from_contiguous(Arc::clone(&bytes)).unwrap();
    assert_eq!(contiguous.contiguous(), Some(&b"abc"[..]));
    assert!(
        contiguous
            .subrange(std::ops::Range { start: 2, end: 1 })
            .is_err()
    );
    assert!(contiguous.subrange(0..4).is_err());

    assert!(SegmentedBytes::new(vec![ByteSegment::new(1, bytes).unwrap()]).is_err());
    assert!(ByteSegment::new(u64::MAX, Arc::from(&[1_u8][..])).is_err());
}

#[test]
fn public_segmented_validation_accepts_a_wire_golden_without_materialization() {
    let bytes = include_bytes!(
        "../../unity-asset-write/tests/fixtures/serialized_file_wire/v16.assets.bin"
    );
    let backing: Arc<[u8]> = Arc::from(bytes.as_slice());
    let image = SegmentedBytes::new(
        (0..backing.len())
            .map(|index| {
                ByteSegment::from_arc_range(
                    u64::try_from(index).unwrap(),
                    Arc::clone(&backing),
                    index..index + 1,
                )
                .unwrap()
            })
            .collect(),
    )
    .unwrap();
    let mut budget = AssetLoadBudget::default();

    SerializedFileParser::validate_segmented_with_budget(&image, &mut budget).unwrap();
    assert!(image.contiguous().is_none());
    assert!(budget.usage().entries > 0);
    assert!(budget.usage().bytes > 0);
}
