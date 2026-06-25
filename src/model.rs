use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use crossterm::event::KeyEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResourceKind {
    Pods,
    Deployments,
    ReplicaSets,
    StatefulSets,
    DaemonSets,
    Services,
    Ingresses,
    ConfigMaps,
    Secrets,
    Jobs,
    CronJobs,
    PersistentVolumeClaims,
    PersistentVolumes,
    Nodes,
    Namespaces,
    Events,
    ServiceAccounts,
    Roles,
    RoleBindings,
    ClusterRoles,
    ClusterRoleBindings,
    NetworkPolicies,
    HorizontalPodAutoscalers,
    PodDisruptionBudgets,
}

impl ResourceKind {
    pub const ORDERED: [ResourceKind; 24] = [
        ResourceKind::Pods,
        ResourceKind::Deployments,
        ResourceKind::ReplicaSets,
        ResourceKind::StatefulSets,
        ResourceKind::DaemonSets,
        ResourceKind::Services,
        ResourceKind::Ingresses,
        ResourceKind::ConfigMaps,
        ResourceKind::Secrets,
        ResourceKind::Jobs,
        ResourceKind::CronJobs,
        ResourceKind::PersistentVolumeClaims,
        ResourceKind::PersistentVolumes,
        ResourceKind::Nodes,
        ResourceKind::Namespaces,
        ResourceKind::Events,
        ResourceKind::ServiceAccounts,
        ResourceKind::Roles,
        ResourceKind::RoleBindings,
        ResourceKind::ClusterRoles,
        ResourceKind::ClusterRoleBindings,
        ResourceKind::NetworkPolicies,
        ResourceKind::HorizontalPodAutoscalers,
        ResourceKind::PodDisruptionBudgets,
    ];

    pub fn short_name(self) -> &'static str {
        match self {
            ResourceKind::Pods => "po",
            ResourceKind::Deployments => "deploy",
            ResourceKind::ReplicaSets => "rs",
            ResourceKind::StatefulSets => "sts",
            ResourceKind::DaemonSets => "ds",
            ResourceKind::Services => "svc",
            ResourceKind::Ingresses => "ing",
            ResourceKind::ConfigMaps => "cm",
            ResourceKind::Secrets => "secret",
            ResourceKind::Jobs => "job",
            ResourceKind::CronJobs => "cj",
            ResourceKind::PersistentVolumeClaims => "pvc",
            ResourceKind::PersistentVolumes => "pv",
            ResourceKind::Nodes => "no",
            ResourceKind::Namespaces => "ns",
            ResourceKind::Events => "ev",
            ResourceKind::ServiceAccounts => "sa",
            ResourceKind::Roles => "role",
            ResourceKind::RoleBindings => "rb",
            ResourceKind::ClusterRoles => "crole",
            ResourceKind::ClusterRoleBindings => "crb",
            ResourceKind::NetworkPolicies => "netpol",
            ResourceKind::HorizontalPodAutoscalers => "hpa",
            ResourceKind::PodDisruptionBudgets => "pdb",
        }
    }

    pub fn is_namespaced(self) -> bool {
        !matches!(
            self,
            ResourceKind::PersistentVolumes
                | ResourceKind::Nodes
                | ResourceKind::Namespaces
                | ResourceKind::ClusterRoles
                | ResourceKind::ClusterRoleBindings
        )
    }

    /// Kind-specific table columns shown after the universal Namespace/Name/Status/Age.
    ///
    /// This is the single source of truth for both the table header (labels + widths) and the
    /// per-row values: `extract_columns` in the cluster provider MUST emit exactly these values,
    /// in this order, for the same kind. The
    /// `extract_columns_matches_declared_header_count_for_every_kind` test enforces the count.
    /// Kinds with no meaningful extra columns return `&[]` and render with just the four
    /// universal columns.
    pub fn extra_columns(self) -> &'static [ExtraColumn] {
        use ExtraColumn as C;
        const DEPLOY: &[ExtraColumn] = &[C::new("Up-to-date", 11), C::new("Available", 10)];
        const RS: &[ExtraColumn] = &[
            C::new("Desired", 8),
            C::new("Current", 8),
            C::new("Ready", 7),
        ];
        const STS: &[ExtraColumn] = &[C::new("Ready", 8)];
        const DS: &[ExtraColumn] = &[C::new("Desired", 8), C::new("Ready", 7), C::new("Avail", 7)];
        const SVC: &[ExtraColumn] = &[
            C::new("Type", 13),
            C::new("Cluster-IP", 16),
            C::new("Ports", 22),
        ];
        const ING: &[ExtraColumn] = &[
            C::new("Class", 12),
            C::new("Hosts", 30),
            C::new("Address", 20),
        ];
        const JOB: &[ExtraColumn] = &[C::new("Completions", 12), C::new("Duration", 10)];
        const CJ: &[ExtraColumn] = &[
            C::new("Schedule", 14),
            C::new("Suspend", 8),
            C::new("Active", 7),
        ];
        const CM: &[ExtraColumn] = &[C::new("Data", 6)];
        const SECRET: &[ExtraColumn] = &[C::new("Type", 30), C::new("Data", 6)];
        const NODE: &[ExtraColumn] = &[C::new("Roles", 20), C::new("Version", 16)];
        const PVC: &[ExtraColumn] = &[
            C::new("Volume", 22),
            C::new("Capacity", 10),
            C::new("Access", 10),
            C::new("StorageClass", 16),
        ];
        const PV: &[ExtraColumn] = &[
            C::new("Capacity", 10),
            C::new("Access", 10),
            C::new("Reclaim", 10),
            C::new("Claim", 26),
            C::new("StorageClass", 16),
        ];
        const HPA: &[ExtraColumn] = &[
            C::new("Reference", 26),
            C::new("Min Pods", 8),
            C::new("Max Pods", 8),
            C::new("Replicas", 9),
        ];
        const SA: &[ExtraColumn] = &[C::new("Secrets", 8), C::new("Pull-secrets", 13)];
        const BINDING: &[ExtraColumn] = &[C::new("Role", 26), C::new("Subjects", 34)];
        const PDB: &[ExtraColumn] = &[
            C::new("Min-avail", 10),
            C::new("Max-unavail", 12),
            C::new("Allowed", 8),
        ];
        match self {
            ResourceKind::Deployments => DEPLOY,
            ResourceKind::ReplicaSets => RS,
            ResourceKind::StatefulSets => STS,
            ResourceKind::DaemonSets => DS,
            ResourceKind::Services => SVC,
            ResourceKind::Ingresses => ING,
            ResourceKind::Jobs => JOB,
            ResourceKind::CronJobs => CJ,
            ResourceKind::ConfigMaps => CM,
            ResourceKind::Secrets => SECRET,
            ResourceKind::Nodes => NODE,
            ResourceKind::PersistentVolumeClaims => PVC,
            ResourceKind::PersistentVolumes => PV,
            ResourceKind::HorizontalPodAutoscalers => HPA,
            ResourceKind::ServiceAccounts => SA,
            ResourceKind::RoleBindings | ResourceKind::ClusterRoleBindings => BINDING,
            ResourceKind::PodDisruptionBudgets => PDB,
            _ => &[],
        }
    }
}

/// A kind-specific table column appended after the universal Namespace/Name/Status/Age columns.
/// See [`ResourceKind::extra_columns`] for the header/value contract.
#[derive(Debug, Clone, Copy)]
pub struct ExtraColumn {
    pub header: &'static str,
    /// Fixed display width in cells.
    pub width: u16,
}

impl ExtraColumn {
    pub const fn new(header: &'static str, width: u16) -> Self {
        ExtraColumn { header, width }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            ResourceKind::Pods => "Pods",
            ResourceKind::Deployments => "Deployments",
            ResourceKind::ReplicaSets => "ReplicaSets",
            ResourceKind::StatefulSets => "StatefulSets",
            ResourceKind::DaemonSets => "DaemonSets",
            ResourceKind::Services => "Services",
            ResourceKind::Ingresses => "Ingresses",
            ResourceKind::ConfigMaps => "ConfigMaps",
            ResourceKind::Secrets => "Secrets",
            ResourceKind::Jobs => "Jobs",
            ResourceKind::CronJobs => "CronJobs",
            ResourceKind::PersistentVolumeClaims => "PVCs",
            ResourceKind::PersistentVolumes => "PVs",
            ResourceKind::Nodes => "Nodes",
            ResourceKind::Namespaces => "Namespaces",
            ResourceKind::Events => "Events",
            ResourceKind::ServiceAccounts => "ServiceAccounts",
            ResourceKind::Roles => "Roles",
            ResourceKind::RoleBindings => "RoleBindings",
            ResourceKind::ClusterRoles => "ClusterRoles",
            ResourceKind::ClusterRoleBindings => "ClusterRoleBindings",
            ResourceKind::NetworkPolicies => "NetworkPolicies",
            ResourceKind::HorizontalPodAutoscalers => "HPAs",
            ResourceKind::PodDisruptionBudgets => "PDBs",
        };
        write!(f, "{text}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceKey {
    pub context: String,
    pub kind: ResourceKind,
    pub namespace: Option<String>,
    pub name: String,
}

impl ResourceKey {
    pub fn new(
        context: impl Into<String>,
        kind: ResourceKind,
        namespace: Option<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            context: context.into(),
            kind,
            namespace,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEntity {
    pub key: ResourceKey,
    pub status: String,
    pub age: Option<DateTime<Utc>>,
    pub labels: Vec<(String, String)>,
    /// Kind-specific column values, in the order declared by [`ResourceKind::extra_columns`].
    /// Empty for kinds (and pods, which render live metric columns) that have no extra columns.
    pub columns: Vec<String>,
    /// Compact fields extracted once at ingest for the all-rows hot paths (pulse aggregates,
    /// log fan-in owner mapping, container pickers). The full object is fetched on demand for
    /// detail/describe/edit via `ResourceProvider::get_object`, never retained per row.
    pub extracted: Extracted,
}

/// Small, fixed-size per-row payload. Replaces retaining the full object JSON for every entity.
/// Most fields are empty for kinds that don't need them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Extracted {
    /// Pod's scheduled node (pods only).
    pub node_name: Option<String>,
    /// Pod IP (pods only) — shown in the pods table.
    pub pod_ip: Option<String>,
    /// Pod container names (pods only) — used by the container picker and log fan-in.
    pub containers: Vec<String>,
    /// ownerReferences (kind,name) — used to map Deployment -> ReplicaSet -> Pod for logs.
    pub owners: Vec<OwnerRef>,
    /// Aggregated pod resource requests/limits (pods only) — used by the cluster pulse.
    pub pod_resources: Option<PodResources>,
    /// Node capacity/condition summary (nodes only) — used by the cluster pulse.
    pub node_capacity: Option<NodeCapacity>,
    /// Total container restarts (pods only) — crashloop signal for the triage board.
    pub restarts: u32,
    /// Whether the pod's Ready condition holds (pods only; true for kinds where it doesn't apply).
    /// A Running-but-not-Ready pod (failing readiness probe / slow JVM start) is a triage signal.
    pub ready: bool,
}

impl ResourceEntity {
    /// True for Helm 3 release-state secrets (`type: helm.sh/release.v1`, names like
    /// `sh.helm.release.v1.<release>.v<n>`). Used to hide them from the Secrets list by default.
    pub fn is_helm_release(&self) -> bool {
        self.key.kind == ResourceKind::Secrets
            && (self
                .columns
                .first()
                .is_some_and(|t| t == "helm.sh/release.v1")
                || self.key.name.starts_with("sh.helm.release.v1."))
    }
}

impl Extracted {
    pub fn owned_by(&self, kind: &str, name: &str) -> bool {
        self.owners
            .iter()
            .any(|owner| owner.kind == kind && owner.name == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerRef {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PodResources {
    pub cpu_request_m: u64,
    pub cpu_limit_m: u64,
    pub mem_request_b: u64,
    pub mem_limit_b: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct NodeCapacity {
    pub ready: bool,
    pub unschedulable: bool,
    pub cpu_alloc_m: u64,
    pub mem_alloc_b: u64,
    pub pod_alloc: u64,
}

#[derive(Debug, Clone)]
pub enum StateDelta {
    Upsert(ResourceEntity),
    Remove(ResourceKey),
    /// A (re)list of this kind is starting — mark the existing set stale, keep showing it.
    RelistStart {
        context: String,
        kind: ResourceKind,
    },
    /// The (re)list finished — sweep entities not refreshed during it.
    RelistEnd {
        context: String,
        kind: ResourceKind,
    },
    /// All watchers for a context were stopped (dropped from the active + warm set) — drop its
    /// cached entities so store memory stays bounded to active + warm contexts (Phase 1.3).
    EvictContext {
        context: String,
    },
    Error {
        context: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Input(KeyEvent),
    State(StateDelta),
    RenderTick,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum ConfirmationKind {
    Delete(ResourceKey),
}

#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub created_at: std::time::Instant,
    pub ttl: Duration,
    pub kind: ConfirmationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Table,
    Describe,
    SecretDecode,
    Events,
    Logs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    // Universal columns (all kinds), in display order.
    Namespace,
    Name,
    Status,
    Age,
    // Pods-only columns. Restarts/Ip/Node sort from entity fields; the metric columns
    // (Cpu/Mem and the request/limit percentages) are sorted by the live usage map.
    Restarts,
    Cpu,
    CpuReqPct,
    CpuLimPct,
    Mem,
    MemReqPct,
    MemLimPct,
    Ip,
    Node,
}

impl SortColumn {
    /// True if this column is sorted from the live metrics map rather than store fields.
    pub fn is_metric(self) -> bool {
        matches!(
            self,
            SortColumn::Cpu
                | SortColumn::CpuReqPct
                | SortColumn::CpuLimPct
                | SortColumn::Mem
                | SortColumn::MemReqPct
                | SortColumn::MemLimPct
        )
    }

    /// True if this column only exists in the Pods view.
    pub fn is_pod_only(self) -> bool {
        !matches!(
            self,
            SortColumn::Namespace | SortColumn::Name | SortColumn::Status | SortColumn::Age
        )
    }

    /// Canonical short label (shown in the `[SORT]` indicator and accepted by `:sort`).
    pub fn label(self) -> &'static str {
        match self {
            SortColumn::Namespace => "ns",
            SortColumn::Name => "name",
            SortColumn::Status => "status",
            SortColumn::Age => "age",
            SortColumn::Restarts => "restarts",
            SortColumn::Cpu => "cpu",
            SortColumn::CpuReqPct => "cpu/r",
            SortColumn::CpuLimPct => "cpu/l",
            SortColumn::Mem => "mem",
            SortColumn::MemReqPct => "mem/r",
            SortColumn::MemLimPct => "mem/l",
            SortColumn::Ip => "ip",
            SortColumn::Node => "node",
        }
    }

    /// Parse a `:sort` column token (case-insensitive; accepts a few aliases).
    pub fn parse(token: &str) -> Option<SortColumn> {
        match token.to_ascii_lowercase().as_str() {
            "name" => Some(SortColumn::Name),
            "namespace" | "ns" => Some(SortColumn::Namespace),
            "status" => Some(SortColumn::Status),
            "age" => Some(SortColumn::Age),
            "restarts" | "rst" => Some(SortColumn::Restarts),
            "cpu" => Some(SortColumn::Cpu),
            "cpu/r" | "%cpu/r" | "cpureq" => Some(SortColumn::CpuReqPct),
            "cpu/l" | "%cpu/l" | "cpulim" => Some(SortColumn::CpuLimPct),
            "mem" => Some(SortColumn::Mem),
            "mem/r" | "%mem/r" | "memreq" => Some(SortColumn::MemReqPct),
            "mem/l" | "%mem/l" | "memlim" => Some(SortColumn::MemLimPct),
            "ip" => Some(SortColumn::Ip),
            "node" => Some(SortColumn::Node),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(name: &str, columns: Vec<String>) -> ResourceEntity {
        ResourceEntity {
            key: ResourceKey::new("ctx", ResourceKind::Secrets, Some("ns".to_string()), name),
            status: "-".to_string(),
            age: None,
            labels: vec![],
            columns,
            extracted: Default::default(),
        }
    }

    #[test]
    fn is_helm_release_detects_type_and_name_but_not_plain_secrets() {
        // Detected by helm type column.
        assert!(secret("anything", vec!["helm.sh/release.v1".to_string()]).is_helm_release());
        // Detected by name prefix even without the type column.
        assert!(secret("sh.helm.release.v1.app.v2", vec![]).is_helm_release());
        // Plain secret is not a helm release.
        assert!(!secret("db-credentials", vec!["Opaque".to_string()]).is_helm_release());
    }

    #[test]
    fn is_helm_release_only_applies_to_secrets() {
        let mut cm = secret("sh.helm.release.v1.app.v2", vec![]);
        cm.key.kind = ResourceKind::ConfigMaps;
        assert!(!cm.is_helm_release());
    }
}
