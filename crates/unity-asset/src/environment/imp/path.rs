use super::*;

pub(crate) fn canonicalize_if_exists(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn canonicalize_source_if_possible(source: &BinarySource) -> Option<BinarySource> {
    match source {
        BinarySource::Path(p) => {
            let canon = canonicalize_if_exists(p);
            if &canon != p {
                Some(BinarySource::Path(canon))
            } else {
                None
            }
        }
        BinarySource::WebEntry {
            web_path,
            entry_name,
        } => {
            let canon = canonicalize_if_exists(web_path);
            if &canon != web_path {
                Some(BinarySource::WebEntry {
                    web_path: canon,
                    entry_name: entry_name.clone(),
                })
            } else {
                None
            }
        }
    }
}
