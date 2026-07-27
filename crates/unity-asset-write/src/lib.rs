//! Wire-faithful Unity asset encoding and prepared artifact construction.
//!
//! The crate owns TypeTree-driven object encoding, SerializedFile rebuilds, Bundle/WebFile
//! repacking, streamed-resource output, and segmented prepared artifacts that preserve unchanged
//! source ranges. Workspace transaction authority remains in the high-level `unity-asset` crate.

pub mod artifact;
mod binary_writer;
pub mod bundle;
mod compression;
pub mod object;
mod packer;
pub mod resources;
pub mod serialized_file;
mod typetree;
pub mod webfile;

pub use binary_writer::{BinaryWriter, Endian};
pub use compression::{compress_lz4, compress_lzma_unity, compress_lzma_unity_with_size};
pub use packer::{PackingPolicy, PackingPolicyParseError};
pub use unity_asset_core::{Result, UnityAssetError};
