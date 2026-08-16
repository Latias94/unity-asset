//! Strict allocation-free Sprite TypeTree inspection.

use indexmap::IndexMap;
use unity_asset_core::UnityValue;

use crate::media::MediaInspectionError;
use unity_asset_binary::asset::class_ids;
use unity_asset_binary::object::UnityObject;

/// The Texture2D PPtr selected by a strictly inspected Sprite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpriteTextureReference {
    file_id: i32,
    path_id: i64,
}

impl SpriteTextureReference {
    #[must_use]
    pub const fn file_id(self) -> i32 {
        self.file_id
    }

    #[must_use]
    pub const fn path_id(self) -> i64 {
        self.path_id
    }
}

/// Pixel-aligned texture rectangle selected from direct Sprite render data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpritePixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl SpritePixelRect {
    #[must_use]
    pub const fn x(self) -> u32 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> u32 {
        self.y
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Strict Sprite metadata required by extraction planning and preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpriteLayout {
    texture: SpriteTextureReference,
    texture_rect: SpritePixelRect,
}

impl SpriteLayout {
    pub fn inspect(object: &UnityObject) -> Result<Self, MediaInspectionError> {
        if object.class_id() != class_ids::SPRITE {
            return Err(MediaInspectionError::NotApplicable {
                expected: class_ids::SPRITE,
                actual: object.class_id(),
            });
        }
        let properties = object.as_unity_class().properties();
        if properties.is_empty() {
            return Err(MediaInspectionError::TypeTreeUnavailable);
        }
        reject_atlas_reference(properties)?;
        reject_atlas_tags(properties)?;
        let render_data = required_object(properties, "m_RD")?;
        let settings_raw = required_u32(render_data, "settingsRaw")?;
        if settings_raw != 0 {
            return Err(unsupported_layout("packed_or_tight"));
        }
        reject_optional_texture(render_data, "alphaTexture", "split_alpha")?;
        reject_downscale(render_data)?;
        let texture = required_object(render_data, "texture")?;
        let file_id = required_i32(texture, "m_FileID")?;
        let path_id = required_i64(texture, "m_PathID")?;
        if path_id == 0 {
            return Err(MediaInspectionError::InvalidDescriptor {
                field: "m_RD.texture.m_PathID",
                reason: "Sprite texture path ID must not be zero",
            });
        }
        let texture_rect = required_object(render_data, "textureRect")?;
        Ok(Self {
            texture: SpriteTextureReference { file_id, path_id },
            texture_rect: SpritePixelRect {
                x: pixel_coordinate(texture_rect, "x", true)?,
                y: pixel_coordinate(texture_rect, "y", true)?,
                width: pixel_coordinate(texture_rect, "width", false)?,
                height: pixel_coordinate(texture_rect, "height", false)?,
            },
        })
    }

    #[must_use]
    pub const fn texture(self) -> SpriteTextureReference {
        self.texture
    }

    #[must_use]
    pub const fn rect(self) -> SpritePixelRect {
        self.texture_rect
    }
}

fn reject_atlas_reference(
    properties: &IndexMap<String, UnityValue>,
) -> Result<(), MediaInspectionError> {
    let Some(value) = properties.get("m_SpriteAtlas") else {
        return Ok(());
    };
    let atlas = value
        .as_object()
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: "m_SpriteAtlas",
            reason: "field must be a PPtr object",
        })?;
    let _ = required_i32(atlas, "m_FileID")?;
    if required_i64(atlas, "m_PathID")? != 0 {
        return Err(unsupported_layout("sprite_atlas"));
    }
    Ok(())
}

fn reject_atlas_tags(
    properties: &IndexMap<String, UnityValue>,
) -> Result<(), MediaInspectionError> {
    let Some(value) = properties.get("m_AtlasTags") else {
        return Ok(());
    };
    let tags = value
        .as_array()
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: "m_AtlasTags",
            reason: "field must be an array",
        })?;
    if !tags.is_empty() {
        return Err(unsupported_layout("atlas_tag_lookup"));
    }
    Ok(())
}

fn reject_optional_texture(
    render_data: &IndexMap<String, UnityValue>,
    field: &'static str,
    layout: &'static str,
) -> Result<(), MediaInspectionError> {
    let Some(value) = render_data.get(field) else {
        return Ok(());
    };
    let texture = value
        .as_object()
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be a PPtr object",
        })?;
    let _ = required_i32(texture, "m_FileID")?;
    if required_i64(texture, "m_PathID")? != 0 {
        return Err(unsupported_layout(layout));
    }
    Ok(())
}

fn reject_downscale(
    render_data: &IndexMap<String, UnityValue>,
) -> Result<(), MediaInspectionError> {
    let Some(value) = render_data.get("downscaleMultiplier") else {
        return Ok(());
    };
    let multiplier = value
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field: "downscaleMultiplier",
            reason: "field must be a positive finite number",
        })?;
    if multiplier != 1.0 {
        return Err(unsupported_layout("downscaled_render_data"));
    }
    Ok(())
}

const fn unsupported_layout(layout: &'static str) -> MediaInspectionError {
    MediaInspectionError::UnsupportedLayout {
        family: "Sprite",
        layout,
    }
}

fn required_object<'a>(
    fields: &'a IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<&'a IndexMap<String, UnityValue>, MediaInspectionError> {
    fields.get(field).and_then(UnityValue::as_object).ok_or(
        MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be an object",
        },
    )
}

fn required_i32(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<i32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be an i32",
        })
}

fn required_i64(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<i64, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_i64)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be an i64",
        })
}

fn required_u32(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
) -> Result<u32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "field must be a u32",
        })
}

fn pixel_coordinate(
    fields: &IndexMap<String, UnityValue>,
    field: &'static str,
    allow_zero: bool,
) -> Result<u32, MediaInspectionError> {
    fields
        .get(field)
        .and_then(UnityValue::as_f64)
        .filter(|value| {
            value.is_finite()
                && *value >= 0.0
                && *value <= f64::from(u32::MAX)
                && value.fract() == 0.0
        })
        .map(|value| value as u32)
        .filter(|value| allow_zero || *value != 0)
        .ok_or(MediaInspectionError::InvalidDescriptor {
            field,
            reason: "Sprite rectangle coordinates must be finite pixel-aligned u32 values",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_binary::asset::ObjectInfo;
    use unity_asset_core::UnityClass;

    fn object(properties: IndexMap<String, UnityValue>) -> UnityObject {
        let class = UnityClass::with_properties(
            class_ids::SPRITE,
            "Sprite".to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::SPRITE).unwrap();
        UnityObject::from_info_and_class(info, class)
    }

    fn valid_properties() -> IndexMap<String, UnityValue> {
        IndexMap::from([
            (
                "m_RD".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("settingsRaw".to_owned(), UnityValue::Integer(0)),
                    (
                        "texture".to_owned(),
                        UnityValue::Object(IndexMap::from([
                            ("m_FileID".to_owned(), UnityValue::Integer(3)),
                            ("m_PathID".to_owned(), UnityValue::Integer(42)),
                        ])),
                    ),
                    (
                        "textureRect".to_owned(),
                        UnityValue::Object(IndexMap::from([
                            ("x".to_owned(), UnityValue::Float(1.0)),
                            ("y".to_owned(), UnityValue::Float(2.0)),
                            ("width".to_owned(), UnityValue::Float(3.0)),
                            ("height".to_owned(), UnityValue::Float(4.0)),
                        ])),
                    ),
                ])),
            ),
            (
                "m_Rect".to_owned(),
                UnityValue::Object(IndexMap::from([
                    ("x".to_owned(), UnityValue::Float(10.0)),
                    ("y".to_owned(), UnityValue::Float(20.0)),
                    ("width".to_owned(), UnityValue::Float(30.0)),
                    ("height".to_owned(), UnityValue::Float(40.0)),
                ])),
            ),
        ])
    }

    #[test]
    fn strict_layout_requires_typetree_reference_and_pixel_rect() {
        let layout = SpriteLayout::inspect(&object(valid_properties())).unwrap();
        assert_eq!(
            (layout.texture().file_id(), layout.texture().path_id()),
            (3, 42)
        );
        assert_eq!(
            (
                layout.rect().x(),
                layout.rect().y(),
                layout.rect().width(),
                layout.rect().height(),
            ),
            (1, 2, 3, 4)
        );

        assert_eq!(
            SpriteLayout::inspect(&object(IndexMap::new())),
            Err(MediaInspectionError::TypeTreeUnavailable)
        );
        let mut malformed = valid_properties();
        malformed
            .get_mut("m_RD")
            .and_then(UnityValue::as_object_mut)
            .unwrap()
            .shift_remove("textureRect");
        assert!(matches!(
            SpriteLayout::inspect(&object(malformed)),
            Err(MediaInspectionError::InvalidDescriptor {
                field: "textureRect",
                ..
            })
        ));
    }

    #[test]
    fn unsupported_sprite_render_layouts_fail_closed() {
        let cases = [
            ("packed_or_tight", unsupported_packed()),
            ("split_alpha", unsupported_alpha()),
            ("sprite_atlas", unsupported_atlas()),
            ("atlas_tag_lookup", unsupported_atlas_tag()),
        ];
        for (expected, properties) in cases {
            assert_eq!(
                SpriteLayout::inspect(&object(properties)),
                Err(MediaInspectionError::UnsupportedLayout {
                    family: "Sprite",
                    layout: expected,
                })
            );
        }
    }

    fn render_data(
        properties: &mut IndexMap<String, UnityValue>,
    ) -> &mut IndexMap<String, UnityValue> {
        properties
            .get_mut("m_RD")
            .and_then(UnityValue::as_object_mut)
            .unwrap()
    }

    fn unsupported_packed() -> IndexMap<String, UnityValue> {
        let mut properties = valid_properties();
        render_data(&mut properties).insert("settingsRaw".to_owned(), UnityValue::Integer(1));
        properties
    }

    fn unsupported_alpha() -> IndexMap<String, UnityValue> {
        let mut properties = valid_properties();
        render_data(&mut properties).insert(
            "alphaTexture".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(0)),
                ("m_PathID".to_owned(), UnityValue::Integer(99)),
            ])),
        );
        properties
    }

    fn unsupported_atlas() -> IndexMap<String, UnityValue> {
        let mut properties = valid_properties();
        properties.insert(
            "m_SpriteAtlas".to_owned(),
            UnityValue::Object(IndexMap::from([
                ("m_FileID".to_owned(), UnityValue::Integer(0)),
                ("m_PathID".to_owned(), UnityValue::Integer(77)),
            ])),
        );
        properties
    }

    fn unsupported_atlas_tag() -> IndexMap<String, UnityValue> {
        let mut properties = valid_properties();
        properties.insert(
            "m_AtlasTags".to_owned(),
            UnityValue::Array(vec![UnityValue::String("ui".to_owned())]),
        );
        properties
    }
}
