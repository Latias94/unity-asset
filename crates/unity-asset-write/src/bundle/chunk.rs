use unity_asset_core::{Result, UnityAssetError};

use crate::{compress_lz4, compress_lzma_unity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnityFsBlockInfo {
    pub uncompressed_size: u32,
    pub compressed_size: u32,
    pub flags: u16,
}

pub(crate) fn chunk_based_compress(
    data: &[u8],
    block_info_flag: u32,
) -> Result<(Vec<u8>, Vec<UnityFsBlockInfo>)> {
    type CompressFn = fn(&[u8]) -> Result<Vec<u8>>;

    fn compress_lz4_result(chunk: &[u8]) -> Result<Vec<u8>> {
        Ok(compress_lz4(chunk))
    }

    let switch = block_info_flag & 0x3F;

    if switch == 0 {
        let len_u32 = u32::try_from(data.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS block too large for u32 size: {}",
                data.len()
            ))
        })?;
        let flags = u16::try_from(block_info_flag).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS block flag does not fit in u16: {}",
                block_info_flag
            ))
        })?;
        return Ok((
            data.to_vec(),
            vec![UnityFsBlockInfo {
                uncompressed_size: len_u32,
                compressed_size: len_u32,
                flags,
            }],
        ));
    }

    let (chunk_size, compress_fn): (usize, CompressFn) = match switch {
        1 => (usize::MAX, compress_lzma_unity),
        2 | 3 => (0x0002_0000, compress_lz4_result),
        other => {
            return Err(UnityAssetError::format(format!(
                "Unsupported UnityFS compression switch: {}",
                other
            )));
        }
    };

    let mut block_info = Vec::new();
    let mut compressed_data = Vec::new();

    let mut p = 0usize;
    let mut remaining = data.len();

    while remaining > chunk_size {
        let chunk = &data[p..p + chunk_size];
        let c = compress_fn(chunk)?;
        if c.len() > chunk_size {
            // Compression is worse than original: store raw chunk and flip the compression bits.
            compressed_data.extend_from_slice(chunk);
            block_info.push(make_block_info(
                chunk_size,
                chunk_size,
                block_info_flag ^ switch,
            )?);
        } else {
            compressed_data.extend_from_slice(&c);
            block_info.push(make_block_info(chunk_size, c.len(), block_info_flag)?);
        }
        p += chunk_size;
        remaining -= chunk_size;
    }

    if remaining > 0 {
        let chunk = &data[p..];
        let c = compress_fn(chunk)?;
        if c.len() > remaining {
            compressed_data.extend_from_slice(chunk);
            block_info.push(make_block_info(
                remaining,
                remaining,
                block_info_flag ^ switch,
            )?);
        } else {
            compressed_data.extend_from_slice(&c);
            block_info.push(make_block_info(remaining, c.len(), block_info_flag)?);
        }
    }

    Ok((compressed_data, block_info))
}

fn make_block_info(uncompressed: usize, compressed: usize, flags: u32) -> Result<UnityFsBlockInfo> {
    let uncompressed_size = u32::try_from(uncompressed).map_err(|_| {
        UnityAssetError::format(format!(
            "UnityFS block too large for u32 size: {}",
            uncompressed
        ))
    })?;
    let compressed_size = u32::try_from(compressed).map_err(|_| {
        UnityAssetError::format(format!(
            "UnityFS block too large for u32 size: {}",
            compressed
        ))
    })?;
    let flags = u16::try_from(flags).map_err(|_| {
        UnityAssetError::format(format!("UnityFS block flag does not fit in u16: {}", flags))
    })?;
    Ok(UnityFsBlockInfo {
        uncompressed_size,
        compressed_size,
        flags,
    })
}
