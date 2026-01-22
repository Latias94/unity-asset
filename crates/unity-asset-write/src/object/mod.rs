//! UnityPy-like object edit helpers.
//!
//! UnityPy edits objects by mutating a parsed TypeTree dict and then calling `save_typetree`,
//! which stores overridden raw bytes and marks the owning file as changed.
//!
//! This module provides the same workflow in Rust, built on:
//! - `unity-asset-binary` for reading/parsing
//! - `TypeTreeWriter` for encoding
//! - `SerializedFileEdits` for capturing overridden bytes

mod serialized_file_session;

pub use serialized_file_session::SerializedFileEditSession;
