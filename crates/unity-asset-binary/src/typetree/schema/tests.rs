use super::*;
use crate::asset::SerializedType;
use crate::reader::{BinaryReader, ByteOrder};
use crate::typetree::types::{TypeTree, TypeTreeNode};
use unity_asset_core::{AssetLoadBudget, AssetLoadLimits};

fn node(type_name: &str, name: &str) -> TypeTreeNode {
    TypeTreeNode::with_info(type_name.to_string(), name.to_string(), -1)
}

fn tree_with_root(root: TypeTreeNode) -> TypeTree {
    let mut tree = TypeTree::new();
    tree.add_node(root);
    tree
}

#[test]
fn primitive_aliases_compile_to_one_kind() {
    let mut root = node("Root", "Root");
    root.children.push(node("int", "first"));
    root.children.push(node("SInt32", "second"));
    let schema =
        TypeTreeSchema::compile(&tree_with_root(root), &[], &mut AssetLoadBudget::default())
            .unwrap();

    let children: Vec<_> = schema.root().children().collect();
    assert_eq!(children[0].kind(), SemanticKind::Scalar(PrimitiveKind::I32));
    assert_eq!(children[1].kind(), SemanticKind::Scalar(PrimitiveKind::I32));
    assert_eq!(PrimitiveKind::I32.width(), 4);
    assert_eq!(
        PrimitiveKind::I32.signedness(),
        Some(IntegerSignedness::Signed)
    );
}

#[test]
fn sequence_merges_collection_alignment_and_suppresses_bulk_element_alignment() {
    let mut wrapper = node("vector", "items");
    let mut array = node("Array", "Array");
    let mut size = node("int", "size");
    size.meta_flags = 0x4000;
    let mut data = node("UInt32", "data");
    data.meta_flags = 0x4000;
    array.children.push(size);
    array.children.push(data);
    wrapper.children.push(array);

    let schema = TypeTreeSchema::compile(
        &tree_with_root(wrapper),
        &[],
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let root = schema.root();
    assert_eq!(root.kind(), SemanticKind::Sequence);
    assert!(root.align_after());
    let element = root.child(0).unwrap();
    assert!(!element.align_after());
    assert_eq!(element.kind(), SemanticKind::Scalar(PrimitiveKind::U32));
    let SemanticLayout::Sequence(layout) = root.semantic_layout() else {
        panic!("expected a typed sequence layout");
    };
    assert_eq!(layout.element(), element);
    assert_eq!(layout.bulk_primitive(), Some(PrimitiveKind::U32));
}

#[test]
fn sequence_ignores_size_node_alignment() {
    let mut wrapper = node("vector", "items");
    let mut array = node("Array", "Array");
    let mut size = node("int", "size");
    size.meta_flags = 0x4000;
    array.children.push(size);
    array.children.push(node("UInt32", "data"));
    wrapper.children.push(array);

    let schema = TypeTreeSchema::compile(
        &tree_with_root(wrapper),
        &[],
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    assert!(!schema.root().align_after());
}

#[test]
fn pptr_layout_is_preparsed() {
    let mut root = node("Root", "Root");
    let mut pointer = node("PPtr<Object>", "target");
    pointer.children.push(node("int", "m_FileID"));
    pointer.children.push(node("long long", "m_PathID"));
    root.children.push(pointer);

    let schema =
        TypeTreeSchema::compile(&tree_with_root(root), &[], &mut AssetLoadBudget::default())
            .unwrap();
    let pointer = schema.root().child(0).unwrap();
    let SemanticLayout::PPtr(layout) = pointer.semantic_layout() else {
        panic!("expected a typed PPtr layout");
    };
    assert_eq!(layout.file_primitive(), PrimitiveKind::I32);
    assert_eq!(layout.path_primitive(), PrimitiveKind::I64);
    assert_eq!(layout.file_child().name(), "m_FileID");
    assert_eq!(layout.path_child().name(), "m_PathID");
}

#[test]
fn pptr_field_roles_preserve_distinct_integer_constraints() {
    fn compile_pointer(file_type: &str, path_type: &str) -> crate::error::Result<TypeTreeSchema> {
        let mut pointer = node("PPtr<Object>", "target");
        pointer.children.push(node(file_type, "m_FileID"));
        pointer.children.push(node(path_type, "m_PathID"));
        TypeTreeSchema::compile(
            &tree_with_root(pointer),
            &[],
            &mut AssetLoadBudget::default(),
        )
    }

    let error = compile_pointer("long long", "long long").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid data: PPtr file ID field 'm_FileID' is wider than 32 bits"
    );

    let error = compile_pointer("float", "long long").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid data: PPtr file ID field 'm_FileID' has non-integer type 'float'"
    );

    let error = compile_pointer("int", "double").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid data: PPtr path ID field 'm_PathID' has non-integer type 'double'"
    );
}

#[test]
fn pair_layout_exposes_exactly_two_prevalidated_children() {
    let mut pair = node("pair", "entry");
    pair.children.push(node("int", "first"));
    pair.children.push(node("UInt32", "second"));

    let schema =
        TypeTreeSchema::compile(&tree_with_root(pair), &[], &mut AssetLoadBudget::default())
            .unwrap();
    let root = schema.root();
    let SemanticLayout::Pair(layout) = root.semantic_layout() else {
        panic!("expected a typed pair layout");
    };

    assert_eq!(layout.first().name(), "first");
    assert_eq!(layout.second().name(), "second");
    assert_eq!(
        layout.children(),
        [root.child(0).unwrap(), root.child(1).unwrap()]
    );
    assert_eq!(root.pair_layout(), Some(layout));
}

#[test]
fn pair_cardinality_is_validated_once_during_compilation() {
    for child_count in [0, 1, 3] {
        let mut pair = node("pair", "entry");
        pair.children = (0..child_count)
            .map(|index| node("int", &format!("field_{index}")))
            .collect();
        let error =
            TypeTreeSchema::compile(&tree_with_root(pair), &[], &mut AssetLoadBudget::default())
                .unwrap_err();
        assert!(error.to_string().contains("must have exactly two children"));
    }
}

#[test]
fn aligned_pair_element_promotes_alignment_to_collection_tail() {
    let mut wrapper = node("vector", "items");
    let mut array = node("Array", "Array");
    array.children.push(node("int", "size"));
    let mut pair = node("pair", "data");
    pair.meta_flags = 0x4000;
    pair.children.push(node("int", "first"));
    pair.children.push(node("int", "second"));
    array.children.push(pair);
    wrapper.children.push(array);

    let schema = TypeTreeSchema::compile(
        &tree_with_root(wrapper),
        &[],
        &mut AssetLoadBudget::default(),
    )
    .unwrap();
    let root = schema.root();
    let SemanticLayout::Sequence(layout) = root.semantic_layout() else {
        panic!("expected a typed sequence layout");
    };
    assert!(root.align_after());
    assert!(!layout.element().align_after());
    assert!(matches!(
        layout.element().semantic_layout(),
        SemanticLayout::Pair(_)
    ));
    assert_eq!(layout.bulk_primitive(), None);
}

#[test]
fn referenced_object_compiles_dynamic_payload_and_resolves_sorted_catalog() {
    let mut referenced = node("ReferencedObject", "m_Ref");
    let mut type_node = node("ReferencedObjectType", "type");
    let mut class_name = node("string", "m_ClassName");
    let mut string_array = node("Array", "Array");
    string_array.children.push(node("int", "size"));
    string_array.children.push(node("char", "data"));
    class_name.children.push(string_array);
    type_node.children.push(class_name);
    type_node.children.push(node("string", "m_NameSpace"));
    type_node.children.push(node("string", "m_AssemblyName"));
    referenced.children.push(type_node);
    referenced
        .children
        .push(node("ReferencedObjectData", "data"));

    let make_ref_type = |class_name: &str, root_type: &str| {
        let mut root = node(root_type, "Base");
        root.children.push(node("int", "value"));
        let mut ref_type = SerializedType::new(0);
        ref_type.class_name = class_name.to_string();
        ref_type.namespace = "Game".to_string();
        ref_type.assembly_name = "Assembly".to_string();
        ref_type.type_tree = tree_with_root(root);
        ref_type
    };
    let ref_types = [
        make_ref_type("Zulu", "ZuluRoot"),
        make_ref_type("Alpha", "AlphaRoot"),
    ];
    let schema = TypeTreeSchema::compile(
        &tree_with_root(referenced),
        &ref_types,
        &mut AssetLoadBudget::default(),
    )
    .unwrap();

    let root = schema.root();
    let SemanticLayout::ReferencedObject(layout) = root.semantic_layout() else {
        panic!("expected a typed ReferencedObject layout");
    };
    assert_eq!(layout.class_field().name(), "m_ClassName");
    assert_eq!(layout.namespace_field().name(), "m_NameSpace");
    assert_eq!(layout.assembly_field().name(), "m_AssemblyName");
    assert!(layout.is_type_node(root.child(0).unwrap()));
    assert!(layout.is_payload(root.child(1).unwrap()));
    assert!(matches!(layout.payload(), ManagedPayload::Dynamic(_)));
    assert_eq!(layout.payload().node().kind(), SemanticKind::ManagedPayload);
    assert_eq!(
        schema
            .resolve_managed_root("Alpha", "Game", "Assembly")
            .unwrap()
            .type_name(),
        "AlphaRoot"
    );
    assert!(
        schema
            .resolve_managed_root("", "Game", "Assembly")
            .is_none()
    );
    assert!(
        schema
            .resolve_managed_root("Missing", "Game", "Assembly")
            .is_none()
    );
}

#[test]
fn referenced_object_rejects_payload_before_type_key() {
    let mut type_node = node("ReferencedObjectType", "type");
    type_node.children.push(node("string", "m_ClassName"));
    type_node.children.push(node("string", "m_NameSpace"));
    type_node.children.push(node("string", "m_AssemblyName"));
    let mut referenced = node("ReferencedObject", "m_Ref");
    referenced
        .children
        .push(node("ReferencedObjectData", "data"));
    referenced.children.push(type_node);

    assert!(
        TypeTreeSchema::compile(
            &tree_with_root(referenced),
            &[],
            &mut AssetLoadBudget::default(),
        )
        .is_err()
    );
}

#[test]
fn managed_registries_remain_available_to_runtime_context() {
    let registry = |name: &str, child: TypeTreeNode| {
        let mut registry = node("ManagedReferencesRegistry", name);
        registry.children.push(child);
        registry
    };
    let mut pointer = node("PPtr<Object>", "target");
    pointer.children.push(node("int", "m_FileID"));
    pointer.children.push(node("long long", "m_PathID"));

    let mut nested = node("Nested", "nested");
    nested
        .children
        .push(registry("nested_first", node("int", "value")));
    nested
        .children
        .push(registry("nested_second", pointer.clone()));
    let mut root = node("Root", "Root");
    root.children.push(registry("first", node("int", "value")));
    let mut duplicate = registry("second", pointer);
    duplicate.meta_flags = 0x4000;
    root.children.push(duplicate);
    root.children.push(nested);

    let schema =
        TypeTreeSchema::compile(&tree_with_root(root), &[], &mut AssetLoadBudget::default())
            .unwrap();
    let root = schema.root();
    let first = root.child(0).unwrap();
    let second = root.child(1).unwrap();
    let nested = root.child(2).unwrap();
    assert_eq!(first.kind(), SemanticKind::ManagedRegistry);
    assert_eq!(second.kind(), SemanticKind::ManagedRegistry);
    assert_eq!(second.child_count(), 1);
    assert!(second.align_after());
    assert_eq!(
        nested.child(0).unwrap().kind(),
        SemanticKind::ManagedRegistry
    );
    assert_eq!(
        nested.child(1).unwrap().kind(),
        SemanticKind::ManagedRegistry
    );
}

#[test]
fn duplicate_managed_reference_keys_are_rejected() {
    let mut ref_root = node("Referenced", "Referenced");
    ref_root.children.push(node("int", "value"));
    let ref_tree = tree_with_root(ref_root);
    let mut first = SerializedType::new(0);
    first.class_name = "Example".to_string();
    first.namespace = "Game".to_string();
    first.assembly_name = "Assembly-CSharp".to_string();
    first.type_tree = ref_tree.clone();
    let mut second = first.clone();
    second.type_tree = ref_tree;

    let mut type_node = node("ReferencedObjectType", "type");
    type_node.children = vec![
        node("string", "class"),
        node("string", "ns"),
        node("string", "asm"),
    ];
    let mut referenced = node("ReferencedObject", "m_Reference");
    referenced.children = vec![type_node, node("ReferencedObjectData", "data")];
    let mut root = node("Root", "Root");
    root.children.push(referenced);

    let error = TypeTreeSchema::compile(
        &tree_with_root(root),
        &[first, second],
        &mut AssetLoadBudget::default(),
    )
    .expect_err("duplicate keys must not overwrite one another");
    assert!(
        error
            .to_string()
            .contains("Duplicate managed reference type key")
    );
}

fn schema_with_managed_catalog() -> TypeTreeSchema {
    let mut type_node = node("ReferencedObjectType", "type");
    type_node.children = vec![
        node("string", "class"),
        node("string", "ns"),
        node("string", "asm"),
    ];
    let mut referenced = node("ReferencedObject", "m_Reference");
    referenced.children = vec![type_node, node("ReferencedObjectData", "data")];
    let mut object_root = node("Root", "Root");
    object_root.children.push(referenced);

    let mut managed_root = node("Managed", "Managed");
    managed_root.children.push(node("int", "m_Value"));
    let mut managed = SerializedType::new(114);
    managed.class_name = "Managed".to_owned();
    managed.namespace = "Tests".to_owned();
    managed.assembly_name = "Tests".to_owned();
    managed.type_tree = tree_with_root(managed_root);

    TypeTreeSchema::compile(
        &tree_with_root(object_root),
        &[managed],
        &mut AssetLoadBudget::default(),
    )
    .unwrap()
}

#[test]
fn value_traversal_accepts_its_managed_arena_and_rejects_a_foreign_catalog() {
    let schema = schema_with_managed_catalog();
    let managed = schema
        .resolve_managed_root("Managed", "Tests", "Tests")
        .unwrap();
    let bytes = 17_i32.to_le_bytes();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    let value = schema
        .read_value(&mut reader, &mut AssetLoadBudget::default(), managed)
        .unwrap();
    assert_eq!(
        value
            .value
            .as_object()
            .and_then(|properties| properties.get("m_Value"))
            .and_then(unity_asset_core::UnityValue::as_i64),
        Some(17)
    );

    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    schema
        .skip_value(&mut reader, &mut AssetLoadBudget::default(), managed)
        .unwrap();
    assert_eq!(reader.position(), 4);

    let foreign_schema = schema_with_managed_catalog();
    let foreign = foreign_schema
        .resolve_managed_root("Managed", "Tests", "Tests")
        .unwrap();
    let mut reader = BinaryReader::new(&bytes, ByteOrder::Little);
    assert!(
        schema
            .read_value(&mut reader, &mut AssetLoadBudget::default(), foreign)
            .is_err()
    );
}

#[test]
fn object_arena_allocation_is_charged_before_reservation() {
    const PREFLIGHT_STRING_BYTES: u64 = 8;
    let limits = AssetLoadLimits {
        max_bytes: PREFLIGHT_STRING_BYTES,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let error = TypeTreeSchema::compile(&tree_with_root(node("Root", "Root")), &[], &mut budget)
        .expect_err("the schema cannot reserve an unbudgeted arena");
    assert!(matches!(error, crate::error::BinaryError::Budget(_)));
    assert_eq!(budget.usage().bytes, PREFLIGHT_STRING_BYTES);
}

#[test]
fn managed_catalog_arena_allocation_is_charged_before_reservation() {
    const PREFLIGHT_STRING_BYTES: u64 = 9;
    let mut managed = SerializedType::new(114);
    managed.class_name = "Managed".to_owned();
    managed.type_tree = tree_with_root(node("M", "M"));

    let limits = AssetLoadLimits {
        max_bytes: PREFLIGHT_STRING_BYTES,
        ..AssetLoadLimits::default()
    };
    let mut budget = AssetLoadBudget::new(limits).unwrap();
    let error = ManagedReferenceCatalog::compile(&[managed], &mut budget)
        .expect_err("the catalog cannot reserve an unbudgeted arena");

    assert!(matches!(error, crate::error::BinaryError::Budget(_)));
    assert_eq!(budget.usage().bytes, PREFLIGHT_STRING_BYTES);
}
