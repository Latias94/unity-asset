//! Public API contract for the default `unity-asset-binary` package.

pub use unity_asset_binary::{
    SegmentedBytes,
    asset::{
        BuildTarget, SerializedFile, SerializedFileParser, SerializedObjectContext,
        TargetPlatformEvidence,
    },
    bundle::{AssetBundle, BundleLoadOptions, BundleParser},
    file::{
        UnityFile, UnityFileKind, UnityFileLoadOutcome, load_unity_file_from_memory_with_budget,
    },
    shared_bytes::SharedBytes,
    typetree::{
        TypeTree, TypeTreeParseOptions, TypeTreeRegistry, TypeTreeSchema,
        TypeTreeTraversalStats,
    },
};
