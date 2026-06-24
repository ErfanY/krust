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
    pub summary: String,
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
    /// Pod container names (pods only) — used by the container picker and log fan-in.
    pub containers: Vec<String>,
    /// ownerReferences (kind,name) — used to map Deployment -> ReplicaSet -> Pod for logs.
    pub owners: Vec<OwnerRef>,
    /// Aggregated pod resource requests/limits (pods only) — used by the cluster pulse.
    pub pod_resources: Option<PodResources>,
    /// Node capacity/condition summary (nodes only) — used by the cluster pulse.
    pub node_capacity: Option<NodeCapacity>,
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
    Name,
    Namespace,
    Status,
    Age,
}
