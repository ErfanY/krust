use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{AsyncBufReadExt, StreamExt};
use k8s_openapi::{
    NamespaceResourceScope,
    api::{
        apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet},
        autoscaling::v2::HorizontalPodAutoscaler,
        batch::v1::{CronJob, Job},
        core::v1::{
            ConfigMap, Event as KubeEvent, Namespace, Node, PersistentVolume,
            PersistentVolumeClaim, Pod, Secret, Service, ServiceAccount,
        },
        networking::v1::{Ingress, NetworkPolicy},
        policy::v1::PodDisruptionBudget,
        rbac::v1::{ClusterRole, ClusterRoleBinding, Role, RoleBinding},
    },
};
use kube::{
    Api, Client, Config, Resource, ResourceExt,
    api::{DeleteParams, LogParams},
    config::{KubeConfigOptions, Kubeconfig},
};
use kube_runtime::watcher::{self, Event};
use serde::Serialize;
use tokio::{sync::mpsc, time::sleep};
use tracing::{error, warn};

use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

use super::{
    ActionError, ActionExecutor, ActionResult, PodLogEvent, PodLogRequest, PodLogStream,
    ResourceProvider,
};

#[derive(Debug, Clone)]
pub struct KubeProviderOptions {
    pub kubeconfig: Option<PathBuf>,
    pub context: Option<String>,
    pub all_contexts: bool,
    pub namespace: Option<String>,
    pub readonly: bool,
}

#[derive(Clone)]
pub struct KubeResourceProvider {
    contexts: Vec<String>,
    default_context: Option<String>,
    namespace_by_context: HashMap<String, Option<String>>,
    kubeconfig: Kubeconfig,
    readonly: bool,
    clients: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Client>>>,
    watched: std::sync::Arc<tokio::sync::Mutex<HashSet<(String, ResourceKind)>>>,
    delta_tx: std::sync::Arc<tokio::sync::Mutex<Option<mpsc::Sender<StateDelta>>>>,
}

impl KubeResourceProvider {
    pub async fn new(options: KubeProviderOptions) -> anyhow::Result<Self> {
        let kubeconfig = load_kubeconfig(options.kubeconfig.as_ref())?;

        let mut contexts: Vec<String> = kubeconfig
            .contexts
            .iter()
            .map(|ctx| ctx.name.clone())
            .collect();

        if contexts.is_empty() {
            if let Some(current) = kubeconfig.current_context.clone() {
                contexts.push(current);
            } else {
                bail!("no Kubernetes contexts found");
            }
        }

        if let Some(requested) = options.context.as_ref()
            && !contexts.iter().any(|ctx| ctx == requested)
        {
            bail!("requested context '{requested}' is not present in kubeconfig");
        }

        let default_context = options
            .context
            .clone()
            .or_else(|| kubeconfig.current_context.clone())
            .or_else(|| contexts.first().cloned());

        let context_namespace_hint: HashMap<String, Option<String>> = kubeconfig
            .contexts
            .iter()
            .map(|named| {
                (
                    named.name.clone(),
                    named.context.as_ref().and_then(|ctx| ctx.namespace.clone()),
                )
            })
            .collect();

        let mut namespace_by_context = HashMap::new();
        for context in &contexts {
            let namespace = options
                .namespace
                .clone()
                .or_else(|| context_namespace_hint.get(context).cloned().flatten());
            namespace_by_context.insert(context.clone(), namespace);
        }

        let provider = Self {
            contexts,
            default_context,
            namespace_by_context,
            kubeconfig,
            readonly: options.readonly,
            clients: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watched: std::sync::Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            delta_tx: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        };

        // Optional eager auth/client warmup.
        if options.all_contexts {
            let contexts = provider.contexts.clone();
            for context in contexts {
                provider.client_for_context(&context).await?;
            }
        } else if let Some(context) = provider.default_context.clone() {
            provider.client_for_context(&context).await?;
        }

        Ok(provider)
    }

    async fn client_for_context(&self, context: &str) -> anyhow::Result<Client> {
        if !self.contexts.iter().any(|ctx| ctx == context) {
            bail!("unknown context: {context}");
        }

        if let Some(existing) = self.clients.lock().await.get(context).cloned() {
            return Ok(existing);
        }

        let config = Config::from_custom_kubeconfig(
            self.kubeconfig.clone(),
            &KubeConfigOptions {
                context: Some(context.to_string()),
                cluster: None,
                user: None,
            },
        )
        .await
        .with_context(|| format!("failed to build config for context {context}"))?;
        let client = Client::try_from(config)
            .with_context(|| format!("failed to create client for context {context}"))?;

        let mut clients = self.clients.lock().await;
        if let Some(existing) = clients.get(context).cloned() {
            return Ok(existing);
        }
        clients.insert(context.to_string(), client.clone());
        Ok(client)
    }
}

#[async_trait]
impl ResourceProvider for KubeResourceProvider {
    fn context_names(&self) -> &[String] {
        &self.contexts
    }

    fn default_context(&self) -> Option<&str> {
        self.default_context.as_deref()
    }

    async fn start(&self, tx: mpsc::Sender<StateDelta>) -> anyhow::Result<()> {
        *self.delta_tx.lock().await = Some(tx);
        Ok(())
    }

    async fn ensure_watch(&self, context: &str, kind: ResourceKind) -> anyhow::Result<()> {
        if !self.contexts.iter().any(|ctx| ctx == context) {
            bail!("unknown context: {context}");
        }

        {
            let watched = self.watched.lock().await;
            if watched.contains(&(context.to_string(), kind)) {
                return Ok(());
            }
        }

        let tx = self
            .delta_tx
            .lock()
            .await
            .as_ref()
            .cloned()
            .context("resource provider is not started")?;

        {
            let mut watched = self.watched.lock().await;
            if watched.contains(&(context.to_string(), kind)) {
                return Ok(());
            }
            watched.insert((context.to_string(), kind));
        }

        let client = self.client_for_context(context).await?;
        let namespace = self.namespace_by_context.get(context).cloned().flatten();
        spawn_watch_for_kind(client, context.to_string(), kind, namespace, tx);
        Ok(())
    }

    async fn stream_pod_logs(&self, request: PodLogRequest) -> anyhow::Result<PodLogStream> {
        let client = self.client_for_context(&request.context).await?;
        let (tx, rx) = mpsc::channel(1024);

        let task = tokio::spawn(async move {
            let api: Api<Pod> = Api::namespaced(client, &request.namespace);
            let params = LogParams {
                container: request.container.clone(),
                follow: request.follow,
                limit_bytes: None,
                pretty: false,
                previous: request.previous,
                since_seconds: request.since_seconds,
                since_time: None,
                tail_lines: request.tail_lines,
                timestamps: request.timestamps,
            };

            let stream = match api.log_stream(&request.pod, &params).await {
                Ok(stream) => stream,
                Err(err) => {
                    let _ = tx.send(PodLogEvent::Error(err.to_string())).await;
                    return;
                }
            };

            let mut lines = stream.lines();
            while let Some(line) = lines.next().await {
                match line {
                    Ok(line) => {
                        if tx.send(PodLogEvent::Line(line)).await.is_err() {
                            return;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(PodLogEvent::Error(err.to_string())).await;
                        return;
                    }
                }
            }

            let _ = tx.send(PodLogEvent::End).await;
        });

        Ok(PodLogStream { rx, task })
    }
}

#[async_trait]
impl ActionExecutor for KubeResourceProvider {
    async fn delete_resource(&self, key: &ResourceKey) -> Result<ActionResult, ActionError> {
        if self.readonly {
            return Err(ActionError::ReadOnly);
        }

        let client = self
            .client_for_context(&key.context)
            .await
            .map_err(|err| ActionError::Failed(err.to_string()))?;

        match key.kind {
            ResourceKind::Pods => {
                let namespace = key.namespace.as_deref().unwrap_or("default");
                let api: Api<Pod> = Api::namespaced(client, namespace);
                api.delete(&key.name, &DeleteParams::default())
                    .await
                    .map_err(map_kube_error)?;
                Ok(ActionResult {
                    message: format!("pod {} deleted", key.name),
                })
            }
            _ => Err(ActionError::Unsupported(format!(
                "delete {} is not implemented yet",
                key.kind
            ))),
        }
    }
}

fn load_kubeconfig(path: Option<&PathBuf>) -> anyhow::Result<Kubeconfig> {
    if let Some(path) = path {
        return Kubeconfig::read_from(path)
            .with_context(|| format!("failed to read kubeconfig {}", path.display()));
    }
    Kubeconfig::read().context("failed to read kubeconfig")
}

fn spawn_namespaced_watch<K>(
    client: Client,
    context: String,
    kind: ResourceKind,
    namespace: Option<String>,
    tx: mpsc::Sender<StateDelta>,
) where
    K: Clone
        + Resource<Scope = NamespaceResourceScope>
        + ResourceExt
        + serde::de::DeserializeOwned
        + Serialize
        + Send
        + Sync
        + 'static,
    K: Debug,
    <K as Resource>::DynamicType: Default,
{
    tokio::spawn(async move {
        let api: Api<K> = match namespace.as_deref() {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        run_watch_loop(api, context, kind, tx).await;
    });
}

fn spawn_cluster_watch<K>(
    client: Client,
    context: String,
    kind: ResourceKind,
    tx: mpsc::Sender<StateDelta>,
) where
    K: Clone
        + Resource
        + ResourceExt
        + serde::de::DeserializeOwned
        + Serialize
        + Send
        + Sync
        + 'static,
    K: Debug,
    <K as Resource>::DynamicType: Default,
{
    tokio::spawn(async move {
        let api: Api<K> = Api::all(client);
        run_watch_loop(api, context, kind, tx).await;
    });
}

async fn run_watch_loop<K>(
    api: Api<K>,
    context: String,
    kind: ResourceKind,
    tx: mpsc::Sender<StateDelta>,
) where
    K: Clone
        + Resource
        + ResourceExt
        + serde::de::DeserializeOwned
        + Serialize
        + Send
        + Sync
        + Debug
        + 'static,
    <K as Resource>::DynamicType: Default,
{
    let mut backoff = Duration::from_millis(500);

    loop {
        let cfg = watcher::Config::default().timeout(20);
        let mut stream = watcher::watcher(api.clone(), cfg).boxed();

        if tx
            .send(StateDelta::Reset {
                context: context.clone(),
                kind,
            })
            .await
            .is_err()
        {
            return;
        }

        let loop_started = Instant::now();

        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    if handle_watch_event(&context, kind, event, &tx)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(err) => {
                    if is_forbidden_watcher_error(&err) {
                        let message = format!(
                            "{kind}: forbidden (insufficient RBAC). Disabling watcher for this resource/context."
                        );
                        warn!(%context, ?kind, error = %err, "watcher forbidden; disabling resource watcher");
                        let _ = tx
                            .send(StateDelta::Error {
                                context: context.clone(),
                                message,
                            })
                            .await;
                        return;
                    }
                    warn!(%context, ?kind, error = %err, "watch stream error");
                    let _ = tx
                        .send(StateDelta::Error {
                            context: context.clone(),
                            message: format!("{kind}: {err}"),
                        })
                        .await;
                    break;
                }
            }
        }

        if loop_started.elapsed() > Duration::from_secs(20) {
            backoff = Duration::from_millis(500);
        } else {
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }

        sleep(backoff).await;
    }
}

fn is_forbidden_watcher_error(error: &watcher::Error) -> bool {
    match error {
        watcher::Error::InitialListFailed(err)
        | watcher::Error::WatchStartFailed(err)
        | watcher::Error::WatchFailed(err) => is_forbidden_kube_error(err),
        watcher::Error::WatchError(status) => status.code == 403,
        watcher::Error::NoResourceVersion => false,
    }
}

fn is_forbidden_kube_error(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(status) if status.code == 403)
}

async fn handle_watch_event<K>(
    context: &str,
    kind: ResourceKind,
    event: Event<K>,
    tx: &mpsc::Sender<StateDelta>,
) -> Result<(), ()>
where
    K: Clone + ResourceExt + Serialize,
{
    match event {
        Event::Apply(obj) => {
            if let Some(entity) = to_entity(context, kind, &obj) {
                tx.send(StateDelta::Upsert(entity)).await.map_err(|_| ())?;
            }
        }
        Event::Delete(obj) => {
            let key = ResourceKey::new(context, kind, obj.namespace(), obj.name_any());
            tx.send(StateDelta::Remove(key)).await.map_err(|_| ())?;
        }
        Event::Init => {
            tx.send(StateDelta::Reset {
                context: context.to_string(),
                kind,
            })
            .await
            .map_err(|_| ())?;
        }
        Event::InitApply(obj) => {
            if let Some(entity) = to_entity(context, kind, &obj) {
                tx.send(StateDelta::Upsert(entity)).await.map_err(|_| ())?;
            }
        }
        Event::InitDone => {
            // Marker that initial list state has been fully emitted.
        }
    }
    Ok(())
}

fn to_entity<K>(context: &str, kind: ResourceKind, obj: &K) -> Option<ResourceEntity>
where
    K: ResourceExt + Serialize,
{
    let raw = serde_json::to_value(obj).ok()?;
    let key = ResourceKey::new(context, kind, obj.namespace(), obj.name_any());
    let age = obj
        .creation_timestamp()
        .map(|ts| DateTime::<Utc>::from(std::time::SystemTime::from(ts.0)));

    let labels = obj
        .labels()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let status = extract_status(kind, &raw);
    let summary = extract_summary(kind, &raw);

    Some(ResourceEntity {
        key,
        status,
        age,
        labels,
        summary,
        raw,
    })
}

fn extract_status(kind: ResourceKind, raw: &serde_json::Value) -> String {
    match kind {
        ResourceKind::Pods => raw
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown")
            .to_string(),
        ResourceKind::Deployments => {
            let ready = raw
                .pointer("/status/readyReplicas")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let desired = raw
                .pointer("/status/replicas")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            format!("{ready}/{desired} ready")
        }
        ResourceKind::Jobs => raw
            .pointer("/status/succeeded")
            .and_then(serde_json::Value::as_i64)
            .map(|s| format!("succeeded={s}"))
            .unwrap_or_else(|| "Pending".to_string()),
        _ => raw
            .pointer("/status/phase")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                raw.pointer("/status/conditions/0/type")
                    .and_then(serde_json::Value::as_str)
            })
            .unwrap_or("-")
            .to_string(),
    }
}

fn extract_summary(kind: ResourceKind, raw: &serde_json::Value) -> String {
    match kind {
        ResourceKind::Pods => raw
            .pointer("/spec/nodeName")
            .and_then(serde_json::Value::as_str)
            .map(|node| format!("node={node}"))
            .unwrap_or_else(|| "-".to_string()),
        ResourceKind::Services => raw
            .pointer("/spec/type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("ClusterIP")
            .to_string(),
        ResourceKind::Events => raw
            .pointer("/reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-")
            .to_string(),
        _ => raw
            .pointer("/metadata/labels")
            .and_then(serde_json::Value::as_object)
            .map(|labels| format!("{} labels", labels.len()))
            .unwrap_or_else(|| "-".to_string()),
    }
}

fn map_kube_error(error: kube::Error) -> ActionError {
    match error {
        kube::Error::Api(api_err) => {
            if api_err.code == 403 {
                ActionError::PermissionDenied(api_err.message)
            } else {
                ActionError::Failed(format!("{} ({})", api_err.message, api_err.code))
            }
        }
        other => {
            error!(error = %other, "kubernetes action failed");
            ActionError::Failed(other.to_string())
        }
    }
}

fn spawn_watch_for_kind(
    client: Client,
    context: String,
    kind: ResourceKind,
    namespace: Option<String>,
    tx: mpsc::Sender<StateDelta>,
) {
    match kind {
        ResourceKind::Pods => {
            spawn_namespaced_watch::<Pod>(client, context, kind, namespace, tx);
        }
        ResourceKind::Deployments => {
            spawn_namespaced_watch::<Deployment>(client, context, kind, namespace, tx);
        }
        ResourceKind::ReplicaSets => {
            spawn_namespaced_watch::<ReplicaSet>(client, context, kind, namespace, tx);
        }
        ResourceKind::StatefulSets => {
            spawn_namespaced_watch::<StatefulSet>(client, context, kind, namespace, tx);
        }
        ResourceKind::DaemonSets => {
            spawn_namespaced_watch::<DaemonSet>(client, context, kind, namespace, tx);
        }
        ResourceKind::Services => {
            spawn_namespaced_watch::<Service>(client, context, kind, namespace, tx);
        }
        ResourceKind::Ingresses => {
            spawn_namespaced_watch::<Ingress>(client, context, kind, namespace, tx);
        }
        ResourceKind::ConfigMaps => {
            spawn_namespaced_watch::<ConfigMap>(client, context, kind, namespace, tx);
        }
        ResourceKind::Secrets => {
            spawn_namespaced_watch::<Secret>(client, context, kind, namespace, tx);
        }
        ResourceKind::Jobs => {
            spawn_namespaced_watch::<Job>(client, context, kind, namespace, tx);
        }
        ResourceKind::CronJobs => {
            spawn_namespaced_watch::<CronJob>(client, context, kind, namespace, tx);
        }
        ResourceKind::PersistentVolumeClaims => {
            spawn_namespaced_watch::<PersistentVolumeClaim>(client, context, kind, namespace, tx);
        }
        ResourceKind::PersistentVolumes => {
            spawn_cluster_watch::<PersistentVolume>(client, context, kind, tx);
        }
        ResourceKind::Nodes => {
            spawn_cluster_watch::<Node>(client, context, kind, tx);
        }
        ResourceKind::Namespaces => {
            spawn_cluster_watch::<Namespace>(client, context, kind, tx);
        }
        ResourceKind::Events => {
            spawn_namespaced_watch::<KubeEvent>(client, context, kind, namespace, tx);
        }
        ResourceKind::ServiceAccounts => {
            spawn_namespaced_watch::<ServiceAccount>(client, context, kind, namespace, tx);
        }
        ResourceKind::Roles => {
            spawn_namespaced_watch::<Role>(client, context, kind, namespace, tx);
        }
        ResourceKind::RoleBindings => {
            spawn_namespaced_watch::<RoleBinding>(client, context, kind, namespace, tx);
        }
        ResourceKind::ClusterRoles => {
            spawn_cluster_watch::<ClusterRole>(client, context, kind, tx);
        }
        ResourceKind::ClusterRoleBindings => {
            spawn_cluster_watch::<ClusterRoleBinding>(client, context, kind, tx);
        }
        ResourceKind::NetworkPolicies => {
            spawn_namespaced_watch::<NetworkPolicy>(client, context, kind, namespace, tx);
        }
        ResourceKind::HorizontalPodAutoscalers => {
            spawn_namespaced_watch::<HorizontalPodAutoscaler>(client, context, kind, namespace, tx);
        }
        ResourceKind::PodDisruptionBudgets => {
            spawn_namespaced_watch::<PodDisruptionBudget>(client, context, kind, namespace, tx);
        }
    }
}
