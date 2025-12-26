use clap::{Parser, Subcommand};
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

    /// Extract information from Unity files
    Extract {
        /// Input file or directory path
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory
        #[arg(short, long)]
        output: PathBuf,

        /// Unity class types to extract (GameObject, Transform, etc.)
        #[arg(long)]
        types: Vec<String>,
    },

    /// Export objects from AssetBundles using the bundle `m_Container` (UnityPy-like workflow)
    ExportBundle {
        /// Input file or directory path (bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory
        #[arg(short, long)]
        output: PathBuf,

        /// Filter container entries by substring or glob (`*`, `?`) (case-insensitive). Empty means export all.
        #[arg(long, default_value = "")]
        pattern: String,

        /// Limit exported entries (to keep runtime predictable)
        #[arg(long)]
        limit: Option<usize>,

        /// Filter by class id (can be repeated). Only applies to resolvable entries.
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by class name substring (case-insensitive). Only applies to resolvable entries.
        #[arg(long, default_value = "")]
        class_name: String,

        /// Only print what would be exported
        #[arg(long)]
        dry_run: bool,

        /// Decode known types (AudioClip -> WAV, Texture2D -> PNG) instead of exporting raw object bytes
        #[arg(long)]
        decode: bool,

        /// Overwrite existing output files (still avoids in-run collisions)
        #[arg(long, conflicts_with = "skip_existing")]
        overwrite: bool,

        /// Skip entries whose output file already exists
        #[arg(long)]
        skip_existing: bool,

        /// Write a JSON manifest of planned/exported entries (for resume and regression checks)
        #[arg(long)]
        manifest: Option<PathBuf>,

        /// Resume from a previous manifest (skips entries that are already exported and still exist)
        #[arg(long, conflicts_with = "overwrite")]
        resume: Option<PathBuf>,

        /// Retry only failed entries from a previous manifest (uses its `asset_path` and `key`)
        #[arg(long, conflicts_with_all = ["resume", "overwrite"])]
        retry_failed_from: Option<PathBuf>,

        /// Continue exporting even if some entries fail; failed entries are recorded in the manifest
        #[arg(long)]
        continue_on_error: bool,

        /// Parallel export jobs (0 = auto, 1 = serial)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
    },

    /// Export objects from SerializedFiles (e.g. `.asset`, `.assets`) by scanning objects directly
    #[command(name = "export-serialized")]
    ExportSerialized {
        /// Input file or directory path (serialized files will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Output directory
        #[arg(short, long)]
        output: PathBuf,

        /// Restrict exporting to a specific loaded serialized source path
        #[arg(long)]
        source: Option<PathBuf>,

        /// Filter by Unity class id (can be repeated). Empty means export all.
        #[arg(long)]
        class_id: Vec<i32>,

        /// Filter by Unity class name substring (case-insensitive).
        #[arg(long, default_value = "")]
        class_name: String,

        /// Filter by object `m_Name`/`name` substring (case-insensitive) via a TypeTree prefix fast path.
        ///
        /// Note: this requires TypeTree to be present and to include a name field; otherwise the object is treated as non-matching.
        #[arg(long, default_value = "")]
        name: String,

        /// Limit exported objects
        #[arg(long)]
        limit: Option<usize>,

        /// Only print what would be exported
        #[arg(long)]
        dry_run: bool,

        /// Decode known types (AudioClip -> WAV/encoded, Texture2D -> PNG, Sprite -> PNG, TextAsset -> TXT) instead of exporting raw bytes
        #[arg(long)]
        decode: bool,

        /// Overwrite existing output files
        #[arg(long, conflicts_with = "skip_existing")]
        overwrite: bool,

        /// Skip objects whose output file already exists
        #[arg(long)]
        skip_existing: bool,

        /// Write a JSON manifest of planned/exported objects (for resume and regression checks)
        #[arg(long)]
        manifest: Option<PathBuf>,

        /// Resume from a previous manifest (skips objects that are already exported and still exist)
        #[arg(long, conflicts_with = "overwrite")]
        resume: Option<PathBuf>,

        /// Retry only failed objects from a previous manifest
        #[arg(long, conflicts_with_all = ["resume", "overwrite"])]
        retry_failed_from: Option<PathBuf>,

        /// Continue exporting even if some objects fail (failures are recorded in the manifest)
        #[arg(long)]
        continue_on_error: bool,

        /// Parallel export jobs (0 = auto, 1 = serial)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
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

        /// Include entries that could not be resolved to a `BinaryObjectKey`
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

        /// Copy/paste key emitted by `find-object --verbose` (overrides --source/--kind/--asset-index/--path-id)
        #[arg(long)]
        key: Option<String>,

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

    /// Build a best-effort dependency graph using TypeTree PPtr scanning
    ///
    /// This is intentionally optimized for large assets: it prefers the zero-allocation PPtr scan
    /// path (no full object parsing).
    #[command(name = "deps")]
    Deps {
        /// Input file or directory path (assets/bundles will be auto-detected)
        #[arg(short, long)]
        input: PathBuf,

        /// Source kind: `bundle` or `serialized`
        #[arg(long, default_value = "bundle")]
        kind: String,

        /// Source file path that contains the objects (an AssetBundle or a standalone SerializedFile)
        #[arg(long)]
        source: Option<PathBuf>,

        /// Asset index inside the bundle (required when `--kind bundle`)
        #[arg(long)]
        asset_index: Option<usize>,

        /// Output format: `summary`, `edges`, `dot`, or `json`
        #[arg(long, default_value = "summary")]
        format: String,

        /// Include best-effort object names in `edges` output (uses TypeTree prefix peek)
        #[arg(long)]
        names: bool,

        /// Maximum edges to print for `edges`/`dot` output (prevents huge dumps)
        #[arg(long, default_value_t = 2000)]
        max_edges: usize,
    },
}
