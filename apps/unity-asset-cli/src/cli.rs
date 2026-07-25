use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "unity_asset")]
#[command(about = "A Rust-based Unity asset parser")]
#[command(version)]
pub(crate) struct Cli {
    /// Fail-fast TypeTree parsing (no best-effort fallbacks)
    #[arg(long)]
    pub(crate) strict: bool,

    /// Print collected load warnings and TypeTree warnings (when applicable)
    #[arg(long)]
    pub(crate) show_warnings: bool,

    /// External TypeTree registry JSON/TPK (best-effort fallback for stripped assets).
    ///
    /// Can be repeated; earlier registries take precedence (first match wins).
    #[arg(long)]
    pub(crate) typetree_registry: Vec<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Parse a Unity YAML file
    ParseYaml {
        /// Input YAML file path
        #[arg(short, long)]
        input: PathBuf,

        /// Output format (summary, detailed, json)
        #[arg(short, long, default_value = "debug")]
        format: String,

        /// Preserve original types instead of converting to strings
        #[arg(long)]
        preserve_types: bool,
    },

    /// Export one deterministic, revision-bound artifact set.
    ///
    /// Writes the canonical plan (`--dry-run`) or canonical manifest to stdout. Use
    /// `--manifest` to persist the manifest safely under the output root for `--resume`;
    /// all operational diagnostics use stderr.
    Export(Box<ExportCommand>),

    /// Split Unity YAML documents through the safe artifact-set publisher.
    #[command(name = "split-yaml")]
    SplitYaml {
        /// Input Unity YAML source or directory.
        #[arg(short, long)]
        input: PathBuf,

        /// Root directory for split YAML documents.
        #[arg(short, long)]
        output: PathBuf,

        /// `error`, `skip`, or `replace` for existing document paths.
        #[arg(long, default_value = "error")]
        existing_output: String,
    },

    /// List AssetBundle nodes (files) for debugging and inspection
    ListBundle {
        /// Input AssetBundle path
        #[arg(short, long)]
        input: PathBuf,

        /// Filter node names by substring (case-insensitive). Empty means show all.
        #[arg(long, default_value = "")]
        filter: String,

        /// Print offsets and sizes
        #[arg(long)]
        verbose: bool,
    },

    /// List binary objects (path_id/class_id/peek_name) from SerializedFiles or bundles
    #[command(name = "list-objects")]
    ListObjects {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Source kind: `all`, `bundle`, or `serialized`
        #[arg(long, default_value = "serialized")]
        kind: String,

        /// Restrict listing to a specific loaded source path
        #[arg(long)]
        source: Option<PathBuf>,

        /// Restrict listing to a specific bundle asset index (only applies when --kind bundle or all)
        #[arg(long)]
        asset_index: Option<usize>,

        /// Filter by Unity class ID (repeatable). Example: `--class-id 28` (Texture2D).
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by Unity class name substring (case-insensitive). Example: `--class-name Texture`.
        #[arg(long, default_value = "")]
        class_name: String,

        /// Filter by object `m_Name`/`name` substring (case-insensitive) via a TypeTree prefix fast path.
        ///
        /// Note: this requires TypeTree to be present and to include a name field; otherwise the object is treated as non-matching.
        #[arg(long, default_value = "")]
        name: String,

        /// Limit printed objects
        #[arg(long)]
        limit: Option<usize>,

        /// Print one JSON object per line
        #[arg(long)]
        json: bool,
    },

    /// Print parsing stats (SerializedFile header/version/metadata) for loaded sources
    Stats {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Source kind: `all`, `bundle`, or `serialized`
        #[arg(long, default_value = "all")]
        kind: String,

        /// Limit scanned/printed entries
        #[arg(long)]
        limit: Option<usize>,

        /// Print aggregated counts instead of per-source records
        #[arg(long)]
        summary: bool,

        /// Print one JSON object per line
        #[arg(long)]
        json: bool,
    },

    /// Summarize binary `path_id` distributions (negative/zero/positive) for UnityCN/Tuanjie investigations
    #[command(name = "stats-pathid")]
    StatsPathId {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Source kind: `all`, `bundle`, or `serialized`
        #[arg(long, default_value = "all")]
        kind: String,

        /// Limit scanned serialized files (bundle assets count as one each)
        #[arg(long)]
        limit: Option<usize>,

        /// Check for duplicate path IDs within each serialized file (slower; uses a HashSet)
        #[arg(long)]
        check_duplicates: bool,

        /// Print one JSON summary object
        #[arg(long)]
        json: bool,
    },

    /// Find objects by AssetBundle `m_Container` asset path pattern (UnityPy-like discovery)
    FindObject {
        /// Input file or directory path (bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Filter container entries by substring or glob (`*`, `?`) (case-insensitive). Empty means show all.
        #[arg(long, default_value = "")]
        pattern: String,

        /// Filter by object `m_Name`/`name` substring (case-insensitive) via a TypeTree prefix fast path.
        ///
        /// Note: this requires TypeTree to be present and to include a name field; otherwise the object is treated as non-matching.
        #[arg(long, default_value = "")]
        name: String,

        /// Filter by Unity class ID (repeatable). Example: `--class-id 83` (AudioClip).
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by Unity class name substring (case-insensitive). Example: `--class-name Texture`.
        #[arg(long, default_value = "")]
        class_name: String,

        /// Limit matched entries
        #[arg(long)]
        limit: Option<usize>,

        /// Include entries that could not be resolved to an Object Address
        #[arg(long)]
        include_unresolved: bool,

        /// Print extra object info (type_id, byte_size) when resolvable
        #[arg(long)]
        verbose: bool,
    },

    /// Inspect a single object by source location (useful for TypeTree debugging)
    InspectObject {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Object Address emitted by `find-object` (overrides --source/--kind/--asset-index/--path-id)
        #[arg(long)]
        address: Option<String>,

        /// Source file path that contains the object (an AssetBundle or a standalone SerializedFile).
        ///
        /// When `--input` is a single file, this defaults to `--input`.
        #[arg(long)]
        source: Option<PathBuf>,

        /// Source kind: `bundle` or `serialized`
        #[arg(long, default_value = "bundle")]
        kind: String,

        /// Asset index inside the bundle (required when `--kind bundle`)
        #[arg(long)]
        asset_index: Option<usize>,

        /// Object PathID inside the serialized file
        #[arg(long)]
        path_id: Option<i64>,

        /// Limit printed recursion depth
        #[arg(long, default_value_t = 6)]
        max_depth: usize,

        /// Limit total printed nodes (prevents huge dumps)
        #[arg(long, default_value_t = 500)]
        max_items: usize,

        /// Limit printed array items per array node
        #[arg(long, default_value_t = 16)]
        max_array: usize,

        /// Only print paths containing this substring (case-insensitive)
        #[arg(long, default_value = "")]
        filter: String,
    },

    /// Dump a JSON TypeTree registry from loaded files (for stripped-asset fallback parsing)
    #[command(name = "dump-typetree-registry")]
    DumpTypeTreeRegistry {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Output JSON path
        #[arg(short, long)]
        output: PathBuf,

        /// Filter by Unity class ID (repeatable). Empty means dump all.
        #[arg(long)]
        class_id: Vec<i32>,

        /// Emit Unity version as a major.minor prefix (e.g. `2020.3.*`) instead of exact version.
        #[arg(long)]
        version_prefix: bool,

        /// Overwrite existing output file
        #[arg(long)]
        overwrite: bool,
    },

    /// Scan PPtr references (`fileID`, `pathID`) from TypeTree without fully parsing objects
    #[command(name = "scan-pptr")]
    ScanPPtr {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Source kind: `all`, `bundle`, or `serialized`
        #[arg(long, default_value = "all")]
        kind: String,

        /// Restrict scanning to a specific loaded source path
        #[arg(long)]
        source: Option<PathBuf>,

        /// Restrict scanning to a specific bundle asset index (only applies when --kind bundle or all)
        #[arg(long)]
        asset_index: Option<usize>,

        /// Filter by Unity class ID (repeatable). Example: `--class-id 1` (GameObject).
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by object `m_Name`/`name` substring (case-insensitive) via a TypeTree prefix fast path.
        #[arg(long, default_value = "")]
        name: String,

        /// Limit printed objects
        #[arg(long)]
        limit: Option<usize>,

        /// Include objects where TypeTree is unavailable (printed with empty refs)
        #[arg(long)]
        include_no_typetree: bool,

        /// Print one JSON object per line
        #[arg(long)]
        json: bool,
    },

    /// Build one revision-bound reference graph for Unity YAML and binary sources.
    #[command(name = "deps")]
    Deps {
        /// Input Unity source or directory.
        #[arg(short, long)]
        input: PathBuf,

        /// Output format: `summary`, `edges`, `dot`, `json`, or `jsonl`.
        #[arg(long, default_value = "summary")]
        format: String,

        /// Maximum reference facts to emit without limiting graph construction.
        #[arg(long, default_value_t = 2000)]
        max_edges: usize,
    },

    /// Build one revision-bound reference graph for a Unity project directory.
    #[command(name = "project-graph")]
    ProjectGraph {
        /// Unity project root directory.
        #[arg(short, long)]
        input: PathBuf,

        /// Write output to a file instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,

        /// Include Unity YAML documents (`.asset`, `.prefab`, `.unity`).
        #[arg(long)]
        yaml: bool,

        /// Output format: `summary`, `edges`, `dot`, `json`, or `jsonl`.
        #[arg(long, default_value = "summary", value_name = "FORMAT")]
        format: String,

        /// Limit supported root sources loaded after deterministic discovery.
        #[arg(long)]
        max_files: Option<usize>,

        /// Maximum reference facts to emit without limiting graph construction.
        #[arg(long, default_value_t = 200_000)]
        max_edges: usize,
    },
}

#[derive(Args)]
pub(crate) struct ExportCommand {
    /// Input Unity source or directory.
    #[arg(short, long)]
    pub(crate) input: PathBuf,

    /// Root directory for planned artifacts.
    #[arg(short, long)]
    pub(crate) output: PathBuf,

    /// Execute a canonical ExtractionPlan instead of constructing a new plan from selections.
    #[arg(
        long,
        conflicts_with_all = [
            "source",
            "address",
            "bundle_container",
            "class_id",
            "class_name",
            "name",
            "limit",
            "representation",
            "prefix",
            "dry_run"
        ]
    )]
    pub(crate) plan: Option<PathBuf>,

    /// Restrict to a root source alias. Repeat for multiple sources.
    #[arg(long, conflicts_with_all = ["address", "bundle_container"])]
    pub(crate) source: Vec<String>,

    /// Restrict to an ObjectAddress emitted by this CLI. Repeat for multiple objects.
    #[arg(long, conflicts_with_all = ["source", "bundle_container"])]
    pub(crate) address: Vec<String>,

    /// Select objects referenced by AssetBundle `m_Container` entries matching this pattern.
    #[arg(long, conflicts_with_all = ["source", "address"])]
    pub(crate) bundle_container: Option<String>,

    /// Filter selected objects by Unity class ID. Repeat for multiple classes.
    #[arg(long)]
    pub(crate) class_id: Vec<i32>,

    /// Filter selected objects by case-insensitive class-name substring.
    #[arg(long)]
    pub(crate) class_name: Option<String>,

    /// Filter selected objects by case-insensitive object-name substring.
    #[arg(long)]
    pub(crate) name: Option<String>,

    /// Limit the number of planned artifacts.
    #[arg(long)]
    pub(crate) limit: Option<u64>,

    /// `raw`, `prefer-decoded`, or `require-decoded`.
    #[arg(long, default_value = "raw")]
    pub(crate) representation: String,

    /// Optional safe relative prefix under the output root.
    #[arg(long)]
    pub(crate) prefix: Option<String>,

    /// Print the canonical ExtractionPlan without creating output files.
    #[arg(long, conflicts_with_all = ["resume", "plan", "manifest"])]
    pub(crate) dry_run: bool,

    /// Read a canonical extraction manifest and reuse only verified output artifacts.
    ///
    /// Missing or mismatched outputs still obey --existing-output; use `replace`
    /// explicitly when a corrupted resumable output should be rebuilt.
    #[arg(long)]
    pub(crate) resume: Option<PathBuf>,

    /// Safely publish the canonical manifest at this relative path under --output.
    ///
    /// The manifest shares the artifact output lock, cannot collide with planned
    /// artifact paths, and replaces a prior manifest only after execution completes.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) manifest: Option<PathBuf>,

    /// `error`, `skip`, or `replace` for non-resumable existing output paths.
    #[arg(long, default_value = "error")]
    pub(crate) existing_output: String,

    /// `collect-all` or `stop-in-plan-order` after an artifact failure.
    #[arg(long, default_value = "collect-all")]
    pub(crate) failure: String,

    /// Maximum concurrent workers. The default is one deterministic worker.
    #[arg(long)]
    pub(crate) workers: Option<usize>,

    /// Maximum aggregate in-flight bytes across one deterministic work batch.
    #[arg(long)]
    pub(crate) max_in_flight_bytes: Option<u64>,

    /// Maximum concurrently open extraction files.
    #[arg(long)]
    pub(crate) max_open_files: Option<usize>,

    /// Maximum published artifact bytes.
    #[arg(long)]
    pub(crate) max_output_bytes: Option<u64>,

    /// Maximum canonical report bytes.
    #[arg(long)]
    pub(crate) max_report_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Commands};

    #[test]
    fn export_accepts_one_unified_selection_surface() {
        let cli = Cli::try_parse_from([
            "unity-asset",
            "export",
            "--input",
            "game.ab",
            "--output",
            "out",
            "--address",
            "oa2|binary|1|0|0|1|7|game.ab|0|",
            "--representation",
            "prefer-decoded",
            "--dry-run",
        ])
        .unwrap();

        let Commands::Export(command) = cli.command else {
            panic!("export arguments must select the export command");
        };
        assert!(command.dry_run);
    }

    #[test]
    fn export_rejects_ambiguous_selection_and_resume_during_dry_run() {
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--source",
                "game.ab",
                "--bundle-container",
                "assets/*",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--dry-run",
                "--resume",
                "manifest.json",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--dry-run",
                "--manifest",
                "manifest.json",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "unity-asset",
                "export",
                "--input",
                "game.ab",
                "--output",
                "out",
                "--plan",
                "plan.json",
                "--class-id",
                "28",
            ])
            .is_err()
        );
    }

    #[test]
    fn legacy_export_commands_are_removed() {
        for legacy in ["extract", "export-bundle", "export-serialized"] {
            assert!(Cli::try_parse_from(["unity-asset", legacy]).is_err());
        }
    }

    #[test]
    fn split_yaml_has_a_separate_command_contract() {
        let cli = Cli::try_parse_from([
            "unity-asset",
            "split-yaml",
            "--input",
            "scene.prefab",
            "--output",
            "out",
            "--existing-output",
            "replace",
        ])
        .unwrap();

        assert!(matches!(cli.command, Commands::SplitYaml { .. }));
    }
}
