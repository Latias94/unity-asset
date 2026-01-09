use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CachedScanEntry {
    Value(Option<CachedPptrScan>),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CachedPptrScan {
    pub internal: Vec<i64>,
    pub external: Vec<(i32, i64)>,
}

pub(crate) type DependencyScanCache = std::collections::HashMap<BinaryObjectKey, CachedScanEntry>;

/// Build options for `Environment` dependency graph extraction.
#[derive(Debug, Clone, Copy)]
pub struct DependencyGraphBuildOptions {
    /// Include objects that cannot be scanned due to missing TypeTree.
    ///
    /// These objects are still included as nodes either way; this flag controls whether we keep a
    /// warning record for "no typetree" objects.
    pub include_no_typetree_warnings: bool,
    /// Continue building the graph even if some objects fail to scan.
    pub continue_on_error: bool,
    /// Maximum number of objects to scan across all sources.
    pub max_objects: Option<usize>,
}

impl Default for DependencyGraphBuildOptions {
    fn default() -> Self {
        Self {
            include_no_typetree_warnings: false,
            continue_on_error: true,
            max_objects: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyGraphWarning {
    pub key: BinaryObjectKey,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDependencyEdge {
    pub from: BinaryObjectKey,
    pub target: unity_asset_binary::metadata::ExternalObjectRef,
    pub resolved: Option<BinaryObjectKey>,
}

/// A best-effort dependency graph across all loaded binary sources in an `Environment`.
///
/// - Nodes are `BinaryObjectKey` (globally unique within the loaded environment).
/// - Internal edges point to other objects in the same `SerializedFile`.
/// - External edges keep the original `fileID/pathID` pair and optionally a resolved `BinaryObjectKey`.
#[derive(Debug, Clone)]
pub struct EnvironmentDependencyGraph {
    nodes: Vec<BinaryObjectKey>,
    internal_from: std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>>,
    internal_to: std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>>,
    external_from: std::collections::HashMap<BinaryObjectKey, Vec<ExternalDependencyEdge>>,
    warnings: Vec<DependencyGraphWarning>,
}

#[derive(Debug, Clone, Copy)]
pub struct DependencyGraphTraversalOptions {
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    /// Follow `ExternalDependencyEdge.resolved` when present.
    pub follow_resolved_external: bool,
}

impl Default for DependencyGraphTraversalOptions {
    fn default() -> Self {
        Self {
            max_depth: None,
            max_nodes: None,
            follow_resolved_external: false,
        }
    }
}

impl EnvironmentDependencyGraph {
    pub fn nodes(&self) -> &[BinaryObjectKey] {
        &self.nodes
    }

    pub fn internal_edge_count(&self) -> usize {
        self.internal_from.values().map(|v| v.len()).sum()
    }

    pub fn external_edge_count(&self) -> usize {
        self.external_from.values().map(|v| v.len()).sum()
    }

    pub fn resolved_external_edge_count(&self) -> usize {
        self.external_from
            .values()
            .map(|v| v.iter().filter(|e| e.resolved.is_some()).count())
            .sum()
    }

    pub fn warnings(&self) -> &[DependencyGraphWarning] {
        &self.warnings
    }

    pub fn internal_refs_from(&self, key: &BinaryObjectKey) -> &[BinaryObjectKey] {
        self.internal_from
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn internal_refs_to(&self, key: &BinaryObjectKey) -> &[BinaryObjectKey] {
        self.internal_to
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn external_refs_from(&self, key: &BinaryObjectKey) -> &[ExternalDependencyEdge] {
        self.external_from
            .get(key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn neighbors_from(
        &self,
        key: &BinaryObjectKey,
        follow_resolved_external: bool,
    ) -> Vec<BinaryObjectKey> {
        let mut out: Vec<BinaryObjectKey> = Vec::new();
        out.extend(self.internal_refs_from(key).iter().cloned());
        if follow_resolved_external {
            out.extend(
                self.external_refs_from(key)
                    .iter()
                    .filter_map(|e| e.resolved.clone()),
            );
        }
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out.dedup();
        out
    }

    pub fn roots(&self, follow_resolved_external: bool) -> Vec<BinaryObjectKey> {
        let incoming = self.incoming_map(follow_resolved_external);
        let mut out: Vec<BinaryObjectKey> = self
            .nodes
            .iter()
            .filter(|k| incoming.get(*k).map(|v| v.is_empty()).unwrap_or(true))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    pub fn leaves(&self, follow_resolved_external: bool) -> Vec<BinaryObjectKey> {
        let mut out: Vec<BinaryObjectKey> = Vec::new();
        for k in &self.nodes {
            let deg_internal = self.internal_refs_from(k).len();
            let deg_external = if follow_resolved_external {
                self.external_refs_from(k)
                    .iter()
                    .filter(|e| e.resolved.is_some())
                    .count()
            } else {
                0
            };
            if deg_internal + deg_external == 0 {
                out.push(k.clone());
            }
        }
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    /// Return strongly-connected components (SCCs) that represent cycles.
    ///
    /// A component is considered a cycle if it contains 2+ nodes, or it is a self-loop.
    pub fn cycles(
        &self,
        max_components: usize,
        follow_resolved_external: bool,
    ) -> Vec<Vec<BinaryObjectKey>> {
        let sccs = self.strongly_connected_components(follow_resolved_external);

        let mut out: Vec<Vec<BinaryObjectKey>> = Vec::new();
        for mut comp in sccs {
            if comp.is_empty() {
                continue;
            }

            if comp.len() == 1 {
                let node = &comp[0];
                let has_self_loop = self
                    .neighbors_from(node, follow_resolved_external)
                    .iter()
                    .any(|n| n == node);
                if !has_self_loop {
                    continue;
                }
            }

            comp.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            out.push(comp);
            if out.len() >= max_components {
                break;
            }
        }

        out.sort_by(|a, b| b.len().cmp(&a.len()));
        out
    }

    /// Compute the internal dependency closure (reachable set) from a set of root nodes.
    pub fn internal_closure(
        &self,
        roots: &[BinaryObjectKey],
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Vec<BinaryObjectKey> {
        self.closure_with_options(
            roots,
            DependencyGraphTraversalOptions {
                max_depth,
                max_nodes,
                follow_resolved_external: false,
            },
        )
    }

    pub fn closure_with_options(
        &self,
        roots: &[BinaryObjectKey],
        options: DependencyGraphTraversalOptions,
    ) -> Vec<BinaryObjectKey> {
        use std::collections::{HashSet, VecDeque};

        let mut visited: HashSet<BinaryObjectKey> = HashSet::new();
        let mut queue: VecDeque<(BinaryObjectKey, usize)> = VecDeque::new();

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

        let mut out: Vec<BinaryObjectKey> = visited.into_iter().collect();
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        out
    }

    pub fn to_dot(&self, max_edges: usize, follow_resolved_external: bool) -> String {
        let mut out = String::new();
        out.push_str("digraph unity_asset_deps {\n");

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
                    let Some(to) = &ext.resolved else {
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

    fn incoming_map(
        &self,
        follow_resolved_external: bool,
    ) -> std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>> {
        let mut incoming: std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>> =
            std::collections::HashMap::new();

        for node in &self.nodes {
            incoming.entry(node.clone()).or_default();
        }

        for (to, froms) in &self.internal_to {
            incoming
                .entry(to.clone())
                .or_default()
                .extend(froms.iter().cloned());
        }

        if follow_resolved_external {
            for (from, edges) in &self.external_from {
                for edge in edges {
                    let Some(to) = &edge.resolved else {
                        continue;
                    };
                    incoming.entry(to.clone()).or_default().push(from.clone());
                }
            }
        }

        for v in incoming.values_mut() {
            v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            v.dedup();
        }

        incoming
    }

    fn strongly_connected_components(
        &self,
        follow_resolved_external: bool,
    ) -> Vec<Vec<BinaryObjectKey>> {
        // Tarjan SCC algorithm.
        use std::collections::HashMap;

        let mut index: usize = 0;
        let mut stack: Vec<BinaryObjectKey> = Vec::new();
        let mut on_stack: HashMap<BinaryObjectKey, bool> = HashMap::new();
        let mut indices: HashMap<BinaryObjectKey, usize> = HashMap::new();
        let mut lowlink: HashMap<BinaryObjectKey, usize> = HashMap::new();
        let mut out: Vec<Vec<BinaryObjectKey>> = Vec::new();

        fn strongconnect(
            graph: &EnvironmentDependencyGraph,
            v: BinaryObjectKey,
            follow_resolved_external: bool,
            index: &mut usize,
            stack: &mut Vec<BinaryObjectKey>,
            on_stack: &mut HashMap<BinaryObjectKey, bool>,
            indices: &mut HashMap<BinaryObjectKey, usize>,
            lowlink: &mut HashMap<BinaryObjectKey, usize>,
            out: &mut Vec<Vec<BinaryObjectKey>>,
        ) {
            indices.insert(v.clone(), *index);
            lowlink.insert(v.clone(), *index);
            *index = index.saturating_add(1);

            stack.push(v.clone());
            on_stack.insert(v.clone(), true);

            for w in graph.neighbors_from(&v, follow_resolved_external) {
                if !indices.contains_key(&w) {
                    strongconnect(
                        graph,
                        w.clone(),
                        follow_resolved_external,
                        index,
                        stack,
                        on_stack,
                        indices,
                        lowlink,
                        out,
                    );
                    let lw_v = *lowlink.get(&v).unwrap_or(&0);
                    let lw_w = *lowlink.get(&w).unwrap_or(&0);
                    let next = lw_v.min(lw_w);
                    lowlink.insert(v.clone(), next);
                } else if *on_stack.get(&w).unwrap_or(&false) {
                    let lw_v = *lowlink.get(&v).unwrap_or(&0);
                    let idx_w = *indices.get(&w).unwrap_or(&lw_v);
                    lowlink.insert(v.clone(), lw_v.min(idx_w));
                }
            }

            let lw_v = *lowlink.get(&v).unwrap_or(&0);
            let idx_v = *indices.get(&v).unwrap_or(&usize::MAX);
            if lw_v == idx_v {
                let mut comp = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack.insert(w.clone(), false);
                    comp.push(w.clone());
                    if w == v {
                        break;
                    }
                }
                out.push(comp);
            }
        }

        for node in &self.nodes {
            if indices.contains_key(node) {
                continue;
            }
            strongconnect(
                self,
                node.clone(),
                follow_resolved_external,
                &mut index,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlink,
                &mut out,
            );
        }

        out
    }
}

impl Environment {
    pub fn invalidate_dependency_scan_cache(&self) {
        match self.dependency_scan_cache.write() {
            Ok(mut cache) => cache.clear(),
            Err(e) => e.into_inner().clear(),
        }
    }

    pub fn invalidate_dependency_scan_cache_for_source(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        asset_index: Option<usize>,
    ) {
        let mut keys: Vec<BinaryObjectKey> = Vec::new();
        match self.dependency_scan_cache.read() {
            Ok(cache) => {
                for k in cache.keys() {
                    let matches_asset_index = match asset_index {
                        Some(idx) => k.asset_index == Some(idx),
                        None => true,
                    };
                    if &k.source == source && k.source_kind == source_kind && matches_asset_index {
                        keys.push(k.clone());
                    }
                }
            }
            Err(e) => {
                let cache = e.into_inner();
                for k in cache.keys() {
                    let matches_asset_index = match asset_index {
                        Some(idx) => k.asset_index == Some(idx),
                        None => true,
                    };
                    if &k.source == source && k.source_kind == source_kind && matches_asset_index {
                        keys.push(k.clone());
                    }
                }
            }
        }

        if keys.is_empty() {
            return;
        }

        match self.dependency_scan_cache.write() {
            Ok(mut cache) => {
                for k in keys {
                    cache.remove(&k);
                }
            }
            Err(e) => {
                let mut cache = e.into_inner();
                for k in keys {
                    cache.remove(&k);
                }
            }
        }
    }

    pub fn build_dependency_graph_for_source(
        &self,
        source: &BinarySource,
        source_kind: BinarySourceKind,
        asset_index: Option<usize>,
        options: DependencyGraphBuildOptions,
    ) -> Result<EnvironmentDependencyGraph> {
        let file = match source_kind {
            BinarySourceKind::SerializedFile => {
                self.binary_assets.get(source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "SerializedFile not loaded: {}",
                        source.describe()
                    ))
                })?
            }
            BinarySourceKind::AssetBundle => {
                let idx = asset_index.ok_or_else(|| {
                    UnityAssetError::format(
                        "asset_index is required for bundle sources".to_string(),
                    )
                })?;
                let bundle = self.bundles.get(source).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle not loaded: {}",
                        source.describe()
                    ))
                })?;
                bundle.assets.get(idx).ok_or_else(|| {
                    UnityAssetError::format(format!(
                        "AssetBundle asset_index out of range: {} idx={}",
                        source.describe(),
                        idx
                    ))
                })?
            }
        };

        Ok(self.build_dependency_graph_from_files(
            vec![(source, source_kind, asset_index, file)],
            options,
        ))
    }

    fn dependency_scan_cached(&self, obj_ref: &BinaryObjectRef<'_>) -> CachedScanEntry {
        let key = obj_ref.key();
        match self.dependency_scan_cache.read() {
            Ok(cache) => {
                if let Some(v) = cache.get(&key) {
                    return v.clone();
                }
            }
            Err(e) => {
                let cache = e.into_inner();
                if let Some(v) = cache.get(&key) {
                    return v.clone();
                }
            }
        }

        let computed = match obj_ref.object.scan_pptrs() {
            Ok(Some(mut scan)) => {
                scan.internal.sort_unstable();
                scan.internal.dedup();
                scan.external.sort_unstable();
                scan.external.dedup();
                CachedScanEntry::Value(Some(CachedPptrScan {
                    internal: scan.internal,
                    external: scan.external,
                }))
            }
            Ok(None) => CachedScanEntry::Value(None),
            Err(e) => CachedScanEntry::Error(format!("scan_pptrs failed: {}", e)),
        };

        match self.dependency_scan_cache.write() {
            Ok(mut cache) => {
                cache.insert(key, computed.clone());
            }
            Err(e) => {
                e.into_inner().insert(key, computed.clone());
            }
        }

        computed
    }

    pub fn build_dependency_graph(
        &self,
        options: DependencyGraphBuildOptions,
    ) -> EnvironmentDependencyGraph {
        let mut sources: Vec<(
            &BinarySource,
            BinarySourceKind,
            Option<usize>,
            &SerializedFile,
        )> = Vec::new();
        for (source, file) in &self.binary_assets {
            sources.push((source, BinarySourceKind::SerializedFile, None, file));
        }
        for (bundle_source, bundle) in &self.bundles {
            for (asset_index, file) in bundle.assets.iter().enumerate() {
                sources.push((
                    bundle_source,
                    BinarySourceKind::AssetBundle,
                    Some(asset_index),
                    file,
                ));
            }
        }

        self.build_dependency_graph_from_files(sources, options)
    }

    fn build_dependency_graph_from_files(
        &self,
        mut sources: Vec<(
            &BinarySource,
            BinarySourceKind,
            Option<usize>,
            &SerializedFile,
        )>,
        options: DependencyGraphBuildOptions,
    ) -> EnvironmentDependencyGraph {
        let mut nodes: Vec<BinaryObjectKey> = Vec::new();
        let mut internal_from: std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>> =
            std::collections::HashMap::new();
        let mut internal_to: std::collections::HashMap<BinaryObjectKey, Vec<BinaryObjectKey>> =
            std::collections::HashMap::new();
        let mut external_from: std::collections::HashMap<
            BinaryObjectKey,
            Vec<ExternalDependencyEdge>,
        > = std::collections::HashMap::new();
        let mut warnings: Vec<DependencyGraphWarning> = Vec::new();

        let mut remaining = options.max_objects.unwrap_or(usize::MAX);

        sources.sort_by(|a, b| {
            let ak = (a.0.describe(), a.1 as u8, a.2.unwrap_or(usize::MAX));
            let bk = (b.0.describe(), b.1 as u8, b.2.unwrap_or(usize::MAX));
            ak.cmp(&bk)
        });

        for (source, source_kind, asset_index, file) in sources {
            if remaining == 0 {
                break;
            }

            let mut path_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
            path_ids.extend(file.objects.iter().map(|o| o.path_id));

            for handle in file.object_handles() {
                if remaining == 0 {
                    break;
                }

                let from_key = BinaryObjectKey {
                    source: source.clone(),
                    source_kind,
                    asset_index,
                    path_id: handle.path_id(),
                };
                nodes.push(from_key.clone());

                let obj_ref = BinaryObjectRef {
                    source,
                    source_kind,
                    asset_index,
                    object: handle,
                    typetree_options: self.options.typetree,
                    reporter: self.reporter.clone(),
                };

                let scan_entry = self.dependency_scan_cached(&obj_ref);
                let scan = match scan_entry {
                    CachedScanEntry::Value(Some(v)) => v,
                    CachedScanEntry::Value(None) => {
                        if options.include_no_typetree_warnings {
                            warnings.push(DependencyGraphWarning {
                                key: from_key,
                                error: "scan_pptrs unavailable (missing TypeTree)".to_string(),
                            });
                        }
                        remaining = remaining.saturating_sub(1);
                        continue;
                    }
                    CachedScanEntry::Error(e) => {
                        if options.continue_on_error {
                            warnings.push(DependencyGraphWarning {
                                key: from_key,
                                error: e,
                            });
                            remaining = remaining.saturating_sub(1);
                            continue;
                        }
                        warnings.push(DependencyGraphWarning {
                            key: from_key,
                            error: e,
                        });
                        break;
                    }
                };

                // Internal edges (fileID=0).
                for to_path_id in scan.internal {
                    if to_path_id == 0 {
                        continue;
                    }
                    if !path_ids.contains(&to_path_id) {
                        continue;
                    }

                    let to_key = BinaryObjectKey {
                        source: source.clone(),
                        source_kind,
                        asset_index,
                        path_id: to_path_id,
                    };

                    internal_from
                        .entry(from_key.clone())
                        .or_default()
                        .push(to_key.clone());
                    internal_to
                        .entry(to_key)
                        .or_default()
                        .push(from_key.clone());
                }

                // External edges (fileID>0).
                for (file_id, path_id) in scan.external {
                    if path_id == 0 {
                        continue;
                    }

                    let mut file_path: Option<String> = None;
                    let mut guid: Option<[u8; 16]> = None;
                    if file_id > 0 {
                        let idx = usize::try_from(file_id - 1).ok().unwrap_or(usize::MAX);
                        if let Some(ext) = obj_ref.object.file().externals.get(idx) {
                            file_path = if ext.path.is_empty() {
                                None
                            } else {
                                Some(ext.path.clone())
                            };
                            guid = if ext.guid == [0u8; 16] {
                                None
                            } else {
                                Some(ext.guid)
                            };
                        }
                    }

                    let target = unity_asset_binary::metadata::ExternalObjectRef {
                        file_id,
                        path_id,
                        file_path,
                        guid,
                    };
                    let resolved = self.resolve_binary_pptr(&obj_ref, file_id, path_id);

                    external_from.entry(from_key.clone()).or_default().push(
                        ExternalDependencyEdge {
                            from: from_key.clone(),
                            target,
                            resolved,
                        },
                    );
                }

                remaining = remaining.saturating_sub(1);
            }
        }

        // Deduplicate and stabilize.
        nodes.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        nodes.dedup();

        for v in internal_from.values_mut() {
            v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            v.dedup();
        }
        for v in internal_to.values_mut() {
            v.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            v.dedup();
        }
        for v in external_from.values_mut() {
            v.sort_by(|a, b| {
                let ak = (
                    a.target.file_id,
                    a.target.path_id,
                    a.resolved.as_ref().map(|k| k.to_string()),
                );
                let bk = (
                    b.target.file_id,
                    b.target.path_id,
                    b.resolved.as_ref().map(|k| k.to_string()),
                );
                ak.cmp(&bk)
            });
            v.dedup_by(|a, b| {
                a.target.file_id == b.target.file_id
                    && a.target.path_id == b.target.path_id
                    && a.resolved == b.resolved
            });
        }

        EnvironmentDependencyGraph {
            nodes,
            internal_from,
            internal_to,
            external_from,
            warnings,
        }
    }
}
