use unity_asset_binary::bundle::compression::BundleCompression;
use unity_asset_binary::bundle::header::BundleHeader;
use unity_asset_binary::bundle::parser::BundleParser;
use unity_asset_binary::bundle::types::BundleLoadOptions;
use unity_asset_binary::bundle::types::{AssetBundle, BundleFileInfo, DirectoryNode};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits, BudgetError};

fn be_u32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i32(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i64(v: i64) -> [u8; 8] {
    v.to_be_bytes()
}

fn unityfs_with_truncated_payload(block_info_at_end: bool) -> Vec<u8> {
    let mut blocks_info = vec![0u8; 16];
    blocks_info.extend_from_slice(&be_i32(1));
    blocks_info.extend_from_slice(&be_u32(4));
    blocks_info.extend_from_slice(&be_u32(4));
    blocks_info.extend_from_slice(&0u16.to_be_bytes());
    blocks_info.extend_from_slice(&be_i32(0));

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0));
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(if block_info_at_end { 0x80 } else { 0 }));
    let padding = (16 - (bytes.len() % 16)) % 16;
    bytes.extend(std::iter::repeat_n(0, padding));

    if block_info_at_end {
        bytes.extend_from_slice(&[0x11, 0x22]);
        bytes.extend_from_slice(&blocks_info);
        let declared_size = i64::try_from(bytes.len()).unwrap();
        bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(declared_size));
    } else {
        bytes.extend_from_slice(&blocks_info);
        bytes.extend_from_slice(&[0x11, 0x22]);
        let declared_size = i64::try_from(bytes.len()).unwrap();
        bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(declared_size));
        bytes.extend_from_slice(&[0x33, 0x44]);
    }

    bytes
}

#[test]
fn unityfs_header_rejects_negative_size() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(&be_i64(-1));
    bytes.extend_from_slice(&be_u32(1));
    bytes.extend_from_slice(&be_u32(1));
    bytes.extend_from_slice(&be_u32(0));

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
    let err = BundleHeader::from_reader(&mut reader).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn blocks_info_rejects_negative_block_count() {
    let mut data: Vec<u8> = vec![0u8; 16]; // hash
    data.extend_from_slice(&be_i32(-1)); // block_count
    let err = BundleCompression::parse_compression_blocks(&data).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn decompress_blocks_respects_max_memory() {
    let header = BundleHeader::default();
    let blocks = vec![CompressionBlock::new(1024, 1, 0)];
    let compressed = [0_u8; 1];
    let mut reader = BinaryReader::new(&compressed, ByteOrder::Big);

    let err =
        BundleCompression::decompress_data_blocks_limited(&header, &blocks, &mut reader, Some(16))
            .unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}

#[test]
fn unityfs_blocks_info_rejects_negative_node_count() {
    let mut blocks_info: Vec<u8> = vec![0u8; 16]; // hash
    blocks_info.extend_from_slice(&be_i32(1)); // block_count
    blocks_info.extend_from_slice(&be_u32(1)); // uncompressed_size
    blocks_info.extend_from_slice(&be_u32(1)); // compressed_size
    blocks_info.extend_from_slice(&0u16.to_be_bytes()); // flags (None)
    blocks_info.extend_from_slice(&be_i32(-1)); // node_count (invalid)

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0)); // placeholder for size
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(0)); // flags: no compression, blocks info at start

    // UnityFS v7+ aligns blocks info to 16 bytes.
    let pad = (16 - (bytes.len() % 16)) % 16;
    bytes.extend(std::iter::repeat_n(0u8, pad));
    bytes.extend_from_slice(&blocks_info);

    let total_size = bytes.len() as i64;
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let err =
        BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::default()).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn unityfs_blocks_info_respects_max_blocks_info_size() {
    let mut blocks_info: Vec<u8> = vec![0u8; 16]; // hash
    blocks_info.extend_from_slice(&be_i32(0)); // block_count
    blocks_info.extend_from_slice(&be_i32(0)); // node_count

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0)); // placeholder for size
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32((64 * 1024 * 1024 + 1) as u32)); // exceeds default 64MB
    bytes.extend_from_slice(&be_u32(0)); // flags: no compression, blocks info at start

    // UnityFS v7+ aligns blocks info to 16 bytes.
    let pad = (16 - (bytes.len() % 16)) % 16;
    bytes.extend(std::iter::repeat_n(0u8, pad));
    bytes.extend_from_slice(&blocks_info);

    let total_size = bytes.len() as i64;
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let err =
        BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::default()).unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}

#[test]
fn bundle_extract_slice_rejects_offset_size_overflow() {
    let bundle = AssetBundle::new(BundleHeader::default(), vec![0u8; 16]);

    let file = BundleFileInfo::new("a".to_string(), u64::MAX - 1, 10);
    let err = bundle.extract_file_slice(&file).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));

    let node = DirectoryNode::new("b".to_string(), u64::MAX - 1, 10, 0x4);
    let err = bundle.extract_node_slice(&node).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn bundle_validate_rejects_offset_size_overflow() {
    let mut bundle = AssetBundle::new(BundleHeader::default(), vec![0u8; 16]);
    bundle
        .files
        .push(BundleFileInfo::new("a".to_string(), u64::MAX - 1, 10));
    let err = bundle.validate().unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn legacy_directory_respects_max_compressed_size() {
    let compressed_size: u32 = 1024 * 1024;
    let uncompressed_size: u32 = 1;

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityRaw\0");
    bytes.extend_from_slice(&be_u32(3));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");

    // Legacy header fields (UnityPy `read_web_raw` ordering, levelCount=1).
    // We don't need to provide the directory blob itself: the limit is checked before reading bytes.
    let header_start = bytes.len() as u32;
    let mut header_size = header_start.saturating_add(24 + 4 + 4); // v3 includes completeFileSize + fileInfoHeaderSize
    header_size = (header_size.saturating_add(3)) & !3;
    let complete_file_size = header_size.saturating_add(compressed_size);

    bytes.extend_from_slice(&be_u32(complete_file_size)); // minimumStreamedBytes
    bytes.extend_from_slice(&be_u32(header_size)); // headerSize
    bytes.extend_from_slice(&be_u32(1)); // numberOfLevelsToDownloadBeforeStreaming
    bytes.extend_from_slice(&be_i32(1)); // levelCount
    bytes.extend_from_slice(&be_u32(compressed_size));
    bytes.extend_from_slice(&be_u32(uncompressed_size));
    bytes.extend_from_slice(&be_u32(complete_file_size)); // completeFileSize
    bytes.extend_from_slice(&be_u32(4)); // fileInfoHeaderSize (dummy)

    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }

    let options = BundleLoadOptions {
        validate: false,
        max_legacy_directory_compressed_size: Some(16),
        ..Default::default()
    };
    let err = BundleParser::from_bytes_with_options(bytes, options).unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}

#[test]
fn unityfs_blocks_info_respects_max_compressed_blocks_info_size() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0)); // placeholder for size
    bytes.extend_from_slice(&be_u32(1024)); // compressed blocks info size
    bytes.extend_from_slice(&be_u32(1)); // uncompressed blocks info size
    bytes.extend_from_slice(&be_u32(0)); // flags: no compression, blocks info at start

    let total_size = bytes.len() as i64;
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let options = BundleLoadOptions {
        max_compressed_blocks_info_size: Some(16),
        ..Default::default()
    };
    let err = BundleParser::from_bytes_with_options(bytes, options).unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}

#[test]
fn unityfs_lazy_rejects_total_compressed_exceeds_backing() {
    let mut blocks_info: Vec<u8> = vec![0u8; 16]; // hash
    blocks_info.extend_from_slice(&be_i32(1)); // block_count
    blocks_info.extend_from_slice(&be_u32(1)); // uncompressed_size
    blocks_info.extend_from_slice(&be_u32(100)); // compressed_size (no backing bytes for it)
    blocks_info.extend_from_slice(&0u16.to_be_bytes()); // flags (None)
    blocks_info.extend_from_slice(&be_i32(0)); // node_count

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0)); // placeholder for size
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(0)); // flags: no compression, blocks info at start

    // UnityFS v7+ aligns blocks info to 16 bytes.
    let pad = (16 - (bytes.len() % 16)) % 16;
    bytes.extend(std::iter::repeat_n(0u8, pad));
    bytes.extend_from_slice(&blocks_info);

    let total_size = bytes.len() as i64;
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let err = BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy()).unwrap_err();
    assert!(matches!(err, BinaryError::InvalidData(_)));
}

#[test]
fn unityfs_lazy_respects_max_compressed_block_size() {
    let mut blocks_info: Vec<u8> = vec![0u8; 16]; // hash
    blocks_info.extend_from_slice(&be_i32(1)); // block_count
    blocks_info.extend_from_slice(&be_u32(1)); // uncompressed_size
    blocks_info.extend_from_slice(&be_u32(32)); // compressed_size
    blocks_info.extend_from_slice(&0u16.to_be_bytes()); // flags (None)
    blocks_info.extend_from_slice(&be_i32(0)); // node_count

    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0)); // placeholder for size
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(blocks_info.len() as u32));
    bytes.extend_from_slice(&be_u32(0)); // flags: no compression, blocks info at start

    // UnityFS v7+ aligns blocks info to 16 bytes.
    let pad = (16 - (bytes.len() % 16)) % 16;
    bytes.extend(std::iter::repeat_n(0u8, pad));
    bytes.extend_from_slice(&blocks_info);

    // Dummy block data (not read in lazy mode, but used for backing-length validation).
    bytes.extend(std::iter::repeat_n(0u8, 32));

    let total_size = bytes.len() as i64;
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let mut options = BundleLoadOptions::lazy();
    options.max_compressed_block_size = Some(16);
    let err = BundleParser::from_bytes_with_options(bytes, options).unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}

#[test]
fn unityfs_payload_cannot_read_blocks_info_tail() {
    let bytes = unityfs_with_truncated_payload(true);
    let eager = BundleLoadOptions {
        load_assets: false,
        decompress_blocks: true,
        ..BundleLoadOptions::default()
    };

    assert!(BundleParser::from_bytes_with_options(bytes.clone(), eager).is_err());
    assert!(BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy()).is_err());
}

#[test]
fn unityfs_payload_cannot_read_past_declared_bundle_size() {
    let bytes = unityfs_with_truncated_payload(false);
    let eager = BundleLoadOptions {
        load_assets: false,
        decompress_blocks: true,
        ..BundleLoadOptions::default()
    };

    assert!(BundleParser::from_bytes_with_options(bytes.clone(), eager).is_err());
    assert!(BundleParser::from_bytes_with_options(bytes, BundleLoadOptions::lazy()).is_err());
}

#[test]
fn data_block_budget_preflight_is_atomic() {
    let header = BundleHeader::default();
    let blocks = vec![
        CompressionBlock::new(8, 8, 0),
        CompressionBlock::new(8, 8, 0),
    ];
    let bytes = [0x11; 16];
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_decompressed_bytes: 12,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = BundleCompression::decompress_data_blocks_with_budget(
        &header,
        &blocks,
        &mut reader,
        &mut budget,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "decompressed_bytes",
            limit: 12,
            requested: 16,
        })
    ));
    assert_eq!(reader.position(), 0);
    assert_eq!(budget.usage().compressed_bytes, 0);
    assert_eq!(budget.usage().decompressed_bytes, 0);
}

#[test]
fn data_block_reader_range_preflight_is_atomic() {
    let header = BundleHeader::default();
    let blocks = vec![
        CompressionBlock::new(4, 4, 0),
        CompressionBlock::new(4, 4, 0),
    ];
    let bytes = [0x11; 6];
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
    let mut budget = AssetLoadBudget::default();

    let error = BundleCompression::decompress_data_blocks_with_budget(
        &header,
        &blocks,
        &mut reader,
        &mut budget,
    )
    .unwrap_err();

    assert!(matches!(error, BinaryError::NotEnoughData { .. }));
    assert_eq!(reader.position(), 0);
    assert_eq!(budget.usage().compressed_bytes, 0);
    assert_eq!(budget.usage().decompressed_bytes, 0);
}

#[test]
fn data_block_preflight_checks_each_expansion_ratio() {
    let header = BundleHeader::default();
    let blocks = vec![
        CompressionBlock::new(10, 1, 0),
        CompressionBlock::new(1, 9, 0),
    ];
    let bytes = [0x11; 10];
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Big);
    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_expansion_ratio: 2,
        ..AssetLoadLimits::default()
    })
    .unwrap();

    let error = BundleCompression::decompress_data_blocks_with_budget(
        &header,
        &blocks,
        &mut reader,
        &mut budget,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::ExpansionRatioExceeded {
            compressed_bytes: 1,
            decompressed_bytes: 10,
            max_ratio: 2,
        })
    ));
    assert_eq!(reader.position(), 0);
    assert_eq!(budget.usage().compressed_bytes, 0);
    assert_eq!(budget.usage().decompressed_bytes, 0);
}

#[test]
fn compressed_budget_is_checked_before_copying_truncated_blocks_info() {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(b"UnityFS\0");
    bytes.extend_from_slice(&be_u32(7));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_i64(0));
    bytes.extend_from_slice(&be_u32(1024));
    bytes.extend_from_slice(&be_u32(1));
    bytes.extend_from_slice(&be_u32(0));
    let total_size = i64::try_from(bytes.len()).unwrap();
    bytes[size_offset..size_offset + 8].copy_from_slice(&be_i64(total_size));

    let mut budget = AssetLoadBudget::new(AssetLoadLimits {
        max_compressed_bytes: 16,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = BundleParser::from_bytes_with_options_and_budget(
        bytes,
        BundleLoadOptions::lazy(),
        &mut budget,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "compressed_bytes",
            limit: 16,
            requested: 1024,
        })
    ));
    assert_eq!(budget.usage().compressed_bytes, 0);
}

#[test]
fn unityfs_directory_records_are_charged_once_to_the_caller_budget() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/samples/char_118_yuki.ab");
    let bytes = std::fs::read(path).expect("read sample bundle");
    let probe = BundleParser::from_bytes_with_options(bytes.clone(), BundleLoadOptions::lazy())
        .expect("parse sample bundle");
    let expected_entries = u64::try_from(probe.blocks.len() + probe.nodes.len())
        .expect("sample entry count fits in u64");
    assert!(expected_entries > 0);

    let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: expected_entries,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let parsed = BundleParser::from_bytes_with_options_and_budget(
        bytes.clone(),
        BundleLoadOptions::lazy(),
        &mut exact_budget,
    )
    .expect("the exact record budget accepts the bundle");
    assert_eq!(parsed.blocks.len(), probe.blocks.len());
    assert_eq!(parsed.nodes.len(), probe.nodes.len());
    assert_eq!(exact_budget.usage().entries, expected_entries);

    let mut short_budget = AssetLoadBudget::new(AssetLoadLimits {
        max_entries: expected_entries - 1,
        ..AssetLoadLimits::default()
    })
    .unwrap();
    let error = BundleParser::from_bytes_with_options_and_budget(
        bytes,
        BundleLoadOptions::lazy(),
        &mut short_budget,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BinaryError::Budget(BudgetError::Exceeded {
            resource: "entries",
            limit,
            requested,
        }) if limit == expected_entries - 1 && requested == expected_entries
    ));
}
