use std::collections::HashSet;

use super::*;

/// Max top-level rows and children-per-node before truncating, so an xray over a large namespace
/// can't blow up the tree (and we always say what we dropped — no silent caps).
const XRAY_MAX_ROOTS: usize = 100;
const XRAY_MAX_CHILDREN: usize = 50;

/// A node in the relationship tree before flattening.
struct XrayNode {
    id: String,
    /// Short kind tag shown before the name (e.g. `deploy`, `rs`, `po`, `ctr`).
    kind: &'static str,
    name: String,
    /// `None` for container leaves (no status of their own).
    status: Option<String>,
    severity: Severity,
    children: Vec<XrayNode>,
}

/// A flattened, render-ready tree row: tree connectors + expand marker + node identity + status.
pub(super) struct XrayRow {
    pub id: String,
    pub connectors: String,
    pub marker: char,
    pub kind: &'static str,
    pub name: String,
    pub status: Option<String>,
    pub severity: Severity,
    pub has_children: bool,
}

impl App {
    /// Open the live `:xray` relationship graph. `namespace` None = whole cluster.
    pub(super) fn show_xray_overlay(&mut self, namespace: Option<String>) {
        self.overlay = Some(Overlay::Xray {
            namespace,
            selected: 0,
            collapsed: HashSet::new(),
        });
        self.status_line = "xray".to_string();
    }

    /// Flatten the live forest into render rows, honoring the collapsed set. Rebuilt each frame.
    pub(super) fn xray_rows(
        &self,
        namespace: Option<&str>,
        collapsed: &HashSet<String>,
    ) -> Vec<XrayRow> {
        let roots = self.xray_tree(namespace);
        let mut rows = Vec::new();
        flatten(&roots, "", true, collapsed, &mut rows);
        rows
    }

    /// Id of the expandable node under the xray cursor (None if the cursor is on a leaf).
    /// Resolved in a scoped borrow so callers can mutate the overlay afterward.
    fn xray_collapsible_id(&self) -> Option<String> {
        let Some(Overlay::Xray {
            namespace,
            selected,
            collapsed,
        }) = &self.overlay
        else {
            return None;
        };
        let rows = self.xray_rows(namespace.as_deref(), collapsed);
        rows.get(*selected)
            .filter(|row| row.has_children)
            .map(|row| row.id.clone())
    }

    /// Toggle expand/collapse of the node under the cursor (Enter).
    pub(super) fn toggle_xray_collapse(&mut self) {
        let Some(id) = self.xray_collapsible_id() else {
            self.status_line = "Nothing to expand".to_string();
            return;
        };
        if let Some(Overlay::Xray { collapsed, .. }) = &mut self.overlay {
            if collapsed.remove(&id) {
                self.status_line = "Expanded".to_string();
            } else {
                collapsed.insert(id);
                self.status_line = "Collapsed".to_string();
            }
        }
    }

    /// Collapse (`collapse=true`, Left) or expand (`collapse=false`, Right) the node under the
    /// cursor. No-op on leaves or when already in the requested state.
    pub(super) fn set_xray_collapsed(&mut self, collapse: bool) {
        let Some(id) = self.xray_collapsible_id() else {
            return;
        };
        if let Some(Overlay::Xray { collapsed, .. }) = &mut self.overlay {
            if collapse {
                if collapsed.insert(id) {
                    self.status_line = "Collapsed".to_string();
                }
            } else if collapsed.remove(&id) {
                self.status_line = "Expanded".to_string();
            }
        }
    }

    /// Namespace-rooted ownership forest: ns → controllers → pods → containers.
    fn xray_tree(&self, namespace: Option<&str>) -> Vec<XrayNode> {
        let context = self.current_tab().context.clone();
        let namespaces: Vec<String> = match namespace {
            Some(ns) => vec![ns.to_string()],
            None => {
                let mut all = self.store.namespaces(&context);
                all.sort();
                all
            }
        };
        let total_ns = namespaces.len();
        let mut roots: Vec<XrayNode> = namespaces
            .into_iter()
            .take(XRAY_MAX_ROOTS)
            .map(|ns| {
                let children = self.namespace_children(&context, &ns);
                XrayNode {
                    id: format!("ns:{ns}"),
                    kind: "ns",
                    name: ns,
                    status: None,
                    severity: Severity::Ok,
                    children,
                }
            })
            .collect();
        if total_ns > roots.len() {
            roots.push(more_node("namespaces", total_ns - roots.len()));
        }
        roots
    }

    /// Top-level controllers (and standalone replicasets/pods) for one namespace.
    fn namespace_children(&self, context: &str, ns: &str) -> Vec<XrayNode> {
        let ns_opt = Some(ns);
        let mut nodes = Vec::new();

        // Deployments → ReplicaSets → pods → containers.
        for dep in self.store.list(context, ResourceKind::Deployments, ns_opt) {
            let mut node = self.xray_node("deploy", &dep.key, Some(&dep.status));
            node.children = self
                .deployment_replica_set_keys(&dep.key)
                .into_iter()
                .map(|rs_key| {
                    let mut rs = self.xray_node("rs", &rs_key, Some(&self.entity_status(&rs_key)));
                    rs.children = self.pod_nodes(self.replica_set_pod_keys(&rs_key));
                    rs
                })
                .collect();
            nodes.push(node);
        }
        // StatefulSets / DaemonSets → pods.
        for (kind, owner) in [
            (ResourceKind::StatefulSets, "StatefulSet"),
            (ResourceKind::DaemonSets, "DaemonSet"),
        ] {
            for ctrl in self.store.list(context, kind, ns_opt) {
                let mut node = self.xray_node(kind.short_name(), &ctrl.key, Some(&ctrl.status));
                node.children = self.pod_nodes(self.owned_pod_keys(&ctrl.key, owner));
                nodes.push(node);
            }
        }
        // CronJobs → Jobs → pods.
        for cj in self.store.list(context, ResourceKind::CronJobs, ns_opt) {
            let mut node = self.xray_node("cj", &cj.key, Some(&cj.status));
            node.children = self
                .store
                .list(context, ResourceKind::Jobs, ns_opt)
                .into_iter()
                .filter(|job| job.extracted.owned_by("CronJob", &cj.key.name))
                .map(|job| self.job_node(job))
                .collect();
            nodes.push(node);
        }
        // Jobs not owned by a CronJob.
        for job in self.store.list(context, ResourceKind::Jobs, ns_opt) {
            if !job.extracted.owners.iter().any(|o| o.kind == "CronJob") {
                nodes.push(self.job_node(job));
            }
        }
        // ReplicaSets not owned by a Deployment (bare RS).
        for rs in self.store.list(context, ResourceKind::ReplicaSets, ns_opt) {
            if !rs.extracted.owners.iter().any(|o| o.kind == "Deployment") {
                let mut node = self.xray_node("rs", &rs.key, Some(&rs.status));
                node.children = self.pod_nodes(self.replica_set_pod_keys(&rs.key));
                nodes.push(node);
            }
        }
        // Standalone pods (no controller).
        let standalone: Vec<ResourceKey> = self
            .store
            .list(context, ResourceKind::Pods, ns_opt)
            .into_iter()
            .filter(|pod| pod.extracted.owners.is_empty())
            .map(|pod| pod.key.clone())
            .collect();
        nodes.extend(self.pod_nodes(standalone));

        nodes
    }

    fn job_node(&self, job: &crate::model::ResourceEntity) -> XrayNode {
        let mut node = self.xray_node("job", &job.key, Some(&job.status));
        node.children = self.pod_nodes(self.owned_pod_keys(&job.key, "Job"));
        node
    }

    /// Pod nodes (capped) with their container leaves.
    fn pod_nodes(&self, pod_keys: Vec<ResourceKey>) -> Vec<XrayNode> {
        let total = pod_keys.len();
        let mut nodes: Vec<XrayNode> = pod_keys
            .into_iter()
            .take(XRAY_MAX_CHILDREN)
            .map(|pod_key| {
                let mut node = self.xray_node("po", &pod_key, Some(&self.entity_status(&pod_key)));
                node.children = self.container_nodes(&pod_key);
                node
            })
            .collect();
        if total > nodes.len() {
            nodes.push(more_node("po", total - nodes.len()));
        }
        nodes
    }

    fn container_nodes(&self, pod_key: &ResourceKey) -> Vec<XrayNode> {
        self.store
            .get(pod_key)
            .map(|entity| {
                entity
                    .extracted
                    .containers
                    .iter()
                    .map(|name| XrayNode {
                        id: format!("ctr:{}:{name}", pod_key.name),
                        kind: "ctr",
                        name: name.clone(),
                        status: None,
                        severity: Severity::Ok,
                        children: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn xray_node(&self, kind: &'static str, key: &ResourceKey, status: Option<&str>) -> XrayNode {
        let severity = status.map(classify_status_severity).unwrap_or(Severity::Ok);
        XrayNode {
            id: format!(
                "{kind}:{}:{}",
                key.namespace.as_deref().unwrap_or("-"),
                key.name
            ),
            kind,
            name: key.name.clone(),
            status: status.map(str::to_string),
            severity,
            children: Vec::new(),
        }
    }

    fn entity_status(&self, key: &ResourceKey) -> String {
        self.store
            .get(key)
            .map(|e| e.status.clone())
            .unwrap_or_else(|| "-".to_string())
    }

    /// Pods directly owned by a `StatefulSet`/`DaemonSet`.
    fn owned_pod_keys(&self, owner_key: &ResourceKey, owner_kind: &str) -> Vec<ResourceKey> {
        let mut pods: Vec<ResourceKey> = self
            .store
            .list(
                &owner_key.context,
                ResourceKind::Pods,
                owner_key.namespace.as_deref(),
            )
            .into_iter()
            .filter(|pod| pod.extracted.owned_by(owner_kind, &owner_key.name))
            .map(|pod| pod.key.clone())
            .collect();
        pods.sort_by(|a, b| a.name.cmp(&b.name));
        pods
    }
}

/// A synthetic `… N more` row (its own node so it slots into the tree like a sibling).
fn more_node(kind: &'static str, n: usize) -> XrayNode {
    XrayNode {
        id: format!("more:{kind}"),
        kind: "",
        name: format!("… {n} more {kind}"),
        status: None,
        severity: Severity::Ok,
        children: Vec::new(),
    }
}

/// Depth-first flatten with box-drawing connectors. Roots sit flush-left; descendants get
/// `├──`/`└──` branch glyphs and `│` continuation bars for ancestors that still have siblings.
fn flatten(
    nodes: &[XrayNode],
    prefix: &str,
    is_root: bool,
    collapsed: &HashSet<String>,
    rows: &mut Vec<XrayRow>,
) {
    let n = nodes.len();
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i + 1 == n;
        let (connectors, child_prefix) = if is_root {
            (String::new(), String::new())
        } else {
            let glyph = if is_last { "└─ " } else { "├─ " };
            let cont = if is_last { "   " } else { "│  " };
            (format!("{prefix}{glyph}"), format!("{prefix}{cont}"))
        };
        let has_children = !node.children.is_empty();
        let is_collapsed = collapsed.contains(&node.id);
        let marker = if has_children {
            if is_collapsed { '▸' } else { '▾' }
        } else {
            ' '
        };
        rows.push(XrayRow {
            id: node.id.clone(),
            connectors,
            marker,
            kind: node.kind,
            name: node.name.clone(),
            status: node.status.clone(),
            severity: node.severity,
            has_children,
        });
        if has_children && !is_collapsed {
            flatten(&node.children, &child_prefix, false, collapsed, rows);
        }
    }
}
