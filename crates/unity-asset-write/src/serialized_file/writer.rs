use unity_asset_binary::asset::SerializedFile;

use crate::Result;
use crate::serialized_file::edit::SerializedFileEdits;
use crate::serialized_file::plan::SerializedFilePlan;

/// Namespace for canonical SerializedFile encoding entry points.
pub struct SerializedFileWriter;

impl SerializedFileWriter {
    /// Materializes the canonical SerializedFile plan into one contiguous byte vector.
    ///
    /// Prepared artifacts use the same plan and generated-region encoder while retaining
    /// verified source ranges without copying them.
    pub fn save(file: &SerializedFile, edits: &SerializedFileEdits) -> Result<Vec<u8>> {
        SerializedFilePlan::build_for_save(file, edits)?.encode_to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unity_asset_core::AssetLoadBudget;

    #[test]
    fn can_save_serialized_file_extracted_from_bundle_and_reload() {
        let bundle_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/samples/char_118_yuki.ab");
        let bundle_bytes = std::fs::read(bundle_path).unwrap();
        let mut budget = AssetLoadBudget::default();
        let bundle = unity_asset_binary::bundle::BundleParser::from_bytes_with_budget(
            bundle_bytes,
            &mut budget,
        )
        .unwrap();
        let file = bundle.assets.first().expect("bundle has assets");

        let encoded = SerializedFileWriter::save(file, &SerializedFileEdits::new()).unwrap();
        let reparsed =
            unity_asset_binary::asset::SerializedFileParser::from_bytes(encoded).unwrap();

        assert_eq!(reparsed.header.version, file.header.version);
        assert_eq!(reparsed.unity_version, file.unity_version);
        assert_eq!(reparsed.target_platform, file.target_platform);
        assert_eq!(reparsed.type_tree_enabled(), file.type_tree_enabled());
        assert_eq!(reparsed.types().len(), file.types().len());
        assert_eq!(reparsed.objects().len(), file.objects().len());
        assert_eq!(reparsed.externals.len(), file.externals.len());
        assert_eq!(reparsed.ref_types().len(), file.ref_types().len());
    }
}
