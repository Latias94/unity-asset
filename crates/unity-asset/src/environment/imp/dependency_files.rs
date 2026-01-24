use super::*;

use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::PathBuf;

use super::path::{canonicalize_if_exists, find_sensitive_path};

#[derive(Debug, Default, Clone)]
pub(crate) struct DependencyFileIndexStats {
    pub files_visited: usize,
    pub files_indexed: usize,
    pub truncated: bool,
}

#[derive(Debug, Default)]
pub(crate) struct DependencyFileIndex {
    built_for: Option<PathBuf>,
    by_simple_name: HashMap<String, Vec<PathBuf>>,
    stats: DependencyFileIndexStats,
}

impl DependencyFileIndex {
    pub(crate) fn built_for(&self) -> Option<&PathBuf> {
        self.built_for.as_ref()
    }

    pub(crate) fn clear(&mut self) {
        self.built_for = None;
        self.by_simple_name.clear();
        self.stats = DependencyFileIndexStats::default();
    }

    pub(crate) fn build_for_root(
        &mut self,
        root: &Path,
        max_files: Option<usize>,
    ) -> DependencyFileIndexStats {
        self.clear();

        let root = canonicalize_if_exists(root);
        self.built_for = Some(root.clone());

        let skip_dir_names = [
            "Library",
            "Temp",
            "Logs",
            ".git",
            ".vs",
            "obj",
            "bin",
            "UserSettings",
        ];

        let mut builder = WalkBuilder::new(&root);
        builder.follow_links(false);
        builder.hidden(false);
        builder
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true);

        let walker = builder.filter_entry(move |entry| {
            let Some(name) = entry.file_name().to_str() else {
                return false;
            };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                return !skip_dir_names.iter().any(|d| d == &name);
            }
            true
        });

        let mut stats = DependencyFileIndexStats::default();
        for result in walker.build() {
            let entry = match result {
                Ok(v) => v,
                Err(_) => continue,
            };
            if entry.file_type().is_none_or(|t| !t.is_file()) {
                continue;
            }

            stats.files_visited += 1;
            if let Some(max) = max_files {
                if stats.files_visited > max {
                    stats.truncated = true;
                    break;
                }
            }

            let path = canonicalize_if_exists(entry.path());
            let Some(simple) = simplify_name_for_lookup(&path) else {
                continue;
            };
            self.by_simple_name.entry(simple).or_default().push(path);
            stats.files_indexed += 1;
        }

        // Stabilize candidate order for deterministic matching.
        for v in self.by_simple_name.values_mut() {
            v.sort();
            v.dedup();
        }

        self.stats = stats.clone();
        stats
    }

    pub(crate) fn candidates_by_simple_name(&self, simple_name: &str) -> Option<&[PathBuf]> {
        self.by_simple_name.get(simple_name).map(|v| v.as_slice())
    }
}

pub(crate) fn simplify_name_for_lookup(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_lowercase())
}

pub(crate) fn simplify_name_str_for_lookup(name: &str) -> Option<String> {
    let p = Path::new(name);
    simplify_name_for_lookup(p)
}

impl Environment {
    fn ensure_dependency_file_index_built(&self) {
        let root = canonicalize_if_exists(&self.base_path);

        match self.dependency_file_index.read() {
            Ok(idx) => {
                if idx.built_for() == Some(&root) {
                    return;
                }
            }
            Err(e) => {
                if e.into_inner().built_for() == Some(&root) {
                    return;
                }
            }
        }

        match self.dependency_file_index.write() {
            Ok(mut idx) => {
                if idx.built_for() == Some(&root) {
                    return;
                }
                // Best-effort: no default cap; callers can clear/rebuild if needed.
                idx.build_for_root(&root, None);
            }
            Err(e) => {
                let mut idx = e.into_inner();
                if idx.built_for() == Some(&root) {
                    return;
                }
                idx.build_for_root(&root, None);
            }
        }
    }

    /// Best-effort dependency path resolution (UnityPy `Environment.find_file`-style).
    ///
    /// Strategy:
    /// 1) direct absolute path
    /// 2) `base_path`-relative path (including case-insensitive component matching)
    /// 3) recursive scan under `base_path` indexed by simplified name (lowercased basename)
    pub(crate) fn find_dependency_path_best_effort(&self, name: &str) -> Option<PathBuf> {
        let raw = name.trim();
        if raw.is_empty() {
            return None;
        }
        if raw.starts_with("archive:") {
            return None;
        }

        let p = Path::new(raw);
        if p.is_absolute() {
            if p.exists() {
                return Some(canonicalize_if_exists(p));
            }
            return None;
        }

        let joined = self.base_path.join(p);
        if joined.exists() {
            return Some(canonicalize_if_exists(&joined));
        }
        if let Some(found) = find_sensitive_path(&self.base_path, p) {
            if found.exists() {
                return Some(canonicalize_if_exists(&found));
            }
        }

        let simple = simplify_name_str_for_lookup(raw)?;
        self.ensure_dependency_file_index_built();

        let candidates: Vec<PathBuf> = match self.dependency_file_index.read() {
            Ok(idx) => idx
                .candidates_by_simple_name(&simple)
                .map(|v| v.to_vec())
                .unwrap_or_default(),
            Err(e) => e
                .into_inner()
                .candidates_by_simple_name(&simple)
                .map(|v| v.to_vec())
                .unwrap_or_default(),
        };

        match candidates.as_slice() {
            [] => None,
            [only] => Some(only.clone()),
            many => {
                // Prefer candidates that best match the full external path (suffix/name match).
                let mut best_score = 0i32;
                let mut best: Vec<&PathBuf> = Vec::new();
                for c in many {
                    let rel = c
                        .strip_prefix(&self.base_path)
                        .unwrap_or(c.as_path())
                        .to_string_lossy()
                        .replace('\\', "/");
                    let score = super::pptr::match_external_path_score(raw, &rel);
                    if score == 0 {
                        continue;
                    }
                    if score > best_score {
                        best_score = score;
                        best.clear();
                        best.push(c);
                    } else if score == best_score {
                        best.push(c);
                    }
                }
                match best.as_slice() {
                    [only] => Some((*only).clone()),
                    _ => None,
                }
            }
        }
    }
}
