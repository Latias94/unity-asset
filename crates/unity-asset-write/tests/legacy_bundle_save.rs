use unity_asset_binary::bundle::BundleParser;
use unity_asset_write::PackerOptions;
use unity_asset_write::bundle::{BundleEdits, BundleWriter};

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

fn build_minimal_unityraw_v3_bundle(file_name: &str, file_bytes: &[u8]) -> Vec<u8> {
    let signature = "UnityRaw";
    let version = 3u32;
    let version_player = "3.5.0f5";
    let version_engine = "3.5.0f5";

    let mut directory_info = Vec::new();
    write_i32_be(&mut directory_info, 1);

    let mut file_info_header_size = 4usize;
    file_info_header_size += file_name.len() + 1;
    file_info_header_size += 8;
    file_info_header_size = (file_info_header_size + 3) & !3;

    write_cstring(&mut directory_info, file_name);
    write_u32_be(&mut directory_info, file_info_header_size as u32);
    write_u32_be(&mut directory_info, file_bytes.len() as u32);
    while directory_info.len() < file_info_header_size {
        directory_info.push(0);
    }

    let mut blob = Vec::new();
    blob.extend_from_slice(&directory_info);
    blob.extend_from_slice(file_bytes);

    let uncompressed_size = blob.len() as u32;
    let compressed_size = uncompressed_size;

    let mut bundle = Vec::new();
    write_cstring(&mut bundle, signature);
    write_u32_be(&mut bundle, version);
    write_cstring(&mut bundle, version_player);
    write_cstring(&mut bundle, version_engine);

    let mut header_size = (bundle.len() as u32).saturating_add(24);
    header_size = header_size.saturating_add(4);
    header_size = header_size.saturating_add(4);
    header_size = (header_size.saturating_add(3)) & !3;

    let complete_file_size = header_size.saturating_add(compressed_size);

    write_u32_be(&mut bundle, complete_file_size);
    write_u32_be(&mut bundle, header_size);
    write_u32_be(&mut bundle, 1);
    write_i32_be(&mut bundle, 1);
    write_u32_be(&mut bundle, compressed_size);
    write_u32_be(&mut bundle, uncompressed_size);
    write_u32_be(&mut bundle, complete_file_size);
    write_u32_be(&mut bundle, file_info_header_size as u32);

    pad_to_multiple(&mut bundle, 4);
    assert_eq!(bundle.len() as u32, header_size);

    bundle.extend_from_slice(&blob);
    assert_eq!(bundle.len() as u32, complete_file_size);
    bundle
}

#[test]
fn can_save_unityraw_bundle_and_reload_with_edits() {
    let input_bytes = build_minimal_unityraw_v3_bundle("test.txt", b"abc");
    let bundle = BundleParser::from_bytes(input_bytes).unwrap();

    let mut edits = BundleEdits::new();
    edits.replace_file_bytes("test.txt", b"abcd".to_vec());

    let saved = BundleWriter::save(&bundle, &edits, PackerOptions::default()).unwrap();
    let reparsed = BundleParser::from_bytes(saved).unwrap();

    assert_eq!(reparsed.header.signature, "UnityRaw");
    assert_eq!(reparsed.nodes.len(), 1);
    let node = &reparsed.nodes[0];
    let out = reparsed.extract_node_data(node).unwrap();
    assert_eq!(out, b"abcd");
}

fn build_minimal_unityweb_v3_bundle(file_name: &str, file_bytes: &[u8]) -> Vec<u8> {
    let signature = "UnityWeb";
    let version = 3u32;
    let version_player = "3.5.0f5";
    let version_engine = "3.5.0f5";

    let mut directory_info = Vec::new();
    write_i32_be(&mut directory_info, 1);

    let mut file_info_header_size = 4usize;
    file_info_header_size += file_name.len() + 1;
    file_info_header_size += 8;
    file_info_header_size = (file_info_header_size + 3) & !3;

    write_cstring(&mut directory_info, file_name);
    write_u32_be(&mut directory_info, file_info_header_size as u32);
    write_u32_be(&mut directory_info, file_bytes.len() as u32);
    while directory_info.len() < file_info_header_size {
        directory_info.push(0);
    }

    let mut blob = Vec::new();
    blob.extend_from_slice(&directory_info);
    blob.extend_from_slice(file_bytes);

    let uncompressed_size = blob.len() as u32;
    let compressed_blob = unity_asset_write::compress_lzma_unity_with_size(&blob).unwrap();
    let compressed_size = compressed_blob.len() as u32;

    let mut bundle = Vec::new();
    write_cstring(&mut bundle, signature);
    write_u32_be(&mut bundle, version);
    write_cstring(&mut bundle, version_player);
    write_cstring(&mut bundle, version_engine);

    let mut header_size = (bundle.len() as u32).saturating_add(24);
    header_size = header_size.saturating_add(4);
    header_size = header_size.saturating_add(4);
    header_size = (header_size.saturating_add(3)) & !3;

    let complete_file_size = header_size.saturating_add(compressed_size);

    write_u32_be(&mut bundle, complete_file_size);
    write_u32_be(&mut bundle, header_size);
    write_u32_be(&mut bundle, 1);
    write_i32_be(&mut bundle, 1);
    write_u32_be(&mut bundle, compressed_size);
    write_u32_be(&mut bundle, uncompressed_size);
    write_u32_be(&mut bundle, complete_file_size);
    write_u32_be(&mut bundle, file_info_header_size as u32);

    pad_to_multiple(&mut bundle, 4);
    assert_eq!(bundle.len() as u32, header_size);

    bundle.extend_from_slice(&compressed_blob);
    assert_eq!(bundle.len() as u32, complete_file_size);
    bundle
}

#[test]
fn can_save_unityweb_bundle_and_reload_with_edits() {
    let input_bytes = build_minimal_unityweb_v3_bundle("test.txt", b"abc");
    let bundle = BundleParser::from_bytes(input_bytes).unwrap();

    let mut edits = BundleEdits::new();
    edits.replace_file_bytes("test.txt", b"abcd".to_vec());

    let saved = BundleWriter::save(&bundle, &edits, PackerOptions::default()).unwrap();
    let reparsed = BundleParser::from_bytes(saved).unwrap();

    assert_eq!(reparsed.header.signature, "UnityWeb");
    assert_eq!(reparsed.nodes.len(), 1);
    let node = &reparsed.nodes[0];
    let out = reparsed.extract_node_data(node).unwrap();
    assert_eq!(out, b"abcd");
}
