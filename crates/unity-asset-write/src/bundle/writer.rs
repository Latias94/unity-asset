use unity_asset_binary::bundle::{AssetBundle, BundleHeader};
use unity_asset_binary::unity_version::UnityVersion;

use std::collections::HashSet;
use unity_asset_core::{Result, UnityAssetError};

use crate::bundle::BundleEdits;
use crate::bundle::chunk::chunk_based_compress;
use crate::{
    BinaryWriter, Endian, PackerOptions, UnityPyPacker, compress_lz4, compress_lzma_unity,
    compress_lzma_unity_with_size,
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
            "UnityWeb" | "UnityRaw" => Self::save_web_raw(bundle, edits),
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

    /// UnityWeb / UnityRaw (`BundleFile.save_web_raw`) implementation.
    ///
    /// UnityPy only supports saving versions `<= 3` for this format.
    pub fn save_web_raw(bundle: &AssetBundle, edits: &BundleEdits) -> Result<Vec<u8>> {
        let header = &bundle.header;
        if header.version > 3 {
            return Err(UnityAssetError::format(format!(
                "Saving legacy bundles with version > 3 is not supported (got {})",
                header.version
            )));
        }

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
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
                        format!("Failed to extract legacy bundle node data: {}", node.name),
                        e,
                    )
                })?
            };

            existing_names.insert(node.name.as_str());
            files.push((node.name.clone(), bytes));
        }

        // Append new files that were not present in the original bundle.
        let mut extra: Vec<(&str, &[u8])> = edits
            .iter()
            .filter(|(name, _)| !existing_names.contains(*name))
            .collect();
        extra.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (name, bytes) in extra {
            files.push((name.to_string(), bytes.to_vec()));
        }

        // Calculate fileInfoHeaderSize so offsets can be precomputed (UnityPy `save_web_raw`).
        let mut file_info_header_size: u32 = 4; // nodesCount
        for (name, _) in &files {
            file_info_header_size = file_info_header_size.saturating_add(name.len() as u32 + 1);
            file_info_header_size = file_info_header_size.saturating_add(4 * 2); // offset + size
        }
        file_info_header_size = align_u32(file_info_header_size, 4);

        // Prepare directory info + file content.
        let mut directory_info_writer = BinaryWriter::new(Endian::Big);
        let file_count_i32 = i32::try_from(files.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "Legacy bundle file count too large for i32: {}",
                files.len()
            ))
        })?;
        directory_info_writer.write_i32(file_count_i32);

        let mut file_content_writer = BinaryWriter::new(Endian::Big);
        let mut current_offset: u32 = file_info_header_size;
        for (name, data) in &files {
            directory_info_writer.write_string_to_null(name);
            directory_info_writer.write_u32(current_offset);

            let size_u32 = u32::try_from(data.len()).map_err(|_| {
                UnityAssetError::format(format!(
                    "Legacy bundle file '{}' too large for u32 size: {}",
                    name,
                    data.len()
                ))
            })?;
            directory_info_writer.write_u32(size_u32);

            file_content_writer.write(data);
            current_offset = current_offset.checked_add(size_u32).ok_or_else(|| {
                UnityAssetError::format("Legacy bundle file content offset overflow")
            })?;
        }

        // Pad directory info header to `file_info_header_size` (4-byte alignment).
        let dir_len_u64 = directory_info_writer.position();
        let dir_len_u32 = u32::try_from(dir_len_u64).map_err(|_| {
            UnityAssetError::format(format!(
                "Legacy directory info too large for u32: {}",
                dir_len_u64
            ))
        })?;
        if dir_len_u32 > file_info_header_size {
            return Err(UnityAssetError::format(format!(
                "Legacy directory header exceeded computed file_info_header_size: {} > {}",
                dir_len_u32, file_info_header_size
            )));
        }
        directory_info_writer.write(&vec![0u8; (file_info_header_size - dir_len_u32) as usize]);

        let mut uncompressed_content = Vec::new();
        uncompressed_content.extend_from_slice(&directory_info_writer.into_bytes());
        uncompressed_content.extend_from_slice(&file_content_writer.into_bytes());

        let uncompressed_size_u32 = u32::try_from(uncompressed_content.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "Legacy uncompressed content too large for u32: {}",
                uncompressed_content.len()
            ))
        })?;

        let compressed_content = if header.signature == "UnityWeb" {
            compress_lzma_unity_with_size(&uncompressed_content)?
        } else {
            uncompressed_content
        };

        let compressed_size_u32 = u32::try_from(compressed_content.len()).map_err(|_| {
            UnityAssetError::format(format!(
                "Legacy compressed content too large for u32: {}",
                compressed_content.len()
            ))
        })?;

        // Write header.
        let mut writer = BinaryWriter::new(Endian::Big);
        writer.write_string_to_null(&header.signature);
        writer.write_u32(header.version);
        writer.write_string_to_null(&header.unity_version);
        writer.write_string_to_null(&header.unity_revision);

        // header_size = writer.Position + fixed fields (levelCount = 1).
        // Matches UnityPy `save_web_raw` (version<=3).
        let mut header_size_u32 = u32::try_from(writer.position())
            .map_err(|_| UnityAssetError::format("Legacy header position does not fit in u32"))?;
        header_size_u32 = header_size_u32.saturating_add(24);
        if header.version >= 2 {
            header_size_u32 = header_size_u32.saturating_add(4);
        }
        if header.version >= 3 {
            header_size_u32 = header_size_u32.saturating_add(4);
        }
        if header.version >= 4 {
            header_size_u32 = header_size_u32.saturating_add(20);
        }
        header_size_u32 = (header_size_u32.saturating_add(3)) & !3;

        let complete_file_size = header_size_u32
            .checked_add(compressed_size_u32)
            .ok_or_else(|| UnityAssetError::format("Legacy complete file size overflow"))?;

        writer.write_u32(complete_file_size); // minimumStreamedBytes (same as completeFileSize)
        writer.write_u32(header_size_u32); // headerSize
        writer.write_u32(1); // numberOfLevelsToDownloadBeforeStreaming
        writer.write_i32(1); // levelCount

        writer.write_u32(compressed_size_u32);
        writer.write_u32(uncompressed_size_u32);

        if header.version >= 2 {
            writer.write_u32(complete_file_size);
        }
        if header.version >= 3 {
            writer.write_u32(file_info_header_size);
        }

        writer.align_stream(4);
        writer.write(&compressed_content);

        Ok(writer.into_bytes())
    }
}

fn resolve_unityfs_flags(
    header: &BundleHeader,
    bundle: &AssetBundle,
    options: PackerOptions,
) -> Result<(u32, u32)> {
    let (data_flag, block_info_flag) = match options.packer {
        UnityPyPacker::None => (64, 64),
        UnityPyPacker::Lz4 => (194, 2),
        UnityPyPacker::Lzma => (65, 1),
        UnityPyPacker::Original => {
            let block_info_flag = bundle.blocks.first().map(|b| b.flags as u32).unwrap_or(64);
            (header.flags, block_info_flag)
        }
        UnityPyPacker::UnityFsFlags {
            block_info_flag,
            data_flag,
        } => (data_flag, block_info_flag),
    };

    Ok(strip_unityfs_encryption_flags(
        header,
        data_flag,
        block_info_flag,
    ))
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

fn align_u32(v: u32, align: u32) -> u32 {
    if align == 0 {
        return v;
    }
    let rem = v % align;
    if rem == 0 {
        v
    } else {
        v.saturating_add(align - rem)
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

fn strip_unityfs_encryption_flags(
    header: &BundleHeader,
    data_flag: u32,
    block_info_flag: u32,
) -> (u32, u32) {
    // UnityPy clears encryption flags during save because it does not re-encrypt.
    //
    // Note: Unity CN introduced encryption before the alignment fix was introduced.
    // Unity CN also reused the same bit (0x200) later for `BlockInfoNeedPaddingAtStart`.
    // UnityPy disambiguates this based on the engine version, so we do the same.
    let uses_old_flags = unityfs_uses_old_archive_flags(header).unwrap_or(false);
    let encryption_mask = if uses_old_flags {
        0x200 // ArchiveFlagsOld.UsesAssetBundleEncryption
    } else {
        0x1400 // ArchiveFlags.UsesAssetBundleEncryption (old: 0x400, new: 0x1000)
    };

    (
        data_flag & !encryption_mask,
        block_info_flag & !encryption_mask,
    )
}

fn unityfs_uses_old_archive_flags(header: &BundleHeader) -> Option<bool> {
    let parsed = UnityVersion::parse_version(&header.unity_revision)
        .or_else(|_| UnityVersion::parse_version(&header.unity_version))
        .ok()?;

    let (major, minor, build) = (parsed.major, parsed.minor, parsed.build);

    // Mirrors UnityPy `BundleFile.read_fs` version checks for ArchiveFlagsOld vs ArchiveFlags.
    //
    // - version < (2020,)
    // - 2020.x < 2020.3.34
    // - 2021.x < 2021.3.2
    // - 2022.x < 2022.1.1
    let is_old = if major < 2020 {
        true
    } else if major == 2020 {
        minor < 3 || (minor == 3 && build < 34)
    } else if major == 2021 {
        minor < 3 || (minor == 3 && build < 2)
    } else if major == 2022 {
        minor < 1 || (minor == 1 && build < 1)
    } else {
        false
    };

    Some(is_old)
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
    fn unityfs_save_strips_encryption_flag_for_old_versions() {
        let bytes = include_bytes!("../../../../tests/samples/char_118_yuki.ab").to_vec();
        let mut bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes).unwrap();

        bundle.header.unity_revision = "2020.3.33f1".to_string();
        bundle.header.unity_version = "2020.3.33f1".to_string();

        bundle.header.flags |= 0x200;
        if let Some(first) = bundle.blocks.first_mut() {
            first.flags |= 0x200;
        }

        let saved = BundleWriter::save(
            &bundle,
            &BundleEdits::default(),
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
        )
        .unwrap();

        let reparsed = unity_asset_binary::bundle::BundleParser::from_bytes(saved).unwrap();
        assert_eq!(reparsed.header.flags & 0x200, 0);
    }

    #[test]
    fn unityfs_save_strips_new_encryption_bits_but_keeps_padding_bit() {
        let bytes = include_bytes!("../../../../tests/samples/char_118_yuki.ab").to_vec();
        let mut bundle = unity_asset_binary::bundle::BundleParser::from_bytes(bytes).unwrap();

        bundle.header.unity_revision = "2020.3.34f1".to_string();
        bundle.header.unity_version = "2020.3.34f1".to_string();

        bundle.header.flags |= 0x200; // BlockInfoNeedPaddingAtStart (new flags)
        bundle.header.flags |= 0x1000; // UsesAssetBundleEncryption (new flags)
        if let Some(first) = bundle.blocks.first_mut() {
            first.flags |= 0x1000;
        }

        let saved = BundleWriter::save(
            &bundle,
            &BundleEdits::default(),
            PackerOptions {
                packer: UnityPyPacker::Original,
            },
        )
        .unwrap();

        let reparsed = unity_asset_binary::bundle::BundleParser::from_bytes(saved).unwrap();
        assert_ne!(reparsed.header.flags & 0x200, 0);
        assert_eq!(reparsed.header.flags & 0x1000, 0);
    }

    #[test]
    fn can_save_unityfs_bundle_with_lzma_and_reload() {
        let reparsed = save_sample_with_packer(UnityPyPacker::Lzma);
        assert_eq!(reparsed.header.flags, 65);
        assert_roundtrip_contains_serialized_file(&reparsed);
    }
}
