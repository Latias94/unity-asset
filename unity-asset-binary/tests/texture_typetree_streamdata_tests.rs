//! Texture2D TypeTree parsing tests (streamed textures)

#![cfg(feature = "texture")]

use unity_asset_binary::{
    asset::ObjectInfo, asset::class_ids, object::UnityObject, texture::Texture2DConverter,
    unity_version::UnityVersion,
};
use unity_asset_core::{UnityClass, UnityValue};

#[test]
fn texture2d_converter_parses_streamdata_from_typetree() {
    let mut class = UnityClass::new(
        class_ids::TEXTURE_2D,
        "Texture2D".to_string(),
        "1".to_string(),
    );

    class.set("m_Name".to_string(), UnityValue::String("Tex".to_string()));
    class.set("m_Width".to_string(), UnityValue::Integer(2));
    class.set("m_Height".to_string(), UnityValue::Integer(2));
    class.set("m_TextureFormat".to_string(), UnityValue::Integer(4)); // RGBA32 in many Unity enums
    class.set("m_IsReadable".to_string(), UnityValue::Bool(true));

    let mut stream_obj = indexmap::IndexMap::new();
    stream_obj.insert(
        "path".to_string(),
        UnityValue::String("archive:/CAB-abc/CAB-abc.resS".to_string()),
    );
    stream_obj.insert("offset".to_string(), UnityValue::Integer(4096));
    stream_obj.insert("size".to_string(), UnityValue::Integer(16));
    class.set("m_StreamData".to_string(), UnityValue::Object(stream_obj));

    let info = ObjectInfo::new(1, 0, 0, class_ids::TEXTURE_2D, -1);
    let obj = UnityObject::from_info_and_class(info, class);

    let converter = Texture2DConverter::new(UnityVersion::default());
    let tex = converter.from_unity_object(&obj).unwrap();

    assert_eq!(tex.name, "Tex");
    assert_eq!(tex.width, 2);
    assert_eq!(tex.height, 2);
    assert!(tex.image_data.is_empty());
    assert!(tex.is_streamed());
    assert_eq!(tex.stream_info.offset, 4096);
    assert_eq!(tex.stream_info.size, 16);
    assert!(tex.stream_info.path.contains("CAB-abc"));
}
