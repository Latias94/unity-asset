use crate::fast_path;
use crate::shared::{AppContext, cli_warn};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use unity_asset::reference::{
    ReferenceGraph, ReferenceGraphBuildOptions, ReferenceProjectionFormat,
    ReferenceProjectionOptions, ReferenceResolution, ReferenceResolutionCounts,
    ReferenceTruncationKind,
};
use unity_asset::workspace::{AssetWorkspace, SourceOpenRequest, WorkspaceOptions};
use unity_asset::{AssetLoadBudget, BudgetError, ObjectAddress, SourceAlias, SourceKind};
use unity_asset_binary::file::UnityFileKind;

const PROBE_PREFIX_LEN: usize = 64;
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    "Library",
    "Temp",
    "Logs",
    ".git",
    ".vs",
    "obj",
    "bin",
    "UserSettings",
];

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ScanTruncation {
    kind: &'static str,
    limit: usize,
    observed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkspaceScanReport {
    complete: bool,
    files_discovered: usize,
    files_considered: usize,
    files_selected: usize,
    files_skipped: usize,
    root_sources_loaded: usize,
    workspace_sources: u64,
    meta_files: usize,
    yaml_files: usize,
    binary_files: usize,
    container_files: usize,
    truncations: Vec<ScanTruncation>,
}

impl WorkspaceScanReport {
    fn new(files_discovered: usize, files_considered: usize) -> Self {
        Self {
            complete: files_discovered == files_considered,
            files_discovered,
            files_considered,
            files_selected: 0,
            files_skipped: 0,
            root_sources_loaded: 0,
            workspace_sources: 0,
            meta_files: 0,
            yaml_files: 0,
            binary_files: 0,
            container_files: 0,
            truncations: Vec::new(),
        }
    }
}

pub(crate) struct LoadedReferenceGraph {
    pub(crate) graph: ReferenceGraph,
    pub(crate) scan: WorkspaceScanReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateClass {
    Meta,
    Yaml,
    Binary,
    Container,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    class: CandidateClass,
    kind_hint: Option<SourceKind>,
}

struct CandidateDiscovery {
    candidates: Vec<(PathBuf, Candidate)>,
    files_discovered: usize,
    files_skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscoveryPolicy {
    Generic,
    UnityProject,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct TruncationOutput {
    kind: &'static str,
    limit: u64,
    observed: u64,
}

#[derive(Debug, Serialize)]
struct CoverageOutput {
    total_sources: u64,
    scanned_sources: u64,
    total_nodes: u64,
    indexed_nodes: u64,
    fact_count: u64,
    complete: bool,
    truncations: Vec<TruncationOutput>,
}

#[derive(Debug, Serialize)]
struct OutputLimit {
    max_facts: u64,
    facts_written: u64,
    total_facts: u64,
    complete: bool,
    truncations: Vec<TruncationOutput>,
}

#[derive(Debug, Serialize)]
struct ScanLine<'a> {
    kind: &'static str,
    scan: &'a WorkspaceScanReport,
}

#[derive(Debug, Serialize)]
struct OutputLine {
    kind: &'static str,
    output: OutputLimit,
}

struct CliBudgetWriter<'a, W: Write + ?Sized> {
    output: &'a mut W,
    budget: &'a mut AssetLoadBudget,
    budget_error: Option<BudgetError>,
}

impl<W: Write + ?Sized> Write for CliBudgetWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.budget_error.is_some() {
            return Err(io::Error::other("CLI output budget is exhausted"));
        }
        let requested = u64::try_from(buffer.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "CLI output length overflow")
        })?;
        if let Err(error) = self.budget.check_bytes(requested) {
            self.budget_error = Some(error);
            return Err(io::Error::other("CLI output budget is exhausted"));
        }
        let written = self.output.write(buffer)?;
        let written = u64::try_from(written).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "CLI output length overflow")
        })?;
        if let Err(error) = self.budget.consume_bytes(written) {
            self.budget_error = Some(error);
            return Err(io::Error::other("CLI output budget is exhausted"));
        }
        usize::try_from(written)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "CLI output length overflow"))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn with_budgeted_output<W, F>(output: &mut W, budget: &mut AssetLoadBudget, write: F) -> Result<()>
where
    W: Write + ?Sized,
    F: FnOnce(&mut CliBudgetWriter<'_, W>) -> Result<()>,
{
    let mut output = CliBudgetWriter {
        output,
        budget,
        budget_error: None,
    };
    let result = write(&mut output);
    match output.budget_error {
        Some(error) => Err(error.into()),
        None => result,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandFormat {
    Summary,
    Edges,
    Dot,
    Json,
    JsonLines,
}

pub(crate) fn run(
    input: PathBuf,
    format: String,
    max_edges: usize,
    ctx: &AppContext,
) -> Result<()> {
    validate_reference_format(&format)?;
    let mut budget = AssetLoadBudget::default();
    let loaded = load_reference_graph(
        &input,
        true,
        None,
        None,
        DiscoveryPolicy::Generic,
        ctx,
        &mut budget,
    )?;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    write_reference_output(&loaded, &mut output, &format, max_edges, &mut budget)
}

pub(crate) fn load_reference_graph(
    input: &Path,
    include_yaml: bool,
    max_files: Option<usize>,
    excluded_path: Option<&Path>,
    discovery_policy: DiscoveryPolicy,
    ctx: &AppContext,
    budget: &mut AssetLoadBudget,
) -> Result<LoadedReferenceGraph> {
    let input = std::path::absolute(input).context("Failed to normalize the input path")?;
    let input = input.as_path();
    if !input.exists() {
        anyhow::bail!("Input does not exist: {}", input.display());
    }

    let workspace_options = if ctx.strict {
        WorkspaceOptions::strict()
    } else {
        WorkspaceOptions::lenient()
    };
    let workspace_options = workspace_options
        .with_type_tree_registry_paths(ctx.typetree_registries(), budget)
        .context("Failed to load --typetree-registry paths")?;
    let mut workspace = AssetWorkspace::with_options(workspace_options)
        .context("Failed to initialize asset workspace")?;

    let discovery =
        discover_candidates(input, include_yaml, excluded_path, discovery_policy, budget)?;
    let files_selected = discovery.candidates.len();
    let files_to_load = max_files.map_or(files_selected, |maximum| maximum.min(files_selected));
    let mut scan = WorkspaceScanReport::new(discovery.files_discovered, discovery.files_discovered);
    scan.files_selected = files_selected;
    scan.files_skipped = discovery.files_skipped;
    scan.complete = files_to_load == files_selected;
    if files_to_load < files_selected {
        scan.truncations.push(ScanTruncation {
            kind: "files",
            limit: files_to_load,
            observed: files_selected,
        });
    }

    for (path, candidate) in discovery.candidates.into_iter().take(files_to_load) {
        match candidate.class {
            CandidateClass::Meta => scan.meta_files += 1,
            CandidateClass::Yaml => scan.yaml_files += 1,
            CandidateClass::Binary => scan.binary_files += 1,
            CandidateClass::Container => scan.container_files += 1,
        }

        let alias = source_alias(input, &path)?;
        let request = SourceOpenRequest::new(path.clone(), alias);
        let request = match candidate.kind_hint {
            Some(kind) => request.with_kind_hint(kind),
            None => request,
        };
        workspace
            .load_source(request, budget)
            .with_context(|| format!("Failed to load Unity source {}", path.display()))?;
        scan.root_sources_loaded = scan
            .root_sources_loaded
            .checked_add(1)
            .context("Loaded-source count overflow")?;
    }

    if scan.root_sources_loaded == 0 && input.is_file() {
        anyhow::bail!("Input is not a supported Unity source: {}", input.display());
    }
    if scan.root_sources_loaded == 0 {
        cli_warn(
            ctx.show_warnings,
            format!("no supported Unity sources found under {}", input.display()),
        );
    }

    let snapshot = workspace.snapshot();
    let graph = snapshot
        .reference_graph(ReferenceGraphBuildOptions::unbounded(), budget)
        .context("Failed to build the revision-bound reference graph")?;
    scan.workspace_sources = graph.coverage().total_sources();
    Ok(LoadedReferenceGraph { graph, scan })
}

fn discover_candidates(
    input: &Path,
    include_yaml: bool,
    excluded_path: Option<&Path>,
    discovery_policy: DiscoveryPolicy,
    budget: &mut AssetLoadBudget,
) -> Result<CandidateDiscovery> {
    let excluded_path = excluded_path
        .map(std::path::absolute)
        .transpose()
        .context("Failed to normalize the excluded output path")?;
    let mut discovered =
        fast_path::collect_candidate_paths_filtered_budgeted(input, budget, |directory| {
            discovery_policy != DiscoveryPolicy::UnityProject
                || !is_skipped_root_project_directory(input, directory)
        })
        .with_context(|| format!("Failed to discover input files under {}", input.display()))?;
    discovered.retain(|path| {
        excluded_path
            .as_ref()
            .is_none_or(|excluded| path != excluded)
    });
    let files_discovered = discovered.len();
    let explicit_file = input.is_file();
    let mut candidates = Vec::new();
    let candidate_allocation = files_discovered
        .checked_mul(std::mem::size_of::<(PathBuf, Candidate)>())
        .context("Unity source discovery allocation overflow")?;
    budget.check_bytes(
        u64::try_from(candidate_allocation)
            .context("Unity source discovery allocation does not fit u64")?,
    )?;
    candidates
        .try_reserve_exact(files_discovered)
        .context("Failed to reserve Unity source discovery results")?;
    budget.consume_bytes(
        u64::try_from(candidate_allocation)
            .context("Unity source discovery allocation does not fit u64")?,
    )?;
    let mut files_skipped = 0_usize;
    for path in discovered {
        match classify_candidate(&path, include_yaml, explicit_file, budget)? {
            Some(candidate) => candidates.push((path, candidate)),
            None => {
                files_skipped = files_skipped
                    .checked_add(1)
                    .context("Skipped-file count overflow")?;
            }
        }
    }
    Ok(CandidateDiscovery {
        candidates,
        files_discovered,
        files_skipped,
    })
}

pub(crate) fn write_reference_output<W: Write + ?Sized>(
    loaded: &LoadedReferenceGraph,
    output: &mut W,
    format: &str,
    max_edges: usize,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let format = parse_format(format)?;
    let max_facts = u64::try_from(max_edges).context("--max-edges does not fit u64")?;
    match format {
        CommandFormat::Summary | CommandFormat::Edges => {
            let limit = output_limit(&loaded.graph, max_facts)?;
            if format == CommandFormat::Edges {
                budget.check_entries(limit.facts_written)?;
            }
            let counts = loaded.graph.resolution_counts(budget)?;
            if format == CommandFormat::Edges {
                budget.consume_entries(limit.facts_written)?;
            }
            with_budgeted_output(output, budget, |output| match format {
                CommandFormat::Summary => write_summary(loaded, output, &counts, &limit),
                CommandFormat::Edges => write_edges(loaded, output, &counts, &limit),
                CommandFormat::Dot | CommandFormat::Json | CommandFormat::JsonLines => {
                    unreachable!("text output branch only accepts summary or edges")
                }
            })
        }
        CommandFormat::Dot => write_projection(
            loaded,
            output,
            ReferenceProjectionFormat::DotV1,
            max_facts,
            budget,
        ),
        CommandFormat::Json => write_projection(
            loaded,
            output,
            ReferenceProjectionFormat::JsonV1,
            max_facts,
            budget,
        ),
        CommandFormat::JsonLines => write_projection(
            loaded,
            output,
            ReferenceProjectionFormat::JsonLinesV1,
            max_facts,
            budget,
        ),
    }
}

fn classify_candidate(
    path: &Path,
    include_yaml: bool,
    explicit_file: bool,
    budget: &mut AssetLoadBudget,
) -> Result<Option<Candidate>> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("meta") {
        return Ok(Some(Candidate {
            class: CandidateClass::Meta,
            kind_hint: Some(SourceKind::Yaml),
        }));
    }

    let mut prefix = [0_u8; PROBE_PREFIX_LEN];
    let prefix_len = fast_path::read_prefix_into(path, &mut prefix)
        .with_context(|| format!("Failed to probe {}", path.display()))?;
    budget.consume_bytes(
        u64::try_from(prefix_len).context("Unity source probe length does not fit u64")?,
    )?;
    let prefix = &prefix[..prefix_len];
    if let Some(kind) = fast_path::sniff_unity_file_kind_prefix(prefix) {
        let (class, kind_hint) = match kind {
            UnityFileKind::SerializedFile => (CandidateClass::Binary, SourceKind::SerializedFile),
            UnityFileKind::AssetBundle => (CandidateClass::Container, SourceKind::AssetBundle),
            UnityFileKind::WebFile => (CandidateClass::Container, SourceKind::WebFile),
        };
        return Ok(Some(Candidate {
            class,
            kind_hint: Some(kind_hint),
        }));
    }
    if is_archive(path, prefix) {
        return Ok(Some(Candidate {
            class: CandidateClass::Container,
            kind_hint: Some(SourceKind::Archive),
        }));
    }
    if include_yaml && is_project_yaml_extension(extension) {
        return Ok(Some(Candidate {
            class: CandidateClass::Yaml,
            // `.asset` can also contain a SerializedFile. Let the workspace perform its
            // binary-first format decision instead of forcing a YAML parse.
            kind_hint: None,
        }));
    }
    if explicit_file {
        return Ok(Some(Candidate {
            class: CandidateClass::Yaml,
            kind_hint: None,
        }));
    }
    Ok(None)
}

fn is_project_yaml_extension(extension: &str) -> bool {
    ["asset", "prefab", "unity"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn is_archive(path: &Path, prefix: &[u8]) -> bool {
    let zip_signature = prefix.starts_with(b"PK\x03\x04")
        || prefix.starts_with(b"PK\x05\x06")
        || prefix.starts_with(b"PK\x07\x08");
    zip_signature
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("apk")
            })
}

fn is_skipped_root_project_directory(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                SKIPPED_DIRECTORY_NAMES
                    .iter()
                    .any(|skipped| name.eq_ignore_ascii_case(skipped))
            })
}

fn source_alias(root: &Path, path: &Path) -> Result<SourceAlias> {
    let relative = if root.is_dir() {
        path.strip_prefix(root).with_context(|| {
            format!(
                "Discovered source {} is outside input root {}",
                path.display(),
                root.display()
            )
        })?
    } else {
        path.file_name()
            .map(Path::new)
            .context("Input source has no file name")?
    };
    let relative = relative
        .to_str()
        .with_context(|| format!("Source path is not UTF-8: {}", relative.display()))?;
    let portable = relative.replace('\\', "/");
    SourceAlias::new(portable).context("Source path cannot be represented as a portable alias")
}

fn parse_format(format: &str) -> Result<CommandFormat> {
    match format.to_ascii_lowercase().as_str() {
        "summary" => Ok(CommandFormat::Summary),
        "edges" => Ok(CommandFormat::Edges),
        "dot" => Ok(CommandFormat::Dot),
        "json" => Ok(CommandFormat::Json),
        "jsonl" | "json-lines" => Ok(CommandFormat::JsonLines),
        other => anyhow::bail!(
            "Invalid --format: {} (expected summary|edges|dot|json|jsonl)",
            other
        ),
    }
}

pub(crate) fn validate_reference_format(format: &str) -> Result<()> {
    parse_format(format).map(|_| ())
}

fn coverage_output(graph: &ReferenceGraph) -> CoverageOutput {
    let coverage = graph.coverage();
    CoverageOutput {
        total_sources: coverage.total_sources(),
        scanned_sources: coverage.scanned_sources(),
        total_nodes: coverage.total_nodes(),
        indexed_nodes: coverage.indexed_nodes(),
        fact_count: coverage.fact_count(),
        complete: coverage.is_complete(),
        truncations: coverage
            .truncations()
            .iter()
            .map(|truncation| TruncationOutput {
                kind: match truncation.kind() {
                    ReferenceTruncationKind::Nodes => "nodes",
                    ReferenceTruncationKind::Facts => "facts",
                },
                limit: truncation.limit(),
                observed: truncation.observed(),
            })
            .collect(),
    }
}

fn output_limit(graph: &ReferenceGraph, max_facts: u64) -> Result<OutputLimit> {
    let total_facts = u64::try_from(graph.facts().len()).context("Fact count does not fit u64")?;
    let facts_written = max_facts.min(total_facts);
    let truncations = (facts_written < total_facts)
        .then_some(TruncationOutput {
            kind: "facts",
            limit: max_facts,
            observed: total_facts,
        })
        .into_iter()
        .collect();
    Ok(OutputLimit {
        max_facts,
        facts_written,
        total_facts,
        complete: facts_written == total_facts,
        truncations,
    })
}

fn write_common_header<W: Write + ?Sized>(
    loaded: &LoadedReferenceGraph,
    output: &mut W,
    counts: &ReferenceResolutionCounts,
    limit: &OutputLimit,
) -> Result<()> {
    writeln!(output, "workspace={}", loaded.graph.workspace_id())?;
    writeln!(output, "revision={}", loaded.graph.revision())?;
    output.write_all(b"scan=")?;
    serde_json::to_writer(&mut *output, &loaded.scan)?;
    output.write_all(b"\ncoverage=")?;
    serde_json::to_writer(&mut *output, &coverage_output(&loaded.graph))?;
    output.write_all(b"\nresolution_counts=")?;
    serde_json::to_writer(&mut *output, counts)?;
    output.write_all(b"\nprojection=")?;
    serde_json::to_writer(&mut *output, limit)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn write_summary<W: Write + ?Sized>(
    loaded: &LoadedReferenceGraph,
    output: &mut W,
    counts: &ReferenceResolutionCounts,
    limit: &OutputLimit,
) -> Result<()> {
    write_common_header(loaded, output, counts, limit)?;
    writeln!(output, "nodes={}", loaded.graph.nodes().len())?;
    writeln!(output, "facts={}", loaded.graph.facts().len())?;
    writeln!(output, "roots={}", loaded.graph.roots().count())?;
    writeln!(output, "leaves={}", loaded.graph.leaves().count())?;
    writeln!(output, "diagnostics={}", loaded.graph.diagnostics().len())?;
    Ok(())
}

fn write_edges<W: Write + ?Sized>(
    loaded: &LoadedReferenceGraph,
    output: &mut W,
    counts: &ReferenceResolutionCounts,
    limit: &OutputLimit,
) -> Result<()> {
    write_common_header(loaded, output, counts, limit)?;
    let maximum = usize::try_from(limit.facts_written).unwrap_or(usize::MAX);
    for fact in loaded.graph.facts().iter().take(maximum) {
        let source = loaded.graph.address(fact.source())?;
        let resolution = ResolutionLabel::new(&loaded.graph, fact.resolution())?;
        writeln!(
            output,
            "edge source={} field={} state={} raw={:?}",
            AddressLabel(source),
            fact.field_path(),
            resolution,
            fact.raw_target()
        )?;
    }
    Ok(())
}

fn write_projection<W: Write + ?Sized>(
    loaded: &LoadedReferenceGraph,
    output: &mut W,
    format: ReferenceProjectionFormat,
    max_facts: u64,
    budget: &mut AssetLoadBudget,
) -> Result<()> {
    let options = ReferenceProjectionOptions::new(format).with_max_facts(max_facts);
    let output_limit = output_limit(&loaded.graph, max_facts)?;
    match format {
        ReferenceProjectionFormat::JsonV1 => {
            with_budgeted_output(output, budget, |output| {
                output.write_all(b"{\"scan\":")?;
                serde_json::to_writer(&mut *output, &loaded.scan)?;
                output.write_all(b",\"output\":")?;
                serde_json::to_writer(&mut *output, &output_limit)?;
                output.write_all(b",\"graph\":")?;
                Ok(())
            })?;
            loaded
                .graph
                .write_projection(&mut *output, options, budget)?;
            with_budgeted_output(output, budget, |output| {
                output.write_all(b"}\n")?;
                Ok(())
            })?;
        }
        ReferenceProjectionFormat::JsonLinesV1 => {
            loaded
                .graph
                .write_projection(&mut *output, options, budget)?;
            with_budgeted_output(output, budget, |output| {
                serde_json::to_writer(
                    &mut *output,
                    &ScanLine {
                        kind: "scan",
                        scan: &loaded.scan,
                    },
                )?;
                output.write_all(b"\n")?;
                serde_json::to_writer(
                    &mut *output,
                    &OutputLine {
                        kind: "output",
                        output: output_limit,
                    },
                )?;
                output.write_all(b"\n")?;
                Ok(())
            })?;
        }
        ReferenceProjectionFormat::DotV1 => {
            with_budgeted_output(output, budget, |output| {
                output.write_all(b"// scan=")?;
                serde_json::to_writer(&mut *output, &loaded.scan)?;
                output.write_all(b"\n// output=")?;
                serde_json::to_writer(&mut *output, &output_limit)?;
                output.write_all(b"\n")?;
                Ok(())
            })?;
            loaded
                .graph
                .write_projection(&mut *output, options, budget)?;
        }
    }
    Ok(())
}

struct AddressLabel<'a>(&'a ObjectAddress);

impl fmt::Display for AddressLabel<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let locator = self.0.source_locator();
        formatter.write_str(locator.root_alias().as_str())?;
        for step in locator.members() {
            write!(
                formatter,
                "::{}[occurrence={}]:{}",
                step.container().tag(),
                step.member().same_name_occurrence(),
                step.name()
            )?;
        }
        formatter.write_str(":")?;
        if let Some(path_id) = self.0.binary_path_id() {
            write!(formatter, "path_id={path_id}")
        } else if let Some(anchor) = self.0.yaml_anchor() {
            write!(formatter, "anchor={anchor}")
        } else if let Some(document) = self.0.yaml_document_ordinal() {
            write!(formatter, "document={document}")
        } else {
            formatter.write_str("unknown")
        }
    }
}

struct ResolutionLabel<'a> {
    resolution: &'a ReferenceResolution,
    resolved: Option<&'a ObjectAddress>,
}

impl<'a> ResolutionLabel<'a> {
    fn new(graph: &'a ReferenceGraph, resolution: &'a ReferenceResolution) -> Result<Self> {
        let resolved = resolution
            .resolved()
            .map(|target| graph.address(target))
            .transpose()?;
        Ok(Self {
            resolution,
            resolved,
        })
    }
}

impl fmt::Display for ResolutionLabel<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.resolution {
            ReferenceResolution::Null => formatter.write_str("null"),
            ReferenceResolution::Resolved(_) => {
                let target = self.resolved.ok_or(fmt::Error)?;
                write!(formatter, "resolved({})", AddressLabel(target))
            }
            ReferenceResolution::Unloaded { source } => {
                write!(formatter, "unloaded({source:?})")
            }
            ReferenceResolution::Missing { target } => write!(formatter, "missing({target:?})"),
            ReferenceResolution::Ambiguous { candidates } => {
                write!(formatter, "ambiguous(candidates={})", candidates.len())
            }
            ReferenceResolution::Invalid { diagnostic } => {
                write!(formatter, "invalid(code={})", diagnostic.code())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use unity_asset::{AssetLoadLimits, ContainmentKind, SourceLocator, SourceMemberId};

    use super::*;

    #[test]
    fn nested_sources_receive_portable_root_relative_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let source = root.join("Assets").join("Shared").join("thing.asset");
        assert_eq!(
            source_alias(root, &source).unwrap().as_str(),
            "Assets/Shared/thing.asset"
        );
    }

    #[test]
    fn address_labels_distinguish_duplicate_container_member_occurrences() {
        let address = |occurrence| {
            ObjectAddress::yaml(
                SourceLocator::path("content.zip")
                    .unwrap()
                    .child(
                        ContainmentKind::Archive,
                        SourceMemberId::with_occurrence("nested/target.prefab", occurrence)
                            .unwrap(),
                    )
                    .unwrap(),
                "123",
            )
            .unwrap()
        };
        let first = AddressLabel(&address(0)).to_string();
        let second = AddressLabel(&address(1)).to_string();

        assert_ne!(first, second);
        assert!(first.contains("::archive[occurrence=0]:nested/target.prefab"));
        assert!(second.contains("::archive[occurrence=1]:nested/target.prefab"));
    }

    #[test]
    fn meta_is_selected_even_when_yaml_documents_are_disabled() {
        let candidate = classify_candidate(
            Path::new("asset.prefab.meta"),
            false,
            false,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(
            candidate,
            Some(Candidate {
                class: CandidateClass::Meta,
                kind_hint: Some(SourceKind::Yaml),
            })
        );
    }

    #[test]
    fn discovery_prunes_generated_directories_before_selecting_supported_sources() {
        let directory = tempfile::tempdir().unwrap();
        let assets = directory.path().join("Assets");
        let library = directory.path().join("Library");
        let nested_temp = assets.join("Temp");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&nested_temp).unwrap();
        let target = assets.join("000-target.prefab");
        std::fs::write(
            &target,
            b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Name: Target\n",
        )
        .unwrap();
        std::fs::write(assets.join("000-unrelated.txt"), b"not a Unity source").unwrap();
        std::fs::write(library.join("000-generated.prefab"), b"generated").unwrap();
        let nested = nested_temp.join("nested.prefab");
        std::fs::write(&nested, b"nested Unity YAML").unwrap();

        let discovery = discover_candidates(
            directory.path(),
            true,
            None,
            DiscoveryPolicy::UnityProject,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.files_discovered, 3);
        assert_eq!(discovery.files_skipped, 1);
        assert_eq!(discovery.candidates.len(), 2);
        assert_eq!(discovery.candidates[0].0, target);
        assert_eq!(discovery.candidates[1].0, nested);

        let generic = discover_candidates(
            directory.path(),
            true,
            None,
            DiscoveryPolicy::Generic,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(generic.files_discovered, 4);
        assert_eq!(generic.candidates.len(), 3);

        let output = assets.join("graph.asset");
        std::fs::write(&output, b"stale output").unwrap();
        let discovery = discover_candidates(
            directory.path(),
            true,
            Some(&output),
            DiscoveryPolicy::UnityProject,
            &mut AssetLoadBudget::default(),
        )
        .unwrap();
        assert_eq!(discovery.files_discovered, 3);
        assert_eq!(discovery.candidates.len(), 2);
        assert_eq!(discovery.candidates[0].0, target);
    }

    #[test]
    fn cli_envelope_writer_has_an_exact_byte_budget_boundary() {
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 3,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let mut output = Vec::new();
        with_budgeted_output(&mut output, &mut exact, |output| {
            output.write_all(b"abc")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(exact.usage().bytes, 3);

        let mut one_short = AssetLoadBudget::new(AssetLoadLimits {
            max_bytes: 2,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        let error = with_budgeted_output(&mut Vec::new(), &mut one_short, |output| {
            output.write_all(b"abc")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.downcast_ref::<BudgetError>().is_some());
        assert_eq!(one_short.usage().bytes, 0);
    }

    #[test]
    fn edge_output_uses_member_analysis_and_entry_projection_ledgers_once() {
        let loaded = loaded_graph_with_one_fact();
        let mut exact = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        write_reference_output(&loaded, &mut Vec::new(), "edges", 1, &mut exact).unwrap();
        assert_eq!(exact.usage().entries, 1);
        assert_eq!(exact.usage().members, 1);

        let mut member_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        member_short.consume_members(1).unwrap();
        let mut member_output = Vec::new();
        let error =
            write_reference_output(&loaded, &mut member_output, "edges", 1, &mut member_short)
                .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<unity_asset::reference::ReferenceGraphError>(),
            Some(unity_asset::reference::ReferenceGraphError::Budget(_))
        ));
        assert!(member_output.is_empty());
        assert_eq!(member_short.usage().entries, 0);
        assert_eq!(member_short.usage().members, 1);

        let mut entry_short = AssetLoadBudget::new(AssetLoadLimits {
            max_entries: 1,
            max_members: 1,
            ..AssetLoadLimits::default()
        })
        .unwrap();
        entry_short.consume_entries(1).unwrap();
        let mut entry_output = Vec::new();
        let error =
            write_reference_output(&loaded, &mut entry_output, "edges", 1, &mut entry_short)
                .unwrap_err();
        assert!(error.downcast_ref::<BudgetError>().is_some());
        assert!(entry_output.is_empty());
        assert_eq!(entry_short.usage().entries, 1);
        assert_eq!(entry_short.usage().members, 0);
    }

    fn loaded_graph_with_one_fact() -> LoadedReferenceGraph {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.prefab");
        std::fs::write(
            &source,
            b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n--- !u!1 &1\nGameObject:\n  m_Target: {fileID: 2}\n--- !u!1 &2\nGameObject:\n  m_Name: Target\n",
        )
        .unwrap();
        let mut workspace = AssetWorkspace::new().unwrap();
        workspace
            .load_source(
                SourceOpenRequest::new(source, SourceAlias::new("source.prefab").unwrap())
                    .with_kind_hint(SourceKind::Yaml),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        let graph = workspace
            .snapshot()
            .reference_graph(
                ReferenceGraphBuildOptions::unbounded(),
                &mut AssetLoadBudget::default(),
            )
            .unwrap();
        assert_eq!(graph.facts().len(), 1);
        LoadedReferenceGraph {
            graph,
            scan: WorkspaceScanReport::new(1, 1),
        }
    }
}
