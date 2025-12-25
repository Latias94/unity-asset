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
        #[serde(default)]
        stream_path_suffix: Option<String>,
        #[serde(default)]
        compression_format: Option<i64>,
    },
    #[serde(rename = "texture2d")]
    Texture2D {
        width: i64,
        height: i64,
        texture_format: i64,
        #[serde(default)]
        stream_offset: Option<i64>,
        #[serde(default)]
        stream_size: Option<i64>,
        #[serde(default)]
        stream_path_suffix: Option<String>,
        #[serde(default)]
        complete_image_size: Option<i64>,
    },
    #[serde(rename = "sprite")]
    Sprite {
        rect_width: f64,
        rect_height: f64,
        #[serde(default)]
        texture_file_id: Option<i64>,
        #[serde(default)]
        texture_path_id: Option<i64>,
        #[serde(default)]
        index_buffer_len: Option<usize>,
        #[serde(default)]
        index_buffer_prefix: Vec<i64>,
        #[serde(default)]
        vertex_data_len: Option<usize>,
        #[serde(default)]
        vertex_data_prefix: Vec<i64>,
        #[serde(default)]
        pptr_internal: Vec<i64>,
        #[serde(default)]
        pptr_external: Vec<[i64; 2]>,
    },
    #[serde(rename = "mesh")]
    Mesh {
        #[serde(default)]
        index_buffer_len: Option<usize>,
        #[serde(default)]
        index_buffer_prefix: Vec<i64>,
        #[serde(default)]
        vertex_data_len: Option<usize>,
        #[serde(default)]
        vertex_data_prefix: Vec<i64>,
        #[serde(default)]
        pptr_internal: Vec<i64>,
        #[serde(default)]
        pptr_external: Vec<[i64; 2]>,
    },
    #[serde(rename = "peek_only")]
    PeekOnly {
        #[serde(default)]
        pptr_internal: Vec<i64>,
        #[serde(default)]
        pptr_external: Vec<[i64; 2]>,
    },
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

fn scan_pptrs(env: &Environment, key: &BinaryObjectKey) -> (Vec<i64>, Vec<[i64; 2]>) {
    let scan = match key.source_kind {
        BinarySourceKind::AssetBundle => env
            .bundles()
            .get(&key.source)
            .and_then(|b| key.asset_index.and_then(|i| b.assets.get(i)))
            .and_then(|f| f.find_object_handle(key.path_id))
            .and_then(|h| h.scan_pptrs().ok().flatten()),
        BinarySourceKind::SerializedFile => env
            .binary_assets()
            .get(&key.source)
            .and_then(|f| f.find_object_handle(key.path_id))
            .and_then(|h| h.scan_pptrs().ok().flatten()),
    };

    let Some(scan) = scan else {
        return (Vec::new(), Vec::new());
    };

    let mut internal = scan.internal;
    internal.sort();
    internal.dedup();
    let mut external: Vec<[i64; 2]> = scan
        .external
        .into_iter()
        .map(|(fid, pid)| [fid as i64, pid])
        .collect();
    external.sort();
    external.dedup();

    (internal, external)
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
            GoldenExpect::PeekOnly {
                pptr_internal,
                pptr_external,
            } => {
                let (internal, external) = scan_pptrs(&env, &key);
                assert_eq!(
                    internal, pptr_internal,
                    "scan_pptrs internal mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    external, pptr_external,
                    "scan_pptrs external mismatch (case={})",
                    case.id
                );
                continue;
            }
            GoldenExpect::AudioClipStreamed {
                stream_offset,
                stream_size,
                stream_path_suffix,
                compression_format,
            } => {
                let obj = env
                    .read_binary_object_key(&key)
                    .unwrap_or_else(|_| panic!("read_binary_object_key failed (case={})", case.id));
                assert_eq!(
                    obj.name().as_deref(),
                    Some(case.name.as_str()),
                    "m_Name mismatch (case={})",
                    case.id
                );
                if let Some(expected) = compression_format {
                    assert_eq!(
                        i64_field(&obj, "m_CompressionFormat"),
                        expected,
                        "m_CompressionFormat mismatch (case={})",
                        case.id
                    );
                }

                if let Some(expected_suffix) = stream_path_suffix.as_deref() {
                    let res = obj
                        .get("m_Resource")
                        .expect("m_Resource present")
                        .as_object()
                        .expect("m_Resource object");
                    let source = res
                        .get("m_Source")
                        .and_then(|v| v.as_str())
                        .expect("m_Resource.m_Source string");
                    assert!(
                        source.ends_with(expected_suffix),
                        "m_Resource.m_Source suffix mismatch (case={})",
                        case.id
                    );
                    let off = res
                        .get("m_Offset")
                        .and_then(|v| v.as_i64())
                        .expect("m_Resource.m_Offset int");
                    let size = res
                        .get("m_Size")
                        .and_then(|v| v.as_i64())
                        .expect("m_Resource.m_Size int");
                    assert_eq!(
                        off as u64, stream_offset,
                        "m_Resource.m_Offset mismatch (case={})",
                        case.id
                    );
                    assert_eq!(
                        size as u32, stream_size,
                        "m_Resource.m_Size mismatch (case={})",
                        case.id
                    );
                }

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
                stream_offset,
                stream_size,
                stream_path_suffix,
                complete_image_size,
            } => {
                let obj = env
                    .read_binary_object_key(&key)
                    .unwrap_or_else(|_| panic!("read_binary_object_key failed (case={})", case.id));
                assert_eq!(
                    obj.name().as_deref(),
                    Some(case.name.as_str()),
                    "m_Name mismatch (case={})",
                    case.id
                );
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

                if let Some(expected) = complete_image_size {
                    assert_eq!(
                        i64_field(&obj, "m_CompleteImageSize"),
                        expected,
                        "m_CompleteImageSize mismatch (case={})",
                        case.id
                    );
                }

                if stream_offset.is_some() || stream_size.is_some() || stream_path_suffix.is_some()
                {
                    let sd = obj
                        .get("m_StreamData")
                        .expect("m_StreamData present")
                        .as_object()
                        .expect("m_StreamData object");

                    if let Some(expected) = stream_offset {
                        let off = sd
                            .get("offset")
                            .and_then(|v| v.as_i64())
                            .expect("m_StreamData.offset int");
                        assert_eq!(
                            off, expected,
                            "m_StreamData.offset mismatch (case={})",
                            case.id
                        );
                    }
                    if let Some(expected) = stream_size {
                        let size = sd
                            .get("size")
                            .and_then(|v| v.as_i64())
                            .expect("m_StreamData.size int");
                        assert_eq!(
                            size, expected,
                            "m_StreamData.size mismatch (case={})",
                            case.id
                        );
                    }
                    if let Some(expected_suffix) = stream_path_suffix.as_deref() {
                        let path = sd
                            .get("path")
                            .and_then(|v| v.as_str())
                            .expect("m_StreamData.path string");
                        assert!(
                            path.ends_with(expected_suffix),
                            "m_StreamData.path suffix mismatch (case={})",
                            case.id
                        );
                    }
                }
            }
            GoldenExpect::Sprite {
                rect_width,
                rect_height,
                texture_file_id,
                texture_path_id,
                index_buffer_len,
                index_buffer_prefix,
                vertex_data_len,
                vertex_data_prefix,
                pptr_internal,
                pptr_external,
            } => {
                let (internal, external) = scan_pptrs(&env, &key);
                assert_eq!(
                    internal, pptr_internal,
                    "scan_pptrs internal mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    external, pptr_external,
                    "scan_pptrs external mismatch (case={})",
                    case.id
                );

                let obj = env
                    .read_binary_object_key(&key)
                    .unwrap_or_else(|_| panic!("read_binary_object_key failed (case={})", case.id));
                assert_eq!(
                    obj.name().as_deref(),
                    Some(case.name.as_str()),
                    "m_Name mismatch (case={})",
                    case.id
                );
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

                if texture_file_id.is_some() || texture_path_id.is_some() {
                    let rd = obj
                        .get("m_RD")
                        .expect("m_RD present")
                        .as_object()
                        .expect("m_RD object");
                    let tex = rd
                        .get("texture")
                        .expect("m_RD.texture present")
                        .as_object()
                        .expect("m_RD.texture object");
                    if let Some(expected) = texture_file_id {
                        let fid = tex
                            .get("m_FileID")
                            .and_then(|v| v.as_i64())
                            .expect("m_RD.texture.m_FileID int");
                        assert_eq!(fid, expected, "m_RD.texture.m_FileID mismatch (case={})", case.id);
                    }
                    if let Some(expected) = texture_path_id {
                        let pid = tex
                            .get("m_PathID")
                            .and_then(|v| v.as_i64())
                            .expect("m_RD.texture.m_PathID int");
                        assert_eq!(pid, expected, "m_RD.texture.m_PathID mismatch (case={})", case.id);
                    }
                }

                if index_buffer_len.is_some() || !index_buffer_prefix.is_empty() {
                    let rd = obj
                        .get("m_RD")
                        .expect("m_RD present")
                        .as_object()
                        .expect("m_RD object");
                    let buf_v = rd.get("m_IndexBuffer").expect("m_RD.m_IndexBuffer present");
                    if let Some(len) = index_buffer_len {
                        match buf_v {
                            UnityValue::Bytes(b) => {
                                assert_eq!(
                                    b.len(),
                                    len,
                                    "m_RD.m_IndexBuffer len mismatch (case={})",
                                    case.id
                                );
                                if !index_buffer_prefix.is_empty() {
                                    let prefix: Vec<i64> = b
                                        .iter()
                                        .take(index_buffer_prefix.len())
                                        .map(|v| *v as i64)
                                        .collect();
                                    assert_eq!(
                                        prefix, index_buffer_prefix,
                                        "m_RD.m_IndexBuffer prefix mismatch (case={})",
                                        case.id
                                    );
                                }
                            }
                            UnityValue::Array(arr) => {
                                assert_eq!(
                                    arr.len(),
                                    len,
                                    "m_RD.m_IndexBuffer len mismatch (case={})",
                                    case.id
                                );
                                if !index_buffer_prefix.is_empty() {
                                    let prefix: Vec<i64> = arr
                                        .iter()
                                        .take(index_buffer_prefix.len())
                                        .map(|v| v.as_i64().expect("m_RD.m_IndexBuffer byte"))
                                        .collect();
                                    assert_eq!(
                                        prefix, index_buffer_prefix,
                                        "m_RD.m_IndexBuffer prefix mismatch (case={})",
                                        case.id
                                    );
                                }
                            }
                            other => panic!("unexpected m_RD.m_IndexBuffer type: {other:?}"),
                        }
                    }
                }

                if vertex_data_len.is_some() || !vertex_data_prefix.is_empty() {
                    let rd = obj
                        .get("m_RD")
                        .expect("m_RD present")
                        .as_object()
                        .expect("m_RD object");
                    let vd = rd
                        .get("m_VertexData")
                        .expect("m_RD.m_VertexData present")
                        .as_object()
                        .expect("m_RD.m_VertexData object");
                    let buf_v = vd
                        .get("m_DataSize")
                        .expect("m_RD.m_VertexData.m_DataSize present");
                    if let Some(len) = vertex_data_len {
                        match buf_v {
                            UnityValue::Bytes(b) => {
                                assert_eq!(
                                    b.len(),
                                    len,
                                    "m_RD.m_VertexData.m_DataSize len mismatch (case={})",
                                    case.id
                                );
                                if !vertex_data_prefix.is_empty() {
                                    let prefix: Vec<i64> = b
                                        .iter()
                                        .take(vertex_data_prefix.len())
                                        .map(|v| *v as i64)
                                        .collect();
                                    assert_eq!(
                                        prefix, vertex_data_prefix,
                                        "m_RD.m_VertexData.m_DataSize prefix mismatch (case={})",
                                        case.id
                                    );
                                }
                            }
                            UnityValue::Array(arr) => {
                                assert_eq!(
                                    arr.len(),
                                    len,
                                    "m_RD.m_VertexData.m_DataSize len mismatch (case={})",
                                    case.id
                                );
                                if !vertex_data_prefix.is_empty() {
                                    let prefix: Vec<i64> = arr
                                        .iter()
                                        .take(vertex_data_prefix.len())
                                        .map(|v| v.as_i64().expect("m_RD.m_DataSize byte"))
                                        .collect();
                                    assert_eq!(
                                        prefix, vertex_data_prefix,
                                        "m_RD.m_VertexData.m_DataSize prefix mismatch (case={})",
                                        case.id
                                    );
                                }
                            }
                            other => panic!("unexpected m_RD.m_VertexData.m_DataSize type: {other:?}"),
                        }
                    }
                }
            }
            GoldenExpect::Mesh {
                index_buffer_len,
                index_buffer_prefix,
                vertex_data_len,
                vertex_data_prefix,
                pptr_internal,
                pptr_external,
            } => {
                let (internal, external) = scan_pptrs(&env, &key);
                assert_eq!(
                    internal, pptr_internal,
                    "scan_pptrs internal mismatch (case={})",
                    case.id
                );
                assert_eq!(
                    external, pptr_external,
                    "scan_pptrs external mismatch (case={})",
                    case.id
                );

                let obj = env
                    .read_binary_object_key(&key)
                    .unwrap_or_else(|_| panic!("read_binary_object_key failed (case={})", case.id));
                assert_eq!(
                    obj.name().as_deref(),
                    Some(case.name.as_str()),
                    "m_Name mismatch (case={})",
                    case.id
                );
                if let Some(len) = index_buffer_len {
                    let buf_v = obj.get("m_IndexBuffer").expect("m_IndexBuffer present");
                    match buf_v {
                        UnityValue::Bytes(b) => {
                            assert_eq!(
                                b.len(),
                                len,
                                "m_IndexBuffer len mismatch (case={})",
                                case.id
                            );
                            if !index_buffer_prefix.is_empty() {
                                let prefix: Vec<i64> = b
                                    .iter()
                                    .take(index_buffer_prefix.len())
                                    .map(|v| *v as i64)
                                    .collect();
                                assert_eq!(
                                    prefix, index_buffer_prefix,
                                    "m_IndexBuffer prefix mismatch (case={})",
                                    case.id
                                );
                            }
                        }
                        UnityValue::Array(arr) => {
                            assert_eq!(
                                arr.len(),
                                len,
                                "m_IndexBuffer len mismatch (case={})",
                                case.id
                            );
                            if !index_buffer_prefix.is_empty() {
                                let prefix: Vec<i64> = arr
                                    .iter()
                                    .take(index_buffer_prefix.len())
                                    .map(|v| v.as_i64().expect("m_IndexBuffer byte"))
                                    .collect();
                                assert_eq!(
                                    prefix, index_buffer_prefix,
                                    "m_IndexBuffer prefix mismatch (case={})",
                                    case.id
                                );
                            }
                        }
                        other => panic!("unexpected m_IndexBuffer type: {other:?}"),
                    }
                }

                if let Some(len) = vertex_data_len {
                    let vd = obj
                        .get("m_VertexData")
                        .expect("m_VertexData present")
                        .as_object()
                        .expect("m_VertexData object");
                    let buf_v = vd
                        .get("m_DataSize")
                        .expect("m_VertexData.m_DataSize present");
                    match buf_v {
                        UnityValue::Bytes(b) => {
                            assert_eq!(
                                b.len(),
                                len,
                                "m_VertexData.m_DataSize len mismatch (case={})",
                                case.id
                            );
                            if !vertex_data_prefix.is_empty() {
                                let prefix: Vec<i64> = b
                                    .iter()
                                    .take(vertex_data_prefix.len())
                                    .map(|v| *v as i64)
                                    .collect();
                                assert_eq!(
                                    prefix, vertex_data_prefix,
                                    "m_VertexData.m_DataSize prefix mismatch (case={})",
                                    case.id
                                );
                            }
                        }
                        UnityValue::Array(arr) => {
                            assert_eq!(
                                arr.len(),
                                len,
                                "m_VertexData.m_DataSize len mismatch (case={})",
                                case.id
                            );
                            if !vertex_data_prefix.is_empty() {
                                let prefix: Vec<i64> = arr
                                    .iter()
                                    .take(vertex_data_prefix.len())
                                    .map(|v| v.as_i64().expect("m_DataSize byte"))
                                    .collect();
                                assert_eq!(
                                    prefix, vertex_data_prefix,
                                    "m_VertexData.m_DataSize prefix mismatch (case={})",
                                    case.id
                                );
                            }
                        }
                        other => panic!("unexpected m_VertexData.m_DataSize type: {other:?}"),
                    }
                }
            }
        }
    }
}
