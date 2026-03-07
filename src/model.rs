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
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum StateDelta {
    Upsert(ResourceEntity),
    Remove(ResourceKey),
    Reset { context: String, kind: ResourceKind },
    Error { context: String, message: String },
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
