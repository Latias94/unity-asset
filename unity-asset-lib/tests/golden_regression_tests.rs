use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use unity_asset::environment::{BinaryObjectKey, BinarySource, BinarySourceKind, Environment};
use unity_asset_core::UnityValue;
use unity_asset_decode::audio::AudioClipConverter;
use unity_asset_decode::unity_version::UnityVersion;

#[derive(Debug, Deserialize)]
struct GoldenFile {
    schema: u32,
    cases: Vec<GoldenCase>,
}

#[derive(Debug, Deserialize)]
struct GoldenCase {
    id: String,
    source: String,
    source_kind: String,
    asset_index: usize,
    asset_path: String,
    path_id: i64,
    type_id: i32,
    byte_size: u32,
    name: String,
    expect: GoldenExpect,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum GoldenExpect {
    #[serde(rename = "audioclip_streamed")]
    AudioClipStreamed {
        stream_offset: u64,
        stream_size: u32,
    },
    #[serde(rename = "texture2d")]
    Texture2D {
        width: i64,
        height: i64,
        texture_format: i64,
    },
    #[serde(rename = "sprite")]
    Sprite { rect_width: f64, rect_height: f64 },
    #[serde(rename = "mesh")]
    Mesh,
    #[serde(rename = "peek_only")]
    PeekOnly,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/golden/golden_v1.json")
}

fn load_golden() -> GoldenFile {
    let text = fs::read_to_string(golden_path()).expect("read golden JSON");
    let golden: GoldenFile = serde_json::from_str(&text).expect("parse golden JSON");
    assert_eq!(golden.schema, 1);
    golden
}

fn workspace_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../")
        .join(rel)
}

fn i64_field(obj: &unity_asset_binary::object::UnityObject, key: &str) -> i64 {
    match obj.get(key) {
        Some(UnityValue::Integer(v)) => *v,
        other => panic!("expected integer field {key}, got {other:?}"),
    }
}

fn object_type_info(env: &Environment, key: &BinaryObjectKey) -> (i32, u32) {
    match key.source_kind {
        BinarySourceKind::AssetBundle => env
            .bundles()
            .get(&key.source)
            .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
        BinarySourceKind::SerializedFile => env
            .binary_assets()
            .get(&key.source)
            .and_then(|f| f.find_object(key.path_id))
            .map(|info| (info.type_id, info.byte_size))
            .unwrap_or((0, 0)),
    }
}

#[test]
fn golden_regression_smoke() {
    let golden = load_golden();
    let mut env = Environment::new();
    env.load(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/samples"))
        .expect("load samples");

    for case in golden.cases {
        let source_kind = match case.source_kind.as_str() {
            "bundle" => BinarySourceKind::AssetBundle,
            "serialized" => BinarySourceKind::SerializedFile,
            other => panic!("unknown source_kind in golden: {other} (case={})", case.id),
        };

        let source_path = workspace_path(&case.source);
        let asset_index = match source_kind {
            BinarySourceKind::AssetBundle => Some(case.asset_index),
            BinarySourceKind::SerializedFile => None,
        };
        let key = BinaryObjectKey {
            source: BinarySource::path(&source_path),
            source_kind,
            asset_index,
            path_id: case.path_id,
        };

        // Container entry existence is part of the discovery contract.
        if source_kind == BinarySourceKind::AssetBundle {
            let entries = env
                .bundle_container_entries(&source_path)
                .unwrap_or_else(|_| panic!("bundle_container_entries failed (case={})", case.id));
            assert!(
                entries
                    .iter()
                    .any(|e| e.asset_path == case.asset_path && e.key.as_ref() == Some(&key)),
                "expected container entry with matching key (case={})",
                case.id
            );
        }

        let (type_id, byte_size) = object_type_info(&env, &key);
        assert_eq!(type_id, case.type_id, "type_id mismatch (case={})", case.id);
        assert_eq!(
            byte_size, case.byte_size,
            "byte_size mismatch (case={})",
            case.id
        );

        let peek = env
            .peek_binary_object_name(&key)
            .unwrap_or_else(|_| panic!("peek_binary_object_name failed (case={})", case.id));
        assert_eq!(
            peek.as_deref(),
            Some(case.name.as_str()),
            "peek_name mismatch (case={})",
            case.id
        );

        match case.expect {
            GoldenExpect::PeekOnly => continue,
            _ => {}
        }

        let obj = env
            .read_binary_object_key(&key)
            .unwrap_or_else(|_| panic!("read_binary_object_key failed (case={})", case.id));
        assert_eq!(
            obj.name().as_deref(),
            Some(case.name.as_str()),
            "m_Name mismatch (case={})",
            case.id
        );

        match case.expect {
            GoldenExpect::AudioClipStreamed {
                stream_offset,
                stream_size,
            } => {
                let unity_version = env
                    .bundles()
                    .get(&key.source)
                    .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
                    .and_then(|f| UnityVersion::parse_version(&f.unity_version).ok())
                    .unwrap_or_default();
                let converter = AudioClipConverter::new(unity_version);
                let clip = converter
                    .from_unity_object(&obj)
                    .unwrap_or_else(|_| panic!("AudioClipConverter failed (case={})", case.id));
                assert!(
                    clip.is_streamed(),
                    "expected streamed clip (case={})",
                    case.id
                );
                assert_eq!(
                    clip.stream_info.offset, stream_offset,
                    "stream_offset mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    clip.stream_info.size, stream_size,
                    "stream_size mismatch (case={})",
                    case.id
                );
            }
            GoldenExpect::Texture2D {
                width,
                height,
                texture_format,
            } => {
                assert_eq!(
                    i64_field(&obj, "m_Width"),
                    width,
                    "m_Width mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    i64_field(&obj, "m_Height"),
                    height,
                    "m_Height mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    i64_field(&obj, "m_TextureFormat"),
                    texture_format,
                    "m_TextureFormat mismatch (case={})",
                    case.id
                );
            }
            GoldenExpect::Sprite {
                rect_width,
                rect_height,
            } => {
                let rect = obj
                    .get("m_Rect")
                    .expect("m_Rect present")
                    .as_object()
                    .expect("m_Rect object");
                let w = rect
                    .get("width")
                    .and_then(|v| v.as_f64())
                    .expect("m_Rect.width float");
                let h = rect
                    .get("height")
                    .and_then(|v| v.as_f64())
                    .expect("m_Rect.height float");
                assert!(
                    (w - rect_width).abs() < 1e-6,
                    "m_Rect.width mismatch (case={})",
                    case.id
                );
                assert!(
                    (h - rect_height).abs() < 1e-6,
                    "m_Rect.height mismatch (case={})",
                    case.id
                );
            }
            GoldenExpect::Mesh => {
                // Name assertion above is the core fast-path compatibility check.
                // If Mesh TypeTree changes, the regression will surface here.
            }
            GoldenExpect::PeekOnly => {}
        }
    }
}
