//! Raw AssetBundle container fallback for files without a usable TypeTree.

use super::serialized_file::SerializedFile;
use super::types::ObjectInfo;
use crate::error::{BinaryError, Result};
use crate::random_access::{BorrowedBytes, ByteCursor};
use crate::reader::{BinaryInput, BinaryReader, not_enough_data_u64};
use unity_asset_core::AssetLoadBudget;

#[derive(Clone, Copy)]
enum AssetInfoLayout {
    PointerOnly,
    PointerThenPreload,
    PreloadThenPointer,
}

type ExternalRefCandidate = (String, i32, i64);
type BestCandidate = (usize, Vec<ExternalRefCandidate>);

fn parse_pptr(input: &mut (impl BinaryInput + ?Sized)) -> Result<(i32, i64)> {
    let file_id = input.read_i32()?;
    let path_id = input.read_i64()?;
    Ok((file_id, path_id))
}

fn parse_asset_info(
    input: &mut (impl BinaryInput + ?Sized),
    layout: AssetInfoLayout,
) -> Result<(i32, i64)> {
    match layout {
        AssetInfoLayout::PointerOnly => parse_pptr(input),
        AssetInfoLayout::PointerThenPreload => {
            let pptr = parse_pptr(input)?;
            let _preload_index = input.read_i32()?;
            let _preload_size = input.read_i32()?;
            Ok(pptr)
        }
        AssetInfoLayout::PreloadThenPointer => {
            let _preload_index = input.read_i32()?;
            let _preload_size = input.read_i32()?;
            parse_pptr(input)
        }
    }
}

fn read_aligned_string(input: &mut (impl BinaryInput + ?Sized)) -> Result<String> {
    let raw_length = input.read_i32()?;
    let length = u64::try_from(raw_length)
        .map_err(|_| BinaryError::invalid_data(format!("Negative string length: {raw_length}")))?;
    let max_length = u64::try_from(BinaryReader::DEFAULT_MAX_STRING_LEN)
        .map_err(|_| BinaryError::invalid_data("string length limit does not fit in u64"))?;
    if length > max_length {
        return Err(BinaryError::invalid_data(format!(
            "String length {length} exceeds limit {max_length}"
        )));
    }
    if length > input.remaining() {
        return Err(not_enough_data_u64(length, input.remaining()));
    }

    let length = usize::try_from(length)
        .map_err(|_| BinaryError::memory_error("string length does not fit in usize"))?;
    let bytes = input.read_bytes(length)?;
    input.align()?;
    Ok(String::from_utf8(bytes)?)
}

fn parse_container_candidate(
    input: &mut (impl BinaryInput + ?Sized),
    asset_info_layout: AssetInfoLayout,
) -> Result<Vec<ExternalRefCandidate>> {
    let _name = read_aligned_string(input)?;

    let preload_size = input.read_i32()?;
    if !(0..=1_000_000).contains(&preload_size) {
        return Err(BinaryError::invalid_data(format!(
            "Invalid AssetBundle preload table size: {preload_size}"
        )));
    }
    for _ in 0..preload_size {
        let _ = parse_pptr(input)?;
    }
    input.align()?;

    let container_size = input.read_i32()?;
    if !(0..=1_000_000).contains(&container_size) {
        return Err(BinaryError::invalid_data(format!(
            "Invalid AssetBundle container size: {container_size}"
        )));
    }

    let container_size = u64::try_from(container_size)
        .map_err(|_| BinaryError::invalid_data("negative AssetBundle container size"))?;
    input.consume_entries(container_size)?;
    let container_size = usize::try_from(container_size)
        .map_err(|_| BinaryError::memory_error("container size does not fit in usize"))?;
    let mut output = Vec::new();
    output.try_reserve_exact(container_size).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {container_size} AssetBundle container entries: {error}"
        ))
    })?;
    for _ in 0..container_size {
        let asset_path = read_aligned_string(input)?;
        let (file_id, path_id) = parse_asset_info(input, asset_info_layout)?;
        output.push((asset_path, file_id, path_id));
    }
    input.align()?;

    let _ = parse_asset_info(input, asset_info_layout)?;
    input.align()?;

    Ok(output)
}

impl SerializedFile {
    /// Best-effort raw parser for Unity `AssetBundle` (class id `142`) `m_Container`.
    ///
    /// This exists as a fallback when TypeTree is stripped or unavailable. The layout is
    /// version-dependent, so this function tries multiple 4-byte-aligned starting offsets and
    /// applies sanity checks.
    ///
    /// Returns a list of `(asset_path, file_id, path_id)` tuples.
    ///
    /// Candidate scans, entry growth, and owned string bytes are charged to `budget`. The same
    /// budget should be shared with the operation that loaded the containing file.
    pub fn assetbundle_container_raw(
        &self,
        info: &ObjectInfo,
        budget: &mut AssetLoadBudget,
    ) -> Result<Vec<(String, i32, i64)>> {
        let data = self.object_bytes(info)?;
        let byte_order = self.header.byte_order();

        let mut last_error: Option<BinaryError> = None;
        let externals_len: i32 = self.externals.len().try_into().unwrap_or(i32::MAX);
        let mut best: Option<BestCandidate> = None;
        let score = |entries: &[ExternalRefCandidate]| -> usize {
            entries
                .iter()
                .filter(|(path, file_id, path_id)| {
                    if path.is_empty() || *path_id == 0 || *file_id < 0 {
                        return false;
                    }
                    if *file_id == 0 {
                        self.find_object(*path_id).is_some()
                    } else {
                        (*file_id - 1) < externals_len
                    }
                })
                .count()
        };

        for offset in (0..=256usize).step_by(4) {
            if offset >= data.len() {
                break;
            }

            for layout in [
                AssetInfoLayout::PointerThenPreload,
                AssetInfoLayout::PreloadThenPointer,
                AssetInfoLayout::PointerOnly,
            ] {
                let source = BorrowedBytes::new(&data[offset..]);
                let mut input = ByteCursor::new(&source, byte_order, budget)?;
                match parse_container_candidate(&mut input, layout) {
                    Ok(entries) => {
                        let candidate_score = score(&entries);
                        let better = match &best {
                            None => true,
                            Some((best_score, best_entries)) => {
                                candidate_score > *best_score
                                    || (candidate_score == *best_score
                                        && entries.len() > best_entries.len())
                            }
                        };
                        if better {
                            best = Some((candidate_score, entries));
                        }
                    }
                    Err(error) if error.is_resource_error() => return Err(error),
                    Err(error) => last_error = Some(error),
                }
            }
        }

        if let Some((_score, entries)) = best
            && entries.iter().any(|(path, _, _)| !path.is_empty())
        {
            return Ok(entries);
        }

        Err(last_error.unwrap_or_else(|| {
            BinaryError::invalid_data(
                "Failed to parse AssetBundle container (no candidates matched)",
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadLimits;

    fn push_i32(bytes: &mut Vec<u8>, value: i32) {
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn pointer_only_candidate(container_size: i32, asset_path: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        push_i32(&mut bytes, 0);
        push_i32(&mut bytes, 0);
        push_i32(&mut bytes, container_size);
        if container_size > 0 {
            push_i32(&mut bytes, i32::try_from(asset_path.len()).unwrap());
            bytes.extend_from_slice(asset_path);
            while bytes.len() % 4 != 0 {
                bytes.push(0);
            }
            push_i32(&mut bytes, 0);
            bytes.extend_from_slice(&1_i64.to_le_bytes());
            push_i32(&mut bytes, 0);
            bytes.extend_from_slice(&0_i64.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn container_entries_are_limited_before_reservation() {
        let bytes = pointer_only_candidate(2, b"");
        let source = BorrowedBytes::new(&bytes);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut input =
            ByteCursor::new(&source, crate::reader::ByteOrder::Little, &mut budget).unwrap();

        let error = parse_container_candidate(&mut input, AssetInfoLayout::PointerOnly)
            .expect_err("entry budget must reject the candidate");

        assert!(matches!(
            error,
            BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
                resource: "entries",
                limit: 1,
                requested: 2,
            })
        ));
        assert_eq!(budget.usage().entries, 0);
    }

    #[test]
    fn owned_string_bytes_are_limited_before_allocation() {
        let bytes = pointer_only_candidate(1, b"path");
        let source = BorrowedBytes::new(&bytes);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 19,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut input =
            ByteCursor::new(&source, crate::reader::ByteOrder::Little, &mut budget).unwrap();

        let error = parse_container_candidate(&mut input, AssetInfoLayout::PointerOnly)
            .expect_err("byte budget must reject the owned path");

        assert!(matches!(
            error,
            BinaryError::Budget(unity_asset_core::BudgetError::Exceeded {
                resource: "bytes",
                limit: 19,
                requested: 20,
            })
        ));
        assert_eq!(budget.usage().bytes, 16);
        assert_eq!(budget.usage().entries, 1);
    }
}
