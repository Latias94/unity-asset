use std::collections::TryReserveError;
use std::io::{self, Write};

use image::RgbaImage;
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};

use super::inspection::{SpriteLayout, SpritePixelRect};
use crate::descriptor::{
    MediaDescriptor, MediaDescriptorError, MediaDimensions, MediaOutputEstimate,
};
use crate::media::BudgetedMediaBytes;
use crate::texture::Texture2DLayout;
use crate::texture::prepared::{PreparedTextureImage, TexturePreparationError, encode_png};

/// Prepared Sprite PNG bytes and their closed media descriptor.
pub struct PreparedSpritePng {
    descriptor: MediaDescriptor,
    bytes: Vec<u8>,
}

impl PreparedSpritePng {
    pub fn prepare(
        sprite_layout: SpriteLayout,
        texture_layout: Texture2DLayout<'_>,
        texture_source: BudgetedMediaBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, SpritePreparationError> {
        let texture = PreparedTextureImage::prepare(texture_layout, texture_source, budget)?;
        let (x, flipped_y, width, height) = strict_sprite_bounds(
            sprite_layout.rect(),
            texture.image.width(),
            texture.image.height(),
        )?;
        let image = crop_image(&texture.image, x, flipped_y, width, height, budget)?;
        let bytes = encode_png(&image, budget)?;
        let output_length = u64::try_from(bytes.len())
            .map_err(|_| SpritePreparationError::LengthOverflow("sprite PNG output"))?;
        let descriptor = MediaDescriptor::sprite_png(
            texture.encoding,
            MediaDimensions::new(width, height)?,
            texture.source_length,
            MediaOutputEstimate::exact(output_length)?,
        )?;
        Ok(Self { descriptor, bytes })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &MediaDescriptor {
        &self.descriptor
    }

    pub fn write_to<W: Write + ?Sized>(
        &self,
        writer: &mut W,
    ) -> Result<(), SpritePreparationError> {
        writer
            .write_all(&self.bytes)
            .map_err(SpritePreparationError::Output)
    }
}

fn crop_image(
    texture: &RgbaImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    budget: &mut AssetLoadBudget,
) -> Result<RgbaImage, SpritePreparationError> {
    let row_bytes = usize::try_from(
        u64::from(width)
            .checked_mul(4)
            .ok_or(SpritePreparationError::LengthOverflow("sprite row"))?,
    )
    .map_err(|_| SpritePreparationError::LengthOverflow("sprite row"))?;
    let output_length = row_bytes
        .checked_mul(
            usize::try_from(height)
                .map_err(|_| SpritePreparationError::LengthOverflow("sprite height"))?,
        )
        .ok_or(SpritePreparationError::LengthOverflow("sprite pixels"))?;
    let output_length_u64 = u64::try_from(output_length)
        .map_err(|_| SpritePreparationError::LengthOverflow("sprite pixels"))?;
    budget.check_bytes(output_length_u64)?;

    let mut pixels = Vec::new();
    pixels.try_reserve_exact(output_length).map_err(|source| {
        SpritePreparationError::Allocation {
            resource: "sprite RGBA output",
            requested: output_length,
            source,
        }
    })?;
    let retained = u64::try_from(pixels.capacity())
        .map_err(|_| SpritePreparationError::LengthOverflow("sprite pixels"))?;
    budget.consume_bytes(retained)?;

    let texture_row_bytes = usize::try_from(
        u64::from(texture.width())
            .checked_mul(4)
            .ok_or(SpritePreparationError::LengthOverflow("texture row"))?,
    )
    .map_err(|_| SpritePreparationError::LengthOverflow("texture row"))?;
    let x_bytes = usize::try_from(
        u64::from(x)
            .checked_mul(4)
            .ok_or(SpritePreparationError::LengthOverflow("sprite x offset"))?,
    )
    .map_err(|_| SpritePreparationError::LengthOverflow("sprite x offset"))?;
    let source = texture.as_raw();
    for row in y..y + height {
        let start = usize::try_from(row)
            .ok()
            .and_then(|row| row.checked_mul(texture_row_bytes))
            .and_then(|offset| offset.checked_add(x_bytes))
            .ok_or(SpritePreparationError::LengthOverflow(
                "sprite source offset",
            ))?;
        let end = start
            .checked_add(row_bytes)
            .ok_or(SpritePreparationError::LengthOverflow(
                "sprite source extent",
            ))?;
        pixels.extend_from_slice(
            source
                .get(start..end)
                .ok_or(SpritePreparationError::InvalidSpriteRect)?,
        );
    }
    RgbaImage::from_raw(width, height, pixels).ok_or(SpritePreparationError::InvalidSpriteRect)
}

fn strict_sprite_bounds(
    rect: SpritePixelRect,
    texture_width: u32,
    texture_height: u32,
) -> Result<(u32, u32, u32, u32), SpritePreparationError> {
    let right = rect
        .x()
        .checked_add(rect.width())
        .ok_or(SpritePreparationError::InvalidSpriteRect)?;
    let top = rect
        .y()
        .checked_add(rect.height())
        .ok_or(SpritePreparationError::InvalidSpriteRect)?;
    if right > texture_width || top > texture_height {
        return Err(SpritePreparationError::InvalidSpriteRect);
    }
    Ok((rect.x(), texture_height - top, rect.width(), rect.height()))
}

#[derive(Debug, Error)]
pub enum SpritePreparationError {
    #[error("sprite rectangle is empty, non-finite, or outside its texture")]
    InvalidSpriteRect,
    #[error("sprite {0} length overflows its supported domain")]
    LengthOverflow(&'static str),
    #[error("failed to allocate {resource} ({requested} bytes): {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    Texture(#[from] TexturePreparationError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Descriptor(#[from] MediaDescriptorError),
    #[error("failed to write prepared Sprite PNG: {0}")]
    Output(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use unity_asset_binary::asset::{ObjectInfo, class_ids};
    use unity_asset_binary::object::UnityObject;
    use unity_asset_core::{AssetLoadLimits, UnityClass, UnityValue};

    use super::*;

    fn object(
        class_id: i32,
        class_name: &str,
        properties: IndexMap<String, UnityValue>,
    ) -> UnityObject {
        let class = UnityClass::with_properties(
            class_id,
            class_name.to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_id).unwrap();
        UnityObject::from_info_and_class(info, class)
    }

    fn sprite_object() -> UnityObject {
        object(
            class_ids::SPRITE,
            "Sprite",
            IndexMap::from([
                (
                    "m_RD".to_owned(),
                    UnityValue::Object(IndexMap::from([
                        ("settingsRaw".to_owned(), UnityValue::Integer(0)),
                        (
                            "texture".to_owned(),
                            UnityValue::Object(IndexMap::from([
                                ("m_FileID".to_owned(), UnityValue::Integer(0)),
                                ("m_PathID".to_owned(), UnityValue::Integer(1)),
                            ])),
                        ),
                        (
                            "textureRect".to_owned(),
                            UnityValue::Object(IndexMap::from([
                                ("x".to_owned(), UnityValue::Float(1.0)),
                                ("y".to_owned(), UnityValue::Float(1.0)),
                                ("width".to_owned(), UnityValue::Float(1.0)),
                                ("height".to_owned(), UnityValue::Float(1.0)),
                            ])),
                        ),
                    ])),
                ),
                (
                    "m_Rect".to_owned(),
                    UnityValue::Object(IndexMap::from([
                        ("x".to_owned(), UnityValue::Float(0.0)),
                        ("y".to_owned(), UnityValue::Float(0.0)),
                        ("width".to_owned(), UnityValue::Float(1.0)),
                        ("height".to_owned(), UnityValue::Float(1.0)),
                    ])),
                ),
            ]),
        )
    }

    fn texture_object(source: &[u8]) -> UnityObject {
        object(
            class_ids::TEXTURE_2D,
            "Texture2D",
            IndexMap::from([
                ("m_Width".to_owned(), UnityValue::Integer(2)),
                ("m_Height".to_owned(), UnityValue::Integer(2)),
                ("m_TextureFormat".to_owned(), UnityValue::Integer(4)),
                ("m_MipCount".to_owned(), UnityValue::Integer(1)),
                ("m_ImageCount".to_owned(), UnityValue::Integer(1)),
                ("m_TextureDimension".to_owned(), UnityValue::Integer(2)),
                ("m_CompleteImageSize".to_owned(), UnityValue::Integer(16)),
                ("image_data".to_owned(), UnityValue::Bytes(source.to_vec())),
            ]),
        )
    }

    fn budgeted_source(source: Vec<u8>, budget: &mut AssetLoadBudget) -> BudgetedMediaBytes {
        BudgetedMediaBytes::from_vec(source, "test sprite texture source", budget).unwrap()
    }

    #[test]
    fn sprite_preparation_budget_has_exact_and_one_short_boundaries() {
        let source = vec![
            255, 0, 0, 255, 0, 255, 0, 255, // Unity bottom row
            0, 0, 255, 255, 255, 255, 0, 255, // Unity top row
        ];
        let sprite = sprite_object();
        let texture = texture_object(&source);
        let sprite_layout = SpriteLayout::inspect(&sprite).unwrap();
        let texture_layout = Texture2DLayout::inspect_for_test(&texture, Some(5)).unwrap();
        let mut measured = AssetLoadBudget::default();
        let measured_source = budgeted_source(source.clone(), &mut measured);
        let prepared = PreparedSpritePng::prepare(
            sprite_layout,
            texture_layout,
            measured_source,
            &mut measured,
        )
        .unwrap();
        assert_eq!(
            prepared.descriptor().dimensions(),
            Some(MediaDimensions::new(1, 1).unwrap())
        );
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgba8();
        assert_eq!(image.get_pixel(0, 0).0, [255, 255, 0, 255]);
        let usage = measured.usage();
        let limits = AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        };

        let mut exact = AssetLoadBudget::new(limits).unwrap();
        let exact_source = budgeted_source(source.clone(), &mut exact);
        PreparedSpritePng::prepare(sprite_layout, texture_layout, exact_source, &mut exact)
            .unwrap();
        assert_eq!(exact.usage().bytes, usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..limits
        })
        .unwrap();
        let one_short_source = budgeted_source(source, &mut one_short);
        assert!(matches!(
            PreparedSpritePng::prepare(
                sprite_layout,
                texture_layout,
                one_short_source,
                &mut one_short
            ),
            Err(SpritePreparationError::Budget(BudgetError::Exceeded { .. }))
                | Err(SpritePreparationError::Texture(
                    TexturePreparationError::Budget(BudgetError::Exceeded { .. })
                ))
        ));
    }
}
