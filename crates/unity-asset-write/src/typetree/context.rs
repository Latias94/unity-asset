use unity_asset_binary::asset::SerializedType;

/// Mutable write context, mirroring UnityPy's `TypeTreeConfig` behavior.
#[derive(Debug, Clone, Default)]
pub struct TypeTreeWriteContext<'a> {
    /// UnityPy's `ManagedReferencesRegistry` can appear multiple times; UnityPy writes only the
    /// first one and skips subsequent ones.
    pub has_managed_registry: bool,
    /// Optional managed reference type list (Unity `ref_types`) for resolving `ReferencedObjectData`.
    pub ref_types: Option<&'a [SerializedType]>,
}
