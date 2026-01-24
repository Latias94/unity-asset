use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};

fn push_cstring(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn make_minimal_serialized_file_v8_le() -> Vec<u8> {
    let version: u32 = 8;
    // UnityPy's file type detection skips AssetsFile checks for files < 128 bytes.
    // Keep this synthetic sample comfortably above that threshold.
    let data_offset: u32 = 128;

    let mut meta: Vec<u8> = Vec::new();
    push_cstring(&mut meta, "2.5.0f5"); // default UnityPy fallback
    meta.extend_from_slice(&0i32.to_le_bytes()); // target_platform
    meta.extend_from_slice(&0i32.to_le_bytes()); // type_count
    meta.extend_from_slice(&0i32.to_le_bytes()); // big_id_enabled (7<=v<14)
    meta.extend_from_slice(&0i32.to_le_bytes()); // object_count
    meta.extend_from_slice(&0i32.to_le_bytes()); // externals_count
    push_cstring(&mut meta, ""); // user_information (v>=5)

    let metadata_size: u32 = (1u32).saturating_add(meta.len() as u32); // +1 endian boolean
    let file_size: u32 = data_offset.saturating_add(metadata_size);

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&metadata_size.to_be_bytes());
    out.extend_from_slice(&file_size.to_be_bytes());
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&data_offset.to_be_bytes());

    // Pad to data_offset (no objects => empty data section).
    if out.len() < data_offset as usize {
        out.resize(data_offset as usize, 0);
    }

    // Metadata section at the end: endian boolean + metadata payload.
    out.push(0u8); // endian: 0 = little
    out.extend_from_slice(&meta);

    out
}

#[test]
fn legacy_v8_serialized_file_can_roundtrip_save_and_reparse() -> anyhow::Result<()> {
    let bytes = make_minimal_serialized_file_v8_le();
    let file = SerializedFileParser::from_bytes(bytes)?;
    assert_eq!(file.header.version, 8);
    assert_eq!(file.header.endian, 0);
    assert_eq!(file.types.len(), 0);
    assert_eq!(file.objects.len(), 0);

    let saved = SerializedFileWriter::save(&file, &SerializedFileEdits::default())?;
    let reparsed = SerializedFileParser::from_bytes(saved)?;
    assert_eq!(reparsed.header.version, 8);
    assert_eq!(reparsed.header.endian, 0);
    assert_eq!(reparsed.types.len(), 0);
    assert_eq!(reparsed.objects.len(), 0);

    Ok(())
}
