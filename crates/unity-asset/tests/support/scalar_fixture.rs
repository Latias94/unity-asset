use unity_asset_binary::asset::SerializedFileParser;
use unity_asset_binary::typetree::{TypeTree, TypeTreeNode};
use unity_asset_write::serialized_file::{SerializedFileEdits, SerializedFileWriter};

const ORIGINAL_SCALAR_PATH_ID: i64 = 42;
const ORIGINAL_SCALAR_VALUE: i32 = 0x16AA_BBCC;

pub(crate) fn record_scalar_v22(base: &[u8], path_id: i64, value: i32) -> Vec<u8> {
    let mut bytes = base.to_vec();
    replace_unique(
        &mut bytes,
        &ORIGINAL_SCALAR_PATH_ID.to_be_bytes(),
        &path_id.to_be_bytes(),
    );
    replace_unique(
        &mut bytes,
        &ORIGINAL_SCALAR_VALUE.to_be_bytes(),
        &value.to_be_bytes(),
    );

    let file = SerializedFileParser::from_bytes(bytes).expect("parse scalar wire fixture");
    let mut types = file.types().to_vec();
    let serialized_type = types
        .first_mut()
        .expect("scalar wire fixture has one serialized type");
    let original_tree = &serialized_type.type_tree;
    let mut field = original_tree
        .nodes
        .first()
        .expect("scalar wire fixture has one TypeTree node")
        .clone();

    let mut tree = TypeTree {
        version: original_tree.version,
        platform: original_tree.platform,
        has_type_dependencies: original_tree.has_type_dependencies,
        ..TypeTree::new()
    };
    let root_offset = tree.add_string("FixtureScalar");
    let field_type_offset = tree.add_string(&field.type_name);
    let field_name_offset = tree.add_string(&field.name);

    field.level = 1;
    field.index = 1;
    field.type_str_offset = field_type_offset;
    field.name_str_offset = field_name_offset;

    let mut root = TypeTreeNode::with_info(
        "FixtureScalar".to_owned(),
        "FixtureScalar".to_owned(),
        field.byte_size,
    );
    root.version = field.version;
    root.type_str_offset = root_offset;
    root.name_str_offset = root_offset;
    root.children.push(field);
    tree.add_node(root);
    serialized_type.type_tree = tree;

    let ref_types = file.ref_types().to_vec();
    SerializedFileWriter::save(
        &file.with_type_tables(types, ref_types),
        &SerializedFileEdits::default(),
    )
    .expect("encode record-root scalar fixture")
}

fn replace_unique(bytes: &mut [u8], original: &[u8], replacement: &[u8]) {
    assert_eq!(original.len(), replacement.len());
    let offsets = bytes
        .windows(original.len())
        .enumerate()
        .filter_map(|(offset, candidate)| (candidate == original).then_some(offset))
        .collect::<Vec<_>>();
    assert_eq!(offsets.len(), 1);
    let offset = offsets[0];
    bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
}
