use unity_asset_binary::webfile::{WebFile, WebFileCompression};
use unity_asset_core::AssetLoadBudget;
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactBudgetError, ArtifactBuildError,
    ArtifactLimits, LogicalArtifactName, PreparedArtifactKind,
};
use unity_asset_write::webfile::{
    WebFileArtifactMember, WebFileEdits, WebFilePackingPolicy, WebFileWriteError, WebFileWriter,
};

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

    let saved = WebFileWriter::save(&web, &edits, WebFilePackingPolicy::Uncompressed)?;
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

    let saved = WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePackingPolicy::Gzip)?;
    let web2 = WebFile::from_bytes(saved)?;

    assert_eq!(web2.compression, WebFileCompression::Gzip);
    assert_eq!(web2.extract_file("a.txt")?, b"hello");

    Ok(())
}

#[test]
fn webfile_writer_can_emit_brotli_with_fallback_detection() -> anyhow::Result<()> {
    let bytes = build_uncompressed_webfile(vec![("a.txt".to_string(), b"hello".to_vec())]);
    let web = WebFile::from_bytes(bytes)?;

    let saved = WebFileWriter::save(&web, &WebFileEdits::default(), WebFilePackingPolicy::Brotli)?;
    let web2 = WebFile::from_bytes(saved)?;

    assert_eq!(web2.compression, WebFileCompression::Brotli);
    assert_eq!(web2.extract_file("a.txt")?, b"hello");

    Ok(())
}

#[test]
fn webfile_writer_preserves_duplicate_member_occurrences_in_wire_order() -> anyhow::Result<()> {
    let bytes = build_uncompressed_webfile(vec![
        ("shared.assets".to_string(), b"first".to_vec()),
        ("shared.assets".to_string(), b"second".to_vec()),
    ]);
    let web = WebFile::from_bytes(bytes)?;
    assert_eq!(web.files().len(), 2);

    let saved = WebFileWriter::save(
        &web,
        &WebFileEdits::default(),
        WebFilePackingPolicy::Preserve,
    )?;
    let reparsed = WebFile::from_bytes(saved)?;

    assert_eq!(reparsed.files().len(), 2);
    assert_eq!(reparsed.files()[0].name, "shared.assets");
    assert_eq!(reparsed.files()[1].name, "shared.assets");
    assert_eq!(
        reparsed.extract_file_slice_by_info(&reparsed.files()[0])?,
        b"first"
    );
    assert_eq!(
        reparsed.extract_file_slice_by_info(&reparsed.files()[1])?,
        b"second"
    );

    Ok(())
}

#[test]
fn prepared_webfile_records_member_edges_for_every_packing_mode() -> anyhow::Result<()> {
    let empty_bytes = build_uncompressed_webfile(Vec::new());
    let empty_webfile = WebFile::from_bytes(empty_bytes.clone())?;
    let expected_root = WebFile::from_bytes(build_uncompressed_webfile(vec![
        ("shared.web".to_string(), empty_bytes.clone()),
        ("shared.web".to_string(), empty_bytes),
    ]))?;

    for (policy, expected_compression) in [
        (WebFilePackingPolicy::Uncompressed, WebFileCompression::None),
        (WebFilePackingPolicy::Gzip, WebFileCompression::Gzip),
        (WebFilePackingPolicy::Brotli, WebFileCompression::Brotli),
    ] {
        let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default())?;
        let mut inspection_budget = AssetLoadBudget::default();
        let mut declaration =
            ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget)?;
        let output = declaration.declare_output(LogicalArtifactName::new("nested.web")?)?;
        let mut batch = declaration.seal_output_names()?;

        let child = WebFileWriter::prepare(
            &mut batch,
            &empty_webfile,
            &[],
            WebFilePackingPolicy::Uncompressed,
        )?;
        let members = [
            WebFileArtifactMember::new(&batch, "shared.web", child)?,
            WebFileArtifactMember::new(&batch, "shared.web", child)?,
        ];
        let root = WebFileWriter::prepare(&mut batch, &empty_webfile, &members, policy)?;
        batch.bind_output(output, root)?;
        let prepared = batch.finish()?;

        // The child remains reachable through both the segmented and compressed derivation paths.
        assert_eq!(prepared.proof_image_count(), 2);
        let output = prepared.outputs().next().expect("declared output exists");
        assert_eq!(
            output.artifact().format().kind(),
            PreparedArtifactKind::WebFile
        );

        let mut encoded = Vec::new();
        output.artifact().stream_verified_to(&mut encoded)?;
        let expected = WebFileWriter::save(&expected_root, &WebFileEdits::default(), policy)?;
        assert_eq!(encoded, expected);
        let reparsed = WebFile::from_bytes(encoded)?;
        assert_eq!(reparsed.compression, expected_compression);
        assert_eq!(reparsed.files().len(), 2);
        assert_eq!(reparsed.files()[0].name, "shared.web");
        assert_eq!(reparsed.files()[1].name, "shared.web");
        for member in reparsed.files() {
            let nested = reparsed.extract_file_slice_by_info(member)?;
            let parsed_nested = WebFile::from_bytes(nested.to_vec())?;
            assert!(parsed_nested.files().is_empty());
        }
    }

    Ok(())
}

#[test]
fn prepared_brotli_webfile_reports_codec_scratch_budget_failure() -> anyhow::Result<()> {
    let empty_webfile = WebFile::from_bytes(build_uncompressed_webfile(Vec::new()))?;
    let limits = ArtifactLimits::default().with_max_scratch_bytes(64 * 1024);
    let mut artifact_budget = ArtifactBudget::new(limits)?;
    let mut inspection_budget = AssetLoadBudget::default();
    let mut declaration =
        ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut inspection_budget)?;
    declaration.declare_output(LogicalArtifactName::new("limited.web")?)?;
    let mut batch = declaration.seal_output_names()?;

    let error = WebFileWriter::prepare(
        &mut batch,
        &empty_webfile,
        &[],
        WebFilePackingPolicy::Brotli,
    )
    .expect_err("Brotli codec allocations must respect the artifact scratch limit");
    assert!(matches!(
        error,
        WebFileWriteError::Artifact(error)
            if matches!(
                *error,
                ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                    resource: "scratch_bytes",
                    ..
                })
            )
    ));

    drop(batch);
    assert_eq!(artifact_budget.committed_usage(), Default::default());
    Ok(())
}
