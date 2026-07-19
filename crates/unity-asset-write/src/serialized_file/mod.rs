//! SerializedFile saving (UnityPy parity target).
//!
//! This module rebuilds a Unity SerializedFile:
//! - metadata stream (types, object table, scripts, externals, ref types, user info)
//! - data stream (object payloads)
//! - header + offsets + alignment

mod artifact_writer;
mod edit;
mod external_table;
mod sink;
mod types_write;
mod typetree_dump;
mod writer;

pub use artifact_writer::SerializedFileSource;
pub use edit::SerializedFileEdits;
pub use external_table::{
    ExternalIdentifierField, ExternalMetadataField, ExternalTableAllocator, ExternalTableError,
};
pub use writer::{SerializedFileSaveOptions, SerializedFileWriter};
