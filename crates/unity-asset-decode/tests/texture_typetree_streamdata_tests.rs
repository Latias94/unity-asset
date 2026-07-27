//! Texture2D TypeTree parsing tests (streamed textures)

#![cfg(feature = "texture")]

use unity_asset_core::{UnityClass, UnityValue};
use unity_asset_decode::{
    asset::ObjectInfo, asset::class_ids, object::UnityObject, texture::Texture2DConverter,
};

#[test]
fn texture2d_converter_parses_streamdata_from_typetree() {
    let mut stream_obj = indexmap::IndexMap::new();
    stream_obj.insert(
        "path".to_string(),
        UnityValue::String("archive:/CAB-abc/CAB-abc.resS".to_string()),
    );
    stream_obj.insert("offset".to_string(), UnityValue::from(u64::MAX));
    stream_obj.insert("size".to_string(), UnityValue::Integer(16));
    let class = UnityClass::with_properties(
        class_ids::TEXTURE_2D,
        "Texture2D".to_string(),
        "1".to_string(),
        indexmap::IndexMap::from([
            ("m_Name".to_string(), UnityValue::String("Tex".to_string())),
            ("m_Width".to_string(), UnityValue::Integer(2)),
            ("m_Height".to_string(), UnityValue::Integer(2)),
            ("m_TextureFormat".to_string(), UnityValue::Integer(4)),
            ("m_IsReadable".to_string(), UnityValue::Bool(true)),
            ("m_StreamData".to_string(), UnityValue::Object(stream_obj)),
        ]),
    );

    let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::TEXTURE_2D)
        .expect("valid standalone texture object");
    let obj = UnityObject::from_info_and_class(info, class);

    let converter = Texture2DConverter::new();
    let tex = converter.from_unity_object(&obj).unwrap();

    assert_eq!(tex.name, "Tex");
    assert_eq!(tex.width, 2);
    assert_eq!(tex.height, 2);
    assert!(tex.image_data.is_empty());
    assert!(tex.is_streamed());
    assert_eq!(tex.stream_info.offset, u64::MAX);
    assert_eq!(tex.stream_info.size, 16);
    assert!(tex.stream_info.path.contains("CAB-abc"));
}
