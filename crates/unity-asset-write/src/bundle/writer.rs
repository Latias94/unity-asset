use unity_asset_binary::bundle::{AssetBundle, BundleHeader};
use unity_asset_binary::unity_version::UnityVersion;

use std::collections::HashSet;
use unity_asset_core::{Result, UnityAssetError};

use crate::bundle::BundleEdits;
use crate::bundle::chunk::chunk_based_compress;
use crate::{
    BinaryWriter, Endian, PackerOptions, UnityPyPacker, compress_lz4, compress_lzma_unity,
};

pub struct BundleWriter;

impl BundleWriter {
    /// Save/repack a bundle.
    ///
    /// Currently, only UnityFS saving is implemented (UnityPy `save_fs` parity).
    pub fn save(
        bundle: &AssetBundle,
        edits: &BundleEdits,
        options: PackerOptions,
    ) -> Result<Vec<u8>> {
        match bundle.header.signature.as_str() {
            "UnityFS" => Self::save_unityfs(bundle, edits, options),
            other => Err(UnityAssetError::format(format!(
                "Bundle saving not implemented for signature: {}",
                other
            ))),
        }
    }

    /// UnityFS (`BundleFile.save_fs`) implementation.
    pub fn save_unityfs(
        bundle: &AssetBundle,
        edits: &BundleEdits,
        options: PackerOptions,
    ) -> Result<Vec<u8>> {
        let header = &bundle.header;

        let (data_flag, block_info_flag) = resolve_unityfs_flags(header, bundle, options)?;

        if (data_flag & 0x40) == 0 {
            return Err(UnityAssetError::format(
                "UnityFS writer requires DirectoryInfo (data_flag must include 0x40)",
            ));
        }

        // Build the concatenated file data stream and the directory table (name, flags, length).
        let mut data_writer = BinaryWriter::new(Endian::Big);
        let mut files: Vec<(String, u32, u64)> = Vec::new();
        let mut existing_names: HashSet<&str> = HashSet::new();

        for node in &bundle.nodes {
            if !node.is_file() {
                continue;
            }

            let bytes = if let Some(replaced) = edits.get(&node.name) {
                replaced.to_vec()
            } else {
                bundle.extract_node_data(node).map_err(|e| {
                    UnityAssetError::with_source(
                        format!("Failed to extract bundle node data: {}", node.name),
                        e,
                    )
                })?
            };

            existing_names.insert(node.name.as_str());

            let len_u64 = bytes.len() as u64;
            data_writer.write(&bytes);
            let flags = edits.flags(&node.name).unwrap_or(node.flags);
            files.push((node.name.clone(), flags, len_u64));
        }

        // Append new files that were not present in the original bundle.
        let mut extra: Vec<(&str, &[u8])> = edits
            .iter()
            .filter(|(name, _)| !existing_names.contains(*name))
            .collect();
        extra.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, bytes) in extra {
            let len_u64 = bytes.len() as u64;
            data_writer.write(bytes);
            let flags = edits.flags(name).unwrap_or(0);
            files.push((name.to_string(), flags, len_u64));
        }

        let file_data = data_writer.into_bytes();

        // Compress the file data into UnityFS blocks (UnityPy chunk_based_compress).
        let (file_data, block_info) = chunk_based_compress(&file_data, block_info_flag)?;

        // Build block info (uncompressed) = hash(16) + block table + directory table.
        let mut block_writer = BinaryWriter::new(Endian::Big);
        block_writer.write(&[0u8; 16]);

        let block_count_i32 = i32::try_from(block_info.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS block count too large for i32: {}",
                block_info.len()
            ))
        })?;
        block_writer.write_i32(block_count_i32);
        for b in &block_info {
            block_writer.write_u32(b.uncompressed_size);
            block_writer.write_u32(b.compressed_size);
            block_writer.write_u16(b.flags);
        }

        let file_count_i32 = i32::try_from(files.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS file count too large for i32: {}",
                files.len()
            ))
        })?;
        block_writer.write_i32(file_count_i32);

        let mut offset: i64 = 0;
        for (name, flags, len) in &files {
            block_writer.write_i64(offset);
            block_writer.write_i64(*len as i64);
            offset = offset
                .checked_add(*len as i64)
                .ok_or_else(|| UnityAssetError::format("UnityFS directory offset overflow"))?;
            block_writer.write_u32(*flags);
            block_writer.write_string_to_null(name);
        }

        let uncompressed_block_data = block_writer.into_bytes();

        let block_data = compress_unityfs_blob(&uncompressed_block_data, data_flag & 0x3F)?;

        // Write bundle header + payload (UnityPy ordering and alignment).
        let uses_block_alignment = unityfs_uses_block_alignment(header);

        let mut writer = BinaryWriter::new(Endian::Big);
        writer.write_string_to_null(&header.signature);
        writer.write_u32(header.version);
        writer.write_string_to_null(&header.unity_version);
        writer.write_string_to_null(&header.unity_revision);

        let writer_header_pos = writer.position();
        writer.write_i64(0); // bundle_size placeholder
        writer.write_u32(u32::try_from(block_data.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS compressed blocks info too large for u32: {}",
                block_data.len()
            ))
        })?);
        writer.write_u32(u32::try_from(uncompressed_block_data.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "UnityFS uncompressed blocks info too large for u32: {}",
                uncompressed_block_data.len()
            ))
        })?);
        writer.write_u32(data_flag);

        if uses_block_alignment {
            writer.align_stream(16);
        }

        if (data_flag & 0x80) != 0 {
            // BlocksInfoAtEnd
            if (data_flag & 0x200) != 0 {
                writer.align_stream(16);
            }
            writer.write(&file_data);
            writer.write(&block_data);
        } else {
            writer.write(&block_data);
            if (data_flag & 0x200) != 0 {
                writer.align_stream(16);
            }
            writer.write(&file_data);
        }

        let writer_end_pos = writer.position();
        writer.set_position(writer_header_pos);
        writer.write_i64(writer_end_pos as i64);
        writer.set_position(writer_end_pos);

        Ok(writer.into_bytes())
    }
}

fn resolve_unityfs_flags(
    header: &BundleHeader,
    bundle: &AssetBundle,
    options: PackerOptions,
) -> Result<(u32, u32)> {
    match options.packer {
        UnityPyPacker::None => Ok((64, 64)),
        UnityPyPacker::Lz4 => Ok((194, 2)),
        UnityPyPacker::Lzma => Ok((65, 1)),
        UnityPyPacker::Original => {
            let block_info_flag = bundle.blocks.first().map(|b| b.flags as u32).unwrap_or(64);
            Ok((header.flags, block_info_flag))
        }
        UnityPyPacker::UnityFsFlags {
            block_info_flag,
            data_flag,
        } => Ok((data_flag, block_info_flag)),
    }
}

fn compress_unityfs_blob(data: &[u8], switch: u32) -> Result<Vec<u8>> {
    match switch {
        0 => Ok(data.to_vec()),
        1 => Ok(compress_lzma_unity(data)?),
        2 | 3 => Ok(compress_lz4(data)),
        other => Err(UnityAssetError::format(format!(
            "Unsupported UnityFS blob compression switch: {}",
            other
        ))),
    }
}

fn unityfs_uses_block_alignment(header: &BundleHeader) -> bool {
    if header.version >= 7 {
        return true;
    }

    // UnityPy heuristics: 2019.4+ commonly aligns.
    let parsed = UnityVersion::parse_version(&header.unity_revision)
        .or_else(|_| UnityVersion::parse_version(&header.unity_version));
    let Ok(parsed) = parsed else {
        return false;
    };
    parsed.major > 2019 || (parsed.major == 2019 && parsed.minor >= 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_sample_with_packer(packer: UnityPyPacker) -> unity_asset_binary::bundle::AssetBundle {
        let bytes = include_bytes!("../../../../tests/samples/char_118_yuki.ab").to_vec();
        let bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes).unwrap();

        let saved =
            BundleWriter::save(&bundle, &BundleEdits::default(), PackerOptions { packer }).unwrap();

        unity_asset_binary::bundle::BundleParser::from_bytes(saved).unwrap()
    }

    fn assert_roundtrip_contains_serialized_file(bundle: &unity_asset_binary::bundle::AssetBundle) {
        assert_eq!(bundle.header.signature, "UnityFS");
        assert!(!bundle.nodes.is_empty());

        let node = bundle
            .nodes
            .iter()
            .find(|n| n.is_file() && !n.name.ends_with(".resS") && !n.name.ends_with(".resource"))
            .expect("expected at least one serialized file node in saved bundle");
        let node_bytes = bundle.extract_node_data(node).unwrap();
        let sf = unity_asset_binary::asset::SerializedFileParser::from_bytes(node_bytes).unwrap();
        assert!(!sf.objects.is_empty());
    }

    #[test]
    fn can_save_unityfs_bundle_and_reload() {
        let reparsed = save_sample_with_packer(UnityPyPacker::Original);
        assert_roundtrip_contains_serialized_file(&reparsed);
    }

    #[test]
    fn can_save_unityfs_bundle_with_no_compression_and_reload() {
        let reparsed = save_sample_with_packer(UnityPyPacker::None);
        assert_eq!(reparsed.header.flags, 64);
        assert_roundtrip_contains_serialized_file(&reparsed);
    }

    #[test]
    fn can_save_unityfs_bundle_with_lz4_and_reload() {
        let reparsed = save_sample_with_packer(UnityPyPacker::Lz4);
        assert_eq!(reparsed.header.flags, 194);
        assert_roundtrip_contains_serialized_file(&reparsed);
    }

    #[test]
    fn can_save_unityfs_bundle_with_lzma_and_reload() {
        let reparsed = save_sample_with_packer(UnityPyPacker::Lzma);
        assert_eq!(reparsed.header.flags, 65);
        assert_roundtrip_contains_serialized_file(&reparsed);
    }
}
