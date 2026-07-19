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
fn construction_reuses_nonempty_segment_storage_and_preserves_capacity() {
    let backing: Arc<[u8]> = Arc::from(&b"abcd"[..]);
    let mut segments = Vec::with_capacity(16);
    segments.push(ByteSegment::new(0, Arc::clone(&backing)).unwrap());
    segments.push(ByteSegment::from_arc_range(4, backing, 4..4).unwrap());
    let original_pointer = segments.as_ptr();
    let original_capacity = segments.capacity();

    let image = SegmentedBytes::new(segments).unwrap();

    assert_eq!(image.segments().len(), 1);
    assert_eq!(image.segments().as_ptr(), original_pointer);
    assert_eq!(image.segment_capacity(), original_capacity);
}

#[test]
fn construction_releases_an_all_empty_segment_buffer() {
    let backing: Arc<[u8]> = Arc::from(&b"unused"[..]);
    let mut segments = Vec::with_capacity(16);
    segments.push(ByteSegment::from_arc_range(0, backing, 0..0).unwrap());

    let image = SegmentedBytes::new(segments).unwrap();

    assert!(image.is_empty());
    assert!(image.segments().is_empty());
    assert_eq!(image.segment_capacity(), 0);
}

#[test]
fn generated_vec_backing_and_subranges_are_zero_copy() {
    let generated = Arc::new(b"generated-bytes".to_vec());
    let image = SegmentedBytes::new(vec![
        ByteSegment::from_arc_vec_range(0, Arc::clone(&generated), 2..11).unwrap(),
    ])
    .unwrap();

    assert_eq!(image.segments()[0].as_slice(), b"nerated-b");
    assert_eq!(
        image.segments()[0].as_slice().as_ptr(),
        generated[2..].as_ptr()
    );

    let view = image.subrange(1..7).unwrap();
    assert_eq!(view.segments()[0].as_slice(), b"erated");
    assert_eq!(
        view.segments()[0].as_slice().as_ptr(),
        generated[3..].as_ptr()
    );
    assert_eq!(Arc::strong_count(&generated), 3);
}

#[test]
fn segments_rebase_complete_and_partial_ranges_without_copying() {
    let backing: Arc<[u8]> = Arc::from(b"0123456789abcdef".as_slice());
    let segment = ByteSegment::from_arc_range(10, Arc::clone(&backing), 2..14).unwrap();

    let rebased = segment.rebase(100).unwrap();
    assert_eq!(rebased.logical_range(), 100..112);
    assert_eq!(rebased.as_slice(), b"23456789abcd");
    assert_eq!(rebased.as_slice().as_ptr(), segment.as_slice().as_ptr());

    let partial = segment.rebase_subrange(13..18, 200).unwrap();
    assert_eq!(partial.logical_range(), 200..205);
    assert_eq!(partial.as_slice(), b"56789");
    assert_eq!(
        partial.as_slice().as_ptr(),
        segment.as_slice()[3..].as_ptr()
    );

    for range in [9..10, 10..23, std::ops::Range { start: 18, end: 17 }] {
        assert!(segment.rebase_subrange(range, 0).is_err());
    }
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

    let proof = SerializedFileParser::validate_segmented_with_budget(&image, &mut budget).unwrap();
    let contiguous =
        SerializedFileParser::inspect_slice_with_budget(bytes, &mut AssetLoadBudget::default())
            .unwrap();
    let parsed = SerializedFileParser::from_bytes(bytes.to_vec()).unwrap();

    assert_eq!(proof, contiguous);
    assert_eq!(proof.version(), parsed.header.version);
    assert_eq!(proof.byte_order(), parsed.header.byte_order());
    assert_eq!(proof.metadata_size(), parsed.header.metadata_size);
    assert_eq!(proof.data_offset(), parsed.header.data_offset);
    assert_eq!(proof.declared_file_size(), bytes.len() as u64);
    assert_eq!(proof.retained_heap_bytes().unwrap(), 0);
    assert!(image.contiguous().is_none());
    assert!(budget.usage().entries > 0);
    assert!(budget.usage().bytes > 0);
}

#[test]
fn serialized_file_inspection_rejects_wrong_size_offset_and_untyped_tail() {
    let golden = include_bytes!(
        "../../unity-asset-write/tests/fixtures/serialized_file_wire/v16.assets.bin"
    );

    let mut wrong_size = golden.to_vec();
    let impossible_size = u32::try_from(golden.len() + 1).unwrap();
    wrong_size[4..8].copy_from_slice(&impossible_size.to_be_bytes());
    let error = SerializedFileParser::inspect_slice_with_budget(
        &wrong_size,
        &mut AssetLoadBudget::default(),
    )
    .expect_err("declared size beyond the image must be rejected");
    assert!(error.to_string().contains("file size"));

    let mut wrong_offset = golden.to_vec();
    wrong_offset[12..16].copy_from_slice(&0_u32.to_be_bytes());
    let error = SerializedFileParser::inspect_slice_with_budget(
        &wrong_offset,
        &mut AssetLoadBudget::default(),
    )
    .expect_err("data offset before the header must be rejected");
    assert!(error.to_string().contains("data offset"));

    let mut trailing = golden.to_vec();
    trailing.push(0);
    let error =
        SerializedFileParser::inspect_slice_with_budget(&trailing, &mut AssetLoadBudget::default())
            .expect_err("bytes after the declared image must be rejected");
    assert!(error.to_string().contains("image contains"));
}
