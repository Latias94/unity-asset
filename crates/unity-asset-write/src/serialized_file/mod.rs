//! SerializedFile saving (UnityPy parity target).
//!
//! This module rebuilds a Unity SerializedFile:
//! - metadata stream (types, object table, scripts, externals, ref types, user info)
//! - data stream (object payloads)
//! - header + offsets + alignment

mod edit;
mod types_write;
mod typetree_dump;
mod writer;

pub use edit::SerializedFileEdits;
pub use writer::{SerializedFileSaveOptions, SerializedFileWriter};
