use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use same_file::Handle;
use unity_asset_core::{AssetLoadBudget, BudgetError, arc_value_allocation_bytes};

use super::{FileSnapshot, ScanError, checked_vec_bytes};
use crate::SearchIndexOptions;

const POLICY_SOURCE: &str = "project-root ignore policy";
const PARSER_PASSES: u64 = 2;

// The patched globset compiles every non-literal strategy into one RegexSet. Its meta regex can
// retain forward/reverse 10 MiB NFAs and 10 MiB hybrid caches, plus one-pass and small DFA state.
const REGEX_SET_BYTES: u64 = 48 * 1024 * 1024;
// Aho-Corasick may use a 256-way table of four-byte state IDs. The remaining 64 bytes cover
// token storage and the original, normalized, and regex text retained during compilation.
const COMPILER_BYTES_PER_RULE_BYTE: u64 = (256 * 4) + 64;
// GitignoreBuilder::build temporarily retains both builder and compiled rule metadata.
const RULE_METADATA_COPIES: u64 = 4;
// globset's pooled match Vec grows geometrically and retains at most twice the rule count.
const MATCH_RESULT_POOL_CAPACITY_FACTOR: u64 = 2;
// globset's shared RegexSet keeps one lazily-created PatternSet in the single-threaded matcher
// pool. It is a boxed `[bool]` indexed by the number of regex patterns.
const MATCH_PATTERN_SET_POOL_CAPACITY_FACTOR: u64 = 1;
#[cfg(windows)]
const WINDOWS_MATCH_CANDIDATE_COPIES: u64 = 3;

const ROOT_IGNORE_FILES: [RootIgnoreFile; 3] = [
    RootIgnoreFile {
        slot: 0,
        name: ".gitignore",
        git_only: true,
    },
    RootIgnoreFile {
        slot: 1,
        name: ".ignore",
        git_only: false,
    },
    RootIgnoreFile {
        slot: 2,
        name: ".unity-asset-search-ignore",
        git_only: false,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreLimitResource {
    FileBytes,
    LineBytes,
    Patterns,
    ParserWork,
}

impl fmt::Display for IgnoreLimitResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FileBytes => "file bytes",
            Self::LineBytes => "line bytes",
            Self::Patterns => "patterns",
            Self::ParserWork => "parser work",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreReadOperation {
    Open,
    Inspect,
    Read,
    Reopen,
}

impl fmt::Display for IgnoreReadOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::Inspect => "inspect",
            Self::Read => "read",
            Self::Reopen => "reopen",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IgnoreSyntaxReason {
    InvalidUtf8,
    MatcherRejectedRule,
    MatcherCompilationFailed,
}

impl fmt::Display for IgnoreSyntaxReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidUtf8 => "line is not valid UTF-8",
            Self::MatcherRejectedRule => "gitignore matcher rejected the rule",
            Self::MatcherCompilationFailed => "gitignore matcher compilation failed",
        })
    }
}

#[derive(Debug)]
struct IgnoreSource {
    bytes: Vec<u8>,
    handle: Handle,
    snapshot: FileSnapshot,
}

#[derive(Debug)]
pub(super) struct RootIgnoreMatcher {
    sources: [Option<IgnoreSource>; ROOT_IGNORE_FILES.len()],
    matcher: Gitignore,
    rule_count: usize,
}

impl RootIgnoreMatcher {
    pub(super) fn load(
        read_root: &super::platform::ProjectReadRoot,
        options: SearchIndexOptions,
        budget: &mut AssetLoadBudget,
    ) -> Result<Arc<Self>, ScanError> {
        let mut sources: [Option<IgnoreSource>; ROOT_IGNORE_FILES.len()] =
            std::array::from_fn(|_| None);
        if !options.respect_project_root_ignore_files {
            return retain_matcher(
                Self {
                    sources,
                    matcher: Gitignore::empty(),
                    rule_count: 0,
                },
                budget,
            );
        }

        let mut encoded_bytes = 0_u64;
        for file in ROOT_IGNORE_FILES
            .iter()
            .filter(|file| file.is_enabled(options))
        {
            let Some(source) = read_ignore_source(
                read_root,
                file.name,
                options.max_project_root_ignore_file_bytes,
                budget,
            )?
            else {
                continue;
            };
            encoded_bytes = checked_add(
                encoded_bytes,
                u64::try_from(source.bytes.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                    resource: "project-root ignore encoded bytes",
                })?,
                "project-root ignore encoded bytes",
            )?;
            sources[file.slot] = Some(source);
        }

        let parser_work = checked_mul(
            encoded_bytes,
            PARSER_PASSES,
            "project-root ignore parser work",
        )?;
        if parser_work > options.max_project_root_ignore_parser_work {
            return Err(ScanError::IgnoreLimitExceeded {
                file: POLICY_SOURCE,
                resource: IgnoreLimitResource::ParserWork,
                observed_at_least: parser_work,
                limit: options.max_project_root_ignore_parser_work,
            });
        }
        budget.consume_bytes(parser_work)?;

        let mut rule_count = 0_usize;
        let mut rule_bytes = 0_u64;
        for (source_slot, source) in sources.iter().enumerate() {
            let Some(source) = source else {
                continue;
            };
            let file = ROOT_IGNORE_FILES[source_slot];
            visit_lines(
                file.name,
                &source.bytes,
                options.max_project_root_ignore_line_bytes,
                |_, line| {
                    let Some(rule) = normalized_rule_line(line) else {
                        return Ok(());
                    };
                    let observed =
                        rule_count
                            .checked_add(1)
                            .ok_or(BudgetError::ArithmeticOverflow {
                                resource: "project-root ignore pattern count",
                            })?;
                    if observed > options.max_project_root_ignore_patterns {
                        return Err(ScanError::IgnoreLimitExceeded {
                            file: POLICY_SOURCE,
                            resource: IgnoreLimitResource::Patterns,
                            observed_at_least: u64::try_from(observed).map_err(|_| {
                                BudgetError::ArithmeticOverflow {
                                    resource: "project-root ignore pattern count",
                                }
                            })?,
                            limit: u64::try_from(options.max_project_root_ignore_patterns)
                                .map_err(|_| BudgetError::ArithmeticOverflow {
                                    resource: "project-root ignore pattern count",
                                })?,
                        });
                    }
                    rule_count = observed;
                    rule_bytes = checked_add(
                        rule_bytes,
                        u64::try_from(rule.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                            resource: "project-root ignore rule bytes",
                        })?,
                        "project-root ignore rule bytes",
                    )?;
                    Ok(())
                },
            )?;
        }

        if rule_count == 0 {
            return retain_matcher(
                Self {
                    sources,
                    matcher: Gitignore::empty(),
                    rule_count: 0,
                },
                budget,
            );
        }

        let rule_entries =
            u64::try_from(rule_count).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "project-root ignore pattern count",
            })?;
        let compiler_bytes = compiler_reservation_bytes(rule_bytes, rule_count)?;
        budget.check_entries(rule_entries)?;
        budget.check_bytes(compiler_bytes)?;
        budget.consume_entries(rule_entries)?;
        budget.consume_bytes(compiler_bytes)?;

        let matcher = {
            let mut builder = GitignoreBuilder::new(".");
            for (source_slot, source) in sources.iter().enumerate() {
                let Some(source) = source else {
                    continue;
                };
                let file = ROOT_IGNORE_FILES[source_slot];
                visit_lines(
                    file.name,
                    &source.bytes,
                    options.max_project_root_ignore_line_bytes,
                    |line_number, line| {
                        builder.add_line(None, line).map(|_| ()).map_err(|_| {
                            ScanError::IgnoreSyntax {
                                file: file.name,
                                line: Some(line_number),
                                reason: IgnoreSyntaxReason::MatcherRejectedRule,
                            }
                        })
                    },
                )?;
            }
            builder.build().map_err(|_| ScanError::IgnoreSyntax {
                file: POLICY_SOURCE,
                line: None,
                reason: IgnoreSyntaxReason::MatcherCompilationFailed,
            })?
        };
        debug_assert_eq!(matcher.len(), rule_count);

        retain_matcher(
            Self {
                sources,
                matcher,
                rule_count,
            },
            budget,
        )
    }

    pub(super) fn is_ignored(
        &self,
        relative: &Path,
        is_dir: bool,
        budget: &mut AssetLoadBudget,
    ) -> Result<bool, ScanError> {
        if self.rule_count == 0 {
            return Ok(false);
        }
        let Some(relative) = relative.to_str() else {
            return Ok(false);
        };
        budget.consume_bytes(match_invocation_bytes(relative)?)?;
        Ok(self.matcher.matched(relative, is_dir).is_ignore())
    }

    pub(super) fn validate_current(
        &self,
        read_root: &super::platform::ProjectReadRoot,
        options: SearchIndexOptions,
    ) -> Result<(), ScanError> {
        for file in ROOT_IGNORE_FILES
            .iter()
            .filter(|file| file.is_enabled(options))
        {
            match &self.sources[file.slot] {
                Some(source) => validate_source_current(read_root, file.name, source)?,
                None => match read_root.open_relative(Path::new(file.name)) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(ScanError::IgnoreIo {
                            file: file.name,
                            operation: IgnoreReadOperation::Reopen,
                            source,
                        });
                    }
                    Ok(_) => {
                        return Err(ScanError::IgnoreChangedDuringRead { file: file.name });
                    }
                },
            }
        }
        Ok(())
    }
}

pub(super) fn is_configured_project_root_ignore_file(
    project_root: &Path,
    path: &Path,
    options: SearchIndexOptions,
) -> bool {
    if path.parent() != Some(project_root) {
        return false;
    }
    is_named_project_root_ignore_file(path, options)
}

pub(super) fn is_named_project_root_ignore_file(path: &Path, options: SearchIndexOptions) -> bool {
    if !options.respect_project_root_ignore_files {
        return false;
    }
    ROOT_IGNORE_FILES.iter().any(|file| {
        file.is_enabled(options)
            && path
                .file_name()
                .is_some_and(|name| ignore_file_name_eq(name, file.name))
    })
}

#[cfg(not(windows))]
fn ignore_file_name_eq(name: &std::ffi::OsStr, expected: &str) -> bool {
    name == expected
}

#[cfg(windows)]
fn ignore_file_name_eq(name: &std::ffi::OsStr, expected: &str) -> bool {
    name.to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

#[derive(Debug, Clone, Copy)]
struct RootIgnoreFile {
    slot: usize,
    name: &'static str,
    git_only: bool,
}

impl RootIgnoreFile {
    fn is_enabled(self, options: SearchIndexOptions) -> bool {
        options.respect_project_root_ignore_files
            && (!self.git_only || options.respect_project_root_gitignore)
    }
}

fn retain_matcher(
    matcher: RootIgnoreMatcher,
    budget: &mut AssetLoadBudget,
) -> Result<Arc<RootIgnoreMatcher>, ScanError> {
    let retained_bytes = arc_value_allocation_bytes::<RootIgnoreMatcher>().map_err(|_| {
        BudgetError::ArithmeticOverflow {
            resource: "project-root ignore matcher",
        }
    })?;
    budget.consume_bytes(retained_bytes)?;
    Ok(Arc::new(matcher))
}

fn compiler_reservation_bytes(rule_bytes: u64, rule_count: usize) -> Result<u64, ScanError> {
    let input_dependent = checked_mul(
        rule_bytes,
        COMPILER_BYTES_PER_RULE_BYTE,
        "project-root ignore compiler input",
    )?;
    let rule_metadata = checked_mul(
        checked_vec_bytes::<ignore::gitignore::Glob>(rule_count)?,
        RULE_METADATA_COPIES,
        "project-root ignore rule metadata",
    )?;
    let match_result_pool = checked_mul(
        checked_vec_bytes::<usize>(rule_count)?,
        MATCH_RESULT_POOL_CAPACITY_FACTOR,
        "project-root ignore match result pool",
    )?;
    let match_pattern_set_pool = checked_mul(
        checked_vec_bytes::<bool>(rule_count)?,
        MATCH_PATTERN_SET_POOL_CAPACITY_FACTOR,
        "project-root ignore pattern set pool",
    )?;
    checked_add(
        checked_add(
            checked_add(
                REGEX_SET_BYTES,
                input_dependent,
                "project-root ignore compiler reservation",
            )?,
            rule_metadata,
            "project-root ignore compiler reservation",
        )?,
        checked_add(
            match_result_pool,
            match_pattern_set_pool,
            "project-root ignore compiler reservation",
        )?,
        "project-root ignore compiler reservation",
    )
}

fn match_invocation_bytes(relative: &str) -> Result<u64, ScanError> {
    #[cfg(windows)]
    {
        let path_bytes = checked_mul(
            u64::try_from(relative.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "project-root ignore match path bytes",
            })?,
            WINDOWS_MATCH_CANDIDATE_COPIES,
            "project-root ignore match path scratch",
        )?;
        Ok(path_bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = relative;
        Ok(0)
    }
}

fn checked_add(left: u64, right: u64, resource: &'static str) -> Result<u64, ScanError> {
    left.checked_add(right)
        .ok_or(BudgetError::ArithmeticOverflow { resource }.into())
}

fn checked_mul(left: u64, right: u64, resource: &'static str) -> Result<u64, ScanError> {
    left.checked_mul(right)
        .ok_or(BudgetError::ArithmeticOverflow { resource }.into())
}

fn read_ignore_source(
    read_root: &super::platform::ProjectReadRoot,
    file_name: &'static str,
    file_limit: u64,
    budget: &mut AssetLoadBudget,
) -> Result<Option<IgnoreSource>, ScanError> {
    let file = match read_root.open_relative(Path::new(file_name)) {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ScanError::IgnoreIo {
                file: file_name,
                operation: IgnoreReadOperation::Open,
                source,
            });
        }
    };
    let handle = Handle::from_file(file).map_err(|source| ScanError::IgnoreIo {
        file: file_name,
        operation: IgnoreReadOperation::Open,
        source,
    })?;
    let metadata = handle
        .as_file()
        .metadata()
        .map_err(|source| ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Inspect,
            source,
        })?;
    if !metadata.is_file() {
        return Err(ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Inspect,
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "project-root ignore source is not a regular file",
            ),
        });
    }
    let expected_bytes = metadata.len();
    if expected_bytes > file_limit {
        return Err(ScanError::IgnoreLimitExceeded {
            file: file_name,
            resource: IgnoreLimitResource::FileBytes,
            observed_at_least: expected_bytes,
            limit: file_limit,
        });
    }
    let expected_capacity =
        usize::try_from(expected_bytes).map_err(|_| ScanError::IgnoreLimitExceeded {
            file: file_name,
            resource: IgnoreLimitResource::FileBytes,
            observed_at_least: expected_bytes,
            limit: file_limit,
        })?;
    budget.check_entries(1)?;
    budget.check_bytes(expected_bytes)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(expected_capacity)
        .map_err(|source| ScanError::AllocationFailed {
            allocation: "project-root ignore source",
            requested: expected_capacity,
            source,
        })?;
    let retained_bytes =
        u64::try_from(bytes.capacity()).map_err(|_| BudgetError::ArithmeticOverflow {
            resource: "project-root ignore source capacity",
        })?;
    budget.check_bytes(retained_bytes)?;
    budget.consume_entries(1)?;
    budget.consume_bytes(retained_bytes)?;

    let before = FileSnapshot::from_metadata(&metadata);
    let mut reader = handle.as_file();
    let mut buffer = [0_u8; 16 * 1024];
    while bytes.len() < expected_capacity {
        let remaining = expected_capacity - bytes.len();
        let read_length = remaining.min(buffer.len());
        let read = read_retry(&mut reader, &mut buffer[..read_length]).map_err(|source| {
            ScanError::IgnoreIo {
                file: file_name,
                operation: IgnoreReadOperation::Read,
                source,
            }
        })?;
        if read == 0 {
            return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }

    let mut extra = [0_u8; 1];
    if read_retry(&mut reader, &mut extra).map_err(|source| ScanError::IgnoreIo {
        file: file_name,
        operation: IgnoreReadOperation::Read,
        source,
    })? != 0
    {
        return if expected_bytes >= file_limit {
            Err(ScanError::IgnoreLimitExceeded {
                file: file_name,
                resource: IgnoreLimitResource::FileBytes,
                observed_at_least: expected_bytes.saturating_add(1),
                limit: file_limit,
            })
        } else {
            Err(ScanError::IgnoreChangedDuringRead { file: file_name })
        };
    }

    let after = handle
        .as_file()
        .metadata()
        .map_err(|source| ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Inspect,
            source,
        })?;
    if FileSnapshot::from_metadata(&after) != before {
        return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
    }
    let reopened = read_root
        .open_relative(Path::new(file_name))
        .and_then(Handle::from_file)
        .map_err(|source| ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Reopen,
            source,
        })?;
    let reopened_metadata =
        reopened
            .as_file()
            .metadata()
            .map_err(|source| ScanError::IgnoreIo {
                file: file_name,
                operation: IgnoreReadOperation::Inspect,
                source,
            })?;
    if reopened != handle || FileSnapshot::from_metadata(&reopened_metadata) != before {
        return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
    }

    Ok(Some(IgnoreSource {
        bytes,
        handle,
        snapshot: before,
    }))
}

fn validate_source_current(
    read_root: &super::platform::ProjectReadRoot,
    file_name: &'static str,
    source: &IgnoreSource,
) -> Result<(), ScanError> {
    let reopened = reopen_source_current(read_root, file_name, source)?;

    let mut reader = reopened.as_file();
    let mut buffer = [0_u8; 16 * 1024];
    let mut offset = 0_usize;
    while offset < source.bytes.len() {
        let expected = (source.bytes.len() - offset).min(buffer.len());
        let read = read_retry(&mut reader, &mut buffer[..expected]).map_err(|source| {
            ScanError::IgnoreIo {
                file: file_name,
                operation: IgnoreReadOperation::Read,
                source,
            }
        })?;
        if read == 0 || buffer[..read] != source.bytes[offset..offset + read] {
            return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
        }
        offset += read;
    }
    let mut extra = [0_u8; 1];
    if read_retry(&mut reader, &mut extra).map_err(|source| ScanError::IgnoreIo {
        file: file_name,
        operation: IgnoreReadOperation::Read,
        source,
    })? != 0
    {
        return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
    }
    let after = reopened
        .as_file()
        .metadata()
        .map_err(|source| ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Inspect,
            source,
        })?;
    if FileSnapshot::from_metadata(&after) != source.snapshot {
        return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
    }

    // Rebind after the full read: an atomic replacement can leave the opened file unchanged.
    let _ = reopen_source_current(read_root, file_name, source)?;
    Ok(())
}

fn reopen_source_current(
    read_root: &super::platform::ProjectReadRoot,
    file_name: &'static str,
    source: &IgnoreSource,
) -> Result<Handle, ScanError> {
    let reopened_file = match read_root.open_relative(Path::new(file_name)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
        }
        Err(source) => {
            return Err(ScanError::IgnoreIo {
                file: file_name,
                operation: IgnoreReadOperation::Reopen,
                source,
            });
        }
    };
    let reopened = Handle::from_file(reopened_file).map_err(|source| ScanError::IgnoreIo {
        file: file_name,
        operation: IgnoreReadOperation::Reopen,
        source,
    })?;
    let metadata = reopened
        .as_file()
        .metadata()
        .map_err(|source| ScanError::IgnoreIo {
            file: file_name,
            operation: IgnoreReadOperation::Inspect,
            source,
        })?;
    if reopened != source.handle || FileSnapshot::from_metadata(&metadata) != source.snapshot {
        return Err(ScanError::IgnoreChangedDuringRead { file: file_name });
    }
    Ok(reopened)
}

fn read_retry(reader: &mut &std::fs::File, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn visit_lines(
    file_name: &'static str,
    bytes: &[u8],
    line_limit: usize,
    mut visit: impl FnMut(u64, &str) -> Result<(), ScanError>,
) -> Result<(), ScanError> {
    let mut line_start = 0_usize;
    let mut line_number = 1_u64;
    for boundary in 0..=bytes.len() {
        if boundary != bytes.len() && bytes[boundary] != b'\n' {
            continue;
        }
        let mut line = &bytes[line_start..boundary];
        if boundary < bytes.len() && line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.len() > line_limit {
            let observed_at_least =
                u64::try_from(line.len()).map_err(|_| BudgetError::ArithmeticOverflow {
                    resource: "project-root ignore line bytes",
                })?;
            let limit = u64::try_from(line_limit).map_err(|_| BudgetError::ArithmeticOverflow {
                resource: "project-root ignore line bytes",
            })?;
            return Err(ScanError::IgnoreLimitExceeded {
                file: file_name,
                resource: IgnoreLimitResource::LineBytes,
                observed_at_least,
                limit,
            });
        }
        let line = std::str::from_utf8(line).map_err(|_| ScanError::IgnoreSyntax {
            file: file_name,
            line: Some(line_number),
            reason: IgnoreSyntaxReason::InvalidUtf8,
        })?;
        let line = if line_number == 1 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        visit(line_number, line)?;
        line_start = boundary.saturating_add(1);
        line_number = line_number
            .checked_add(1)
            .ok_or(BudgetError::ArithmeticOverflow {
                resource: "project-root ignore line number",
            })?;
    }
    Ok(())
}

fn normalized_rule_line(line: &str) -> Option<&str> {
    if line.starts_with('#') {
        return None;
    }
    let normalized = if line.ends_with("\\ ") {
        line
    } else {
        line.trim_end()
    };
    (!normalized.is_empty()).then_some(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use unity_asset_core::AssetLoadLimits;

    fn read_root(project: &TempDir) -> super::super::platform::ProjectReadRoot {
        super::super::platform::ProjectReadRoot::open(
            &project
                .path()
                .canonicalize()
                .expect("canonical project root"),
        )
        .expect("open project root")
    }

    #[test]
    fn matcher_uses_standard_gitignore_semantics_and_file_precedence() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".gitignore"),
            b"*.asset\n/Only.txt\n[Ll]ibrary/\nAssets/**/Generated?.bin\n",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".ignore"),
            b"!Assets/Allowed.asset\n!Assets/Reblocked.asset\n",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".unity-asset-search-ignore"),
            b"Assets/Reblocked.asset\n",
        )
        .unwrap();
        let root = read_root(&project);
        let mut budget = AssetLoadBudget::default();
        let matcher =
            RootIgnoreMatcher::load(&root, SearchIndexOptions::default(), &mut budget).unwrap();

        assert!(
            matcher
                .is_ignored(Path::new("Assets/Scene.asset"), false, &mut budget)
                .unwrap()
        );
        assert!(
            !matcher
                .is_ignored(Path::new("Assets/Allowed.asset"), false, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Assets/Reblocked.asset"), false, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Only.txt"), false, &mut budget)
                .unwrap()
        );
        assert!(
            !matcher
                .is_ignored(Path::new("Assets/Only.txt"), false, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Library"), true, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("library"), true, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Assets/Generated1.bin"), false, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(
                    Path::new("Assets/Nested/GeneratedA.bin"),
                    false,
                    &mut budget
                )
                .unwrap()
        );
    }

    #[test]
    fn shared_regex_set_preserves_extension_negation_directory_and_anchor_rules() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".ignore"),
            b"Assets/*.asset\n!Assets/Keep.asset\nAssets/Blocked/\nAssets/*/Generated?.asset\n/Root.asset\n",
        )
        .unwrap();
        let root = read_root(&project);
        let mut budget = AssetLoadBudget::default();
        let matcher =
            RootIgnoreMatcher::load(&root, SearchIndexOptions::default(), &mut budget).unwrap();

        assert!(
            matcher
                .is_ignored(
                    Path::new("Assets/Level/Generated1.asset"),
                    false,
                    &mut budget
                )
                .unwrap()
        );
        assert!(
            !matcher
                .is_ignored(Path::new("Assets/Keep.asset"), false, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Assets/Blocked"), true, &mut budget)
                .unwrap()
        );
        assert!(
            matcher
                .is_ignored(Path::new("Root.asset"), false, &mut budget)
                .unwrap()
        );
        assert!(
            !matcher
                .is_ignored(Path::new("Nested/Root.asset"), false, &mut budget)
                .unwrap()
        );
    }

    #[test]
    fn matcher_accepts_gitignore_bom_and_unclosed_class_compatibility() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".ignore"),
            "\u{feff}# generated\n[abc\n",
        )
        .unwrap();
        let root = read_root(&project);
        let mut budget = AssetLoadBudget::default();
        let matcher =
            RootIgnoreMatcher::load(&root, SearchIndexOptions::default(), &mut budget).unwrap();

        assert!(
            matcher
                .is_ignored(Path::new("[abc"), false, &mut budget)
                .unwrap()
        );
        assert!(
            !matcher
                .is_ignored(Path::new("# generated"), false, &mut budget)
                .unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn root_ignore_change_detection_follows_windows_case_insensitivity() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(".GITIGNORE");

        assert!(is_configured_project_root_ignore_file(
            project.path(),
            &path,
            SearchIndexOptions::default(),
        ));
    }

    #[test]
    fn loader_honors_the_exact_caller_budget_and_rejects_one_byte_less() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".ignore"), b"*.asset\n").unwrap();
        let root = read_root(&project);
        let options = SearchIndexOptions {
            max_project_root_ignore_file_bytes: 16,
            max_project_root_ignore_line_bytes: 16,
            max_project_root_ignore_patterns: 1,
            max_project_root_ignore_parser_work: 16,
            ..SearchIndexOptions::default()
        };
        let mut probe = AssetLoadBudget::default();
        RootIgnoreMatcher::load(&root, options, &mut probe).unwrap();
        let exact = probe.usage().bytes;

        let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        RootIgnoreMatcher::load(&root, options, &mut exact_budget).unwrap();

        let mut short_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        assert!(matches!(
            RootIgnoreMatcher::load(&root, options, &mut short_budget),
            Err(ScanError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == exact - 1 && requested == exact
        ));
    }

    #[cfg(windows)]
    #[test]
    fn each_match_honors_the_exact_caller_budget_and_rejects_one_byte_less() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".ignore"), b"*.asset\n").unwrap();
        let root = read_root(&project);
        let options = SearchIndexOptions::default();
        let path = Path::new("Assets/Scene.asset");

        let mut probe = AssetLoadBudget::default();
        let matcher = RootIgnoreMatcher::load(&root, options, &mut probe).unwrap();
        assert!(matcher.is_ignored(path, false, &mut probe).unwrap());
        let exact = probe.usage().bytes;

        let mut exact_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let matcher = RootIgnoreMatcher::load(&root, options, &mut exact_budget).unwrap();
        assert!(matcher.is_ignored(path, false, &mut exact_budget).unwrap());

        let mut short_budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: exact - 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let matcher = RootIgnoreMatcher::load(&root, options, &mut short_budget).unwrap();
        assert!(matches!(
            matcher.is_ignored(path, false, &mut short_budget),
            Err(ScanError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                requested,
            })) if limit == exact - 1 && requested == exact
        ));
    }

    #[test]
    fn compiler_reservation_is_charged_before_matcher_allocations() {
        let project = tempfile::tempdir().unwrap();
        let encoded = b"*.asset\n";
        std::fs::write(project.path().join(".ignore"), encoded).unwrap();
        let root = read_root(&project);
        let options = SearchIndexOptions {
            max_project_root_ignore_file_bytes: encoded.len() as u64,
            max_project_root_ignore_line_bytes: encoded.len(),
            max_project_root_ignore_patterns: 1,
            max_project_root_ignore_parser_work: (encoded.len() as u64) * PARSER_PASSES,
            ..SearchIndexOptions::default()
        };
        let source_and_parser_bytes = (encoded.len() as u64) * (PARSER_PASSES + 1);
        let mut budget = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: source_and_parser_bytes,
            ..AssetLoadLimits::default()
        })
        .unwrap();

        assert!(matches!(
            RootIgnoreMatcher::load(&root, options, &mut budget),
            Err(ScanError::Budget(BudgetError::Exceeded {
                resource: "bytes",
                limit,
                ..
            })) if limit == source_and_parser_bytes
        ));
    }

    #[test]
    fn compiler_reservation_uses_one_shared_regex_set_for_many_rules() {
        let one_rule = compiler_reservation_bytes(128, 1).unwrap();
        let many_rules = compiler_reservation_bytes(128, 1024).unwrap();

        assert!(many_rules > one_rule);
        assert!(many_rules < REGEX_SET_BYTES.saturating_mul(2));
    }

    #[test]
    fn final_validation_detects_same_length_content_replacement() {
        let project = tempfile::tempdir().unwrap();
        let ignore = project.path().join(".ignore");
        std::fs::write(&ignore, b"First.asset\n").unwrap();
        let root = read_root(&project);
        let options = SearchIndexOptions::default();
        let matcher =
            RootIgnoreMatcher::load(&root, options, &mut AssetLoadBudget::default()).unwrap();
        std::fs::write(&ignore, b"Other.asset\n").unwrap();

        assert!(matches!(
            matcher.validate_current(&root, options),
            Err(ScanError::IgnoreChangedDuringRead { file: ".ignore" })
        ));
    }

    #[test]
    fn parser_work_limit_is_exact_and_typed() {
        let project = tempfile::tempdir().unwrap();
        let encoded = b"*.asset\n";
        std::fs::write(project.path().join(".ignore"), encoded).unwrap();
        let root = read_root(&project);
        let exact_work = (encoded.len() as u64) * PARSER_PASSES;
        let exact_options = SearchIndexOptions {
            max_project_root_ignore_file_bytes: encoded.len() as u64,
            max_project_root_ignore_line_bytes: encoded.len(),
            max_project_root_ignore_patterns: 1,
            max_project_root_ignore_parser_work: exact_work,
            ..SearchIndexOptions::default()
        };

        RootIgnoreMatcher::load(&root, exact_options, &mut AssetLoadBudget::default()).unwrap();

        let error = RootIgnoreMatcher::load(
            &root,
            SearchIndexOptions {
                max_project_root_ignore_parser_work: exact_work - 1,
                ..exact_options
            },
            &mut AssetLoadBudget::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ScanError::IgnoreLimitExceeded {
                file: POLICY_SOURCE,
                resource: IgnoreLimitResource::ParserWork,
                observed_at_least,
                limit,
            } if observed_at_least == exact_work && limit == exact_work - 1
        ));
    }
}
