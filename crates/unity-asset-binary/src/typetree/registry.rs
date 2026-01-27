//! External TypeTree registry (UnityPy TPK-like fallback).
//!
//! Unity assets can be built with stripped TypeTrees (`enableTypeTree = false`). In those cases,
//! consumers may still want a best-effort parser by supplying an external registry of TypeTrees.
//!
//! This module provides an injectable registry abstraction and a simple JSON-backed implementation.

use crate::typetree::TypeTree;
use crate::{error::BinaryError, error::Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::RwLock;

pub trait TypeTreeRegistry: Send + Sync + std::fmt::Debug {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>>;

    /// Resolve a script type tree (e.g. MonoBehaviour) using the script's 16-byte ID.
    ///
    /// Unity stores script types as `class_id=114` with a `script_id` value in `SerializedType`.
    /// UnityPy uses `TypeTreeGeneratorAPI` to produce a per-script TypeTree; this hook lets callers
    /// provide equivalent data via registries.
    fn resolve_script(
        &self,
        _unity_version: &str,
        _class_id: i32,
        _script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        None
    }
}

/// A generator hook for script-specific TypeTrees (UnityPy `typetree_generator` equivalent).
///
/// Unity's `SerializedType` entries for `MonoBehaviour` can carry a `script_id` (16-byte Hash128),
/// and UnityPy uses `TypeTreeGeneratorAPI` to generate TypeTrees for those scripts on demand.
///
/// This trait allows injecting an equivalent generator. A generator can be backed by:
/// - a precomputed mapping table (e.g. `script_id -> TypeTree`),
/// - a local binding to an external generator (IL2CPP/managed), etc.
pub trait ScriptTypeTreeGenerator: Send + Sync + std::fmt::Debug {
    fn generate(&self, unity_version: &str, class_id: i32, script_id: [u8; 16])
    -> Option<TypeTree>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ScriptCacheKey {
    unity_version: String,
    class_id: i32,
    script_id: [u8; 16],
}

/// A `TypeTreeRegistry` adapter that calls a script TypeTree generator on demand and caches results.
#[derive(Debug)]
pub struct ScriptTypeTreeGeneratorRegistry {
    generator: Arc<dyn ScriptTypeTreeGenerator>,
    cache: RwLock<HashMap<ScriptCacheKey, Arc<TypeTree>>>,
}

impl ScriptTypeTreeGeneratorRegistry {
    pub fn new(generator: Arc<dyn ScriptTypeTreeGenerator>) -> Self {
        Self {
            generator,
            cache: RwLock::new(HashMap::new()),
        }
    }
}

impl TypeTreeRegistry for ScriptTypeTreeGeneratorRegistry {
    fn resolve(&self, _unity_version: &str, _class_id: i32) -> Option<Arc<TypeTree>> {
        None
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        let key = ScriptCacheKey {
            unity_version: unity_version.to_string(),
            class_id,
            script_id,
        };

        if let Ok(cache) = self.cache.read()
            && let Some(hit) = cache.get(&key)
        {
            return Some(hit.clone());
        }

        let generated = self
            .generator
            .generate(unity_version, class_id, script_id)?;
        let arc = Arc::new(generated);

        match self.cache.write() {
            Ok(mut cache) => {
                cache.insert(key, arc.clone());
            }
            Err(e) => {
                e.into_inner().insert(key, arc.clone());
            }
        }

        Some(arc)
    }
}

/// A registry that resolves by trying multiple registries in order (first match wins).
#[derive(Debug, Default, Clone)]
pub struct CompositeTypeTreeRegistry {
    registries: Vec<Arc<dyn TypeTreeRegistry>>,
}

impl CompositeTypeTreeRegistry {
    pub fn new(registries: Vec<Arc<dyn TypeTreeRegistry>>) -> Self {
        Self { registries }
    }

    pub fn push(&mut self, registry: Arc<dyn TypeTreeRegistry>) {
        self.registries.push(registry);
    }

    pub fn extend(&mut self, registries: impl IntoIterator<Item = Arc<dyn TypeTreeRegistry>>) {
        self.registries.extend(registries);
    }

    pub fn is_empty(&self) -> bool {
        self.registries.is_empty()
    }
}

impl TypeTreeRegistry for CompositeTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        for r in &self.registries {
            if let Some(t) = r.resolve(unity_version, class_id) {
                return Some(t);
            }
        }
        None
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        for r in &self.registries {
            if let Some(t) = r.resolve_script(unity_version, class_id, script_id) {
                return Some(t);
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
enum VersionSelector {
    Any,
    Exact(String),
    Prefix(String),
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    selector: VersionSelector,
    tree: Arc<TypeTree>,
}

/// A simple in-memory registry keyed by Unity class ID.
#[derive(Debug, Default, Clone)]
pub struct InMemoryTypeTreeRegistry {
    by_class_id: HashMap<i32, Vec<RegistryEntry>>,
    by_script_id: HashMap<[u8; 16], Vec<RegistryEntry>>,
}

impl InMemoryTypeTreeRegistry {
    pub fn insert_any(&mut self, class_id: i32, tree: TypeTree) {
        self.insert_internal(class_id, VersionSelector::Any, tree);
    }

    pub fn insert_exact(&mut self, unity_version: String, class_id: i32, tree: TypeTree) {
        self.insert_internal(class_id, VersionSelector::Exact(unity_version), tree);
    }

    pub fn insert_prefix(&mut self, unity_version_prefix: String, class_id: i32, tree: TypeTree) {
        self.insert_internal(
            class_id,
            VersionSelector::Prefix(unity_version_prefix),
            tree,
        );
    }

    pub fn insert_script_any(&mut self, script_id: [u8; 16], tree: TypeTree) {
        self.insert_script_internal(script_id, VersionSelector::Any, tree);
    }

    pub fn insert_script_exact(
        &mut self,
        unity_version: String,
        script_id: [u8; 16],
        tree: TypeTree,
    ) {
        self.insert_script_internal(script_id, VersionSelector::Exact(unity_version), tree);
    }

    pub fn insert_script_prefix(
        &mut self,
        unity_version_prefix: String,
        script_id: [u8; 16],
        tree: TypeTree,
    ) {
        self.insert_script_internal(
            script_id,
            VersionSelector::Prefix(unity_version_prefix),
            tree,
        );
    }

    fn insert_internal(&mut self, class_id: i32, selector: VersionSelector, tree: TypeTree) {
        self.by_class_id
            .entry(class_id)
            .or_default()
            .push(RegistryEntry {
                selector,
                tree: Arc::new(tree),
            });
    }

    fn insert_script_internal(
        &mut self,
        script_id: [u8; 16],
        selector: VersionSelector,
        tree: TypeTree,
    ) {
        self.by_script_id
            .entry(script_id)
            .or_default()
            .push(RegistryEntry {
                selector,
                tree: Arc::new(tree),
            });
    }
}

impl TypeTreeRegistry for InMemoryTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        let entries = self.by_class_id.get(&class_id)?;

        // 1) exact match
        for e in entries {
            if matches!(&e.selector, VersionSelector::Exact(v) if v == unity_version) {
                return Some(e.tree.clone());
            }
        }

        // 2) best (longest) prefix match
        let mut best: Option<(&RegistryEntry, usize)> = None;
        for e in entries {
            let VersionSelector::Prefix(prefix) = &e.selector else {
                continue;
            };
            if unity_version.starts_with(prefix) {
                let len = prefix.len();
                match best {
                    Some((_prev, prev_len)) if prev_len >= len => {}
                    _ => best = Some((e, len)),
                }
            }
        }
        if let Some((e, _)) = best {
            return Some(e.tree.clone());
        }

        // 3) any
        for e in entries {
            if matches!(e.selector, VersionSelector::Any) {
                return Some(e.tree.clone());
            }
        }

        None
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        _class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        let entries = self.by_script_id.get(&script_id)?;

        // 1) exact match
        for e in entries {
            if matches!(&e.selector, VersionSelector::Exact(v) if v == unity_version) {
                return Some(e.tree.clone());
            }
        }

        // 2) best (longest) prefix match
        let mut best: Option<(&RegistryEntry, usize)> = None;
        for e in entries {
            let VersionSelector::Prefix(prefix) = &e.selector else {
                continue;
            };
            if unity_version.starts_with(prefix) {
                let len = prefix.len();
                match best {
                    Some((_prev, prev_len)) if prev_len >= len => {}
                    _ => best = Some((e, len)),
                }
            }
        }
        if let Some((e, _)) = best {
            return Some(e.tree.clone());
        }

        // 3) any
        for e in entries {
            if matches!(e.selector, VersionSelector::Any) {
                return Some(e.tree.clone());
            }
        }

        None
    }
}

#[derive(Debug, Deserialize)]
struct JsonRegistryFile {
    schema: u32,
    entries: Vec<JsonRegistryEntry>,
}

#[derive(Debug, Deserialize)]
struct JsonRegistryEntry {
    #[serde(default)]
    unity_version: Option<String>,
    class_id: i32,
    #[serde(default)]
    script_id: Option<String>,
    type_tree: TypeTree,
}

/// JSON-backed TypeTree registry.
///
/// Format:
/// ```json
/// { "schema": 1, "entries": [ { "unity_version": "2020.3.*", "class_id": 28, "type_tree": { ... } } ] }
/// ```
#[derive(Debug, Default, Clone)]
pub struct JsonTypeTreeRegistry {
    inner: InMemoryTypeTreeRegistry,
}

impl JsonTypeTreeRegistry {
    pub fn from_reader(mut reader: impl Read) -> Result<Self> {
        let mut buf = String::new();
        reader
            .read_to_string(&mut buf)
            .map_err(|e| BinaryError::generic(format!("Failed to read registry JSON: {}", e)))?;
        let parsed: JsonRegistryFile = serde_json::from_str(&buf)
            .map_err(|e| BinaryError::invalid_data(format!("Invalid registry JSON: {}", e)))?;
        if parsed.schema != 1 && parsed.schema != 2 {
            return Err(BinaryError::invalid_data(format!(
                "Unsupported registry schema: {}",
                parsed.schema
            )));
        }

        let mut inner = InMemoryTypeTreeRegistry::default();
        for e in parsed.entries {
            if let Some(script_id) = e.script_id {
                let script_id = parse_hex_32_bytes(&script_id).ok_or_else(|| {
                    BinaryError::invalid_data(format!("Invalid script_id hex: {}", script_id))
                })?;
                match e.unity_version {
                    None => inner.insert_script_any(script_id, e.type_tree),
                    Some(v) => {
                        if v.is_empty() {
                            inner.insert_script_any(script_id, e.type_tree);
                        } else if let Some(prefix) = v.strip_suffix('*') {
                            inner.insert_script_prefix(prefix.to_string(), script_id, e.type_tree);
                        } else {
                            inner.insert_script_exact(v, script_id, e.type_tree);
                        }
                    }
                }
            } else {
                match e.unity_version {
                    None => inner.insert_any(e.class_id, e.type_tree),
                    Some(v) => {
                        if v.is_empty() {
                            inner.insert_any(e.class_id, e.type_tree);
                        } else if let Some(prefix) = v.strip_suffix('*') {
                            inner.insert_prefix(prefix.to_string(), e.class_id, e.type_tree);
                        } else {
                            inner.insert_exact(v, e.class_id, e.type_tree);
                        }
                    }
                }
            }
        }

        Ok(Self { inner })
    }

    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let mut f = std::fs::File::open(path.as_ref()).map_err(|e| {
            BinaryError::generic(format!(
                "Failed to open registry JSON {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;
        Self::from_reader(&mut f)
    }
}

impl TypeTreeRegistry for JsonTypeTreeRegistry {
    fn resolve(&self, unity_version: &str, class_id: i32) -> Option<Arc<TypeTree>> {
        self.inner.resolve(unity_version, class_id)
    }

    fn resolve_script(
        &self,
        unity_version: &str,
        class_id: i32,
        script_id: [u8; 16],
    ) -> Option<Arc<TypeTree>> {
        self.inner
            .resolve_script(unity_version, class_id, script_id)
    }
}

fn parse_hex_32_bytes(raw: &str) -> Option<[u8; 16]> {
    let s = raw.trim();
    if s.len() != 32 {
        return None;
    }

    let mut out = [0u8; 16];
    for (i, chunk) in s.as_bytes().chunks_exact(2).enumerate() {
        out[i] = (hex_nibble(chunk[0])? << 4) | hex_nibble(chunk[1])?;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dummy_tree(tag: u32) -> TypeTree {
        let mut t = TypeTree::new();
        t.version = tag;
        t.platform = tag;
        t
    }

    #[test]
    fn in_memory_registry_version_precedence() {
        let class_id = 28;

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_any(class_id, dummy_tree(1));
        reg.insert_prefix("2020.3.".to_string(), class_id, dummy_tree(2));
        reg.insert_exact("2020.3.48f1".to_string(), class_id, dummy_tree(3));

        let exact = reg.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(exact.version, 3);

        let prefix = reg.resolve("2020.3.9f1", class_id).unwrap();
        assert_eq!(prefix.version, 2);

        let any = reg.resolve("2019.4.40f1", class_id).unwrap();
        assert_eq!(any.version, 1);
    }

    #[test]
    fn in_memory_registry_longest_prefix_wins() {
        let class_id = 28;

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_prefix("2020.".to_string(), class_id, dummy_tree(1));
        reg.insert_prefix("2020.3.".to_string(), class_id, dummy_tree(2));

        let t = reg.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn composite_registry_first_match_wins() {
        let class_id = 28;

        let mut a = InMemoryTypeTreeRegistry::default();
        a.insert_any(class_id, dummy_tree(1));
        let mut b = InMemoryTypeTreeRegistry::default();
        b.insert_any(class_id, dummy_tree(2));

        let composite_ab = CompositeTypeTreeRegistry::new(vec![Arc::new(a), Arc::new(b)]);
        let t = composite_ab.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 1);

        let mut a2 = InMemoryTypeTreeRegistry::default();
        a2.insert_any(class_id, dummy_tree(1));
        let mut b2 = InMemoryTypeTreeRegistry::default();
        b2.insert_any(class_id, dummy_tree(2));

        let composite_ba = CompositeTypeTreeRegistry::new(vec![Arc::new(b2), Arc::new(a2)]);
        let t = composite_ba.resolve("2020.3.48f1", class_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn json_registry_supports_wildcard_and_exact() {
        let json = r#"
        {
          "schema": 1,
          "entries": [
            { "unity_version": "2020.3.*", "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 2, "platform": 2, "has_type_dependencies": false } },
            { "unity_version": "2020.3.48f1", "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 3, "platform": 3, "has_type_dependencies": false } },
            { "class_id": 28, "type_tree": { "nodes": [], "string_buffer": [], "version": 1, "platform": 1, "has_type_dependencies": false } }
          ]
        }
        "#;

        let reg = JsonTypeTreeRegistry::from_reader(json.as_bytes()).unwrap();

        let exact = reg.resolve("2020.3.48f1", 28).unwrap();
        assert_eq!(exact.version, 3);

        let prefix = reg.resolve("2020.3.9f1", 28).unwrap();
        assert_eq!(prefix.version, 2);

        let any = reg.resolve("2019.4.40f1", 28).unwrap();
        assert_eq!(any.version, 1);
    }

    #[test]
    fn in_memory_registry_resolves_script_id() {
        let script_id = [0x01u8; 16];

        let mut reg = InMemoryTypeTreeRegistry::default();
        reg.insert_script_any(script_id, dummy_tree(1));

        let t = reg
            .resolve_script(
                "2020.3.48f1",
                unity_asset_core::class_ids::MONO_BEHAVIOUR,
                script_id,
            )
            .unwrap();
        assert_eq!(t.version, 1);
    }

    #[test]
    fn json_registry_schema_2_supports_script_id_hex() {
        let json = r#"
        {
          "schema": 2,
          "entries": [
            { "unity_version": "2020.3.*", "class_id": 114, "script_id": "01010101010101010101010101010101", "type_tree": { "nodes": [], "string_buffer": [], "version": 2, "platform": 2, "has_type_dependencies": false } }
          ]
        }
        "#;

        let reg = JsonTypeTreeRegistry::from_reader(json.as_bytes()).unwrap();
        let script_id = [0x01u8; 16];
        let t = reg.resolve_script("2020.3.9f1", 114, script_id).unwrap();
        assert_eq!(t.version, 2);
    }

    #[test]
    fn script_generator_registry_caches_results() {
        #[derive(Debug)]
        struct Generator {
            calls: AtomicUsize,
        }

        impl ScriptTypeTreeGenerator for Generator {
            fn generate(
                &self,
                _unity_version: &str,
                _class_id: i32,
                _script_id: [u8; 16],
            ) -> Option<TypeTree> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Some(dummy_tree(42))
            }
        }

        let generator = Arc::new(Generator {
            calls: AtomicUsize::new(0),
        });
        let reg = ScriptTypeTreeGeneratorRegistry::new(generator.clone());

        let script_id = [0x02u8; 16];
        let t1 = reg.resolve_script("2020.3.48f1", 114, script_id).unwrap();
        let t2 = reg.resolve_script("2020.3.48f1", 114, script_id).unwrap();
        assert_eq!(t1.version, 42);
        assert_eq!(t2.version, 42);
        assert_eq!(generator.calls.load(Ordering::SeqCst), 1);
    }
}
