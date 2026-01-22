use unity_asset_binary::webfile::{WebFile, WebFileCompression};
use unity_asset_write::webfile::{WebFileEdits, WebFilePacker, WebFileWriter};

fn build_uncompressed_webfile(entries: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let signature = b"UnityWebData1.0\0";

    let entry_table_len: usize = entries
        .iter()
        .map(|(name, _)| 12usize.saturating_add(name.len()))
        .sum();
    let header_len: usize = signature
        .len()
        .saturating_add(std::mem::size_of::<i32>())
        .saturating_add(entry_table_len);

    let head_length_i32: i32 = header_len
        .try_into()
        .expect("header_len fits i32 for test webfile");

    let mut out: Vec<u8> = Vec::with_capacity(
        header_len.saturating_add(entries.iter().map(|(_, b)| b.len()).sum::<usize>()),
    );
    out.extend_from_slice(signature);
    out.extend_from_slice(&head_length_i32.to_le_bytes());

    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
    let mut cursor = header_len;

    for (name, bytes) in entries {
        let offset_i32: i32 = cursor.try_into().expect("offset fits i32");
        let length_i32: i32 = bytes.len().try_into().expect("length fits i32");
        let name_len_i32: i32 = name.len().try_into().expect("name_len fits i32");

        out.extend_from_slice(&offset_i32.to_le_bytes());
        out.extend_from_slice(&length_i32.to_le_bytes());
        out.extend_from_slice(&name_len_i32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());

        cursor = cursor.saturating_add(bytes.len());
        payloads.push(bytes);
    }

    for payload in payloads {
        out.extend_from_slice(&payload);
    }

    out
}

#[test]
fn webfile_writer_roundtrips_with_replacements() -> anyhow::Result<()> {
    let original_a = b"hello".to_vec();
    let original_b = b"world".to_vec();

    let bytes = build_uncompressed_webfile(vec![
        ("a.txt".to_string(), original_a.clone()),
        ("b.bin".to_string(), original_b.clone()),
    ]);

    let web = WebFile::from_bytes(bytes)?;

    let mut edits = WebFileEdits::default();
    edits.replace_file_bytes("a.txt", b"HELLO2".to_vec());

    let saved = WebFileWriter::save(&web, &edits, WebFilePacker::None, None)?;
    let web2 = WebFile::from_bytes(saved)?;

    assert_eq!(web2.compression, WebFileCompression::None);
    assert_eq!(web2.extract_file("a.txt")?, b"HELLO2");
    assert_eq!(web2.extract_file("b.bin")?, original_b);

    Ok(())
}

#[test]
fn webfile_writer_can_emit_gzip() -> anyhow::Result<()> {
    let bytes = build_uncompressed_webfile(vec![("a.txt".to_string(), b"hello".to_vec())]);
    let web = WebFile::from_bytes(bytes)?;

    let saved = WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePacker::Gzip, None)?;
    let web2 = WebFile::from_bytes(saved)?;

    assert_eq!(web2.compression, WebFileCompression::Gzip);
    assert_eq!(web2.extract_file("a.txt")?, b"hello");

    Ok(())
}

#[test]
fn webfile_writer_can_emit_brotli_with_fallback_detection() -> anyhow::Result<()> {
    let bytes = build_uncompressed_webfile(vec![("a.txt".to_string(), b"hello".to_vec())]);
    let web = WebFile::from_bytes(bytes)?;

    let saved = WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePacker::Brotli, None)?;
    let web2 = WebFile::from_bytes(saved)?;

    assert_eq!(web2.compression, WebFileCompression::Brotli);
    assert_eq!(web2.extract_file("a.txt")?, b"hello");

    Ok(())
}
