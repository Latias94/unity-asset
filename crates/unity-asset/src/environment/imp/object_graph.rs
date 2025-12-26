use super::*;

/// A globally-unique identifier for a YAML object.
///
/// YAML anchors are only unique within a single YAML file, so the key also includes the file path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct YamlObjectKey {
    pub path: PathBuf,
    pub anchor: String,
}

/// A unified key across YAML and binary objects within an `Environment`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EnvironmentObjectKey {
    Yaml(YamlObjectKey),
    Binary(BinaryObjectKey),
}

impl std::fmt::Display for EnvironmentObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvironmentObjectKey::Yaml(k) => {
                write!(f, "yok1|{}|{}", k.path.to_string_lossy(), k.anchor)
            }
            EnvironmentObjectKey::Binary(k) => write!(f, "{}", k),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlExternalEdge {
    pub from: YamlObjectKey,
    /// The referenced `fileID` value in the YAML PPtr-like object.
    pub file_id: i64,
    /// Optional Unity GUID, when present in YAML.
    pub guid: Option<[u8; 16]>,
    /// Best-effort asset path resolved from `.meta` GUID indexing.
    pub asset_path: Option<PathBuf>,
    /// Best-effort resolved key (currently only YAML targets are resolved).
    pub resolved: Option<EnvironmentObjectKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalObjectEdge {
    Binary(ExternalDependencyEdge),
    Yaml(YamlExternalEdge),
}

/// Build options for `Environment` object graph extraction (YAML + binary).
#[derive(Debug, Clone, Copy)]
pub struct ObjectGraphBuildOptions {
    pub include_yaml: bool,
    pub include_binary: bool,
    pub binary: DependencyGraphBuildOptions,
}

impl Default for ObjectGraphBuildOptions {
    fn default() -> Self {
        Self {
            include_yaml: true,
            include_binary: true,
            binary: DependencyGraphBuildOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ObjectGraphTraversalOptions {
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    pub follow_resolved_external: bool,
}

impl Default for ObjectGraphTraversalOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            max_nodes: None,
            follow_resolved_external: false,
        }
    }
}

/// A best-effort object graph across all loaded sources in an `Environment`.
///
/// - Nodes are `EnvironmentObjectKey` (globally unique within the loaded environment).
/// - Internal edges point to other objects in the same YAML file or `SerializedFile`.
/// - External edges keep raw reference info and optionally a resolved key.
#[derive(Debug, Clone)]
pub struct EnvironmentObjectGraph {
    nodes: Vec<EnvironmentObjectKey>,
    internal_from: std::collections::HashMap<EnvironmentObjectKey, Vec<EnvironmentObjectKey>>,
    internal_to: std::collections::HashMap<EnvironmentObjectKey, Vec<EnvironmentObjectKey>>,
    external_from: std::collections::HashMap<EnvironmentObjectKey, Vec<ExternalObjectEdge>>,
}

impl EnvironmentObjectGraph {
    pub fn nodes(&self) -> &[EnvironmentObjectKey] {
        &self.nodes
    }

    pub fn internal_refs_from(&self, key: &EnvironmentObjectKey) -> &[EnvironmentObjectKey] {
        self.internal_from
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn internal_refs_to(&self, key: &EnvironmentObjectKey) -> &[EnvironmentObjectKey] {
        self.internal_to
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn external_refs_from(&self, key: &EnvironmentObjectKey) -> &[ExternalObjectEdge] {
        self.external_from
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn neighbors_from(
        &self,
        key: &EnvironmentObjectKey,
        follow_resolved_external: bool,
    ) -> Vec<EnvironmentObjectKey> {
        let mut out: Vec<EnvironmentObjectKey> = Vec::new();
        out.extend(self.internal_refs_from(key).iter().cloned());

        if follow_resolved_external {
            for ext in self.external_refs_from(key) {
                match ext {
                    ExternalObjectEdge::Binary(b) => {
                        if let Some(resolved) = &b.resolved {
                            out.push(EnvironmentObjectKey::Binary(resolved.clone()));
                        }
                    }
                    ExternalObjectEdge::Yaml(y) => {
                        if let Some(resolved) = &y.resolved {
                            out.push(resolved.clone());
                        }
                    }
                }
            }
        }

        out
    }

    pub fn closure_with_options(
        &self,
        roots: &[EnvironmentObjectKey],
        options: ObjectGraphTraversalOptions,
    ) -> Vec<EnvironmentObjectKey> {
        use std::collections::{HashSet, VecDeque};

        let mut visited: HashSet<EnvironmentObjectKey> = HashSet::new();
        let mut queue: VecDeque<(EnvironmentObjectKey, usize)> = VecDeque::new();

        for r in roots {
            queue.push_back((r.clone(), 0));
        }

        while let Some((node, depth)) = queue.pop_front() {
            if let Some(max) = options.max_nodes {
                if visited.len() >= max {
                    break;
                }
            }

            if !visited.insert(node.clone()) {
                continue;
            }

            if let Some(max) = options.max_depth {
                if depth >= max {
                    continue;
                }
            }

            for next in self.neighbors_from(&node, options.follow_resolved_external) {
                queue.push_back((next, depth.saturating_add(1)));
            }
        }

        let mut out: Vec<EnvironmentObjectKey> = visited.into_iter().collect();
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    pub fn roots(&self, follow_resolved_external: bool) -> Vec<EnvironmentObjectKey> {
        let mut incoming: std::collections::HashMap<EnvironmentObjectKey, usize> =
            std::collections::HashMap::new();
        for node in &self.nodes {
            incoming.insert(node.clone(), 0);
        }

        for from in &self.nodes {
            for to in self.internal_refs_from(from) {
                if let Some(v) = incoming.get_mut(to) {
                    *v += 1;
                }
            }
            if follow_resolved_external {
                for ext in self.external_refs_from(from) {
                    let resolved = match ext {
                        ExternalObjectEdge::Binary(b) => b
                            .resolved
                            .as_ref()
                            .map(|k| EnvironmentObjectKey::Binary(k.clone())),
                        ExternalObjectEdge::Yaml(y) => y.resolved.clone(),
                    };
                    let Some(to) = resolved else { continue };
                    if let Some(v) = incoming.get_mut(&to) {
                        *v += 1;
                    }
                }
            }
        }

        let mut out: Vec<EnvironmentObjectKey> = incoming
            .into_iter()
            .filter_map(|(k, deg)| if deg == 0 { Some(k) } else { None })
            .collect();
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    pub fn leaves(&self, follow_resolved_external: bool) -> Vec<EnvironmentObjectKey> {
        let mut out_deg: std::collections::HashMap<EnvironmentObjectKey, usize> =
            std::collections::HashMap::new();
        for node in &self.nodes {
            out_deg.insert(node.clone(), 0);
        }

        for from in &self.nodes {
            let mut deg = 0usize;
            deg += self.internal_refs_from(from).len();
            if follow_resolved_external {
                deg += self
                    .external_refs_from(from)
                    .iter()
                    .filter(|e| match e {
                        ExternalObjectEdge::Binary(b) => b.resolved.is_some(),
                        ExternalObjectEdge::Yaml(y) => y.resolved.is_some(),
                    })
                    .count();
            }
            if let Some(v) = out_deg.get_mut(from) {
                *v = deg;
            }
        }

        let mut out: Vec<EnvironmentObjectKey> = out_deg
            .into_iter()
            .filter_map(|(k, deg)| if deg == 0 { Some(k) } else { None })
            .collect();
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    /// Find cycles using Tarjan SCC (returns SCCs with >=2 nodes or self-loop).
    pub fn cycles(
        &self,
        max_components: usize,
        follow_resolved_external: bool,
    ) -> Vec<Vec<EnvironmentObjectKey>> {
        struct TarjanState {
            index: usize,
            stack: Vec<EnvironmentObjectKey>,
            on_stack: std::collections::HashSet<EnvironmentObjectKey>,
            indices: std::collections::HashMap<EnvironmentObjectKey, usize>,
            lowlink: std::collections::HashMap<EnvironmentObjectKey, usize>,
            components: Vec<Vec<EnvironmentObjectKey>>,
        }

        fn strong_connect(
            v: EnvironmentObjectKey,
            graph: &EnvironmentObjectGraph,
            follow_resolved_external: bool,
            st: &mut TarjanState,
            max_components: usize,
        ) {
            st.indices.insert(v.clone(), st.index);
            st.lowlink.insert(v.clone(), st.index);
            st.index += 1;
            st.stack.push(v.clone());
            st.on_stack.insert(v.clone());

            for w in graph.neighbors_from(&v, follow_resolved_external) {
                if !st.indices.contains_key(&w) {
                    strong_connect(
                        w.clone(),
                        graph,
                        follow_resolved_external,
                        st,
                        max_components,
                    );
                    let lw_v = st.lowlink.get(&v).copied().unwrap_or(0);
                    let lw_w = st.lowlink.get(&w).copied().unwrap_or(0);
                    if lw_w < lw_v {
                        st.lowlink.insert(v.clone(), lw_w);
                    }
                } else if st.on_stack.contains(&w) {
                    let idx_v = st.lowlink.get(&v).copied().unwrap_or(0);
                    let idx_w = st.indices.get(&w).copied().unwrap_or(0);
                    if idx_w < idx_v {
                        st.lowlink.insert(v.clone(), idx_w);
                    }
                }
            }

            let is_root = st.indices.get(&v) == st.lowlink.get(&v);
            if is_root {
                let mut comp: Vec<EnvironmentObjectKey> = Vec::new();
                loop {
                    let w = st.stack.pop().expect("stack pop");
                    st.on_stack.remove(&w);
                    comp.push(w.clone());
                    if w == v {
                        break;
                    }
                }

                let has_self_loop = comp.len() == 1
                    && graph
                        .neighbors_from(&comp[0], follow_resolved_external)
                        .iter()
                        .any(|n| n == &comp[0]);
                if comp.len() > 1 || has_self_loop {
                    comp.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
                    st.components.push(comp);
                    if st.components.len() >= max_components {
                        return;
                    }
                }
            }
        }

        let mut st = TarjanState {
            index: 0,
            stack: Vec::new(),
            on_stack: std::collections::HashSet::new(),
            indices: std::collections::HashMap::new(),
            lowlink: std::collections::HashMap::new(),
            components: Vec::new(),
        };

        for node in &self.nodes {
            if st.components.len() >= max_components {
                break;
            }
            if !st.indices.contains_key(node) {
                strong_connect(
                    node.clone(),
                    self,
                    follow_resolved_external,
                    &mut st,
                    max_components,
                );
            }
        }

        st.components.sort_by(|a, b| b.len().cmp(&a.len()));
        st.components
    }

    pub fn to_dot(&self, max_edges: usize, follow_resolved_external: bool) -> String {
        let mut out = String::new();
        out.push_str("digraph unity_asset_object_graph {\n");

        let mut edges_written = 0usize;

        for from in &self.nodes {
            for to in self.internal_refs_from(from) {
                if edges_written >= max_edges {
                    break;
                }
                out.push_str(&format!("  \"{}\" -> \"{}\";\n", from, to));
                edges_written += 1;
            }
            if edges_written >= max_edges {
                break;
            }

            if follow_resolved_external {
                for ext in self.external_refs_from(from) {
                    let resolved = match ext {
                        ExternalObjectEdge::Binary(b) => b
                            .resolved
                            .as_ref()
                            .map(|k| EnvironmentObjectKey::Binary(k.clone())),
                        ExternalObjectEdge::Yaml(y) => y.resolved.clone(),
                    };
                    let Some(to) = resolved else {
                        continue;
                    };
                    if edges_written >= max_edges {
                        break;
                    }
                    out.push_str(&format!("  \"{}\" -> \"{}\";\n", from, to));
                    edges_written += 1;
                }
            }
            if edges_written >= max_edges {
                break;
            }
        }

        if edges_written >= max_edges {
            out.push_str(&format!("  // truncated: max_edges={max_edges}\n"));
        }

        out.push_str("}\n");
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YamlPptrRef {
    file_id: i64,
    guid: Option<[u8; 16]>,
}

fn parse_yaml_pptr(obj: &unity_asset_core::UnityValue) -> Option<YamlPptrRef> {
    let map = obj.as_object()?;
    if map.is_empty() || map.len() > 3 {
        return None;
    }
    for k in map.keys() {
        if k != "fileID" && k != "guid" && k != "type" {
            return None;
        }
    }

    let file_id = map.get("fileID")?.as_i64()?;
    if file_id == 0 {
        return None;
    }

    let guid = map
        .get("guid")
        .and_then(|v| v.as_str())
        .and_then(super::meta_guid::parse_guid_32_hex)
        .filter(|g| *g != [0u8; 16]);

    Some(YamlPptrRef { file_id, guid })
}

fn scan_yaml_pptrs(value: &UnityValue, out: &mut Vec<YamlPptrRef>) {
    if let Some(pptr) = parse_yaml_pptr(value) {
        out.push(pptr);
        return;
    }

    match value {
        UnityValue::Array(items) => {
            for v in items {
                scan_yaml_pptrs(v, out);
            }
        }
        UnityValue::Object(map) => {
            for v in map.values() {
                scan_yaml_pptrs(v, out);
            }
        }
        _ => {}
    }
}

impl Environment {
    pub fn build_object_graph(&self, options: ObjectGraphBuildOptions) -> EnvironmentObjectGraph {
        use std::collections::{HashMap, HashSet};

        let mut nodes_set: HashSet<EnvironmentObjectKey> = HashSet::new();
        let mut internal_from: HashMap<EnvironmentObjectKey, Vec<EnvironmentObjectKey>> =
            HashMap::new();
        let mut internal_to: HashMap<EnvironmentObjectKey, Vec<EnvironmentObjectKey>> =
            HashMap::new();
        let mut external_from: HashMap<EnvironmentObjectKey, Vec<ExternalObjectEdge>> =
            HashMap::new();

        if options.include_binary {
            let graph = self.build_dependency_graph(options.binary);
            for node in graph.nodes() {
                nodes_set.insert(EnvironmentObjectKey::Binary(node.clone()));
            }

            for from in graph.nodes() {
                let from_key = EnvironmentObjectKey::Binary(from.clone());

                for to in graph.internal_refs_from(from) {
                    let to_key = EnvironmentObjectKey::Binary(to.clone());
                    internal_from
                        .entry(from_key.clone())
                        .or_default()
                        .push(to_key.clone());
                    internal_to
                        .entry(to_key)
                        .or_default()
                        .push(from_key.clone());
                }

                for ext in graph.external_refs_from(from) {
                    nodes_set.insert(from_key.clone());
                    external_from
                        .entry(from_key.clone())
                        .or_default()
                        .push(ExternalObjectEdge::Binary(ext.clone()));

                    if let Some(resolved) = &ext.resolved {
                        nodes_set.insert(EnvironmentObjectKey::Binary(resolved.clone()));
                    }
                }
            }
        }

        if options.include_yaml {
            let mut anchor_index: HashMap<PathBuf, HashMap<String, YamlObjectKey>> = HashMap::new();
            for (path, doc) in &self.yaml_documents {
                let mut map: HashMap<String, YamlObjectKey> = HashMap::new();
                for obj in doc.entries() {
                    let key = YamlObjectKey {
                        path: path.clone(),
                        anchor: obj.anchor.clone(),
                    };
                    map.insert(obj.anchor.clone(), key.clone());
                    nodes_set.insert(EnvironmentObjectKey::Yaml(key));
                }
                anchor_index.insert(path.clone(), map);
            }

            for (path, doc) in &self.yaml_documents {
                let Some(anchors) = anchor_index.get(path) else {
                    continue;
                };

                for obj in doc.entries() {
                    let from_yaml = YamlObjectKey {
                        path: path.clone(),
                        anchor: obj.anchor.clone(),
                    };
                    let from_key = EnvironmentObjectKey::Yaml(from_yaml.clone());
                    nodes_set.insert(from_key.clone());

                    let mut refs: Vec<YamlPptrRef> = Vec::new();
                    for v in obj.properties().values() {
                        scan_yaml_pptrs(v, &mut refs);
                    }

                    for r in refs {
                        if let Some(guid) = r.guid {
                            let asset_path = self.asset_path_for_guid(guid);
                            let mut resolved: Option<EnvironmentObjectKey> = None;
                            if let Some(p) = &asset_path {
                                if let Some(targets) = anchor_index.get(p) {
                                    if let Some(target) = targets.get(&r.file_id.to_string()) {
                                        resolved = Some(EnvironmentObjectKey::Yaml(target.clone()));
                                    }
                                }
                            }
                            external_from.entry(from_key.clone()).or_default().push(
                                ExternalObjectEdge::Yaml(YamlExternalEdge {
                                    from: from_yaml.clone(),
                                    file_id: r.file_id,
                                    guid: Some(guid),
                                    asset_path,
                                    resolved,
                                }),
                            );
                            continue;
                        }

                        // No GUID => treat as same-file YAML reference when possible.
                        if let Some(to_yaml) = anchors.get(&r.file_id.to_string()) {
                            let to_key = EnvironmentObjectKey::Yaml(to_yaml.clone());
                            internal_from
                                .entry(from_key.clone())
                                .or_default()
                                .push(to_key.clone());
                            internal_to
                                .entry(to_key)
                                .or_default()
                                .push(from_key.clone());
                        } else {
                            external_from.entry(from_key.clone()).or_default().push(
                                ExternalObjectEdge::Yaml(YamlExternalEdge {
                                    from: from_yaml.clone(),
                                    file_id: r.file_id,
                                    guid: None,
                                    asset_path: None,
                                    resolved: None,
                                }),
                            );
                        }
                    }
                }
            }
        }

        for v in internal_from.values_mut() {
            v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            v.dedup();
        }
        for v in internal_to.values_mut() {
            v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            v.dedup();
        }

        let mut nodes: Vec<EnvironmentObjectKey> = nodes_set.into_iter().collect();
        nodes.sort_by(|a, b| a.to_string().cmp(&b.to_string()));

        EnvironmentObjectGraph {
            nodes,
            internal_from,
            internal_to,
            external_from,
        }
    }
}
