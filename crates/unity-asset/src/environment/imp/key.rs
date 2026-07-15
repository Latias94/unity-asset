use super::{BinaryObjectKey, BinarySource, BinarySourceKind};

impl std::fmt::Display for BinaryObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // This representation is diagnostic-only. Persisted and copy/paste identities use
        // `ObjectAddress`, whose source ownership does not depend on bundle vector indexes.
        let kind = match self.source_kind {
            BinarySourceKind::SerializedFile => "serialized",
            BinarySourceKind::AssetBundle => "bundle",
        };
        let asset_index = self
            .asset_index
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());

        match &self.source {
            BinarySource::Path(p) => {
                let outer = p.to_string_lossy().to_string();
                write!(
                    f,
                    "bok2|{}|{}|{}|{}|{}|{}|",
                    kind,
                    asset_index,
                    self.path_id,
                    outer.len(),
                    outer,
                    0
                )
            }
            BinarySource::WebEntry {
                web_path,
                entry_name,
            } => {
                let outer = web_path.to_string_lossy().to_string();
                write!(
                    f,
                    "bok3|{}|{}|{}|{}|{}|w|{}|{}",
                    kind,
                    asset_index,
                    self.path_id,
                    outer.len(),
                    outer,
                    entry_name.len(),
                    entry_name
                )
            }
            BinarySource::ArchiveEntry {
                archive_path,
                entry_name,
            } => {
                let outer = archive_path.to_string_lossy().to_string();
                write!(
                    f,
                    "bok3|{}|{}|{}|{}|{}|a|{}|{}",
                    kind,
                    asset_index,
                    self.path_id,
                    outer.len(),
                    outer,
                    entry_name.len(),
                    entry_name
                )
            }
        }
    }
}
