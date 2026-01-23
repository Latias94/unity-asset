use super::*;

pub(crate) fn canonicalize_if_exists(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn find_sensitive_path(root: &Path, insensitive_path: &Path) -> Option<PathBuf> {
    if insensitive_path.as_os_str().is_empty() {
        return None;
    }
    if insensitive_path.is_absolute() {
        // Best-effort: avoid guessing across platform-specific absolute path semantics.
        return None;
    }

    let mut cur = root.to_path_buf();
    for comp in insensitive_path.components() {
        let comp_str = comp.as_os_str().to_string_lossy();
        if comp_str.is_empty() || comp_str == "." {
            continue;
        }
        if comp_str == ".." {
            cur.pop();
            continue;
        }

        let Ok(entries) = std::fs::read_dir(&cur) else {
            return None;
        };
        let target_lower = comp_str.to_lowercase();

        let mut matched: Option<PathBuf> = None;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.to_lowercase() == target_lower {
                matched = Some(cur.join(name));
                break;
            }
        }
        cur = matched?;
    }

    Some(cur)
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
