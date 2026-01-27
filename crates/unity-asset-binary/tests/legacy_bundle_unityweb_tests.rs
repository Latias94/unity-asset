use std::io::{BufReader, Cursor};

use unity_asset_binary::bundle::BundleParser;

fn write_cstring(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn write_u32_be(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn write_i32_be(buf: &mut Vec<u8>, v: i32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn pad_to_multiple(buf: &mut Vec<u8>, align: usize) {
    if align == 0 {
        return;
    }
    while !buf.len().is_multiple_of(align) {
        buf.push(0);
    }
}

fn compress_lzma_unity_with_size(data: &[u8]) -> Vec<u8> {
    let mut input = Cursor::new(data);
    let mut input = BufReader::new(&mut input);
    let mut out = Vec::new();

    let options = lzma_rs::compress::Options {
        unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(Some(data.len() as u64)),
    };

    lzma_rs::lzma_compress_with_options(&mut input, &mut out, &options).unwrap();
    out
}

#[test]
fn can_parse_minimal_unityweb_v3_bundle_and_extract_file() {
    let signature = "UnityWeb";
    let version = 3u32;
    let version_player = "3.5.0f5";
    let version_engine = "3.5.0f5";

    let file_name = "test.txt";
    let file_bytes = b"abc";

    // Build uncompressed blob: [directory_info][file_content].
    let mut directory_info = Vec::new();
    write_i32_be(&mut directory_info, 1); // nodesCount

    // fileInfoHeaderSize = 4 (nodesCount) + (len(name)+1) + 8 (offset+size), aligned to 4.
    let mut file_info_header_size = 4usize;
    file_info_header_size += file_name.len() + 1;
    file_info_header_size += 8;
    file_info_header_size = (file_info_header_size + 3) & !3;

    write_cstring(&mut directory_info, file_name);
    write_u32_be(&mut directory_info, file_info_header_size as u32); // offset (relative to blob start)
    write_u32_be(&mut directory_info, file_bytes.len() as u32); // size

    while directory_info.len() < file_info_header_size {
        directory_info.push(0);
    }
    assert_eq!(directory_info.len(), file_info_header_size);

    let mut uncompressed_blob = Vec::new();
    uncompressed_blob.extend_from_slice(&directory_info);
    uncompressed_blob.extend_from_slice(file_bytes);

    let compressed_blob = compress_lzma_unity_with_size(&uncompressed_blob);

    let uncompressed_size = uncompressed_blob.len() as u32;
    let compressed_size = compressed_blob.len() as u32;

    // Build full bundle bytes.
    let mut bundle = Vec::new();
    write_cstring(&mut bundle, signature);
    write_u32_be(&mut bundle, version);
    write_cstring(&mut bundle, version_player);
    write_cstring(&mut bundle, version_engine);

    // UnityPy save_web_raw header size formula (assuming levelCount=1).
    let mut header_size = (bundle.len() as u32).saturating_add(24);
    header_size = header_size.saturating_add(4); // version >= 2: completeFileSize
    header_size = header_size.saturating_add(4); // version >= 3: fileInfoHeaderSize
    header_size = (header_size.saturating_add(3)) & !3; // align to 4

    let complete_file_size = header_size.saturating_add(compressed_size);

    // Legacy header fields (UnityPy `read_web_raw` / `save_web_raw` ordering).
    write_u32_be(&mut bundle, complete_file_size); // minimumStreamedBytes
    write_u32_be(&mut bundle, header_size); // headerSize
    write_u32_be(&mut bundle, 1); // numberOfLevelsToDownloadBeforeStreaming
    write_i32_be(&mut bundle, 1); // levelCount
    write_u32_be(&mut bundle, compressed_size);
    write_u32_be(&mut bundle, uncompressed_size);
    write_u32_be(&mut bundle, complete_file_size); // version >= 2
    write_u32_be(&mut bundle, file_info_header_size as u32); // version >= 3

    pad_to_multiple(&mut bundle, 4);
    assert_eq!(bundle.len() as u32, header_size);

    bundle.extend_from_slice(&compressed_blob);
    assert_eq!(bundle.len() as u32, complete_file_size);

    let parsed = BundleParser::from_bytes(bundle).unwrap();
    assert_eq!(parsed.header.signature, "UnityWeb");
    assert_eq!(parsed.header.version, 3);
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.nodes.len(), 1);

    let node = &parsed.nodes[0];
    assert_eq!(node.name, "test.txt");
    assert_eq!(node.offset, file_info_header_size as u64);
    assert_eq!(node.size, 3);

    let out = parsed.extract_node_data(node).unwrap();
    assert_eq!(out, file_bytes);
}
