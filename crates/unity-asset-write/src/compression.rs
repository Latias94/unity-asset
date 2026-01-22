use std::io::{BufReader, Cursor};

use unity_asset_core::{Result, UnityAssetError};

/// Compress data using UnityPy-style LZ4 block compression.
///
/// UnityPy uses `lz4.block.compress(..., store_size=False)`, which maps to the raw LZ4 block
/// format (size must be provided externally for decompression).
pub fn compress_lz4(data: &[u8]) -> Vec<u8> {
    lz4_flex::block::compress(data)
}

/// Compress data using UnityPy-style Brotli compression.
///
/// Note: UnityPy uses `brotli.compress(data)` with default parameters.
pub fn compress_brotli(data: &[u8]) -> Vec<u8> {
    use std::io::Write;

    // UnityPy uses defaults; this matches the common defaults (quality=11, lgwin=22).
    let mut out = Vec::new();
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, 11, 22);
        w.write_all(data)
            .expect("writing brotli-compressed bytes into Vec should not fail");
    }
    out
}

/// Compress data using UnityPy-style GZIP compression.
///
/// UnityPy uses `gzip.compress(data)` which defaults to `compresslevel=9` and `mtime=0`.
pub fn compress_gzip(data: &[u8]) -> Vec<u8> {
    use flate2::{Compression, GzBuilder};
    use std::io::Write;

    let mut encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::best());
    encoder
        .write_all(data)
        .expect("writing gzip-compressed bytes into Vec should not fail");
    encoder
        .finish()
        .expect("finishing gzip-compressed bytes into Vec should not fail")
}

/// Compress data using UnityPy-style Unity LZMA encoding.
///
/// UnityPy's `compress_lzma(..., write_decompressed_size=False)` produces:
/// - 1 byte: props
/// - 4 bytes: dict size (LE)
/// - raw LZMA1 stream (no 8-byte unpacked size field)
///
/// `lzma-rs` outputs the "LZMA-Alone" header which includes an extra 8-byte unpacked size.
/// We strip that field to match UnityPy.
pub fn compress_lzma_unity(data: &[u8]) -> Result<Vec<u8>> {
    compress_lzma_unity_impl(data, None)
}

/// Compress data using UnityPy-style Unity LZMA encoding with an explicit unpacked size.
///
/// This matches UnityPy's `compress_lzma(..., write_decompressed_size=True)` layout:
/// props + dict_size + unpacked_size(u64 LE) + raw stream.
pub fn compress_lzma_unity_with_size(data: &[u8]) -> Result<Vec<u8>> {
    compress_lzma_unity_impl(data, Some(data.len() as u64))
}

fn compress_lzma_unity_impl(data: &[u8], unpacked_size: Option<u64>) -> Result<Vec<u8>> {
    let mut input = Cursor::new(data);
    let mut input = BufReader::new(&mut input);
    let mut out = Vec::new();

    let options = lzma_rs::compress::Options {
        unpacked_size: lzma_rs::compress::UnpackedSize::WriteToHeader(unpacked_size),
    };

    lzma_rs::lzma_compress_with_options(&mut input, &mut out, &options)?;

    // LZMA-Alone header is: props(1) + dict(4 LE) + unpacked_size(8 LE) = 13 bytes.
    if out.len() < 13 {
        return Err(UnityAssetError::format(format!(
            "LZMA output too short for header: {}",
            out.len()
        )));
    }

    if unpacked_size.is_some() {
        return Ok(out);
    }

    // Strip the unpacked-size field to match UnityPy's 5-byte header.
    let mut unity = Vec::with_capacity(out.len().saturating_sub(8));
    unity.extend_from_slice(&out[0..5]);
    unity.extend_from_slice(&out[13..]);
    Ok(unity)
}
