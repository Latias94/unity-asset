//! Unity asset edit/write support.
//!
//! This crate is the home for UnityPy-parity write pipelines:
//! - TypeTree-driven object encoding
//! - SerializedFile rebuild/save
//! - Bundle/WebFile repacking
//! - `.resS` / streamed resource write support
//!
//! The initial milestones intentionally ship APIs and scaffolding first, before wiring up the full
//! implementation.

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
pub use compression::*;
pub use packer::{PackingPolicy, PackingPolicyParseError};
pub use unity_asset_core::{Result, UnityAssetError};
