//! Canonical TypeTree validation, encoding, and byte-preserving rewrite.

mod output;
mod primitives;
mod template;
mod writer;

#[cfg(test)]
mod characterization;
#[cfg(test)]
mod test_support;

use indexmap::IndexMap;
use unity_asset_core::{AssetLoadBudget, UnityValue};

use crate::reader::ByteOrder;

use super::{
    SchemaNode, TypeTreeSchema, TypeTreeTraversalContext, TypeTreeTraversalStats,
    TypeTreeWriteResult,
};

/// Encoded bytes and traversal statistics produced by a TypeTree write.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeTreeEncodeOutput {
    bytes: Vec<u8>,
    stats: TypeTreeTraversalStats,
}

impl TypeTreeEncodeOutput {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn stats(&self) -> TypeTreeTraversalStats {
        self.stats
    }

    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, TypeTreeTraversalStats) {
        (self.bytes, self.stats)
    }
}

/// Traversal statistics produced by a byte-preserving TypeTree rewrite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TypeTreeRewriteStats {
    input: TypeTreeTraversalStats,
    output: TypeTreeTraversalStats,
    preserved_bytes: u64,
}

impl TypeTreeRewriteStats {
    #[must_use]
    pub const fn input(&self) -> TypeTreeTraversalStats {
        self.input
    }

    #[must_use]
    pub const fn output(&self) -> TypeTreeTraversalStats {
        self.output
    }

    #[must_use]
    pub const fn preserved_bytes(&self) -> u64 {
        self.preserved_bytes
    }

    #[must_use]
    pub const fn into_parts(self) -> (TypeTreeTraversalStats, TypeTreeTraversalStats, u64) {
        (self.input, self.output, self.preserved_bytes)
    }
}

/// Rewritten bytes and preservation statistics produced by a TypeTree rewrite.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeTreeRewriteOutput {
    bytes: Vec<u8>,
    stats: TypeTreeRewriteStats,
}

impl TypeTreeRewriteOutput {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub const fn stats(&self) -> TypeTreeRewriteStats {
        self.stats
    }

    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, TypeTreeRewriteStats) {
        (self.bytes, self.stats)
    }
}

impl TypeTreeSchema {
    /// Encodes the root object with caller-owned resource accounting.
    pub fn encode_object(
        &self,
        properties: &IndexMap<String, UnityValue>,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
    ) -> TypeTreeWriteResult<TypeTreeEncodeOutput> {
        let (bytes, stats) = writer::encode_object(self, properties, byte_order, budget)?;
        Ok(TypeTreeEncodeOutput { bytes, stats })
    }

    /// Validates the root object without allocating an output buffer.
    pub fn validate_object(
        &self,
        properties: &IndexMap<String, UnityValue>,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
    ) -> TypeTreeWriteResult<TypeTreeTraversalStats> {
        writer::validate_object(self, properties, byte_order, budget)
    }

    /// Validates a schema-owned node as a standalone encoded value.
    pub fn validate_value(
        &self,
        node: SchemaNode<'_>,
        value: &UnityValue,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
    ) -> TypeTreeWriteResult<TypeTreeTraversalStats> {
        self.validate_value_with_context(
            node,
            value,
            byte_order,
            budget,
            TypeTreeTraversalContext::root(),
            0,
        )
    }

    /// Validates a schema-owned node under an inherited traversal context.
    pub fn validate_value_with_context(
        &self,
        node: SchemaNode<'_>,
        value: &UnityValue,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> TypeTreeWriteResult<TypeTreeTraversalStats> {
        if !self.owns_node(node) {
            return Err(super::TypeTreeWriteError::foreign_node());
        }
        writer::validate_value(self, node, value, byte_order, budget, context, depth)
    }

    /// Validates a schema-owned semantic candidate for a byte-preserving rewrite.
    ///
    /// Unnamed fields may be absent from `value` because [`Self::rewrite_object`] retains their
    /// original wire bytes. `context` and `depth` must describe the node's location in the root
    /// traversal so managed-reference and resource limits match the eventual rewrite. This method
    /// does not inspect a template and therefore cannot prove that template parsing or relocation
    /// will succeed.
    pub fn validate_rewrite_candidate_with_context(
        &self,
        node: SchemaNode<'_>,
        value: &UnityValue,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
        context: TypeTreeTraversalContext,
        depth: u32,
    ) -> TypeTreeWriteResult<TypeTreeTraversalStats> {
        if !self.owns_node(node) {
            return Err(super::TypeTreeWriteError::foreign_node());
        }
        writer::validate_rewrite_value(self, node, value, byte_order, budget, context, depth)
    }

    /// Rewrites an object while preserving unchanged input byte ranges when possible.
    pub fn rewrite_object(
        &self,
        original_bytes: &[u8],
        properties: &IndexMap<String, UnityValue>,
        byte_order: ByteOrder,
        budget: &mut AssetLoadBudget,
    ) -> TypeTreeWriteResult<TypeTreeRewriteOutput> {
        let (bytes, stats) =
            template::rewrite_object(self, properties, original_bytes, byte_order, budget)?;
        Ok(TypeTreeRewriteOutput { bytes, stats })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{aligned, node, record, sequence};
    use super::*;
    use crate::typetree::{TypeTree, TypeTreeNode, TypeTreeWriteError};

    fn compile(root: TypeTreeNode) -> TypeTreeSchema {
        let mut tree = TypeTree::new();
        tree.add_node(root);
        TypeTreeSchema::compile(&tree, &[], &mut AssetLoadBudget::default()).unwrap()
    }

    #[test]
    fn schema_write_api_wraps_outputs_and_rejects_foreign_nodes() {
        let schema = compile(record(vec![node("int", "m_Value")]));
        let mut properties = IndexMap::new();
        properties.insert("m_Value".to_owned(), UnityValue::Integer(7));

        let mut validation_budget = AssetLoadBudget::default();
        let validation = schema
            .validate_object(&properties, ByteOrder::Little, &mut validation_budget)
            .unwrap();
        assert_eq!(validation.wire_bytes, 4);

        let mut encode_budget = AssetLoadBudget::default();
        let encoded = schema
            .encode_object(&properties, ByteOrder::Little, &mut encode_budget)
            .unwrap();
        assert_eq!(encoded.bytes(), 7_i32.to_le_bytes());
        assert_eq!(encoded.stats().wire_bytes, 4);
        let (bytes, encoded_stats) = encoded.into_parts();
        assert_eq!(encoded_stats.wire_bytes, 4);

        let mut rewrite_budget = AssetLoadBudget::default();
        let rewritten = schema
            .rewrite_object(&bytes, &properties, ByteOrder::Little, &mut rewrite_budget)
            .unwrap();
        assert_eq!(rewritten.bytes(), bytes);
        assert_eq!(rewritten.stats().preserved_bytes(), 4);
        let (_, rewrite_stats) = rewritten.into_parts();
        let (input, output, preserved_bytes) = rewrite_stats.into_parts();
        assert_eq!(input.wire_bytes, 4);
        assert_eq!(output.wire_bytes, 4);
        assert_eq!(preserved_bytes, 4);

        let foreign_schema = compile(record(vec![node("int", "m_Value")]));
        let mut foreign_budget = AssetLoadBudget::default();
        let error = schema
            .validate_value(
                foreign_schema.root(),
                &UnityValue::Integer(7),
                ByteOrder::Little,
                &mut foreign_budget,
            )
            .unwrap_err();
        assert!(matches!(error, TypeTreeWriteError::ForeignNode));
    }

    #[test]
    fn encoding_validation_requires_unnamed_fields_but_rewrite_validation_preserves_them() {
        let schema = compile(record(vec![node("int", ""), node("int", "m_Named")]));
        let properties = IndexMap::from([("m_Named".to_owned(), UnityValue::Integer(7))]);
        let value = UnityValue::Object(properties.clone());

        let mut encode_budget = AssetLoadBudget::default();
        assert!(matches!(
            schema.validate_object(&properties, ByteOrder::Little, &mut encode_budget),
            Err(TypeTreeWriteError::InvalidValue { .. })
        ));

        let mut encode_budget = AssetLoadBudget::default();
        assert!(matches!(
            schema.validate_value(schema.root(), &value, ByteOrder::Little, &mut encode_budget),
            Err(TypeTreeWriteError::InvalidValue { .. })
        ));

        let mut rewrite_budget = AssetLoadBudget::default();
        schema
            .validate_rewrite_candidate_with_context(
                schema.root(),
                &value,
                ByteOrder::Little,
                &mut rewrite_budget,
                TypeTreeTraversalContext::root(),
                0,
            )
            .unwrap();
    }

    #[test]
    fn truncated_templates_are_classified_as_malformed_input() {
        let cases = [
            (
                sequence("m_Values", node("int", "data")),
                UnityValue::Array(Vec::new()),
                vec![0, 0, 0],
            ),
            (node("int", "m_Value"), UnityValue::Integer(7), Vec::new()),
            (
                aligned(node("UInt8", "m_Value")),
                UnityValue::Integer(7),
                vec![7],
            ),
            (
                node("int", "m_Value"),
                UnityValue::Integer(7),
                vec![7, 0, 0],
            ),
        ];

        for (field, value, original) in cases {
            let field_name = field.name.clone();
            let schema = compile(record(vec![field]));
            let properties = IndexMap::from([(field_name, value)]);
            let mut budget = AssetLoadBudget::default();
            assert!(matches!(
                schema.rewrite_object(&original, &properties, ByteOrder::Little, &mut budget,),
                Err(TypeTreeWriteError::MalformedTemplate { .. })
            ));
        }
    }
}
