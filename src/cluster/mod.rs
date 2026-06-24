mod extract;
mod kube_provider;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kube::core::ApiResource;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::model::{ResourceKey, ResourceKind};

pub use kube_provider::{KubeProviderOptions, KubeResourceProvider};

/// An API resource discovered on a cluster (built-in or CRD), beyond krust's curated kinds.
/// Phase 4.1: enables read-only browse/describe of arbitrary GVKs via the dynamic path.
#[derive(Debug, Clone)]
pub struct DiscoveredResource {
    pub api_resource: ApiResource,
    pub namespaced: bool,
}

impl DiscoveredResource {
    pub fn group(&self) -> &str {
        &self.api_resource.group
    }
    pub fn version(&self) -> &str {
        &self.api_resource.version
    }
    pub fn kind(&self) -> &str {
        &self.api_resource.kind
    }
    pub fn plural(&self) -> &str {
        &self.api_resource.plural
    }
    /// `kind.group` (or just `kind` for core), lowercased — a stable display/match handle.
    pub fn full_name(&self) -> String {
        if self.api_resource.group.is_empty() {
            self.api_resource.plural.clone()
        } else {
            format!("{}.{}", self.api_resource.plural, self.api_resource.group)
        }
    }
}

/// Actual resource usage for one node, from `metrics.k8s.io` (Phase 4.2).
#[derive(Debug, Clone, Copy)]
pub struct NodeUsage {
    pub cpu_used_m: u64,
    pub mem_used_b: u64,
}

/// Actual resource usage for one pod (summed across its containers), from `metrics.k8s.io`.
#[derive(Debug, Clone)]
pub struct PodUsage {
    pub namespace: String,
    pub name: String,
    pub cpu_used_m: u64,
    pub mem_used_b: u64,
}

/// A generic row from a one-shot dynamic list (no curated columns).
#[derive(Debug, Clone)]
pub struct DynamicRow {
    pub namespace: Option<String>,
    pub name: String,
    pub age: Option<DateTime<Utc>>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ActionResult {
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct PodLogRequest {
    pub context: String,
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub since_seconds: Option<i64>,
    pub previous: bool,
    pub timestamps: bool,
}

#[derive(Debug, Clone)]
pub enum PodLogEvent {
    Line(String),
    End,
    Error(String),
}

#[derive(Debug)]
pub struct PodLogStream {
    pub rx: mpsc::Receiver<PodLogEvent>,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchTarget {
    pub kind: ResourceKind,
    pub namespace: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("read-only mode is enabled")]
    ReadOnly,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("unsupported action: {0}")]
    Unsupported(String),
    #[error("action failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait ResourceProvider: Send + Sync {
    fn context_names(&self) -> &[String];
    fn default_context(&self) -> Option<&str>;
    async fn start(
        &self,
        tx: tokio::sync::mpsc::Sender<crate::model::StateDelta>,
    ) -> anyhow::Result<()>;
    async fn replace_watch_plan(
        &self,
        context: &str,
        targets: &[WatchTarget],
    ) -> anyhow::Result<()>;
    async fn stream_pod_logs(&self, request: PodLogRequest) -> anyhow::Result<PodLogStream>;
    /// Fetch the full object for a single resource on demand (for detail/describe/decode/edit).
    /// Not retained per row — see the lean entity model (docs/design/phase-1.1-lean-entity-model.md).
    async fn get_object(&self, key: &ResourceKey) -> anyhow::Result<serde_json::Value>;

    /// Discover all API resources available on a context (built-ins + CRDs). One cached call per
    /// context. Phase 4.1 (dynamic resources).
    async fn discover(&self, context: &str) -> anyhow::Result<Vec<DiscoveredResource>>;

    /// One-shot list of a discovered resource's objects (bounded), with generic columns.
    async fn list_dynamic(
        &self,
        context: &str,
        resource: &DiscoveredResource,
        namespace: Option<&str>,
    ) -> anyhow::Result<Vec<DynamicRow>>;

    /// Fetch a single dynamic object's full manifest for describe.
    async fn get_dynamic(
        &self,
        context: &str,
        resource: &DiscoveredResource,
        namespace: Option<&str>,
        name: &str,
    ) -> anyhow::Result<serde_json::Value>;

    /// Per-node actual usage from `metrics.k8s.io` (Phase 4.2). Errors if the metrics API is not
    /// served (no metrics-server); callers degrade gracefully.
    async fn node_metrics(&self, context: &str) -> anyhow::Result<Vec<NodeUsage>>;

    /// Per-pod actual usage (summed across containers) from `metrics.k8s.io`, scoped to a namespace
    /// or cluster-wide. Errors if the metrics API is not served; callers degrade gracefully.
    async fn pod_metrics(
        &self,
        context: &str,
        namespace: Option<&str>,
    ) -> anyhow::Result<Vec<PodUsage>>;
}

#[async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn delete_resource(&self, key: &ResourceKey) -> Result<ActionResult, ActionError>;
    async fn replace_resource(
        &self,
        key: &ResourceKey,
        manifest: serde_json::Value,
    ) -> Result<ActionResult, ActionError>;
}
