//! Strict, budgeted Texture2D-to-PNG preparation.

use std::collections::TryReserveError;
use std::io::{self, Write};

use crc32fast::Hasher;
use image::RgbaImage;
use thiserror::Error;
use unity_asset_core::{AssetLoadBudget, BudgetError};

use super::decoders::{TextureDecodeFailure, TextureDecoder};
use super::inspection::{Texture2DLayout, TextureStorageLayout};
use crate::descriptor::{
    MediaDescriptor, MediaDescriptorError, MediaDimensions, MediaOutputEstimate,
    UnityTextureEncoding,
};
use crate::media::BudgetedMediaBytes;
use unity_asset_binary::BinaryError;

/// Prepared PNG bytes and their closed media descriptor.
pub struct PreparedTexturePng {
    descriptor: MediaDescriptor,
    bytes: Vec<u8>,
}

pub(crate) struct PreparedTextureImage {
    pub(crate) source_length: u64,
    pub(crate) encoding: UnityTextureEncoding,
    pub(crate) image: RgbaImage,
}

impl PreparedTexturePng {
    /// Conservative PNG bound when only the RGBA byte count is known.
    pub fn output_bound_for_rgba(rgba_bytes: u64) -> Result<u64, TexturePreparationError> {
        png_output_bound(rgba_bytes)
    }

    /// Strictly validates, decodes, and encodes one Texture2D before publication begins.
    pub fn prepare(
        layout: Texture2DLayout<'_>,
        source: BudgetedMediaBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, TexturePreparationError> {
        let prepared = PreparedTextureImage::prepare(layout, source, budget)?;
        let dimensions = MediaDimensions::new(prepared.image.width(), prepared.image.height())?;
        let bytes = encode_png(&prepared.image, budget)?;
        let output_length = u64::try_from(bytes.len())
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG output"))?;
        let descriptor = MediaDescriptor::texture_png(
            prepared.encoding,
            dimensions,
            prepared.source_length,
            MediaOutputEstimate::exact(output_length)?,
        )?;
        Ok(Self { descriptor, bytes })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &MediaDescriptor {
        &self.descriptor
    }

    /// Writes the prepared artifact without reparsing or re-encoding source data.
    pub fn write_to<W: Write + ?Sized>(
        &self,
        writer: &mut W,
    ) -> Result<(), TexturePreparationError> {
        writer
            .write_all(&self.bytes)
            .map_err(TexturePreparationError::Output)
    }
}

impl PreparedTextureImage {
    pub(crate) fn prepare(
        layout: Texture2DLayout<'_>,
        source: BudgetedMediaBytes,
        budget: &mut AssetLoadBudget,
    ) -> Result<Self, TexturePreparationError> {
        let source_length = u64::try_from(source.len())
            .map_err(|_| TexturePreparationError::LengthOverflow("texture source"))?;
        if source_length != layout.complete_image_size() {
            return Err(TexturePreparationError::SourceLengthMismatch {
                declared: layout.complete_image_size(),
                actual: source_length,
            });
        }
        let encoding = layout
            .format()
            .descriptor_encoding()
            .ok_or(TexturePreparationError::UnsupportedFormat(layout.format()))?;
        let source = prepare_platform_source(layout, source.into_vec(budget)?, budget)?;
        let mut image = TextureDecoder::new()
            .decode_prepared(
                layout.width(),
                layout.height(),
                layout.format(),
                &source,
                budget,
            )
            .map_err(map_decode_failure)?;
        normalize_top_left(&mut image);

        Ok(Self {
            source_length,
            encoding,
            image,
        })
    }
}

fn prepare_platform_source(
    layout: Texture2DLayout<'_>,
    mut source: Vec<u8>,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, TexturePreparationError> {
    match layout.context().storage() {
        TextureStorageLayout::Linear | TextureStorageLayout::SwitchLinear => Ok(source),
        TextureStorageLayout::Xbox360WordSwapped => {
            for word in source.chunks_mut(2) {
                word.reverse();
            }
            Ok(source)
        }
        TextureStorageLayout::SwitchBlockLinear {
            block_width,
            block_height,
            gobs_per_block,
        } => {
            let requested = source.len();
            let requested_u64 = u64::try_from(requested)
                .map_err(|_| TexturePreparationError::LengthOverflow("Switch texture source"))?;
            budget.check_bytes(requested_u64)?;
            let mut linear = Vec::new();
            linear.try_reserve_exact(requested).map_err(|source| {
                TexturePreparationError::Allocation {
                    resource: "Nintendo Switch linear texture",
                    requested,
                    source,
                }
            })?;
            let retained = u64::try_from(linear.capacity())
                .map_err(|_| TexturePreparationError::LengthOverflow("Switch linear texture"))?;
            budget.consume_bytes(retained)?;
            linear.resize(requested, 0);
            deswizzle_switch_block_linear(
                &source,
                &mut linear,
                layout.width(),
                layout.height(),
                block_width,
                block_height,
                gobs_per_block,
            )
            .map_err(TexturePreparationError::Decode)?;
            Ok(linear)
        }
    }
}

fn normalize_top_left(image: &mut RgbaImage) {
    image::imageops::flip_vertical_in_place(image);
}

fn deswizzle_switch_block_linear(
    source: &[u8],
    output: &mut [u8],
    width: u32,
    height: u32,
    block_width: u8,
    block_height: u8,
    gobs_per_block: u8,
) -> Result<(), BinaryError> {
    const GOB_WIDTH_UNITS: usize = 4;
    const GOB_HEIGHT_UNITS: usize = 8;
    const STORAGE_UNIT_BYTES: usize = 16;

    let block_width = usize::from(block_width);
    let block_height = usize::from(block_height);
    let gobs_per_block = usize::from(gobs_per_block);
    let width = usize::try_from(width)
        .map_err(|_| BinaryError::invalid_data("Switch texture width exceeds usize"))?;
    let height = usize::try_from(height)
        .map_err(|_| BinaryError::invalid_data("Switch texture height exceeds usize"))?;
    if block_width == 0 || block_height == 0 || gobs_per_block == 0 {
        return Err(BinaryError::invalid_data(
            "Switch texture storage geometry contains zero",
        ));
    }
    if width % block_width != 0 || height % block_height != 0 {
        return Err(BinaryError::invalid_data(
            "Switch texture dimensions are not block aligned",
        ));
    }
    let units_x = width / block_width;
    let units_y = height / block_height;
    let block_rows = GOB_HEIGHT_UNITS
        .checked_mul(gobs_per_block)
        .ok_or_else(|| BinaryError::invalid_data("Switch block height overflows usize"))?;
    if units_x % GOB_WIDTH_UNITS != 0 || units_y % block_rows != 0 {
        return Err(BinaryError::invalid_data(
            "Switch texture is not aligned to complete GOB blocks",
        ));
    }
    let expected = units_x
        .checked_mul(units_y)
        .and_then(|units| units.checked_mul(STORAGE_UNIT_BYTES))
        .ok_or_else(|| BinaryError::invalid_data("Switch texture storage size overflows"))?;
    if source.len() != expected || output.len() != expected {
        return Err(BinaryError::invalid_data(
            "Switch texture storage length does not match its geometry",
        ));
    }

    let mut source_offset = 0_usize;
    for block_y in 0..(units_y / block_rows) {
        for block_x in 0..(units_x / GOB_WIDTH_UNITS) {
            let base_x = block_x * GOB_WIDTH_UNITS;
            for gob_y in 0..gobs_per_block {
                let base_y = (block_y * gobs_per_block + gob_y) * GOB_HEIGHT_UNITS;
                for ordinal in 0..32_usize {
                    let local_x = ((ordinal >> 3) & 0b10) | ((ordinal >> 1) & 0b1);
                    let local_y = ((ordinal >> 1) & 0b110) | (ordinal & 0b1);
                    let destination = (base_y + local_y)
                        .checked_mul(units_x)
                        .and_then(|offset| offset.checked_add(base_x + local_x))
                        .and_then(|offset| offset.checked_mul(STORAGE_UNIT_BYTES))
                        .ok_or_else(|| {
                            BinaryError::invalid_data("Switch destination offset overflows")
                        })?;
                    let source_end =
                        source_offset
                            .checked_add(STORAGE_UNIT_BYTES)
                            .ok_or_else(|| {
                                BinaryError::invalid_data("Switch source offset overflows")
                            })?;
                    let destination_end =
                        destination.checked_add(STORAGE_UNIT_BYTES).ok_or_else(|| {
                            BinaryError::invalid_data("Switch destination extent overflows")
                        })?;
                    output
                        .get_mut(destination..destination_end)
                        .ok_or_else(|| {
                            BinaryError::invalid_data("Switch destination exceeds output")
                        })?
                        .copy_from_slice(source.get(source_offset..source_end).ok_or_else(
                            || BinaryError::invalid_data("Switch source ends inside a GOB"),
                        )?);
                    source_offset = source_end;
                }
            }
        }
    }
    if source_offset != source.len() {
        return Err(BinaryError::invalid_data(
            "Switch deswizzle did not consume the complete source",
        ));
    }
    Ok(())
}

fn map_decode_failure(error: TextureDecodeFailure) -> TexturePreparationError {
    match error {
        TextureDecodeFailure::Decode(error) => TexturePreparationError::Decode(error),
        TextureDecodeFailure::Budget(error) => TexturePreparationError::Budget(error),
        TextureDecodeFailure::Allocation {
            resource,
            requested,
            source,
        } => TexturePreparationError::Allocation {
            resource,
            requested,
            source,
        },
    }
}

pub(crate) fn encode_png(
    image: &RgbaImage,
    budget: &mut AssetLoadBudget,
) -> Result<Vec<u8>, TexturePreparationError> {
    let output_length = png_output_length(image.width(), image.height())?;
    let mut output = ReservedVecWriter::new(output_length, budget)?;
    output.extend(PNG_SIGNATURE)?;

    let mut ihdr = [0_u8; 13];
    ihdr[..4].copy_from_slice(&image.width().to_be_bytes());
    ihdr[4..8].copy_from_slice(&image.height().to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    write_chunk(&mut output, *b"IHDR", &[&ihdr])?;

    let mut filtered = FilteredRgba::new(image)?;
    let block_count = stored_block_count(filtered.remaining())?;
    let mut adler = Adler32::new();
    for block_index in 0..block_count {
        let first = block_index == 0;
        let final_block = block_index + 1 == block_count;
        let block_length = filtered.remaining().min(DEFLATE_STORED_BLOCK_MAX);
        let block_length_u16 = u16::try_from(block_length)
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG DEFLATE block"))?;
        let zlib_prefix = first.then_some([0x78, 0x01]);
        let block_header = [u8::from(final_block)];
        let length = block_length_u16.to_le_bytes();
        let inverse_length = (!block_length_u16).to_le_bytes();
        let checksum_length = if final_block {
            std::mem::size_of::<u32>()
        } else {
            0
        };
        let chunk_length = block_length
            .checked_add(DEFLATE_STORED_HEADER_BYTES)
            .and_then(|length| length.checked_add(zlib_prefix.map_or(0, |_| 2)))
            .and_then(|length| length.checked_add(checksum_length))
            .ok_or(TexturePreparationError::LengthOverflow("PNG IDAT chunk"))?;
        let mut chunk = PngChunkWriter::new(&mut output, *b"IDAT", chunk_length)?;
        if let Some(prefix) = zlib_prefix {
            chunk.write(&prefix)?;
        }
        chunk.write(&block_header)?;
        chunk.write(&length)?;
        chunk.write(&inverse_length)?;
        filtered.write_exact(block_length, &mut chunk, &mut adler)?;
        if final_block {
            chunk.write(&adler.finish().to_be_bytes())?;
        }
        chunk.finish()?;
    }
    write_chunk(&mut output, *b"IEND", &[])?;
    if output.len() != usize::try_from(output_length).unwrap_or(usize::MAX) {
        return Err(TexturePreparationError::LengthOverflow(
            "prepared PNG length mismatch",
        ));
    }
    Ok(output.into_inner())
}

fn png_output_bound(rgba_bytes: u64) -> Result<u64, TexturePreparationError> {
    let maximum_rows = rgba_bytes / RGBA_BYTES_PER_PIXEL_U64;
    let filtered_bytes =
        rgba_bytes
            .checked_add(maximum_rows)
            .ok_or(TexturePreparationError::LengthOverflow(
                "PNG filtered input",
            ))?;
    png_length_from_filtered_bytes(filtered_bytes, "PNG output bound")
}

fn png_output_length(width: u32, height: u32) -> Result<u64, TexturePreparationError> {
    let row_bytes = u64::from(width)
        .checked_mul(RGBA_BYTES_PER_PIXEL_U64)
        .ok_or(TexturePreparationError::LengthOverflow("PNG row"))?;
    let filtered_bytes = row_bytes
        .checked_add(1)
        .and_then(|row| row.checked_mul(u64::from(height)))
        .ok_or(TexturePreparationError::LengthOverflow(
            "PNG filtered input",
        ))?;
    png_length_from_filtered_bytes(filtered_bytes, "PNG output")
}

fn png_length_from_filtered_bytes(
    filtered_bytes: u64,
    resource: &'static str,
) -> Result<u64, TexturePreparationError> {
    let blocks = div_ceil_u64(filtered_bytes, DEFLATE_STORED_BLOCK_MAX_U64)?;
    filtered_bytes
        .checked_add(
            blocks
                .checked_mul(PNG_IDAT_BLOCK_OVERHEAD_U64)
                .ok_or(TexturePreparationError::LengthOverflow("PNG IDAT overhead"))?,
        )
        .and_then(|bytes| bytes.checked_add(PNG_FIXED_BYTES_U64))
        .ok_or(TexturePreparationError::LengthOverflow(resource))
}

fn div_ceil_u64(value: u64, divisor: u64) -> Result<u64, TexturePreparationError> {
    value
        .checked_add(divisor - 1)
        .map(|value| value / divisor)
        .ok_or(TexturePreparationError::LengthOverflow("PNG block count"))
}

fn stored_block_count(filtered_bytes: usize) -> Result<usize, TexturePreparationError> {
    filtered_bytes
        .checked_add(DEFLATE_STORED_BLOCK_MAX - 1)
        .map(|bytes| bytes / DEFLATE_STORED_BLOCK_MAX)
        .filter(|blocks| *blocks != 0)
        .ok_or(TexturePreparationError::LengthOverflow(
            "PNG DEFLATE block count",
        ))
}

fn write_chunk(
    output: &mut ReservedVecWriter,
    kind: [u8; 4],
    segments: &[&[u8]],
) -> Result<(), TexturePreparationError> {
    let length = segments
        .iter()
        .try_fold(0_usize, |length, segment| length.checked_add(segment.len()));
    let length = length.ok_or(TexturePreparationError::LengthOverflow("PNG chunk"))?;
    let mut chunk = PngChunkWriter::new(output, kind, length)?;
    for segment in segments {
        chunk.write(segment)?;
    }
    chunk.finish()
}

struct PngChunkWriter<'output> {
    output: &'output mut ReservedVecWriter,
    hasher: Hasher,
    remaining: usize,
}

impl<'output> PngChunkWriter<'output> {
    fn new(
        output: &'output mut ReservedVecWriter,
        kind: [u8; 4],
        length: usize,
    ) -> Result<Self, TexturePreparationError> {
        let length_u32 = u32::try_from(length)
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG chunk"))?;
        output.extend(&length_u32.to_be_bytes())?;
        output.extend(&kind)?;
        let mut hasher = Hasher::new();
        hasher.update(&kind);
        Ok(Self {
            output,
            hasher,
            remaining: length,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), TexturePreparationError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or(TexturePreparationError::LengthOverflow("PNG chunk payload"))?;
        self.hasher.update(bytes);
        self.output.extend(bytes)
    }

    fn finish(self) -> Result<(), TexturePreparationError> {
        if self.remaining != 0 {
            return Err(TexturePreparationError::LengthOverflow("PNG chunk payload"));
        }
        let checksum = self.hasher.finalize();
        self.output.extend(&checksum.to_be_bytes())
    }
}

struct FilteredRgba<'image> {
    bytes: &'image [u8],
    row_bytes: usize,
    source_offset: usize,
    row_offset: usize,
    filter_pending: bool,
    remaining: usize,
}

impl<'image> FilteredRgba<'image> {
    fn new(image: &'image RgbaImage) -> Result<Self, TexturePreparationError> {
        let row_bytes = usize::try_from(image.width())
            .ok()
            .and_then(|width| width.checked_mul(RGBA_BYTES_PER_PIXEL))
            .ok_or(TexturePreparationError::LengthOverflow("PNG row"))?;
        let height = usize::try_from(image.height())
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG height"))?;
        let remaining = row_bytes
            .checked_add(1)
            .and_then(|row| row.checked_mul(height))
            .ok_or(TexturePreparationError::LengthOverflow(
                "PNG filtered input",
            ))?;
        Ok(Self {
            bytes: image.as_raw(),
            row_bytes,
            source_offset: 0,
            row_offset: 0,
            filter_pending: true,
            remaining,
        })
    }

    const fn remaining(&self) -> usize {
        self.remaining
    }

    fn write_exact(
        &mut self,
        mut length: usize,
        output: &mut PngChunkWriter<'_>,
        adler: &mut Adler32,
    ) -> Result<(), TexturePreparationError> {
        self.remaining =
            self.remaining
                .checked_sub(length)
                .ok_or(TexturePreparationError::LengthOverflow(
                    "PNG filtered input",
                ))?;
        while length != 0 {
            if self.filter_pending {
                output.write(&[0])?;
                adler.update(&[0]);
                self.filter_pending = false;
                length -= 1;
                continue;
            }
            let available = self.row_bytes - self.row_offset;
            let amount = available.min(length);
            let end = self
                .source_offset
                .checked_add(amount)
                .ok_or(TexturePreparationError::LengthOverflow("PNG source offset"))?;
            let bytes = self
                .bytes
                .get(self.source_offset..end)
                .ok_or(TexturePreparationError::LengthOverflow("PNG source length"))?;
            output.write(bytes)?;
            adler.update(bytes);
            self.source_offset = end;
            self.row_offset += amount;
            length -= amount;
            if self.row_offset == self.row_bytes {
                self.row_offset = 0;
                self.filter_pending = self.source_offset < self.bytes.len();
            }
        }
        Ok(())
    }
}

struct Adler32 {
    first: u32,
    second: u32,
}

impl Adler32 {
    const fn new() -> Self {
        Self {
            first: 1,
            second: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(ADLER_REDUCTION_CHUNK) {
            for byte in chunk {
                self.first += u32::from(*byte);
                self.second += self.first;
            }
            self.first %= ADLER_MODULUS;
            self.second %= ADLER_MODULUS;
        }
    }

    const fn finish(&self) -> u32 {
        (self.second << 16) | self.first
    }
}

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const RGBA_BYTES_PER_PIXEL: usize = 4;
const RGBA_BYTES_PER_PIXEL_U64: u64 = 4;
const DEFLATE_STORED_BLOCK_MAX: usize = u16::MAX as usize;
const DEFLATE_STORED_BLOCK_MAX_U64: u64 = u16::MAX as u64;
const DEFLATE_STORED_HEADER_BYTES: usize = 5;
const PNG_CHUNK_OVERHEAD: usize = 12;
const PNG_IDAT_BLOCK_OVERHEAD: usize = DEFLATE_STORED_HEADER_BYTES + PNG_CHUNK_OVERHEAD;
const PNG_IDAT_BLOCK_OVERHEAD_U64: u64 = PNG_IDAT_BLOCK_OVERHEAD as u64;
const PNG_FIXED_BYTES: usize = PNG_SIGNATURE.len() + 25 + 12 + 2 + 4;
const PNG_FIXED_BYTES_U64: u64 = PNG_FIXED_BYTES as u64;
const ADLER_MODULUS: u32 = 65_521;
const ADLER_REDUCTION_CHUNK: usize = 5_552;

struct ReservedVecWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl ReservedVecWriter {
    fn new(maximum: u64, budget: &mut AssetLoadBudget) -> Result<Self, TexturePreparationError> {
        let maximum = usize::try_from(maximum)
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG output bound"))?;
        let maximum_u64 = u64::try_from(maximum)
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG output bound"))?;
        budget.check_bytes(maximum_u64)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum)
            .map_err(|source| TexturePreparationError::Allocation {
                resource: "PNG output",
                requested: maximum,
                source,
            })?;
        let retained = u64::try_from(bytes.capacity())
            .map_err(|_| TexturePreparationError::LengthOverflow("PNG output"))?;
        budget.consume_bytes(retained)?;
        Ok(Self { bytes, maximum })
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }

    const fn len(&self) -> usize {
        self.bytes.len()
    }

    fn extend(&mut self, buffer: &[u8]) -> Result<(), TexturePreparationError> {
        let end = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .ok_or(TexturePreparationError::LengthOverflow("PNG output"))?;
        if end > self.maximum {
            return Err(TexturePreparationError::LengthOverflow(
                "PNG output exceeds its prepared bound",
            ));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(())
    }
}

/// Strict texture preparation or publication failure.
#[derive(Debug, Error)]
pub enum TexturePreparationError {
    #[error(
        "texture source length does not match its descriptor: declared={declared}, actual={actual}"
    )]
    SourceLengthMismatch { declared: u64, actual: u64 },
    #[error("texture format {0:?} has no strict PNG preparation path")]
    UnsupportedFormat(super::formats::TextureFormat),
    #[error("texture {0} length overflows its supported domain")]
    LengthOverflow(&'static str),
    #[error("failed to allocate {resource} ({requested} bytes): {source}")]
    Allocation {
        resource: &'static str,
        requested: usize,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Descriptor(#[from] MediaDescriptorError),
    #[error("failed to decode strict Texture2D payload: {0}")]
    Decode(#[source] BinaryError),
    #[error("failed to write prepared Texture2D PNG: {0}")]
    Output(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;
    use unity_asset_core::{AssetLoadLimits, UnityClass, UnityValue};

    use super::*;
    use crate::descriptor::{CanonicalMediaExtension, MediaFamily, MediaMime};
    use unity_asset_binary::asset::{ObjectInfo, class_ids};
    use unity_asset_binary::object::UnityObject;

    fn inspect_layout(object: &UnityObject) -> Texture2DLayout<'_> {
        inspect_layout_for_platform(object, 5)
    }

    fn inspect_layout_for_platform(
        object: &UnityObject,
        target_platform: i32,
    ) -> Texture2DLayout<'_> {
        Texture2DLayout::inspect_for_test(object, Some(target_platform)).unwrap()
    }

    fn texture_object(
        format: super::super::formats::TextureFormat,
        width: i64,
        height: i64,
        bytes: &[u8],
        streamed: bool,
    ) -> UnityObject {
        texture_object_with_platform_blob(format, width, height, bytes, streamed, None)
    }

    fn texture_object_with_platform_blob(
        format: super::super::formats::TextureFormat,
        width: i64,
        height: i64,
        bytes: &[u8],
        streamed: bool,
        platform_blob: Option<Vec<u8>>,
    ) -> UnityObject {
        let mut properties = IndexMap::from([
            ("m_Width".to_owned(), UnityValue::Integer(width)),
            ("m_Height".to_owned(), UnityValue::Integer(height)),
            (
                "m_TextureFormat".to_owned(),
                UnityValue::Integer(i64::from(format as i32)),
            ),
            ("m_MipCount".to_owned(), UnityValue::Integer(1)),
            ("m_ImageCount".to_owned(), UnityValue::Integer(1)),
            ("m_TextureDimension".to_owned(), UnityValue::Integer(2)),
            (
                "m_CompleteImageSize".to_owned(),
                UnityValue::Integer(i64::try_from(bytes.len()).unwrap()),
            ),
        ]);
        if let Some(platform_blob) = platform_blob {
            properties.insert(
                "m_PlatformBlob".to_owned(),
                UnityValue::Bytes(platform_blob),
            );
        }
        if streamed {
            properties.insert(
                "m_StreamData".to_owned(),
                UnityValue::Object(IndexMap::from([
                    (
                        "path".to_owned(),
                        UnityValue::String("archive:/CAB-test/CAB-test.resS".to_owned()),
                    ),
                    ("offset".to_owned(), UnityValue::from(7_u64)),
                    (
                        "size".to_owned(),
                        UnityValue::from(u64::try_from(bytes.len()).unwrap()),
                    ),
                ])),
            );
        } else {
            properties.insert("image_data".to_owned(), UnityValue::Bytes(bytes.to_vec()));
        }
        let class = UnityClass::with_properties(
            class_ids::TEXTURE_2D,
            "Texture2D".to_owned(),
            "1".to_owned(),
            properties,
        );
        let info = ObjectInfo::for_standalone_class(1, 0, 0, class_ids::TEXTURE_2D).unwrap();
        UnityObject::from_info_and_class(info, class)
    }

    fn switch_swizzle_rgba32(linear: &[u8], width: usize, height: usize, gobs: usize) -> Vec<u8> {
        let units_x = width / 4;
        let units_y = height;
        let mut swizzled = vec![0_u8; linear.len()];
        let mut destination = 0_usize;
        for block_y in 0..(units_y / (8 * gobs)) {
            for block_x in 0..(units_x / 4) {
                let base_x = block_x * 4;
                for gob_y in 0..gobs {
                    let base_y = (block_y * gobs + gob_y) * 8;
                    for ordinal in 0..32_usize {
                        let local_x = ((ordinal >> 3) & 0b10) | ((ordinal >> 1) & 0b1);
                        let local_y = ((ordinal >> 1) & 0b110) | (ordinal & 0b1);
                        let source = ((base_y + local_y) * units_x + base_x + local_x) * 16;
                        swizzled[destination..destination + 16]
                            .copy_from_slice(&linear[source..source + 16]);
                        destination += 16;
                    }
                }
            }
        }
        assert_eq!(destination, swizzled.len());
        swizzled
    }

    fn rgba_layout() -> (UnityObject, Vec<u8>) {
        let bytes = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0];
        (
            texture_object(
                super::super::formats::TextureFormat::RGBA32,
                2,
                2,
                &bytes,
                false,
            ),
            bytes,
        )
    }

    fn budgeted_source(source: Vec<u8>, budget: &mut AssetLoadBudget) -> BudgetedMediaBytes {
        BudgetedMediaBytes::from_vec(source, "test texture source", budget).unwrap()
    }

    #[test]
    fn prepared_texture_owns_exact_png_identity() {
        let (object, source) = rgba_layout();
        let layout = inspect_layout(&object);
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(source, &mut budget);
        let prepared = PreparedTexturePng::prepare(layout, source, &mut budget).unwrap();

        assert_eq!(prepared.descriptor().family(), MediaFamily::Texture);
        assert_eq!(
            prepared.descriptor().canonical_extension(),
            CanonicalMediaExtension::Png
        );
        assert_eq!(prepared.descriptor().mime(), MediaMime::ImagePng);
        assert!(prepared.descriptor().output().is_exact());
        let mut output = Vec::new();
        prepared.write_to(&mut output).unwrap();
        assert_eq!(&output[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            u64::try_from(output.len()).unwrap(),
            prepared.descriptor().output().upper_bound()
        );
    }

    #[test]
    fn prepared_texture_normalizes_unity_rows_to_top_left_once() {
        let source = vec![
            255, 0, 0, 255, 0, 255, 0, 255, // Unity bottom row
            0, 0, 255, 255, 255, 255, 0, 255, // Unity top row
        ];
        let object = texture_object(
            super::super::formats::TextureFormat::RGBA32,
            2,
            2,
            &source,
            false,
        );
        let layout = inspect_layout(&object);
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(source, &mut budget);
        let prepared = PreparedTexturePng::prepare(layout, source, &mut budget).unwrap();
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgba8();

        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(image.get_pixel(1, 0).0, [255, 255, 0, 255]);
        assert_eq!(image.get_pixel(0, 1).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(1, 1).0, [0, 255, 0, 255]);
    }

    #[test]
    fn xbox_360_rgb565_words_are_restored_before_decode() {
        let source = vec![0xf8, 0x00];
        let object = texture_object(
            super::super::formats::TextureFormat::RGB565,
            1,
            1,
            &source,
            false,
        );
        let layout = inspect_layout_for_platform(&object, 11);
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(source, &mut budget);
        let prepared = PreparedTexturePng::prepare(layout, source, &mut budget).unwrap();
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgba8();

        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn xbox_360_argb4444_words_are_restored_before_decode() {
        let source = vec![0xfa, 0xbc];
        let object = texture_object(
            super::super::formats::TextureFormat::ARGB4444,
            1,
            1,
            &source,
            false,
        );
        let layout = inspect_layout_for_platform(&object, 11);
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(source, &mut budget);
        let prepared = PreparedTexturePng::prepare(layout, source, &mut budget).unwrap();
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgba8();

        assert_eq!(image.get_pixel(0, 0).0, [0xaa, 0xbb, 0xcc, 0xff]);
    }

    #[test]
    fn switch_block_linear_rgba_is_deswizzled_before_origin_normalization() {
        let width = 16_usize;
        let height = 16_usize;
        let mut linear = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                linear.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 255]);
            }
        }
        let swizzled = switch_swizzle_rgba32(&linear, width, height, 2);
        let mut platform_blob = vec![0_u8; 12];
        platform_blob[8..12].copy_from_slice(&1_u32.to_le_bytes());
        let object = texture_object_with_platform_blob(
            super::super::formats::TextureFormat::RGBA32,
            width as i64,
            height as i64,
            &swizzled,
            false,
            Some(platform_blob),
        );
        let layout = inspect_layout_for_platform(&object, 38);
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(swizzled, &mut budget);
        let prepared = PreparedTexturePng::prepare(layout, source, &mut budget).unwrap();
        let image = image::load_from_memory(&prepared.bytes).unwrap().to_rgba8();

        assert_eq!(image.get_pixel(0, 0).0, [0, 15, 15, 255]);
        assert_eq!(image.get_pixel(13, 0).0, [13, 15, 2, 255]);
        assert_eq!(image.get_pixel(0, 15).0, [0, 0, 0, 255]);
    }

    #[test]
    fn prepared_png_streams_across_idat_blocks_with_an_exact_bound() {
        let image = RgbaImage::from_fn(512, 64, |x, y| {
            image::Rgba([
                x.to_le_bytes()[0],
                y.to_le_bytes()[0],
                x.wrapping_add(y).to_le_bytes()[0],
                255,
            ])
        });
        let mut budget = AssetLoadBudget::default();
        let encoded = encode_png(&image, &mut budget).unwrap();
        assert_eq!(
            u64::try_from(encoded.len()).unwrap(),
            png_output_length(image.width(), image.height()).unwrap()
        );

        let mut cursor = PNG_SIGNATURE.len();
        let mut idat_chunks = 0;
        while cursor < encoded.len() {
            let length = usize::try_from(u32::from_be_bytes(
                encoded[cursor..cursor + 4].try_into().unwrap(),
            ))
            .unwrap();
            if &encoded[cursor + 4..cursor + 8] == b"IDAT" {
                idat_chunks += 1;
            }
            cursor += PNG_CHUNK_OVERHEAD + length;
        }
        assert_eq!(cursor, encoded.len());
        assert!(idat_chunks >= 3);

        let decoded = image::load_from_memory_with_format(&encoded, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded, image);
    }

    #[test]
    fn rgba_only_bound_covers_the_tallest_possible_image() {
        let height = 70_000_u32;
        let rgba_bytes = u64::from(height) * RGBA_BYTES_PER_PIXEL_U64;
        assert!(png_output_bound(rgba_bytes).unwrap() >= png_output_length(1, height).unwrap());
    }

    #[test]
    fn texture_preparation_budget_has_exact_and_one_short_boundaries() {
        let (object, source) = rgba_layout();
        let layout = inspect_layout(&object);
        let mut measured = AssetLoadBudget::default();
        let measured_source = budgeted_source(source.clone(), &mut measured);
        PreparedTexturePng::prepare(layout, measured_source, &mut measured).unwrap();
        let usage = measured.usage();
        let limits = AssetLoadLimits {
            max_bytes: usage.bytes,
            ..AssetLoadLimits::default()
        };

        let mut exact = AssetLoadBudget::new(limits).unwrap();
        let exact_source = budgeted_source(source.clone(), &mut exact);
        PreparedTexturePng::prepare(layout, exact_source, &mut exact).unwrap();
        assert_eq!(exact.usage().bytes, usage.bytes);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: usage.bytes - 1,
            ..limits
        })
        .unwrap();
        let one_short_source = budgeted_source(source, &mut one_short);
        assert!(matches!(
            PreparedTexturePng::prepare(layout, one_short_source, &mut one_short),
            Err(TexturePreparationError::Budget(
                BudgetError::Exceeded { .. }
            ))
        ));
    }

    #[test]
    fn inline_and_streamed_payloads_prepare_the_same_descriptor_and_png() {
        let bytes = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 0, 0, 0, 0];
        let inline_object = texture_object(
            super::super::formats::TextureFormat::RGBA32,
            2,
            2,
            &bytes,
            false,
        );
        let streamed_object = texture_object(
            super::super::formats::TextureFormat::RGBA32,
            2,
            2,
            &bytes,
            true,
        );
        let mut inline_budget = AssetLoadBudget::default();
        let inline_source = budgeted_source(bytes.clone(), &mut inline_budget);
        let inline = PreparedTexturePng::prepare(
            inspect_layout(&inline_object),
            inline_source,
            &mut inline_budget,
        )
        .unwrap();
        let mut streamed_budget = AssetLoadBudget::default();
        let streamed_source = budgeted_source(bytes, &mut streamed_budget);
        let streamed = PreparedTexturePng::prepare(
            inspect_layout(&streamed_object),
            streamed_source,
            &mut streamed_budget,
        )
        .unwrap();
        let mut inline_png = Vec::new();
        let mut streamed_png = Vec::new();
        inline.write_to(&mut inline_png).unwrap();
        streamed.write_to(&mut streamed_png).unwrap();

        assert_eq!(inline.descriptor(), streamed.descriptor());
        assert_eq!(inline_png, streamed_png);
    }

    #[test]
    fn preparation_rejects_source_truncation_after_inspection() {
        let (object, mut source) = rgba_layout();
        let layout = inspect_layout(&object);
        source.pop();
        let mut budget = AssetLoadBudget::default();
        let source = budgeted_source(source, &mut budget);

        assert!(matches!(
            PreparedTexturePng::prepare(layout, source, &mut budget),
            Err(TexturePreparationError::SourceLengthMismatch {
                declared: 16,
                actual: 15
            })
        ));
    }

    #[cfg(feature = "texture-advanced")]
    #[test]
    fn advertised_bc_formats_have_strict_prepared_png_paths() {
        use crate::descriptor::UnityTextureEncoding;
        use crate::texture::TextureFormat;

        for (format, source_length, encoding) in [
            (TextureFormat::BC4, 8, UnityTextureEncoding::Bc4),
            (TextureFormat::BC5, 16, UnityTextureEncoding::Bc5),
            (TextureFormat::BC7, 16, UnityTextureEncoding::Bc7),
        ] {
            let source = vec![0_u8; source_length];
            let object = texture_object(format, 4, 4, &source, false);
            let mut budget = AssetLoadBudget::default();
            let source = budgeted_source(source, &mut budget);
            let prepared =
                PreparedTexturePng::prepare(inspect_layout(&object), source, &mut budget)
                    .unwrap_or_else(|error| panic!("{format:?} preparation failed: {error}"));
            assert_eq!(prepared.descriptor().texture_encoding(), Some(encoding));
            let mut png = Vec::new();
            prepared.write_to(&mut png).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        }
    }
}
