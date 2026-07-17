//! Resolves version-specific object table references against a SerializedType table.

use super::format::ObjectTypeEncoding;
use super::types::{ObjectMetadata, ObjectTypeReference, SerializedType};
use crate::error::{BinaryError, Result};
use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone, Copy)]
enum UniqueTypeIndex {
    Unique(usize),
    Ambiguous,
}

enum ObjectTypeLookup {
    Legacy(HashMap<i32, UniqueTypeIndex>),
    TransitionalV16(HashMap<(i32, bool), UniqueTypeIndex>),
    Indexed,
}

pub(super) struct ObjectTypeResolver<'types> {
    types: &'types [SerializedType],
    lookup: ObjectTypeLookup,
}

impl<'types> ObjectTypeResolver<'types> {
    pub(super) fn new(
        encoding: ObjectTypeEncoding,
        types: &'types [SerializedType],
    ) -> Result<Self> {
        let lookup = match encoding {
            ObjectTypeEncoding::Legacy => ObjectTypeLookup::Legacy(index_unique_types(
                types,
                "legacy object types",
                |serialized_type| serialized_type.class_id,
            )?),
            ObjectTypeEncoding::TransitionalV16 => {
                ObjectTypeLookup::TransitionalV16(index_unique_types(
                    types,
                    "SerializedFile v16 object types",
                    |serialized_type| (serialized_type.class_id, serialized_type.is_stripped_type),
                )?)
            }
            ObjectTypeEncoding::Indexed => ObjectTypeLookup::Indexed,
        };
        Ok(Self { types, lookup })
    }

    pub(super) fn resolve(
        &self,
        type_reference: ObjectTypeReference,
        metadata: ObjectMetadata,
    ) -> Result<(i32, Option<u32>)> {
        match (type_reference, &self.lookup) {
            (
                ObjectTypeReference::Legacy {
                    raw_type_id,
                    class_id_bits,
                },
                ObjectTypeLookup::Legacy(candidates),
            ) => {
                let serialized_type_index = match candidates.get(&raw_type_id) {
                    Some(UniqueTypeIndex::Unique(index)) => {
                        Some(u32::try_from(*index).map_err(|_| {
                            BinaryError::invalid_data(format!(
                                "Legacy SerializedType index {index} does not fit u32"
                            ))
                        })?)
                    }
                    Some(UniqueTypeIndex::Ambiguous) => {
                        return Err(BinaryError::invalid_data(format!(
                            "Ambiguous legacy type reference {raw_type_id}: multiple SerializedType candidates"
                        )));
                    }
                    None => None,
                };
                Ok((i32::from(class_id_bits), serialized_type_index))
            }
            (ObjectTypeReference::SerializedTypeIndex { index }, ObjectTypeLookup::Indexed) => {
                let index_usize = usize::try_from(index).map_err(|_| {
                    BinaryError::invalid_data(format!(
                        "SerializedType index {index} does not fit usize"
                    ))
                })?;
                let serialized_type = self.types.get(index_usize).ok_or_else(|| {
                    BinaryError::invalid_data(format!(
                        "SerializedType index {index} is outside table length {}",
                        self.types.len()
                    ))
                })?;
                Ok((serialized_type.class_id, Some(index)))
            }
            (
                ObjectTypeReference::TransitionalV16 { raw },
                ObjectTypeLookup::TransitionalV16(candidates),
            ) => self.resolve_transitional_v16(raw, metadata, candidates),
            _ => Err(BinaryError::invalid_data(
                "Object type reference encoding does not match the SerializedFile format",
            )),
        }
    }

    fn resolve_transitional_v16(
        &self,
        raw: i32,
        metadata: ObjectMetadata,
        candidates: &HashMap<(i32, bool), UniqueTypeIndex>,
    ) -> Result<(i32, Option<u32>)> {
        // UnityPy's ObjectReader indexes `types[raw]` for every v16+ file. The independent v16
        // collision fixture fixes that behavior as our wire oracle: an in-range raw value is an
        // index even when the same value also names a different class ID.
        if let Some((index, serialized_type)) = usize::try_from(raw).ok().and_then(|index| {
            self.types
                .get(index)
                .map(|serialized_type| (index, serialized_type))
        }) {
            let index = u32::try_from(index).map_err(|_| {
                BinaryError::invalid_data(format!(
                    "Resolved SerializedType index {index} does not fit u32"
                ))
            })?;
            return Ok((serialized_type.class_id, Some(index)));
        }

        // Preserve the validated transition fallback only for values that cannot be table indexes.
        let stripped = metadata.stripped_raw().is_some_and(|value| value != 0);
        let (index, serialized_type) = match candidates.get(&(raw, stripped)) {
            Some(UniqueTypeIndex::Unique(index)) => (*index, &self.types[*index]),
            Some(UniqueTypeIndex::Ambiguous) => {
                return Err(BinaryError::invalid_data(format!(
                    "Ambiguous SerializedFile v16 type reference {raw}: multiple type-ID candidates"
                )));
            }
            None => {
                return Err(BinaryError::invalid_data(format!(
                    "SerializedFile v16 type reference {raw} matches neither an index nor a type ID"
                )));
            }
        };
        let index = u32::try_from(index).map_err(|_| {
            BinaryError::invalid_data(format!(
                "Resolved SerializedType index {index} does not fit u32"
            ))
        })?;
        Ok((serialized_type.class_id, Some(index)))
    }
}

fn index_unique_types<K: Eq + Hash>(
    types: &[SerializedType],
    label: &str,
    key: impl Fn(&SerializedType) -> K,
) -> Result<HashMap<K, UniqueTypeIndex>> {
    let mut candidates = HashMap::new();
    candidates.try_reserve(types.len()).map_err(|error| {
        BinaryError::memory_error(format!(
            "Failed to reserve {label} lookup for {} types: {error}",
            types.len()
        ))
    })?;
    for (index, serialized_type) in types.iter().enumerate() {
        candidates
            .entry(key(serialized_type))
            .and_modify(|candidate| *candidate = UniqueTypeIndex::Ambiguous)
            .or_insert(UniqueTypeIndex::Unique(index));
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_object_type(
        type_reference: ObjectTypeReference,
        metadata: ObjectMetadata,
        types: &[SerializedType],
    ) -> Result<(i32, Option<u32>)> {
        let encoding = match type_reference {
            ObjectTypeReference::StandaloneClass { .. } => {
                return Err(BinaryError::invalid_data(
                    "Standalone object types cannot be resolved against a SerializedFile type table",
                ));
            }
            ObjectTypeReference::Legacy { .. } => ObjectTypeEncoding::Legacy,
            ObjectTypeReference::TransitionalV16 { .. } => ObjectTypeEncoding::TransitionalV16,
            ObjectTypeReference::SerializedTypeIndex { .. } => ObjectTypeEncoding::Indexed,
        };
        ObjectTypeResolver::new(encoding, types)?.resolve(type_reference, metadata)
    }

    #[test]
    fn legacy_type_reference_retains_class_bits_and_resolves_raw_type() {
        let types = [SerializedType::new(-1)];
        let resolved = resolve_object_type(
            ObjectTypeReference::Legacy {
                raw_type_id: -1,
                class_id_bits: 114,
            },
            ObjectMetadata::ScriptTypeIndexAndStripped {
                index: 3,
                stripped: 1,
            },
            &types,
        )
        .unwrap();

        assert_eq!(resolved, (114, Some(0)));
    }

    #[test]
    fn legacy_type_reference_rejects_duplicate_raw_type_candidates() {
        let types = [SerializedType::new(-1), SerializedType::new(-1)];
        let error = resolve_object_type(
            ObjectTypeReference::Legacy {
                raw_type_id: -1,
                class_id_bits: 114,
            },
            ObjectMetadata::ScriptTypeIndex { index: 0 },
            &types,
        )
        .expect_err("duplicate raw type IDs are ambiguous");

        assert!(
            error
                .to_string()
                .contains("multiple SerializedType candidates")
        );
    }

    #[test]
    fn transitional_v16_accepts_a_unique_index_candidate() {
        let types = [SerializedType::new(28)];
        assert_eq!(
            resolve_object_type(
                ObjectTypeReference::TransitionalV16 { raw: 0 },
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: -3,
                    stripped: 1,
                },
                &types,
            )
            .unwrap(),
            (28, Some(0))
        );
    }

    #[test]
    fn transitional_v16_prefers_an_in_range_index_over_a_class_id_collision() {
        let types = [SerializedType::new(1), SerializedType::new(28)];
        assert_eq!(
            resolve_object_type(
                ObjectTypeReference::TransitionalV16 { raw: 1 },
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: -1,
                    stripped: 0,
                },
                &types,
            )
            .unwrap(),
            (28, Some(1))
        );
    }

    #[test]
    fn transitional_v16_accepts_a_unique_type_id_candidate() {
        let types = [SerializedType::new(28)];
        assert_eq!(
            resolve_object_type(
                ObjectTypeReference::TransitionalV16 { raw: 28 },
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: -1,
                    stripped: 0,
                },
                &types,
            )
            .unwrap(),
            (28, Some(0))
        );
    }

    #[test]
    fn transitional_v16_accepts_when_both_meanings_select_the_same_type() {
        let types = [SerializedType::new(28), SerializedType::new(1)];
        assert_eq!(
            resolve_object_type(
                ObjectTypeReference::TransitionalV16 { raw: 1 },
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: -1,
                    stripped: 0,
                },
                &types,
            )
            .unwrap(),
            (1, Some(1))
        );
    }

    #[test]
    fn transitional_v16_rejects_duplicate_type_id_candidates() {
        let types = [SerializedType::new(28), SerializedType::new(28)];
        let error = resolve_object_type(
            ObjectTypeReference::TransitionalV16 { raw: 28 },
            ObjectMetadata::ScriptTypeIndexAndStripped {
                index: -1,
                stripped: 0,
            },
            &types,
        )
        .expect_err("duplicate type IDs are ambiguous");
        assert!(error.to_string().contains("multiple type-ID candidates"));
    }

    #[test]
    fn transitional_v16_rejects_a_value_with_no_candidate() {
        let types = [SerializedType::new(28)];
        let error = resolve_object_type(
            ObjectTypeReference::TransitionalV16 { raw: 99 },
            ObjectMetadata::ScriptTypeIndexAndStripped {
                index: -1,
                stripped: 0,
            },
            &types,
        )
        .expect_err("unresolved v16 raw values must be rejected");
        assert!(
            error
                .to_string()
                .contains("matches neither an index nor a type ID")
        );
    }

    #[test]
    fn transitional_v16_accepts_a_negative_raw_type_id_candidate() {
        let types = [SerializedType::new(-1)];
        assert_eq!(
            resolve_object_type(
                ObjectTypeReference::TransitionalV16 { raw: -1 },
                ObjectMetadata::ScriptTypeIndexAndStripped {
                    index: 3,
                    stripped: 0,
                },
                &types,
            )
            .unwrap(),
            (-1, Some(0))
        );
    }
}
