use std::fs;
use std::path::{Path, PathBuf};

const BROTLI_SYMBOL_PREFIX: &str = "unity_asset_write_brotli_1_2_0_";

// Every externally visible symbol in the vendored Brotli 1.2.0 encoder archive. Prefixing the
// complete set prevents another static Brotli archive from satisfying internal cross-object
// references through link order.
const BROTLI_SYMBOLS: &[&str] = &[
    "_kBrotliContextLookupTable",
    "_kBrotliPrefixCodeRanges",
    "AttachPreparedDictionary",
    "BrotliAllocate",
    "BrotliBitsEntropy",
    "BrotliBootstrapAlloc",
    "BrotliBootstrapFree",
    "BrotliBuildAndStoreHuffmanTreeFast",
    "BrotliBuildHistogramsWithContext",
    "BrotliBuildMetaBlock",
    "BrotliBuildMetaBlockGreedy",
    "BrotliCleanupSharedEncoderDictionary",
    "BrotliClusterHistogramsCommand",
    "BrotliClusterHistogramsDistance",
    "BrotliClusterHistogramsLiteral",
    "BrotliCompareAndPushToQueueCommand",
    "BrotliCompareAndPushToQueueDistance",
    "BrotliCompareAndPushToQueueLiteral",
    "BrotliCompressFragmentFast",
    "BrotliCompressFragmentTwoPass",
    "BrotliConvertBitDepthsToSymbols",
    "BrotliCreateBackwardReferences",
    "BrotliCreateHqZopfliBackwardReferences",
    "BrotliCreateHuffmanTree",
    "BrotliCreateManagedDictionary",
    "BrotliCreateZopfliBackwardReferences",
    "BrotliDefaultAllocFunc",
    "BrotliDefaultFreeFunc",
    "BrotliDestroyBlockSplit",
    "BrotliDestroyManagedDictionary",
    "BrotliEncoderAttachPreparedDictionary",
    "BrotliEncoderCompress",
    "BrotliEncoderCompressStream",
    "BrotliEncoderCreateInstance",
    "BrotliEncoderDestroyInstance",
    "BrotliEncoderDestroyPreparedDictionary",
    "BrotliEncoderEnsureStaticInit",
    "BrotliEncoderEstimatePeakMemoryUsage",
    "BrotliEncoderGetPreparedDictionarySize",
    "BrotliEncoderHasMoreOutput",
    "BrotliEncoderIsFinished",
    "BrotliEncoderMaxCompressedSize",
    "BrotliEncoderPrepareDictionary",
    "BrotliEncoderSetParameter",
    "BrotliEncoderTakeOutput",
    "BrotliEncoderVersion",
    "BrotliEstimateBitCostsForLiterals",
    "BrotliFindAllStaticDictionaryMatches",
    "BrotliFree",
    "BrotliGetDictionary",
    "BrotliGetTransforms",
    "BrotliHistogramBitCostDistanceCommand",
    "BrotliHistogramBitCostDistanceDistance",
    "BrotliHistogramBitCostDistanceLiteral",
    "BrotliHistogramCombineCommand",
    "BrotliHistogramCombineDistance",
    "BrotliHistogramCombineLiteral",
    "BrotliHistogramReindexCommand",
    "BrotliHistogramReindexDistance",
    "BrotliHistogramReindexLiteral",
    "BrotliHistogramRemapCommand",
    "BrotliHistogramRemapDistance",
    "BrotliHistogramRemapLiteral",
    "BrotliInitBlockSplit",
    "BrotliInitDistanceParams",
    "BrotliInitMemoryManager",
    "BrotliInitSharedEncoderDictionary",
    "BrotliInitZopfliNodes",
    "BrotliIsMostlyUTF8",
    "BrotliOptimizeHistograms",
    "BrotliOptimizeHuffmanCountsForRle",
    "BrotliPopulationCostCommand",
    "BrotliPopulationCostDistance",
    "BrotliPopulationCostLiteral",
    "BrotliSetDepth",
    "BrotliSetDictionaryData",
    "BrotliSharedDictionaryAttach",
    "BrotliSharedDictionaryCreateInstance",
    "BrotliSharedDictionaryDestroyInstance",
    "BrotliSplitBlock",
    "BrotliStoreHuffmanTree",
    "BrotliStoreMetaBlock",
    "BrotliStoreMetaBlockFast",
    "BrotliStoreMetaBlockTrivial",
    "BrotliStoreUncompressedMetaBlock",
    "BrotliTransformDictionaryWord",
    "BrotliWipeOutMemoryManager",
    "BrotliWriteHuffmanTree",
    "BrotliZopfliComputeShortestPath",
    "BrotliZopfliCreateCommands",
    "CreatePreparedDictionary",
    "DestroyPreparedDictionary",
    "kBrotliCopyBase",
    "kBrotliCopyExtra",
    "kBrotliInsBase",
    "kBrotliInsExtra",
    "kBrotliLog2Table",
    "kBrotliShellGaps",
    "kStaticDictionaryBuckets",
    "kStaticDictionaryHashLengths",
    "kStaticDictionaryHashWords",
    "kStaticDictionaryWords",
];

const BROTLI_SOURCES: &[&str] = &[
    "common/constants.c",
    "common/context.c",
    "common/dictionary.c",
    "common/platform.c",
    "common/shared_dictionary.c",
    "common/transform.c",
    "enc/backward_references.c",
    "enc/backward_references_hq.c",
    "enc/bit_cost.c",
    "enc/block_splitter.c",
    "enc/brotli_bit_stream.c",
    "enc/cluster.c",
    "enc/command.c",
    "enc/compound_dictionary.c",
    "enc/compress_fragment.c",
    "enc/compress_fragment_two_pass.c",
    "enc/dictionary_hash.c",
    "enc/encode.c",
    "enc/encoder_dict.c",
    "enc/entropy_encode.c",
    "enc/fast_log.c",
    "enc/histogram.c",
    "enc/literal_cost.c",
    "enc/memory.c",
    "enc/metablock.c",
    "enc/static_dict.c",
    "enc/static_dict_lut.c",
    "enc/static_init.c",
    "enc/utf8_util.c",
];

fn main() {
    let source_root = Path::new("vendor/brotli");
    track_tree(source_root);
    let mut build = cc::Build::new();
    build
        .include(source_root.join("include"))
        .define("BROTLI_ENCODER_CLEANUP_ON_OOM", None)
        .warnings(false);

    for symbol in BROTLI_SYMBOLS {
        let prefixed = format!("{BROTLI_SYMBOL_PREFIX}{symbol}");
        build.define(symbol, Some(prefixed.as_str()));
    }

    for source in BROTLI_SOURCES {
        let source = source_root.join(source);
        build.file(source);
    }
    build.compile("unity_asset_brotli_encoder");
}

fn track_tree(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| {
                panic!(
                    "failed to read vendored Brotli directory {}: {error}",
                    directory.display()
                )
            })
            .map(|entry| {
                entry
                    .unwrap_or_else(|error| {
                        panic!(
                            "failed to inspect vendored Brotli directory {}: {error}",
                            directory.display()
                        )
                    })
                    .path()
            })
            .collect::<Vec<PathBuf>>();
        entries.sort_unstable();
        for path in entries.into_iter().rev() {
            if path.is_dir() {
                pending.push(path);
            } else {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}
