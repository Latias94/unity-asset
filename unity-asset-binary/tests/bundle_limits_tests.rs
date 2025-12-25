use unity_asset_binary::bundle::compression::BundleCompression;
use unity_asset_binary::bundle::header::BundleHeader;
use unity_asset_binary::bundle::parser::BundleParser;
use unity_asset_binary::bundle::types::BundleLoadOptions;
use unity_asset_binary::bundle::types::{AssetBundle, BundleFileInfo, DirectoryNode};
use unity_asset_binary::compression::CompressionBlock;
use unity_asset_binary::error::BinaryError;
use unity_asset_binary::reader::{BinaryReader, ByteOrder};

fn be_u32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i32(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i64(v: i64) -> [u8; 8] {
    v.to_be_bytes()
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
    let mut reader = BinaryReader::new(&[], ByteOrder::Big);

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
    bytes.extend(std::iter::repeat(0u8).take(pad));
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
    bytes.extend(std::iter::repeat(0u8).take(pad));
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
    bytes.extend_from_slice(&be_u32(6));
    bytes.extend_from_slice(b"2020.3.0f1\0");
    bytes.extend_from_slice(b"2020.3.0f1\0");
    let size_offset = bytes.len();
    bytes.extend_from_slice(&be_u32(0)); // placeholder for size

    // Legacy directory header section
    bytes.extend_from_slice(&be_u32(compressed_size));
    bytes.extend_from_slice(&be_u32(uncompressed_size));
    bytes.extend_from_slice(&be_u32(0)); // skip bytes for version >= 2

    let total_size = bytes.len() as u32;
    bytes[size_offset..size_offset + 4].copy_from_slice(&be_u32(total_size));

    let mut options = BundleLoadOptions::default();
    options.max_legacy_directory_compressed_size = Some(16);
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

    let mut options = BundleLoadOptions::default();
    options.max_compressed_blocks_info_size = Some(16);
    let err = BundleParser::from_bytes_with_options(bytes, options).unwrap_err();
    assert!(matches!(err, BinaryError::ResourceLimitExceeded(_)));
}
