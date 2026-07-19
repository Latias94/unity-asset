use std::ops::Range;
use std::sync::{Arc, OnceLock};

use unity_asset_binary::asset::{ObjectMetadata, ObjectTypeReference, SerializedFileParser};
use unity_asset_binary::error::{BinaryError, BinaryObjectIdentityError};
use unity_asset_core::{
    AssetLoadBudget, SourceId, SourceKind, UnityAssetError, VerifiedSourceImage, WorkspaceId,
};
use unity_asset_write::artifact::{
    ArtifactBatchDeclaration, ArtifactBudget, ArtifactLimits, ArtifactPayload, LogicalArtifactName,
};
use unity_asset_write::serialized_file::{
    SerializedFileEdits, SerializedFileSource, SerializedFileWriter,
};

struct WireCase {
    version: u32,
    bytes: &'static [u8],
    endian: u8,
    path_id: i64,
    big_id_enabled: bool,
}

const CASES: &[WireCase] = &[
    WireCase {
        version: 2,
        bytes: include_bytes!("fixtures/serialized_file_wire/v2.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 3,
        bytes: include_bytes!("fixtures/serialized_file_wire/v3.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 5,
        bytes: include_bytes!("fixtures/serialized_file_wire/v5.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 6,
        bytes: include_bytes!("fixtures/serialized_file_wire/v6.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 7,
        bytes: include_bytes!("fixtures/serialized_file_wire/v7.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 8,
        bytes: include_bytes!("fixtures/serialized_file_wire/v8.assets.bin"),
        endian: 1,
        path_id: 0x0000_0001_0000_002A,
        big_id_enabled: true,
    },
    WireCase {
        version: 9,
        bytes: include_bytes!("fixtures/serialized_file_wire/v9.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 10,
        bytes: include_bytes!("fixtures/serialized_file_wire/v10.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 11,
        bytes: include_bytes!("fixtures/serialized_file_wire/v11.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 12,
        bytes: include_bytes!("fixtures/serialized_file_wire/v12.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 13,
        bytes: include_bytes!("fixtures/serialized_file_wire/v13.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 14,
        bytes: include_bytes!("fixtures/serialized_file_wire/v14.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 15,
        bytes: include_bytes!("fixtures/serialized_file_wire/v15.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 16,
        bytes: include_bytes!("fixtures/serialized_file_wire/v16.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 17,
        bytes: include_bytes!("fixtures/serialized_file_wire/v17.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 18,
        bytes: include_bytes!("fixtures/serialized_file_wire/v18.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 19,
        bytes: include_bytes!("fixtures/serialized_file_wire/v19.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 20,
        bytes: include_bytes!("fixtures/serialized_file_wire/v20.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 21,
        bytes: include_bytes!("fixtures/serialized_file_wire/v21.assets.bin"),
        endian: 0,
        path_id: 42,
        big_id_enabled: false,
    },
    WireCase {
        version: 22,
        bytes: include_bytes!("fixtures/serialized_file_wire/v22.assets.bin"),
        endian: 1,
        path_id: 42,
        big_id_enabled: false,
    },
];

fn assert_wire_case(
    case: &WireCase,
    file: &unity_asset_binary::asset::SerializedFile,
    expected_payload: &[u8],
) {
    assert_eq!(file.header.version, case.version);
    assert_eq!(file.header.endian, case.endian, "v{}", case.version);
    assert_eq!(
        file.header.reserved,
        if case.version >= 9 {
            [0xA1, 0xB2, 0xC3]
        } else {
            [0; 3]
        }
    );
    assert_eq!(
        file.header.unknown,
        if case.version >= 22 {
            0x0112_2334_4556_6778
        } else {
            0
        }
    );
    assert!(
        file.type_tree_enabled(),
        "v{} TypeTree must be enabled",
        case.version
    );
    assert_eq!(
        file.uses_big_ids(),
        case.big_id_enabled,
        "v{}",
        case.version
    );
    assert_eq!(
        file.legacy_big_id(),
        (7..14)
            .contains(&case.version)
            .then_some(if case.version == 8 { 0x1234_5678 } else { 0 }),
        "v{}",
        case.version
    );

    assert_eq!(file.types().len(), 1, "v{}", case.version);
    let serialized_type = &file.types()[0];
    assert_eq!(serialized_type.class_id, 28, "v{}", case.version);
    assert_eq!(
        serialized_type.type_tree.nodes.len(),
        1,
        "v{}",
        case.version
    );
    assert_eq!(serialized_type.type_tree.nodes[0].type_name, "int");
    assert_eq!(serialized_type.type_tree.nodes[0].name, "m_Value");
    let root = &serialized_type.type_tree.nodes[0];
    assert_eq!(root.byte_size, 4);
    assert_eq!(root.type_flags, 3);
    assert_eq!(root.version, 1);
    assert_eq!(
        root.variable_count,
        if case.version == 2 { 0x1122_3344 } else { 0 }
    );
    assert_eq!(root.index, if case.version == 3 { 0 } else { 7 });
    assert_eq!(root.meta_flags, if case.version == 3 { 0 } else { 0x4000 });
    if case.version >= 13 {
        assert_eq!(
            serialized_type.old_type_hash,
            std::array::from_fn(|index| 0x20 + index as u8)
        );
    }
    if case.version >= 21 {
        assert_eq!(serialized_type.type_dependencies, [114, -7]);
    }

    assert_eq!(file.objects().len(), 1, "v{}", case.version);
    let object = &file.objects()[0];
    assert_eq!(object.path_id(), case.path_id, "v{}", case.version);
    assert_eq!(object.class_id(), 28, "v{}", case.version);
    assert_eq!(
        object.serialized_type_index(),
        (case.version != 8).then_some(0),
        "v{}",
        case.version
    );
    let expected_reference = match case.version {
        2..=15 => ObjectTypeReference::Legacy {
            raw_type_id: if case.version == 8 { 0x1357_2468 } else { 28 },
            class_id_bits: 28,
        },
        16 => ObjectTypeReference::TransitionalV16 { raw: 0 },
        17..=22 => ObjectTypeReference::SerializedTypeIndex { index: 0 },
        _ => unreachable!("wire case versions are fixed"),
    };
    assert_eq!(
        object.type_reference(),
        expected_reference,
        "v{}",
        case.version
    );
    let expected_metadata = match case.version {
        2..=10 => ObjectMetadata::Destroyed { value: 0x1234 },
        11..=14 => ObjectMetadata::ScriptTypeIndex { index: -3 },
        15..=16 => ObjectMetadata::ScriptTypeIndexAndStripped {
            index: -3,
            stripped: 1,
        },
        17..=22 => ObjectMetadata::None,
        _ => unreachable!("wire case versions are fixed"),
    };
    assert_eq!(object.metadata(), expected_metadata, "v{}", case.version);
    assert_eq!(
        file.object_bytes(object).expect("fixture object bytes"),
        expected_payload
    );

    assert_eq!(file.externals.len(), 1, "v{}", case.version);
    assert_eq!(file.externals[0].path, "archive:/fixture-dependency.assets");
    if case.version >= 5 {
        assert_eq!(
            file.externals[0].guid,
            std::array::from_fn(|index| index as u8 + 1)
        );
        assert_eq!(file.externals[0].type_, 3);
        assert_eq!(file.user_information, "fixture-user");
    }
    if case.version >= 6 {
        assert_eq!(file.externals[0].temp_empty, "fixture-empty");
    }

    assert_eq!(file.ref_types().len(), usize::from(case.version >= 20));
    if case.version >= 20 {
        let ref_type = &file.ref_types()[0];
        assert_eq!(ref_type.class_id, 114);
        assert_eq!(ref_type.script_type_index, 2);
        assert_eq!(
            ref_type.script_id,
            std::array::from_fn(|index| 0x40 + index as u8)
        );
        assert_eq!(
            ref_type.old_type_hash,
            std::array::from_fn(|index| 0x60 + index as u8)
        );
        assert_eq!(
            ref_type.type_tree.nodes[0].ref_type_hash,
            0x0102_0304_0506_0708
        );
        if case.version >= 21 {
            assert_eq!(ref_type.class_name, "FixtureRef");
            assert_eq!(ref_type.namespace, "Fixture.Tests");
            assert_eq!(ref_type.assembly_name, "Fixture.Assembly");
        }
    }
}

fn parse_case(case: &WireCase, bytes: Vec<u8>) -> unity_asset_binary::asset::SerializedFile {
    SerializedFileParser::from_bytes(bytes)
        .unwrap_or_else(|error| panic!("failed to parse v{} fixture: {error}", case.version))
}

fn prepare_serialized_file(
    file: &unity_asset_binary::asset::SerializedFile,
    edits: &SerializedFileEdits,
) -> anyhow::Result<Vec<u8>> {
    let source_id = SourceId::new(
        WorkspaceId::from_u128(0x5749_5245).unwrap(),
        SourceKind::SerializedFile,
        1,
    )
    .unwrap();
    let image =
        VerifiedSourceImage::verify(SourceKind::SerializedFile, Arc::<[u8]>::from(file.data()));
    let payload = ArtifactPayload::source_backed(source_id, image)?;
    let source = SerializedFileSource::whole(&payload)?;
    let mut artifact_budget = ArtifactBudget::new(ArtifactLimits::default())?;
    let mut load_budget = AssetLoadBudget::default();
    let mut declaration = ArtifactBatchDeclaration::begin(&mut artifact_budget, &mut load_budget)?;
    let output = declaration.declare_output(LogicalArtifactName::new("main.assets")?)?;
    let mut batch = declaration.seal_output_names()?;
    let artifact = SerializedFileWriter::prepare(&mut batch, file, edits, Some(source))?;
    batch.bind_output(output, artifact)?;
    let artifacts = batch.finish()?;
    let mut bytes = Vec::new();
    artifacts
        .outputs()
        .next()
        .expect("one declared output")
        .artifact()
        .stream_verified_to(&mut bytes)?;
    Ok(bytes)
}

fn wire_case(version: u32) -> &'static WireCase {
    CASES
        .iter()
        .find(|case| case.version == version)
        .expect("wire version exists")
}

fn manifest() -> &'static serde_json::Value {
    static MANIFEST: OnceLock<serde_json::Value> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        serde_json::from_str(include_str!("fixtures/serialized_file_wire/manifest.json"))
            .expect("valid wire fixture manifest")
    })
}

fn manifest_case(version: u32) -> &'static serde_json::Value {
    manifest()["cases"]
        .as_array()
        .expect("manifest cases")
        .iter()
        .find(|case| case["expected"]["version"].as_u64() == Some(u64::from(version)))
        .expect("manifest version exists")
}

fn field_range(version: u32, field: &str) -> Range<usize> {
    let field = &manifest_case(version)["expected"]["ranges"]["metadata_fields"][field];
    let start = usize::try_from(field["offset"].as_u64().expect("field offset")).unwrap();
    let size = usize::try_from(field["size"].as_u64().expect("field size")).unwrap();
    start..start + size
}

fn write_i32(bytes: &mut [u8], version: u32, field: &str, value: i32) {
    let range = field_range(version, field);
    assert_eq!(range.len(), 4);
    let encoded = if wire_case(version).endian == 0 {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[range].copy_from_slice(&encoded);
}

fn write_u32(bytes: &mut [u8], version: u32, field: &str, value: u32) {
    let range = field_range(version, field);
    assert_eq!(range.len(), 4);
    let encoded = if wire_case(version).endian == 0 {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[range].copy_from_slice(&encoded);
}

fn write_i64(bytes: &mut [u8], version: u32, field: &str, value: i64) {
    let range = field_range(version, field);
    assert_eq!(range.len(), 8);
    let encoded = if wire_case(version).endian == 0 {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[range].copy_from_slice(&encoded);
}

fn parse_mutation(version: u32, mutate: impl FnOnce(&mut Vec<u8>)) -> BinaryError {
    let case = wire_case(version);
    let mut bytes = case.bytes.to_vec();
    mutate(&mut bytes);
    SerializedFileParser::from_bytes(bytes).expect_err("hostile mutation must be rejected")
}

fn decode_hex_fixture(contents: &str) -> Vec<u8> {
    let digits: Vec<u8> = contents
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert_eq!(digits.len() % 2, 0, "hex fixture must contain byte pairs");
    digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).expect("valid hex digit");
            let low = (pair[1] as char).to_digit(16).expect("valid hex digit");
            u8::try_from((high << 4) | low).expect("hex pair fits u8")
        })
        .collect()
}

#[test]
fn parses_independent_wire_goldens_across_format_transitions() {
    for case in CASES {
        let file = parse_case(case, case.bytes.to_vec());
        let expected_payload = [case.version as u8, 0xAA, 0xBB, 0xCC];
        assert_wire_case(case, &file, &expected_payload);
    }
}

#[test]
fn rewrites_wire_goldens_without_reconstructing_object_metadata() {
    for case in CASES {
        let file = parse_case(case, case.bytes.to_vec());
        let original_payload = [case.version as u8, 0xAA, 0xBB, 0xCC];

        let no_op = prepare_serialized_file(&file, &SerializedFileEdits::default())
            .unwrap_or_else(|error| panic!("failed to rewrite v{} fixture: {error}", case.version));
        let reparsed = parse_case(case, no_op);
        assert_wire_case(case, &reparsed, &original_payload);

        let edited_payload = [0xD0, case.version as u8, 0xAD, 0xBE, 0xEF];
        let mut edits = SerializedFileEdits::default();
        edits.set_object_bytes(case.path_id, edited_payload.to_vec());
        let edited = prepare_serialized_file(&file, &edits).unwrap_or_else(|error| {
            panic!(
                "failed to edit and rewrite v{} fixture: {error}",
                case.version
            )
        });
        let reparsed = parse_case(case, edited);
        assert_wire_case(case, &reparsed, &edited_payload);
    }
}

#[test]
fn v16_type_index_collision_follows_the_unitypy_wire_oracle() {
    let bytes = decode_hex_fixture(include_str!(
        "fixtures/serialized_file_wire/v16_type_index_collision.assets.hex"
    ));
    let file = SerializedFileParser::from_bytes(bytes).expect("parse v16 collision fixture");

    assert_eq!(
        file.types()
            .iter()
            .map(|serialized_type| serialized_type.class_id)
            .collect::<Vec<_>>(),
        [1, 28]
    );
    let object = &file.objects()[0];
    assert_eq!(
        object.type_reference(),
        ObjectTypeReference::TransitionalV16 { raw: 1 }
    );
    assert_eq!(object.serialized_type_index(), Some(1));
    assert_eq!(object.class_id(), 28);
    assert_eq!(file.object_bytes(object).unwrap(), [0x10, 0xAA, 0xBB, 0xCC]);

    let rewritten = prepare_serialized_file(&file, &SerializedFileEdits::default())
        .expect("rewrite v16 collision fixture");
    let reparsed =
        SerializedFileParser::from_bytes(rewritten).expect("reparse v16 collision fixture");
    let object = &reparsed.objects()[0];
    assert_eq!(
        object.type_reference(),
        ObjectTypeReference::TransitionalV16 { raw: 1 }
    );
    assert_eq!(object.serialized_type_index(), Some(1));
    assert_eq!(object.class_id(), 28);
}

#[derive(Debug, Clone, Copy)]
enum WriterRejection {
    HeaderVersionMismatch,
    InvalidEndian,
    DisableImplicitTypeTree,
    UnsupportedUnityVersion,
    UnsupportedReferenceTypes,
    UnknownObjectEdit,
    ConflictingExternal,
}

struct WriterRejectionCase {
    name: &'static str,
    version: u32,
    rejection: WriterRejection,
    expected_fragment: &'static str,
}

#[test]
fn writer_rejects_publicly_constructible_unrepresentable_states() {
    let cases = [
        WriterRejectionCase {
            name: "header version disagrees with retained format",
            version: 22,
            rejection: WriterRejection::HeaderVersionMismatch,
            expected_fragment: "header version 21 disagrees with format 22",
        },
        WriterRejectionCase {
            name: "invalid endian flag",
            version: 22,
            rejection: WriterRejection::InvalidEndian,
            expected_fragment: "Invalid SerializedFile endian flag 2",
        },
        WriterRejectionCase {
            name: "implicit TypeTree disabled",
            version: 2,
            rejection: WriterRejection::DisableImplicitTypeTree,
            expected_fragment: "implicit TypeTree enablement",
        },
        WriterRejectionCase {
            name: "old format Unity version field",
            version: 2,
            rejection: WriterRejection::UnsupportedUnityVersion,
            expected_fragment: "cannot encode a Unity version string",
        },
        WriterRejectionCase {
            name: "old format reference types",
            version: 15,
            rejection: WriterRejection::UnsupportedReferenceTypes,
            expected_fragment: "cannot encode reference types",
        },
        WriterRejectionCase {
            name: "unknown object edit",
            version: 22,
            rejection: WriterRejection::UnknownObjectEdit,
            expected_fragment: "unknown object path ID",
        },
        WriterRejectionCase {
            name: "conflicting external metadata",
            version: 22,
            rejection: WriterRejection::ConflictingExternal,
            expected_fragment: "Conflicting external metadata",
        },
    ];

    for case in cases {
        let wire_case = wire_case(case.version);
        let mut file = parse_case(wire_case, wire_case.bytes.to_vec());
        let mut edits = SerializedFileEdits::default();
        match case.rejection {
            WriterRejection::HeaderVersionMismatch => file.header.version = 21,
            WriterRejection::InvalidEndian => file.header.endian = 2,
            WriterRejection::DisableImplicitTypeTree => file.set_type_tree_enabled(false),
            WriterRejection::UnsupportedUnityVersion => {
                file.unity_version = "not-representable".to_string();
            }
            WriterRejection::UnsupportedReferenceTypes => {
                let unsupported = file.types()[0].clone();
                file.ref_types_mut().push(unsupported);
            }
            WriterRejection::UnknownObjectEdit => {
                edits.set_object_bytes(i64::MAX, vec![0xFF]);
            }
            WriterRejection::ConflictingExternal => {
                let mut external = file.externals[0].clone();
                external.guid[0] ^= 0xFF;
                edits.add_external(external);
            }
        }

        let error = match SerializedFileWriter::save(&file, &edits) {
            Ok(_) => panic!("{} must be rejected", case.name),
            Err(error) => error,
        };
        let message = match error {
            UnityAssetError::Format(message) => message,
            other => panic!("{} returned the wrong error kind: {other}", case.name),
        };
        assert!(
            message.contains(case.expected_fragment),
            "{}: {message}",
            case.name
        );
    }
}

#[test]
fn editing_one_object_preserves_the_other_object_wire_semantics() {
    let cases = [
        (
            15_u32,
            include_bytes!("fixtures/serialized_file_wire/multi_v15.assets.bin").as_slice(),
        ),
        (
            22_u32,
            include_bytes!("fixtures/serialized_file_wire/multi_v22.assets.bin").as_slice(),
        ),
    ];

    for (version, bytes) in cases {
        let file = SerializedFileParser::from_bytes(bytes.to_vec())
            .unwrap_or_else(|error| panic!("failed to parse multi-object v{version}: {error}"));
        assert_eq!(file.objects().len(), 2);
        let first_path_id = file.objects()[0].path_id();
        let untouched = &file.objects()[1];
        let original_offset = untouched.byte_start();
        let expected_path_id = untouched.path_id();
        let expected_class_id = untouched.class_id();
        let expected_type_reference = untouched.type_reference();
        let expected_type_index = untouched.serialized_type_index();
        let expected_metadata = untouched.metadata();
        let expected_size = untouched.byte_size();
        let expected_payload = file.object_bytes(untouched).unwrap().to_vec();

        let edited_payload = vec![0xE0; 13];
        let mut edits = SerializedFileEdits::default();
        edits.set_object_bytes(first_path_id, edited_payload.clone());
        let rewritten = prepare_serialized_file(&file, &edits)
            .unwrap_or_else(|error| panic!("failed to rewrite multi-object v{version}: {error}"));
        let reparsed = SerializedFileParser::from_bytes(rewritten)
            .unwrap_or_else(|error| panic!("failed to reparse multi-object v{version}: {error}"));

        assert_eq!(reparsed.objects().len(), 2);
        assert_eq!(
            reparsed.object_bytes(&reparsed.objects()[0]).unwrap(),
            edited_payload
        );
        let untouched = &reparsed.objects()[1];
        assert_eq!(untouched.path_id(), expected_path_id);
        assert_eq!(untouched.class_id(), expected_class_id);
        assert_eq!(untouched.type_reference(), expected_type_reference);
        assert_eq!(untouched.serialized_type_index(), expected_type_index);
        assert_eq!(untouched.metadata(), expected_metadata);
        assert_eq!(untouched.byte_size(), expected_size);
        assert_eq!(reparsed.object_bytes(untouched).unwrap(), expected_payload);
        assert_ne!(
            untouched.byte_start(),
            original_offset,
            "v{version} must relocate the untouched payload after a longer first object"
        );
    }
}

#[test]
fn writer_rejects_an_ambiguous_legacy_type_table_before_encoding() {
    let mut file = SerializedFileParser::from_bytes(
        include_bytes!("fixtures/serialized_file_wire/v15.assets.bin").to_vec(),
    )
    .expect("parse v15 wire fixture");
    let duplicate = file.types()[0].clone();
    file.types_mut().push(duplicate);

    let error = SerializedFileWriter::save(&file, &SerializedFileEdits::default())
        .expect_err("ambiguous legacy type identity must fail before encoding");
    let message = error.to_string();

    assert!(
        message.contains("Invalid SerializedFile wire state"),
        "{message}"
    );
    assert!(
        message.contains("Ambiguous legacy type reference 28"),
        "{message}"
    );
}

#[test]
fn legacy_monobehaviour_resolves_raw_type_without_losing_class_bits() {
    let bytes = include_bytes!("fixtures/serialized_file_wire/legacy_v15_monobehaviour.assets.bin");
    let file =
        SerializedFileParser::from_bytes(bytes.to_vec()).expect("parse legacy script fixture");

    assert_eq!(file.types().len(), 1);
    assert_eq!(file.types()[0].class_id, -1);
    assert_eq!(
        file.types()[0].script_id,
        std::array::from_fn(|index| 0x80 + index as u8)
    );
    let object = &file.objects()[0];
    assert_eq!(object.path_id(), 77);
    assert_eq!(object.class_id(), 114);
    assert_eq!(object.serialized_type_index(), Some(0));
    assert_eq!(
        object.type_reference(),
        ObjectTypeReference::Legacy {
            raw_type_id: -1,
            class_id_bits: 114,
        }
    );
    assert_eq!(
        object.metadata(),
        ObjectMetadata::ScriptTypeIndexAndStripped {
            index: 7,
            stripped: 1,
        }
    );
    let rewritten = prepare_serialized_file(&file, &SerializedFileEdits::default())
        .expect("rewrite legacy script fixture");
    let reparsed = SerializedFileParser::from_bytes(rewritten).expect("reparse rewritten fixture");
    let object = &reparsed.objects()[0];
    assert_eq!(object.class_id(), 114);
    assert_eq!(object.serialized_type_index(), Some(0));
    assert_eq!(
        object.type_reference(),
        ObjectTypeReference::Legacy {
            raw_type_id: -1,
            class_id_bits: 114,
        }
    );
    assert_eq!(
        object.metadata(),
        ObjectMetadata::ScriptTypeIndexAndStripped {
            index: 7,
            stripped: 1,
        }
    );
}

#[test]
fn rejects_negative_and_huge_table_counts_before_allocation() {
    let table_counts = [
        (22, "type_count"),
        (22, "object_count"),
        (15, "script_count"),
        (22, "external_count"),
        (22, "ref_type_count"),
        (21, "type_dependency_count"),
    ];
    for (version, field) in table_counts {
        let negative = parse_mutation(version, |bytes| write_i32(bytes, version, field, -1));
        assert!(
            matches!(negative, BinaryError::InvalidData(_)),
            "{field}: {negative}"
        );

        let huge = parse_mutation(version, |bytes| write_i32(bytes, version, field, i32::MAX));
        assert!(
            matches!(huge, BinaryError::NotEnoughData { .. }),
            "{field}: {huge}"
        );
    }

    for field in ["type_tree_node_count", "type_tree_string_buffer_size"] {
        let negative = parse_mutation(19, |bytes| write_i32(bytes, 19, field, -1));
        assert!(
            matches!(negative, BinaryError::InvalidData(_)),
            "{field}: {negative}"
        );

        let huge = parse_mutation(19, |bytes| write_i32(bytes, 19, field, i32::MAX));
        assert!(
            matches!(huge, BinaryError::ResourceLimitExceeded(_)),
            "{field}: {huge}"
        );
    }
}

#[test]
fn rejects_invalid_identity_type_tree_and_object_ranges_at_parse_entry() {
    let zero_path = parse_mutation(22, |bytes| write_i64(bytes, 22, "object_path_id", 0));
    assert!(matches!(
        zero_path,
        BinaryError::ObjectIdentity(BinaryObjectIdentityError::ZeroPathId)
    ));

    let zero_class = parse_mutation(22, |bytes| {
        write_i32(bytes, 22, "serialized_type_class_id", 0)
    });
    assert!(matches!(zero_class, BinaryError::InvalidData(_)));

    let invalid_node = parse_mutation(19, |bytes| {
        write_i32(bytes, 19, "type_tree_node_byte_size", -2)
    });
    assert!(matches!(invalid_node, BinaryError::InvalidData(_)));

    for raw_index in [-1, 7] {
        let invalid_index = parse_mutation(22, |bytes| {
            write_i32(bytes, 22, "object_raw_type_reference", raw_index)
        });
        assert!(matches!(invalid_index, BinaryError::InvalidData(_)));
    }

    let invalid_range = parse_mutation(22, |bytes| {
        write_i64(bytes, 22, "object_byte_start", i64::MAX)
    });
    assert!(matches!(invalid_range, BinaryError::InvalidData(_)));
}

#[test]
fn rejects_malformed_local_and_common_typetree_strings() {
    let middle = parse_mutation(19, |bytes| {
        write_u32(bytes, 19, "type_tree_type_string_offset", 1)
    });
    assert!(matches!(middle, BinaryError::InvalidData(_)));

    let unknown_common = parse_mutation(19, |bytes| {
        write_u32(bytes, 19, "type_tree_type_string_offset", 0x8001_E240)
    });
    assert!(matches!(unknown_common, BinaryError::InvalidData(_)));

    let invalid_utf8 = parse_mutation(19, |bytes| {
        let range = field_range(19, "type_tree_string_buffer");
        bytes[range.start] = 0xFF;
    });
    assert!(matches!(invalid_utf8, BinaryError::InvalidData(_)));

    let unterminated = parse_mutation(19, |bytes| {
        let range = field_range(19, "type_tree_string_buffer");
        bytes[range.end - 1] = b'X';
    });
    assert!(matches!(unterminated, BinaryError::InvalidData(_)));
}
