//! SerializedFile saving (UnityPy parity target).
//!
//! This module rebuilds a Unity SerializedFile:
//! - metadata stream (types, object table, scripts, externals, ref types, user info)
//! - data stream (object payloads)
//! - header + offsets + alignment

mod artifact_writer;
mod edit;
mod external_table;
mod plan;
mod sink;
mod types_write;
mod typetree_dump;
mod writer;

pub use edit::{SerializedFileEditError, SerializedFileEdits};
pub use external_table::{
    BudgetedExternalPath, ExternalIdentifierField, ExternalMetadataField, ExternalTableAllocator,
    ExternalTableError, PreparedExternalPath,
};
pub use plan::SerializedFileSource;
pub use writer::SerializedFileWriter;
