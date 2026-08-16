use crate::typetree::TypeTreeNode;

pub(crate) fn node(type_name: &str, name: &str) -> TypeTreeNode {
    TypeTreeNode::with_info(type_name.to_owned(), name.to_owned(), -1)
}

pub(crate) fn aligned(mut node: TypeTreeNode) -> TypeTreeNode {
    node.meta_flags = 0x4000;
    node
}

pub(crate) fn record(children: Vec<TypeTreeNode>) -> TypeTreeNode {
    let mut root = node("Root", "Root");
    root.children = children;
    root
}

pub(crate) fn sequence(name: &str, element: TypeTreeNode) -> TypeTreeNode {
    let mut array = node("Array", "Array");
    array.children.push(node("int", "size"));
    array.children.push(element);
    let mut sequence = node("vector", name);
    sequence.children.push(array);
    sequence
}

pub(crate) fn map(name: &str, key: TypeTreeNode, value: TypeTreeNode) -> TypeTreeNode {
    let mut pair = node("pair", "data");
    pair.children.push(key);
    pair.children.push(value);
    let mut array = node("Array", "Array");
    array.children.push(node("int", "size"));
    array.children.push(pair);
    let mut map = node("map", name);
    map.children.push(array);
    map
}

pub(crate) fn pptr(name: &str) -> TypeTreeNode {
    let mut pointer = node("PPtr<Object>", name);
    pointer.children.push(node("int", "m_FileID"));
    pointer.children.push(node("SInt64", "m_PathID"));
    pointer
}
