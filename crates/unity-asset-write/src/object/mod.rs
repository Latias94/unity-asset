//! Atomic SerializedFile object encoding.
//!
//! [`SerializedObjectEncoder`] is the authoritative schema-aware API. It binds one immutable
//! SerializedFile object, applies caller-ordered and digest-guarded semantic mutations, then emits
//! one immutable byte override through a single TypeTree rewrite. Raw replacement is a separate,
//! explicitly acknowledged escape hatch.
//!
//! [`SerializedFileEditSession`] remains available as a compatibility layer while callers migrate
//! away from the older mutable UnityPy-style workflow.

mod encoder;
mod serialized_file_session;

pub use encoder::{
    EncodedSerializedObject, SerializedFieldGuard, SerializedObjectEncodeError,
    SerializedObjectEncoder, SerializedObjectEncodingMode, SerializedObjectEncodingStats,
    SerializedObjectGuard, SerializedObjectMutation, SerializedSequenceEdit, SerializedValueKind,
    UnsafeRawObjectAcknowledgement, UnsafeRawObjectReplacement,
};
pub use serialized_file_session::SerializedFileEditSession;
