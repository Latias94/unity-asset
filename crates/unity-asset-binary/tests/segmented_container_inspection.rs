use std::io::{Cursor, Write};
use std::ops::Range;
use std::sync::Arc;

use flate2::Compression;
use flate2::write::GzEncoder;
use unity_asset_binary::bundle::{BundleLayoutKind, BundleLoadOptions, BundleParser};
use unity_asset_binary::compression::CompressionType;
use unity_asset_binary::webfile::{WebFile, WebFileCompression};
use unity_asset_binary::{ByteSegment, SegmentedBytes};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

fn one_byte_segments(bytes: &[u8]) -> SegmentedBytes {
    let backing: Arc<[u8]> = Arc::from(bytes);
    SegmentedBytes::new(
        (0..backing.len())
            .map(|index| {
                ByteSegment::from_arc_range(
                    u64::try_from(index).expect("test offset fits u64"),
                    Arc::clone(&backing),
                    index..index + 1,
                )
                .expect("one-byte segment is valid")
            })
            .collect(),
    )
    .expect("one-byte image is contiguous")
}

fn unityfs_with_duplicate_nodes(second_offset: i64) -> Vec<u8> {
    let payload = b"abcdefgh";
    let mut blocks_info = vec![0xc3_u8; 16];
    blocks_info.extend_from_slice(&1_i32.to_be_bytes());
    blocks_info.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("payload size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("payload size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(&0_u16.to_be_bytes());
    blocks_info.extend_from_slice(&2_i32.to_be_bytes());
    for (offset, length) in [(0_i64, 4_i64), (second_offset, 4_i64)] {
        blocks_info.extend_from_slice(&offset.to_be_bytes());
        blocks_info.extend_from_slice(&length.to_be_bytes());
        blocks_info.extend_from_slice(&4_u32.to_be_bytes());
        blocks_info.extend_from_slice(b"duplicate.assets\0");
    }

    let mut bytes = b"UnityFS\0".to_vec();
    bytes.extend_from_slice(&6_u32.to_be_bytes());
    bytes.extend_from_slice(b"5.x.x\0");
    bytes.extend_from_slice(b"0.0.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&0_i64.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&blocks_info);
    bytes.extend_from_slice(payload);
    let total_len = i64::try_from(bytes.len()).expect("bundle size fits i64");
    bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());
    bytes
}

struct LegacyFixture {
    bytes: Vec<u8>,
    decoded_len: usize,
    encoded_len: usize,
}

fn legacy_v3_with_duplicate_nodes(signature: &str, second_offset_delta: u32) -> LegacyFixture {
    let entries = [
        ("duplicate.assets", b"left".as_slice()),
        ("duplicate.assets", b"right".as_slice()),
    ];
    let directory_len = 4 + entries
        .iter()
        .map(|(name, _)| name.len() + 1 + 8)
        .sum::<usize>();
    let mut payload_offset = u32::try_from(directory_len).expect("directory size fits u32");
    let mut decoded = Vec::new();
    decoded.extend_from_slice(
        &i32::try_from(entries.len())
            .expect("entry count fits i32")
            .to_be_bytes(),
    );
    for (index, (name, payload)) in entries.iter().enumerate() {
        decoded.extend_from_slice(name.as_bytes());
        decoded.push(0);
        let offset = if index == 1 {
            payload_offset
                .checked_add(second_offset_delta)
                .expect("test legacy offset does not overflow")
        } else {
            payload_offset
        };
        decoded.extend_from_slice(&offset.to_be_bytes());
        decoded.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("payload size fits u32")
                .to_be_bytes(),
        );
        payload_offset = payload_offset
            .checked_add(u32::try_from(payload.len()).expect("payload size fits u32"))
            .expect("payload offset does not overflow");
    }
    for (_, payload) in entries {
        decoded.extend_from_slice(payload);
    }

    let encoded = if signature == "UnityWeb" {
        let mut encoded = Vec::new();
        lzma_rs::lzma_compress(&mut Cursor::new(&decoded), &mut encoded)
            .expect("compress legacy UnityWeb fixture");
        encoded
    } else {
        decoded.clone()
    };

    let mut bytes = Vec::new();
    bytes.extend_from_slice(signature.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&3_u32.to_be_bytes());
    bytes.extend_from_slice(b"2022.3.0f1\0");
    bytes.extend_from_slice(b"2022.3.0f1\0");
    let minimum_streamed_bytes_offset = bytes.len();
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    let header_size_offset = bytes.len();
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u32.to_be_bytes());
    bytes.extend_from_slice(&1_i32.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(encoded.len())
            .expect("encoded size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(decoded.len())
            .expect("decoded size fits u32")
            .to_be_bytes(),
    );
    let complete_file_size_offset = bytes.len();
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(directory_len)
            .expect("directory size fits u32")
            .to_be_bytes(),
    );
    let header_size = u32::try_from(bytes.len()).expect("header size fits u32");
    let complete_file_size = header_size
        .checked_add(u32::try_from(encoded.len()).expect("encoded size fits u32"))
        .expect("complete file size does not overflow");
    bytes[minimum_streamed_bytes_offset..minimum_streamed_bytes_offset + 4]
        .copy_from_slice(&complete_file_size.to_be_bytes());
    bytes[header_size_offset..header_size_offset + 4].copy_from_slice(&header_size.to_be_bytes());
    bytes[complete_file_size_offset..complete_file_size_offset + 4]
        .copy_from_slice(&complete_file_size.to_be_bytes());
    bytes.extend_from_slice(&encoded);

    LegacyFixture {
        bytes,
        decoded_len: decoded.len(),
        encoded_len: encoded.len(),
    }
}

fn lzma_unity_block(bytes: &[u8]) -> Vec<u8> {
    let mut alone = Vec::new();
    lzma_rs::lzma_compress(&mut Cursor::new(bytes), &mut alone)
        .expect("compress Unity LZMA block fixture");
    assert!(alone.len() >= 13);
    let mut encoded = alone[..5].to_vec();
    encoded.extend_from_slice(&alone[13..]);
    encoded
}

fn encode_bundle_section(bytes: &[u8], compression: CompressionType) -> Vec<u8> {
    match compression {
        CompressionType::None => bytes.to_vec(),
        CompressionType::Lzma => lzma_unity_block(bytes),
        CompressionType::Lz4 | CompressionType::Lz4Hc => lz4_flex::block::compress(bytes),
        CompressionType::Brotli => brotli(bytes),
        CompressionType::Lzham => panic!("LZHAM is not supported by the test fixture"),
    }
}

fn file_stream_v6_with_duplicate_nodes(
    signature: &str,
    blocks_info_at_end: bool,
    blocks_info_compression: CompressionType,
    block_compression: CompressionType,
    blocks_info_trailing: &[u8],
    block_trailing: &[u8],
    physical_block_tail: &[u8],
) -> Vec<u8> {
    let payload = b"abcdefgh";
    let mut encoded_payload = encode_bundle_section(payload, block_compression);
    encoded_payload.extend_from_slice(block_trailing);

    let mut blocks_info = vec![0xd7_u8; 16];
    blocks_info.extend_from_slice(&1_i32.to_be_bytes());
    blocks_info.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("payload size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(
        &u32::try_from(encoded_payload.len())
            .expect("encoded payload size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(&(block_compression as u16).to_be_bytes());
    blocks_info.extend_from_slice(&2_i32.to_be_bytes());
    for (offset, length) in [(0_i64, 4_i64), (4_i64, 4_i64)] {
        blocks_info.extend_from_slice(&offset.to_be_bytes());
        blocks_info.extend_from_slice(&length.to_be_bytes());
        blocks_info.extend_from_slice(&4_u32.to_be_bytes());
        blocks_info.extend_from_slice(b"duplicate.assets\0");
    }

    let mut encoded_blocks_info = encode_bundle_section(&blocks_info, blocks_info_compression);
    encoded_blocks_info.extend_from_slice(blocks_info_trailing);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(signature.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&6_u32.to_be_bytes());
    bytes.extend_from_slice(b"5.x.x\0");
    bytes.extend_from_slice(b"0.0.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&0_i64.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(encoded_blocks_info.len())
            .expect("encoded blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("decoded blocks-info size fits u32")
            .to_be_bytes(),
    );
    let mut flags = 0x40_u32 | blocks_info_compression as u32;
    if blocks_info_at_end {
        flags |= 0x80;
    }
    bytes.extend_from_slice(&flags.to_be_bytes());
    if signature != "UnityFS" {
        bytes.push(0x5a);
    }

    if blocks_info_at_end {
        bytes.extend_from_slice(&encoded_payload);
        bytes.extend_from_slice(physical_block_tail);
        bytes.extend_from_slice(&encoded_blocks_info);
    } else {
        bytes.extend_from_slice(&encoded_blocks_info);
        bytes.extend_from_slice(&encoded_payload);
        bytes.extend_from_slice(physical_block_tail);
    }
    let total_len = i64::try_from(bytes.len()).expect("bundle size fits i64");
    bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());
    bytes
}

struct FileStreamBlocksFixture {
    bytes: Vec<u8>,
    payload_range: Range<u64>,
}

fn file_stream_v6_with_mixed_blocks(
    signature: &str,
    blocks_info_at_end: bool,
) -> FileStreamBlocksFixture {
    let blocks: [(&[u8], CompressionType); 3] = [
        (b"raw-block", CompressionType::None),
        (b"lz4-lz4-lz4-lz4-lz4-lz4-lz4-lz4", CompressionType::Lz4),
        (
            b"brotli-brotli-brotli-brotli-brotli-brotli",
            CompressionType::Brotli,
        ),
    ];
    let encoded_blocks = blocks
        .iter()
        .map(|(decoded, compression)| encode_bundle_section(decoded, *compression))
        .collect::<Vec<_>>();
    let encoded_payload = encoded_blocks.concat();
    let decoded_payload_len = blocks
        .iter()
        .map(|(decoded, _)| decoded.len())
        .sum::<usize>();

    let mut blocks_info = vec![0xa9_u8; 16];
    blocks_info.extend_from_slice(
        &i32::try_from(blocks.len())
            .expect("block count fits i32")
            .to_be_bytes(),
    );
    for (index, (decoded, compression)) in blocks.iter().copied().enumerate() {
        blocks_info.extend_from_slice(
            &u32::try_from(decoded.len())
                .expect("decoded block size fits u32")
                .to_be_bytes(),
        );
        blocks_info.extend_from_slice(
            &u32::try_from(encoded_blocks[index].len())
                .expect("encoded block size fits u32")
                .to_be_bytes(),
        );
        blocks_info.extend_from_slice(&(compression as u16).to_be_bytes());
    }
    blocks_info.extend_from_slice(&1_i32.to_be_bytes());
    blocks_info.extend_from_slice(&0_i64.to_be_bytes());
    blocks_info.extend_from_slice(
        &i64::try_from(decoded_payload_len)
            .expect("decoded payload size fits i64")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(&4_u32.to_be_bytes());
    blocks_info.extend_from_slice(b"payload.assets\0");

    let mut bytes = Vec::new();
    bytes.extend_from_slice(signature.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&6_u32.to_be_bytes());
    bytes.extend_from_slice(b"5.x.x\0");
    bytes.extend_from_slice(b"0.0.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&0_i64.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("decoded blocks-info size fits u32")
            .to_be_bytes(),
    );
    let mut flags = 0x40_u32;
    if blocks_info_at_end {
        flags |= 0x80;
    }
    bytes.extend_from_slice(&flags.to_be_bytes());
    if signature != "UnityFS" {
        bytes.push(0x5a);
    }

    let payload_start;
    if blocks_info_at_end {
        payload_start = bytes.len();
        bytes.extend_from_slice(&encoded_payload);
        bytes.extend_from_slice(&blocks_info);
    } else {
        bytes.extend_from_slice(&blocks_info);
        payload_start = bytes.len();
        bytes.extend_from_slice(&encoded_payload);
    }
    let payload_end = payload_start
        .checked_add(encoded_payload.len())
        .expect("payload range does not overflow");
    let total_len = i64::try_from(bytes.len()).expect("bundle size fits i64");
    bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());

    FileStreamBlocksFixture {
        bytes,
        payload_range: u64::try_from(payload_start).expect("payload start fits u64")
            ..u64::try_from(payload_end).expect("payload end fits u64"),
    }
}

fn webfile_with_duplicate_entries(second_offset_delta: i32) -> Vec<u8> {
    let entries = [
        ("same.bin", b"left".as_slice()),
        ("same.bin", b"right".as_slice()),
    ];
    let directory_len = entries
        .iter()
        .map(|(name, _)| 12 + name.len())
        .sum::<usize>();
    let head_length = 20 + directory_len;
    let mut payload_offset = i32::try_from(head_length).expect("header size fits i32");
    let mut directory = Vec::new();
    for (index, (name, payload)) in entries.iter().enumerate() {
        let offset = if index == 1 {
            payload_offset
                .checked_add(second_offset_delta)
                .expect("test offset does not overflow")
        } else {
            payload_offset
        };
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(
            &i32::try_from(payload.len())
                .expect("payload size fits i32")
                .to_le_bytes(),
        );
        directory.extend_from_slice(
            &i32::try_from(name.len())
                .expect("name size fits i32")
                .to_le_bytes(),
        );
        directory.extend_from_slice(name.as_bytes());
        payload_offset = payload_offset
            .checked_add(i32::try_from(payload.len()).expect("payload size fits i32"))
            .expect("payload offset does not overflow");
    }

    let mut bytes = b"UnityWebData1.0\0".to_vec();
    bytes.extend_from_slice(
        &i32::try_from(head_length)
            .expect("header size fits i32")
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&directory);
    for (_, payload) in entries {
        bytes.extend_from_slice(payload);
    }
    bytes
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).expect("write gzip fixture");
    encoder.finish().expect("finish gzip fixture")
}

fn brotli(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    {
        let mut encoder = brotli::CompressorWriter::new(&mut encoded, 4 * 1024, 5, 22);
        encoder.write_all(bytes).expect("write brotli fixture");
    }
    encoded
}

fn lz4_unityfs() -> Vec<u8> {
    let payload = b"abcdefgh";
    let compressed = lz4_flex::block::compress(payload);
    let mut blocks_info = vec![0_u8; 16];
    blocks_info.extend_from_slice(&1_i32.to_be_bytes());
    blocks_info.extend_from_slice(
        &u32::try_from(payload.len())
            .expect("payload size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(
        &u32::try_from(compressed.len())
            .expect("compressed size fits u32")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(&(CompressionType::Lz4 as u16).to_be_bytes());
    blocks_info.extend_from_slice(&1_i32.to_be_bytes());
    blocks_info.extend_from_slice(&0_i64.to_be_bytes());
    blocks_info.extend_from_slice(
        &i64::try_from(payload.len())
            .expect("payload size fits i64")
            .to_be_bytes(),
    );
    blocks_info.extend_from_slice(&4_u32.to_be_bytes());
    blocks_info.extend_from_slice(b"payload.assets\0");

    let mut bytes = b"UnityFS\0".to_vec();
    bytes.extend_from_slice(&6_u32.to_be_bytes());
    bytes.extend_from_slice(b"5.x.x\0");
    bytes.extend_from_slice(b"0.0.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&0_i64.to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(blocks_info.len())
            .expect("blocks-info size fits u32")
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&blocks_info);
    bytes.extend_from_slice(&compressed);
    let total_len = i64::try_from(bytes.len()).expect("bundle size fits i64");
    bytes[size_offset..size_offset + 8].copy_from_slice(&total_len.to_be_bytes());
    bytes
}

#[test]
fn unityfs_one_byte_segments_match_contiguous_inspection_and_preserve_occurrences() {
    let bytes = unityfs_with_duplicate_nodes(4);
    let image = one_byte_segments(&bytes);
    let mut contiguous_budget = AssetLoadBudget::default();
    let mut segmented_budget = AssetLoadBudget::default();

    let contiguous = BundleParser::inspect_slice_with_budget(&bytes, &mut contiguous_budget)
        .expect("inspect contiguous UnityFS");
    let segmented = BundleParser::inspect_segmented_with_budget(&image, &mut segmented_budget)
        .expect("inspect segmented UnityFS");

    assert_eq!(segmented, contiguous);
    assert_eq!(segmented.signature(), "UnityFS");
    assert_eq!(segmented.version(), 6);
    assert_eq!(segmented.flags(), 0);
    assert_eq!(segmented.blocks_info_hash(), Some([0xc3; 16]));
    assert!(segmented.legacy().is_none());
    assert_eq!(segmented.blocks().len(), 1);
    assert_eq!(segmented.directory().len(), 2);
    assert_eq!(segmented.directory()[0].name(), "duplicate.assets");
    assert_eq!(segmented.directory()[0].occurrence(), 0);
    assert_eq!(segmented.directory()[1].occurrence(), 1);
    assert_eq!(segmented.directory()[1].offset(), 4);
    assert_eq!(segmented.directory()[1].length(), 4);
    assert!(segmented.retained_heap_bytes().unwrap() > 0);
    assert!(image.contiguous().is_none());

    let parsed = BundleParser::from_slice_with_options(&bytes, BundleLoadOptions::lazy())
        .expect("parse the same contiguous UnityFS");
    assert_eq!(segmented.signature(), parsed.header.signature);
    assert_eq!(segmented.version(), parsed.header.version);
    assert_eq!(segmented.flags(), parsed.header.flags);
    assert_eq!(segmented.blocks().len(), parsed.blocks.len());
    assert_eq!(segmented.directory().len(), parsed.nodes.len());
    for (inspected, parsed) in segmented.directory().iter().zip(&parsed.nodes) {
        assert_eq!(inspected.name(), parsed.name);
        assert_eq!(inspected.offset(), parsed.offset);
        assert_eq!(inspected.length(), parsed.size);
        assert_eq!(inspected.flags(), parsed.flags);
    }
}

#[test]
fn segmented_unityfs_decodes_each_lz4_block_and_reports_its_wire_flags() {
    let bytes = lz4_unityfs();
    let image = one_byte_segments(&bytes);
    let mut budget = AssetLoadBudget::default();

    let inspection = BundleParser::inspect_segmented_with_budget(&image, &mut budget)
        .expect("inspect LZ4 UnityFS");

    assert_eq!(inspection.blocks().len(), 1);
    assert_eq!(inspection.blocks()[0].compression(), CompressionType::Lz4);
    assert_eq!(inspection.blocks()[0].flags(), CompressionType::Lz4 as u16);
    assert_eq!(inspection.stats().decompressed_bytes(), 69 + 8);

    let mut corrupt = bytes;
    let compressed_len = lz4_flex::block::compress(b"abcdefgh").len();
    let block_start = corrupt
        .len()
        .checked_sub(compressed_len)
        .expect("fixture contains its compressed block");
    corrupt[block_start] = 0xff;
    let corrupt = one_byte_segments(&corrupt);
    let error =
        BundleParser::inspect_segmented_with_budget(&corrupt, &mut AssetLoadBudget::default())
            .expect_err("corrupt LZ4 block must fail independent inspection");
    assert!(error.to_string().contains("LZ4"));
}

#[test]
fn unityfs_segmented_inspection_rejects_a_directory_offset_past_decoded_data() {
    let bytes = unityfs_with_duplicate_nodes(8);
    let image = one_byte_segments(&bytes);
    let mut budget = AssetLoadBudget::default();

    let error = BundleParser::inspect_segmented_with_budget(&image, &mut budget)
        .expect_err("out-of-range node must be rejected");

    let message = error.to_string().to_lowercase();
    assert!(message.contains("directory"), "unexpected error: {error}");
    assert!(
        message.contains("decompressed"),
        "unexpected error: {error}"
    );
}

#[test]
fn segmented_bundle_inspection_honors_the_caller_byte_budget() {
    let bytes = unityfs_with_duplicate_nodes(4);
    let image = one_byte_segments(&bytes);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: 16,
        ..AssetLoadLimits::default()
    })
    .expect("valid budget limits");

    let error = BundleParser::inspect_segmented_with_budget(&image, &mut budget)
        .expect_err("metadata reads must exhaust the byte budget");

    assert!(matches!(
        error,
        unity_asset_binary::BinaryError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit: 16,
            ..
        })
    ));
}

#[test]
fn unityraw_v3_one_byte_segments_preserve_header_fields_and_directory_order() {
    let fixture = legacy_v3_with_duplicate_nodes("UnityRaw", 0);
    let image = one_byte_segments(&fixture.bytes);
    let contiguous =
        BundleParser::inspect_slice_with_budget(&fixture.bytes, &mut AssetLoadBudget::default())
            .expect("inspect contiguous UnityRaw");
    let segmented =
        BundleParser::inspect_segmented_with_budget(&image, &mut AssetLoadBudget::default())
            .expect("inspect segmented UnityRaw");

    assert_eq!(segmented, contiguous);
    assert_eq!(segmented.signature(), "UnityRaw");
    assert_eq!(segmented.compression(), CompressionType::None);
    assert_eq!(segmented.blocks_info_hash(), None);
    let legacy = segmented.legacy().expect("legacy header is retained");
    assert_eq!(legacy.hash(), None);
    assert_eq!(legacy.crc(), None);
    assert_eq!(
        legacy.minimum_streamed_bytes() as usize,
        fixture.bytes.len()
    );
    assert_eq!(
        legacy.header_size() as usize,
        fixture.bytes.len() - fixture.encoded_len
    );
    assert_eq!(legacy.levels_before_streaming(), 1);
    assert_eq!(legacy.level_count(), 1);
    assert_eq!(legacy.compressed_size() as usize, fixture.encoded_len);
    assert_eq!(legacy.uncompressed_size() as usize, fixture.decoded_len);
    assert_eq!(
        legacy.complete_file_size().map(|size| size as usize),
        Some(fixture.bytes.len())
    );
    assert_eq!(legacy.file_info_header_size(), Some(54));
    assert_eq!(segmented.directory().len(), 2);
    assert_eq!(segmented.directory()[0].occurrence(), 0);
    assert_eq!(segmented.directory()[1].occurrence(), 1);
    assert_eq!(segmented.directory()[1].offset(), 58);
    assert_eq!(
        segmented.stats().compressed_bytes(),
        fixture.encoded_len as u64
    );
    assert_eq!(
        segmented.stats().decompressed_bytes(),
        fixture.decoded_len as u64
    );
    assert!(image.contiguous().is_none());

    let parsed = BundleParser::from_slice_with_options(&fixture.bytes, BundleLoadOptions::lazy())
        .expect("parse the same contiguous UnityRaw");
    assert_eq!(segmented.directory().len(), parsed.nodes.len());
    for (inspected, parsed) in segmented.directory().iter().zip(&parsed.nodes) {
        assert_eq!(inspected.name(), parsed.name);
        assert_eq!(inspected.offset(), parsed.offset);
        assert_eq!(inspected.length(), parsed.size);
    }
}

#[test]
fn unityweb_v3_lzma_one_byte_segments_stream_with_exact_budget_accounting() {
    let fixture = legacy_v3_with_duplicate_nodes("UnityWeb", 0);
    let image = one_byte_segments(&fixture.bytes);
    let inspection =
        BundleParser::inspect_segmented_with_budget(&image, &mut AssetLoadBudget::default())
            .expect("inspect segmented UnityWeb");

    assert_eq!(inspection.signature(), "UnityWeb");
    assert_eq!(inspection.compression(), CompressionType::Lzma);
    assert_eq!(inspection.directory().len(), 2);
    assert_eq!(
        inspection.stats().compressed_bytes(),
        fixture.encoded_len as u64
    );
    assert_eq!(
        inspection.stats().decompressed_bytes(),
        fixture.decoded_len as u64
    );

    let mut short = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: u64::try_from(fixture.decoded_len - 1)
            .expect("decoded size fits u64"),
        ..AssetLoadLimits::default()
    })
    .expect("valid budget limits");
    let error = BundleParser::inspect_segmented_with_budget(&image, &mut short)
        .expect_err("legacy LZMA output must honor its caller budget");
    assert!(matches!(
        error,
        unity_asset_binary::BinaryError::Budget(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            ..
        })
    ));

    let mut compressed_short = AssetLoadBudget::new(AssetLoadLimits {
        max_compressed_bytes: u64::try_from(fixture.encoded_len - 1)
            .expect("encoded size fits u64"),
        ..AssetLoadLimits::default()
    })
    .expect("valid budget limits");
    let error = BundleParser::inspect_segmented_with_budget(&image, &mut compressed_short)
        .expect_err("legacy LZMA input must honor its caller budget");
    assert!(matches!(
        error,
        unity_asset_binary::BinaryError::Budget(BudgetError::Exceeded {
            resource: "compressed_bytes",
            ..
        })
    ));
}

#[test]
fn segmented_legacy_inspection_rejects_wrong_offsets_and_trailing_bytes() {
    for signature in ["UnityRaw", "UnityWeb"] {
        let wrong_offset = legacy_v3_with_duplicate_nodes(signature, 1000);
        let error = BundleParser::inspect_segmented_with_budget(
            &one_byte_segments(&wrong_offset.bytes),
            &mut AssetLoadBudget::default(),
        )
        .expect_err("legacy node outside the decoded blob must be rejected");
        assert!(error.to_string().contains("directory"));
    }

    let mut trailing = legacy_v3_with_duplicate_nodes("UnityRaw", 0).bytes;
    trailing.extend_from_slice(b"trailing");
    let error = BundleParser::inspect_segmented_with_budget(
        &one_byte_segments(&trailing),
        &mut AssetLoadBudget::default(),
    )
    .expect_err("untyped bytes after the declared bundle size must be rejected");
    assert!(error.to_string().contains("declared size"));
}

#[test]
fn unityweb_and_unityraw_v6_use_file_stream_layout_in_both_blocks_info_positions() {
    for signature in ["UnityWeb", "UnityRaw"] {
        for blocks_info_at_end in [false, true] {
            let bytes = file_stream_v6_with_duplicate_nodes(
                signature,
                blocks_info_at_end,
                CompressionType::None,
                CompressionType::None,
                &[],
                &[],
                &[],
            );
            let image = one_byte_segments(&bytes);
            let contiguous =
                BundleParser::inspect_slice_with_budget(&bytes, &mut AssetLoadBudget::default())
                    .expect("inspect contiguous v6 file-stream bundle");
            let segmented = BundleParser::inspect_segmented_with_budget(
                &image,
                &mut AssetLoadBudget::default(),
            )
            .expect("inspect segmented v6 file-stream bundle");

            assert_eq!(segmented, contiguous);
            assert_eq!(segmented.signature(), signature);
            assert_eq!(segmented.version(), 6);
            assert_eq!(segmented.layout(), BundleLayoutKind::FileStream);
            assert_eq!(segmented.file_stream_header_byte(), Some(0x5a));
            assert!(segmented.legacy().is_none());
            assert_eq!(segmented.blocks_info_hash(), Some([0xd7; 16]));
            assert_eq!(segmented.directory().len(), 2);
            assert_eq!(segmented.directory()[1].occurrence(), 1);

            let parsed = BundleParser::from_slice_with_options(&bytes, BundleLoadOptions::lazy())
                .expect("parse v6 file-stream bundle through the regular parser");
            assert_eq!(parsed.header.signature, signature);
            assert_eq!(parsed.nodes.len(), 2);
            assert_eq!(parsed.nodes[1].offset, 4);
        }
    }
}

#[test]
fn file_stream_v6_mixed_block_encoded_ranges_exactly_cover_the_physical_payload() {
    let expected_compressions = [
        CompressionType::None,
        CompressionType::Lz4,
        CompressionType::Brotli,
    ];

    for signature in ["UnityFS", "UnityWeb"] {
        for blocks_info_at_end in [false, true] {
            let fixture = file_stream_v6_with_mixed_blocks(signature, blocks_info_at_end);
            let image = one_byte_segments(&fixture.bytes);
            let contiguous = BundleParser::inspect_slice_with_budget(
                &fixture.bytes,
                &mut AssetLoadBudget::default(),
            )
            .expect("inspect contiguous v6 file-stream bundle with mixed compression");
            let inspection = BundleParser::inspect_segmented_with_budget(
                &image,
                &mut AssetLoadBudget::default(),
            )
            .expect("inspect v6 file-stream bundle with mixed block compression");

            assert!(image.contiguous().is_none());
            assert_eq!(inspection, contiguous);
            assert_eq!(inspection.blocks().len(), expected_compressions.len());
            let mut expected_start = fixture.payload_range.start;
            for (block, expected_compression) in inspection
                .blocks()
                .iter()
                .zip(expected_compressions.iter().copied())
            {
                let encoded_range = block.encoded_range();
                assert_eq!(encoded_range.start, expected_start);
                assert_eq!(
                    encoded_range.end - encoded_range.start,
                    u64::from(block.compressed_size())
                );
                assert_eq!(block.compression(), expected_compression);
                expected_start = encoded_range.end;
            }
            assert_eq!(expected_start, fixture.payload_range.end);
        }
    }
}

#[test]
fn unityfs_blocks_must_exactly_cover_the_physical_payload_range() {
    for blocks_info_at_end in [false, true] {
        let bytes = file_stream_v6_with_duplicate_nodes(
            "UnityFS",
            blocks_info_at_end,
            CompressionType::None,
            CompressionType::None,
            &[],
            &[],
            b"untyped-tail",
        );
        let error = BundleParser::inspect_segmented_with_budget(
            &one_byte_segments(&bytes),
            &mut AssetLoadBudget::default(),
        )
        .expect_err("untyped bytes in the physical block range must be rejected");
        assert!(error.to_string().contains("physical payload range"));
    }
}

#[test]
fn unityfs_proof_decoders_reject_trailing_lzma_and_brotli_input() {
    for compression in [CompressionType::Lzma, CompressionType::Brotli] {
        for blocks_info_at_end in [false, true] {
            let valid = file_stream_v6_with_duplicate_nodes(
                "UnityFS",
                blocks_info_at_end,
                compression,
                compression,
                &[],
                &[],
                &[],
            );
            let inspection = BundleParser::inspect_segmented_with_budget(
                &one_byte_segments(&valid),
                &mut AssetLoadBudget::default(),
            )
            .expect("valid blocks-info and data block streams must decode exactly");
            assert_eq!(inspection.blocks().len(), 1);
            assert_eq!(inspection.directory().len(), 2);
            assert!(
                inspection.stats().max_temporary_bytes()
                    > inspection
                        .stats()
                        .compressed_bytes()
                        .checked_add(inspection.stats().decompressed_bytes())
                        .expect("test byte totals do not overflow"),
                "{compression:?} proof statistics must include codec scratch"
            );

            let trailing_info = file_stream_v6_with_duplicate_nodes(
                "UnityFS",
                blocks_info_at_end,
                compression,
                CompressionType::None,
                b"x",
                &[],
                &[],
            );
            let error = BundleParser::inspect_segmented_with_budget(
                &one_byte_segments(&trailing_info),
                &mut AssetLoadBudget::default(),
            )
            .expect_err("blocks-info decoder must consume its declared compressed range");
            assert!(
                error.to_string().contains("trailing"),
                "unexpected {compression:?} blocks-info error (at_end={blocks_info_at_end}): {error}"
            );

            let trailing_block = file_stream_v6_with_duplicate_nodes(
                "UnityFS",
                blocks_info_at_end,
                CompressionType::None,
                compression,
                &[],
                b"x",
                &[],
            );
            let error = BundleParser::inspect_segmented_with_budget(
                &one_byte_segments(&trailing_block),
                &mut AssetLoadBudget::default(),
            )
            .expect_err("data-block decoder must consume its declared compressed range");
            assert!(
                error.to_string().contains("trailing"),
                "unexpected {compression:?} data-block error (at_end={blocks_info_at_end}): {error}"
            );
        }
    }
}

#[test]
fn gzip_webfile_one_byte_segments_match_contiguous_inspection() {
    let decoded = webfile_with_duplicate_entries(0);
    let encoded = gzip(&decoded);
    let image = one_byte_segments(&encoded);
    let mut contiguous_budget = AssetLoadBudget::default();
    let mut segmented_budget = AssetLoadBudget::default();

    let contiguous = WebFile::inspect_slice_with_budget(&encoded, &mut contiguous_budget)
        .expect("inspect contiguous gzip WebFile");
    let segmented = WebFile::inspect_segmented_with_budget(&image, &mut segmented_budget)
        .expect("inspect segmented gzip WebFile");

    assert_eq!(segmented, contiguous);
    assert_eq!(segmented.signature(), "UnityWebData1.0");
    assert_eq!(segmented.version(), "1.0");
    assert_eq!(segmented.compression(), WebFileCompression::Gzip);
    assert_eq!(segmented.directory().len(), 2);
    assert_eq!(segmented.directory()[0].occurrence(), 0);
    assert_eq!(segmented.directory()[1].occurrence(), 1);
    assert_eq!(segmented.stats().decoded_bytes(), decoded.len() as u64);
    assert_eq!(segmented.stats().encoded_bytes(), encoded.len() as u64);
    assert_eq!(segmented.stats().max_buffered_bytes(), 64 * 1024 + 40);
    assert!(segmented.retained_heap_bytes().unwrap() > 0);
    assert!(image.contiguous().is_none());

    let parsed = WebFile::from_bytes(encoded).expect("parse the same contiguous gzip WebFile");
    assert_eq!(segmented.signature(), parsed.signature);
    assert_eq!(segmented.compression(), parsed.compression);
    assert_eq!(segmented.directory().len(), parsed.files.len());
    for (inspected, parsed) in segmented.directory().iter().zip(&parsed.files) {
        assert_eq!(inspected.name(), parsed.name);
        assert_eq!(inspected.offset(), parsed.offset);
        assert_eq!(inspected.length(), parsed.size);
    }
}

#[test]
fn uncompressed_and_brotli_webfiles_have_segmented_inspection_parity() {
    let decoded = webfile_with_duplicate_entries(0);
    for (encoded, compression) in [
        (decoded.clone(), WebFileCompression::None),
        (brotli(&decoded), WebFileCompression::Brotli),
    ] {
        let image = one_byte_segments(&encoded);
        let contiguous =
            WebFile::inspect_slice_with_budget(&encoded, &mut AssetLoadBudget::default())
                .expect("inspect contiguous WebFile");
        let segmented =
            WebFile::inspect_segmented_with_budget(&image, &mut AssetLoadBudget::default())
                .expect("inspect segmented WebFile");

        assert_eq!(segmented, contiguous);
        assert_eq!(segmented.compression(), compression);
        assert_eq!(segmented.stats().decoded_bytes(), decoded.len() as u64);
        assert_eq!(segmented.directory()[1].occurrence(), 1);
    }
}

#[test]
fn brotli_webfile_combines_metadata_and_decoder_scratch_in_one_byte_reservation() {
    let decoded = webfile_with_duplicate_entries(0);
    let encoded = brotli(&decoded);
    let mut reference_budget = AssetLoadBudget::default();
    let inspection = WebFile::inspect_slice_with_budget(&encoded, &mut reference_budget)
        .expect("inspect Brotli WebFile with the default budget");
    let metadata_bytes = inspection.stats().metadata_bytes();
    let total_bytes = reference_budget.usage().bytes;
    let scratch_bytes = total_bytes
        .checked_sub(metadata_bytes)
        .expect("metadata is part of total byte usage");
    assert!(metadata_bytes > 0);
    assert!(scratch_bytes > 0);
    assert!(
        inspection.stats().max_buffered_bytes() >= scratch_bytes + 40,
        "Brotli proof statistics conservatively include decoder scratch and the live directory buffer"
    );

    let limit = metadata_bytes.max(scratch_bytes);
    assert!(metadata_bytes <= limit);
    assert!(scratch_bytes <= limit);
    assert!(total_bytes > limit);
    let mut constrained = AssetLoadBudget::new(AssetLoadLimits {
        max_bytes: limit,
        ..AssetLoadLimits::default()
    })
    .expect("valid combined byte limit");

    let error = WebFile::inspect_slice_with_budget(&encoded, &mut constrained)
        .expect_err("combined metadata and Brotli scratch must exceed the shared reservation");

    assert!(matches!(
        error,
        unity_asset_binary::BinaryError::Budget(BudgetError::Exceeded {
            resource: "bytes",
            limit: actual_limit,
            requested,
        }) if actual_limit == limit && requested > limit
    ));
    assert!(constrained.usage().bytes <= limit);
}

#[test]
fn segmented_webfile_inspection_rejects_a_wrong_payload_offset() {
    let decoded = webfile_with_duplicate_entries(1_000);
    let image = one_byte_segments(&decoded);
    let mut budget = AssetLoadBudget::default();

    let error = WebFile::inspect_segmented_with_budget(&image, &mut budget)
        .expect_err("out-of-range WebFile entry must be rejected");

    assert!(error.to_string().contains("entry data range"));
}

#[test]
fn gzip_segmented_webfile_inspection_honors_decompression_budget() {
    let decoded = webfile_with_duplicate_entries(0);
    let encoded = gzip(&decoded);
    let image = one_byte_segments(&encoded);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: u64::try_from(decoded.len() - 1).expect("size fits u64"),
        ..AssetLoadLimits::default()
    })
    .expect("valid budget limits");

    let error = WebFile::inspect_segmented_with_budget(&image, &mut budget)
        .expect_err("streaming decoder must enforce its output budget");

    assert!(matches!(
        error,
        unity_asset_binary::BinaryError::Budget(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            ..
        })
    ));
}

#[test]
fn segmented_gzip_webfile_rejects_trailing_encoded_bytes() {
    let decoded = webfile_with_duplicate_entries(0);
    let mut encoded = gzip(&decoded);
    encoded.extend_from_slice(b"trailing");
    let image = one_byte_segments(&encoded);

    let error = WebFile::inspect_segmented_with_budget(&image, &mut AssetLoadBudget::default())
        .expect_err("a single-member gzip WebFile must reject trailing bytes");

    assert!(error.to_string().contains("trailing bytes"));
}

#[test]
fn segmented_brotli_webfile_rejects_trailing_encoded_bytes() {
    let decoded = webfile_with_duplicate_entries(0);
    let mut encoded = brotli(&decoded);
    encoded.extend_from_slice(b"trailing");
    let image = one_byte_segments(&encoded);

    let error = WebFile::inspect_segmented_with_budget(&image, &mut AssetLoadBudget::default())
        .expect_err("a single Brotli WebFile stream must reject trailing bytes");

    assert!(error.to_string().contains("trailing bytes"));
}
