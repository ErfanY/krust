mod kube_provider;

use async_trait::async_trait;

use crate::model::ResourceKey;

pub use kube_provider::{KubeProviderOptions, KubeResourceProvider};

#[derive(Debug, Clone)]
pub struct ActionResult {
    pub message: String,
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
    async fn ensure_watch(
        &self,
        context: &str,
        kind: crate::model::ResourceKind,
    ) -> anyhow::Result<()>;
}

#[async_trait]
pub trait ActionExecutor: Send + Sync {
    async fn delete_resource(&self, key: &ResourceKey) -> Result<ActionResult, ActionError>;
}
