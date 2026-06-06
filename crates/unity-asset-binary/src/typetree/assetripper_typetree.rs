use std::{fs::read_dir, io::{self, BufReader, Read}, path::Path, sync::Arc};
use thiserror::Error;
use crate::typetree::{
    InMemoryTypeTreeRegistry, TypeTree, TypeTreeNode, TypeTreeRegistry,
};

#[derive(Error, Debug)]
pub enum AssetRipperTypeTreeGeneratorRegistryError {
    #[error("Deserialization error")]
    Serde(#[from] serde_json::Error),

    #[error("File has not the right format")]
    FileFormatError,

    #[error("IO error")]
    IOError(#[from] io::Error),
}

#[derive(Debug)]
pub struct AssetRipperTypeTreeGeneratorRegistry {
    inner: InMemoryTypeTreeRegistry,
}

impl AssetRipperTypeTreeGeneratorRegistry {

    fn new() -> Self {
        Self {
            inner: InMemoryTypeTreeRegistry::default()
        }
    }

    pub fn new_from_path(path: impl AsRef<Path>) -> Result<Self, AssetRipperTypeTreeGeneratorRegistryError> {
        let mut s = Self::new();
        s.add_via_path(path)?;
        Ok(s)
    }

    pub fn new_from_reader(
        reader: impl Read,
    ) -> Result<Self, AssetRipperTypeTreeGeneratorRegistryError> {
        let mut s = Self::new();
        s.add_via_reader(reader)?;
        Ok(s)
    }

    pub fn add_via_path(&mut self, path: impl AsRef<Path>) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
        if path.as_ref().is_dir() {
            for e in read_dir(path)? {
                let e = e?;
                let path = e.path();
                if path.is_file() && path.extension().is_some_and(|ex| ex == "json") {
                    self.add_via_path(path)?;
                }
            }
        } else {
            let f = std::fs::File::open(path.as_ref())?;
            let buf =  BufReader::new(f);
            self.add_via_reader(buf)?;
        }
        Ok(())
    }

    pub fn add_via_reader(
        &mut self,
        reader: impl Read,
    ) -> Result<(), AssetRipperTypeTreeGeneratorRegistryError> {
        let parsed: serde_json::Value = serde_json::from_reader(reader)?;

        let unity_version = parsed
            .get("Version")
            .and_then(|v| v.as_str());

        let classes = parsed
            .get("Classes")
            .ok_or(AssetRipperTypeTreeGeneratorRegistryError::FileFormatError)?;

        for class in classes
            .as_array()
            .ok_or(AssetRipperTypeTreeGeneratorRegistryError::FileFormatError)?
        {
            if let Some((class_id, type_tree)) =
                AssetRipperTypeTreeGeneratorRegistry::parse_class(class)
            {   
                match unity_version {
                    None => self.inner.insert_any(class_id, type_tree),
                    Some(ref v) => {
                        if v.is_empty() {
                            self.inner.insert_any(class_id, type_tree);
                        } else if let Some(prefix) = v.strip_suffix('*') {
                            self.inner.insert_prefix(prefix.to_string(), class_id, type_tree);
                        } else {
                            self.inner.insert_exact(v.to_string(), class_id, type_tree);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn parse_class(value: &serde_json::Value) -> Option<(i32, TypeTree)> {
        let inner_typ = value.get("EditorRootNode")?;
        let tree = TypeTree {
            string_buffer: vec![],
            platform: 0,
            version: inner_typ
                .get("Version")
                .and_then(|val| val.as_u64())
                .unwrap_or(0) as u32,
            nodes: vec![
                AssetRipperTypeTreeGeneratorRegistry::parse_type(inner_typ)?
            ],
            has_type_dependencies: true,
        };

        Some((
            value
                .get("TypeID")
                .and_then(|val| val.as_i64())
                .unwrap_or(0) as i32,
            tree,
        ))
    }

    fn parse_type(value: &serde_json::Value) -> Option<TypeTreeNode> {
        let subtree = if let Some(values) = value.get("SubNodes").and_then(|val| val.as_array()) {
            values
                .iter()
                .map_while(AssetRipperTypeTreeGeneratorRegistry::parse_type)
                .collect::<Vec<TypeTreeNode>>()
        } else {
            vec![]
        };

        Some(TypeTreeNode {
            byte_size: value.get("ByteSize").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            children: subtree,
            index: value.get("Index").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            level: value.get("Level").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            meta_flags: value.get("MetaFlag").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            name: value.get("Name").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            name_str_offset: 0,
            ref_type_hash: 0,
            type_str_offset: 0,
            type_flags: value.get("TypeFlags").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            type_name: value.get("TypeName").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            version: value.get("Version").and_then(|v| v.as_i64()).unwrap_or_default() as i32,
            variable_count: 0,
        })
    }
}

impl TypeTreeRegistry for AssetRipperTypeTreeGeneratorRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        let opt = self.inner.resolve(unity_version, class_id);
        opt
    }

    // fn resolve_script(
    //     &self,
    //     unity_version: &str,
    //     class_id: i32,
    //     script_id: [u8; 16],
    // ) -> Option<Arc<TypeTree>> {
    //     self.inner
    //         .resolve_script(unity_version, class_id, script_id)
    // }
}
