//! Atomic SerializedFile object encoding.
//!
//! [`SerializedObjectEncoder`] is the authoritative schema-aware API. It binds one immutable
//! SerializedFile object, applies caller-ordered and digest-guarded semantic mutations, then emits
//! one immutable byte override through a single TypeTree rewrite. Raw replacement is a separate,
//! explicitly acknowledged escape hatch.

mod encoder;

pub use encoder::{
    EncodedSerializedObject, PreparedSerializedFieldReplace, PreparedUnsafeRawObject,
    SerializedFieldGuard, SerializedManagedReferenceLayout, SerializedManagedReferenceType,
    SerializedObjectCandidate, SerializedObjectEncodeError, SerializedObjectEncoder,
    SerializedObjectEncodingMode, SerializedObjectEncodingStats, SerializedObjectGuard,
    SerializedObjectMutation, SerializedPPtrLayout, SerializedSequenceEdit, SerializedValueKind,
    SerializedValueSchema, SerializedValueSchemaError, UnsafeRawObjectAcknowledgement,
    UnsafeRawObjectReplacement, ValidatedSerializedFieldGuard,
};
