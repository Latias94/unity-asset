//! Texture2D raw parsing tests (streamed textures)

#![cfg(feature = "texture")]

use unity_asset_decode::object::UnityObject;
use unity_asset_decode::texture::Texture2DConverter;
use unity_asset_decode::unity_version::UnityVersion;

fn aligned_string_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

#[test]
fn texture2d_raw_parsing_can_extract_streamdata() {
    // A minimal, best-effort raw layout matching our current parser:
    // name (aligned string)
    // width i32, height i32, complete_image_size i32, format i32
    // mip_map bool, is_readable bool, align
    // data_size i32 (0 for streamed)
    // m_StreamData (best-effort): path (aligned string), offset u64, size u32, align
    let mut data = Vec::new();
    data.extend_from_slice(&aligned_string_bytes("Tex"));
    data.extend_from_slice(&2i32.to_le_bytes());
    data.extend_from_slice(&2i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&4i32.to_le_bytes());
    data.push(0);
    data.push(0);
    while data.len() % 4 != 0 {
        data.push(0);
    }
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&aligned_string_bytes("archive:/CAB-abc/CAB-abc.resource"));
    data.extend_from_slice(&4096u64.to_le_bytes());
    data.extend_from_slice(&16u32.to_le_bytes());
    while data.len() % 4 != 0 {
        data.push(0);
    }

    let obj = UnityObject::from_raw(28, 1, data);
    let converter = Texture2DConverter::new(UnityVersion::default());
    let tex = converter.from_unity_object(&obj).unwrap();

    assert_eq!(tex.name, "Tex");
    assert!(tex.image_data.is_empty());
    assert!(tex.is_streamed());
    assert_eq!(tex.stream_info.offset, 4096);
    assert_eq!(tex.stream_info.size, 16);
    assert!(tex.stream_info.path.contains("CAB-abc"));
}
