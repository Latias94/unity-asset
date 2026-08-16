use std::io;

use unity_asset_binary::BinaryError;
use unity_asset_binary::asset::SerializedFile;
use unity_asset_core::UnityAssetError;

#[cfg(test)]
use crate::artifact::ArtifactPayload;
use crate::artifact::{ArtifactBatch, ArtifactBuildError, ArtifactHandle};
use crate::serialized_file::edit::SerializedFileEdits;
#[cfg(test)]
use crate::serialized_file::plan::validate_source_binding;
use crate::serialized_file::plan::{
    SerializedFilePlan, SerializedFileSegment, SerializedFileSource,
};
use crate::serialized_file::sink::IoSink;
use crate::serialized_file::writer::SerializedFileWriter;

impl SerializedFileWriter {
    /// Builds one exact, independently inspected SerializedFile proof image.
    ///
    /// Unmodified unloaded object bytes must come from source; explicit edits and preloaded
    /// object payloads are copied once into budgeted generated chunks.
    pub fn prepare(
        batch: &mut ArtifactBatch<'_, '_>,
        file: &SerializedFile,
        edits: &SerializedFileEdits,
        source: Option<SerializedFileSource<'_>>,
    ) -> std::result::Result<ArtifactHandle, ArtifactBuildError> {
        let plan = SerializedFilePlan::build_for_artifact(batch, file, edits, source)
            .map_err(artifact_error)?;
        let declared_len = plan.declared_len();

        batch.prepare_serialized_file(declared_len, |encoder| {
            plan.visit_segments(|segment| {
                match segment {
                    SerializedFileSegment::Generated(region) => {
                        let mut generated = encoder.generated_chunk_writer().map_err(|error| {
                            artifact_operation(
                                "Failed to begin a SerializedFile generated chunk",
                                error,
                            )
                        })?;
                        {
                            let mut sink = IoSink::new(&mut generated);
                            plan.encode_generated(region, &mut sink)?;
                        }
                        let payload =
                            encoder.finish_generated_chunk(generated).map_err(|error| {
                                artifact_operation(
                                    "Failed to finish a SerializedFile generated chunk",
                                    error,
                                )
                            })?;
                        encoder.push_payload_full(&payload).map_err(|error| {
                            artifact_operation(
                                "Failed to append a SerializedFile generated chunk",
                                error,
                            )
                        })?;
                    }
                    SerializedFileSegment::BorrowedSourceRange { payload, range } => {
                        encoder
                            .push_payload_range(payload, range)
                            .map_err(|error| {
                                artifact_operation(
                                    "Failed to append a SerializedFile source range",
                                    error,
                                )
                            })?;
                    }
                }
                Ok(())
            })
            .map_err(artifact_error)
        })
    }
}

fn artifact_operation(context: &'static str, error: ArtifactBuildError) -> UnityAssetError {
    UnityAssetError::with_source(context, error)
}

fn artifact_error(error: UnityAssetError) -> ArtifactBuildError {
    let message = error.to_string();
    match error {
        UnityAssetError::Io(error) => ArtifactBuildError::from(error),
        UnityAssetError::WithSource { source, .. } => {
            let source = match source.downcast::<ArtifactBuildError>() {
                Ok(error) => return *error,
                Err(source) => source,
            };
            if let Ok(error) = source.downcast::<io::Error>() {
                return ArtifactBuildError::from(*error);
            }
            ArtifactBuildError::Binary(BinaryError::invalid_data(message))
        }
        _ => ArtifactBuildError::Binary(BinaryError::invalid_data(message)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use unity_asset_binary::asset::SerializedFileParser;
    use unity_asset_binary::shared_bytes::SharedBytes;
    use unity_asset_core::{
        AssetLoadBudget, AssetLoadLimits, DigestV1, SourceId, SourceKind, VerifiedSourceImage,
        WorkspaceId,
    };

    use super::*;
    use crate::artifact::{
        ArtifactBatchDeclaration, ArtifactBudget, ArtifactBudgetError, ArtifactLimits,
        LogicalArtifactName, PreparedArtifactFormat,
    };
    use crate::object::{
        SerializedObjectEncoder, UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
    };

    const V22_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v22.assets.bin");
    const V2_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v2.assets.bin");
    const V8_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v8.assets.bin");
    const V15_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/serialized_file_wire/v15.assets.bin");

    fn source_payload(bytes: &[u8]) -> ArtifactPayload {
        source_payload_with_kind(bytes, SourceKind::SerializedFile)
    }

    fn source_payload_with_kind(bytes: &[u8], kind: SourceKind) -> ArtifactPayload {
        let source = SourceId::new(WorkspaceId::from_u128(41).unwrap(), kind, 1).unwrap();
        let image = VerifiedSourceImage::verify(kind, Arc::<[u8]>::from(bytes));
        ArtifactPayload::source_backed(source, image).unwrap()
    }

    fn source_payload_from_backing(bytes: Arc<[u8]>) -> ArtifactPayload {
        let source = SourceId::new(
            WorkspaceId::from_u128(41).unwrap(),
            SourceKind::SerializedFile,
            1,
        )
        .unwrap();
        let image = VerifiedSourceImage::verify(SourceKind::SerializedFile, bytes);
        ArtifactPayload::source_backed(source, image).unwrap()
    }

    fn batch<'a, 'b>(
        budget: &'a mut ArtifactBudget,
        load: &'b mut AssetLoadBudget,
    ) -> (crate::artifact::OutputSlot, ArtifactBatch<'a, 'b>) {
        let mut declaration = ArtifactBatchDeclaration::begin(budget, load).unwrap();
        let output = declaration
            .declare_output(LogicalArtifactName::new("main.assets").unwrap())
            .unwrap();
        (output, declaration.seal_output_names().unwrap())
    }

    #[test]
    fn prepared_serialized_file_reuses_unmodified_source_object_ranges() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::whole(&source).unwrap()),
        )
        .unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();
        let output = set.outputs().next().unwrap();

        assert!(matches!(
            output.artifact().format(),
            PreparedArtifactFormat::SerializedFile(proof) if proof.version() == 22
        ));
        assert_eq!(set.source_dependencies().len(), 1);
        assert_eq!(
            set.source_dependencies()[0].referenced_bytes(),
            u64::from(file.objects()[0].byte_size())
        );
        assert_eq!(output.artifact().build_counters().source_ranges(), 1);
        assert_eq!(
            output.artifact().footprint().referenced_source_bytes(),
            u64::from(file.objects()[0].byte_size())
        );
        let mut encoded = Vec::new();
        output.artifact().stream_verified_to(&mut encoded).unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        assert_eq!(reparsed.objects().len(), file.objects().len());
        assert_eq!(
            reparsed.object_bytes(&reparsed.objects()[0]).unwrap(),
            file.object_bytes(&file.objects()[0]).unwrap()
        );
    }

    #[test]
    fn prepared_serialized_file_reuses_a_verified_enclosing_source_range() {
        const PREFIX: &[u8] = b"bundle-prefix";
        const SUFFIX: &[u8] = b"bundle-suffix";

        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut enclosing = Vec::with_capacity(PREFIX.len() + V22_FIXTURE.len() + SUFFIX.len());
        enclosing.extend_from_slice(PREFIX);
        enclosing.extend_from_slice(V22_FIXTURE);
        enclosing.extend_from_slice(SUFFIX);
        let source = source_payload_with_kind(&enclosing, SourceKind::AssetBundle);
        let file_range = PREFIX.len()..PREFIX.len() + V22_FIXTURE.len();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::new(&source, file_range)),
        )
        .unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();

        assert_eq!(set.source_dependencies().len(), 1);
        assert_eq!(
            set.source_dependencies()[0].source().kind(),
            SourceKind::AssetBundle
        );
        let mut encoded = Vec::new();
        set.outputs()
            .next()
            .unwrap()
            .artifact()
            .stream_verified_to(&mut encoded)
            .unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        assert_eq!(
            reparsed.object_bytes(&reparsed.objects()[0]).unwrap(),
            file.object_bytes(&file.objects()[0]).unwrap()
        );
    }

    #[test]
    fn source_binding_uses_backing_identity_without_a_comparison_budget() {
        let backing: Arc<[u8]> = Arc::from(V22_FIXTURE);
        let file = SerializedFileParser::from_shared_range(
            SharedBytes::from_arc(Arc::clone(&backing)),
            0..backing.len(),
        )
        .unwrap();
        let source = source_payload_from_backing(backing);
        let source = SerializedFileSource::whole(&source).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let (_, mut batch) = batch(&mut budget, &mut load);

        validate_source_binding(&mut batch, &file, Some(&source)).unwrap();
    }

    #[test]
    fn detached_source_binding_charges_the_comparison_before_scanning() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let source = SerializedFileSource::whole(&source).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: u64::try_from(V22_FIXTURE.len() - 1).unwrap(),
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error = validate_source_binding(&mut batch, &file, Some(&source)).unwrap_err();
        assert!(matches!(
            artifact_error(error),
            ArtifactBuildError::LoadBudget(unity_asset_core::BudgetError::Exceeded {
                resource: "bytes",
                ..
            })
        ));
    }

    #[test]
    fn prepared_serialized_file_round_trips_legacy_and_modern_layouts() {
        for bytes in [V2_FIXTURE, V8_FIXTURE, V15_FIXTURE, V22_FIXTURE] {
            let file = SerializedFileParser::from_bytes(bytes.to_vec()).unwrap();
            let source = source_payload(bytes);
            let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
            let mut load = AssetLoadBudget::default();
            let (output, mut batch) = batch(&mut budget, &mut load);

            let artifact = SerializedFileWriter::prepare(
                &mut batch,
                &file,
                &SerializedFileEdits::default(),
                Some(SerializedFileSource::whole(&source).unwrap()),
            )
            .unwrap_or_else(|error| {
                panic!("version {} prepare failed: {error}", file.header.version)
            });
            batch.bind_output(output, artifact).unwrap();
            let set = batch.finish().unwrap();
            let output = set.outputs().next().unwrap();
            let mut encoded = Vec::new();
            output.artifact().stream_verified_to(&mut encoded).unwrap();
            let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
            assert_eq!(reparsed.header.version, file.header.version);
            assert_eq!(reparsed.objects().len(), file.objects().len());
        }
    }

    #[test]
    fn prepared_serialized_file_uses_generated_bytes_for_edits() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let path_id = file.objects()[0].path_id();
        let replacement = b"replacement".to_vec();
        let original = file
            .find_object_handle(path_id)
            .unwrap()
            .raw_data()
            .unwrap();
        let encoded = SerializedObjectEncoder::new(&file, path_id)
            .unwrap()
            .encode_unsafe_raw(
                UnsafeRawObjectReplacement::new(
                    DigestV1::hash_bytes(original),
                    replacement.clone(),
                    UnsafeRawObjectAcknowledgement::WireInvariantsAreCallersResponsibilityV1,
                ),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let mut edits = SerializedFileEdits::default();
        edits
            .try_insert_encoded_object(encoded, &mut AssetLoadBudget::default())
            .unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (output, mut batch) = batch(&mut budget, &mut load);

        let artifact = SerializedFileWriter::prepare(&mut batch, &file, &edits, None).unwrap();
        batch.bind_output(output, artifact).unwrap();
        let set = batch.finish().unwrap();
        assert!(set.source_dependencies().is_empty());
        let output = set.outputs().next().unwrap();
        let mut encoded = Vec::new();
        output.artifact().stream_verified_to(&mut encoded).unwrap();
        let reparsed = SerializedFileParser::from_bytes(encoded).unwrap();
        let object = reparsed.find_object(path_id).unwrap();
        assert_eq!(reparsed.object_bytes(object).unwrap(), replacement);
    }

    #[test]
    fn unloaded_object_requires_a_matching_verified_source() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
        let mut load = AssetLoadBudget::default();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error =
            SerializedFileWriter::prepare(&mut batch, &file, &SerializedFileEdits::default(), None)
                .expect_err("an unloaded object cannot lose its provenance");

        assert!(
            error
                .to_string()
                .contains("no verified SerializedFile source")
        );
    }

    #[test]
    fn generated_byte_budget_rejects_one_byte_below_the_exact_footprint() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let exact_generated_bytes = {
            let mut budget = ArtifactBudget::new(ArtifactLimits::default()).unwrap();
            let mut load = AssetLoadBudget::default();
            let (output, mut batch) = batch(&mut budget, &mut load);
            let artifact = SerializedFileWriter::prepare(
                &mut batch,
                &file,
                &SerializedFileEdits::default(),
                Some(SerializedFileSource::whole(&source).unwrap()),
            )
            .unwrap();
            batch.bind_output(output, artifact).unwrap();
            let set = batch.finish().unwrap();
            set.artifact(artifact)
                .unwrap()
                .footprint()
                .generated_bytes()
        };
        assert!(exact_generated_bytes > 1);

        let limits = ArtifactLimits::default().with_max_generated_bytes(exact_generated_bytes - 1);
        let mut budget = ArtifactBudget::new(limits).unwrap();
        let mut load = AssetLoadBudget::default();
        let (_, mut batch) = batch(&mut budget, &mut load);
        let error = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::whole(&source).unwrap()),
        )
        .unwrap_err();

        assert!(
            matches!(
                &error,
                ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                    resource: "generated_bytes",
                    ..
                })
            ),
            "unexpected one-short generated-byte error: {error:?}"
        );
    }

    #[test]
    fn generated_chunk_budget_failure_is_typed_and_poisons_the_batch() {
        let file = SerializedFileParser::from_bytes(V22_FIXTURE.to_vec()).unwrap();
        let source = source_payload(V22_FIXTURE);
        let limits = ArtifactLimits::default().with_max_generated_chunk_bytes(8);
        let mut budget = ArtifactBudget::new(limits).unwrap();
        let mut load = AssetLoadBudget::default();
        let (_, mut batch) = batch(&mut budget, &mut load);

        let error = SerializedFileWriter::prepare(
            &mut batch,
            &file,
            &SerializedFileEdits::default(),
            Some(SerializedFileSource::whole(&source).unwrap()),
        )
        .unwrap_err();
        assert_eq!(
            error.failure_phase(),
            crate::artifact::ArtifactBuildFailurePhase::Encoding
        );
        assert!(matches!(
            error,
            ArtifactBuildError::Budget(ArtifactBudgetError::Exceeded {
                resource: "generated_chunk_bytes",
                ..
            })
        ));
        assert!(matches!(
            SerializedFileWriter::prepare(
                &mut batch,
                &file,
                &SerializedFileEdits::default(),
                Some(SerializedFileSource::whole(&source).unwrap()),
            ),
            Err(ArtifactBuildError::PoisonedBatch)
        ));
    }
}
