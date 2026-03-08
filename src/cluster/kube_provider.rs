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
    api::{DeleteParams, LogParams, PostParams},
    config::{KubeConfigOptions, Kubeconfig},
    core::{ApiResource, DynamicObject, GroupVersionKind},
};
use kube_runtime::watcher::{self, Event};
use serde::Serialize;
use tokio::{sync::mpsc, task::JoinHandle, time::sleep};
use tracing::{error, warn};

use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

use super::{
    ActionError, ActionExecutor, ActionResult, PodLogEvent, PodLogRequest, PodLogStream,
    ResourceProvider, WatchTarget,
};

#[derive(Debug, Clone)]
pub struct KubeProviderOptions {
    pub kubeconfig: Option<PathBuf>,
    pub context: Option<String>,
    pub all_contexts: bool,
    pub readonly: bool,
    pub warm_contexts: usize,
    pub warm_context_ttl_secs: u64,
}

#[derive(Clone)]
pub struct KubeResourceProvider {
    contexts: Vec<String>,
    default_context: Option<String>,
    kubeconfig: Kubeconfig,
    readonly: bool,
    warm_contexts: usize,
    warm_context_ttl: Duration,
    clients: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Client>>>,
    watched: std::sync::Arc<tokio::sync::Mutex<HashMap<WatchKey, JoinHandle<()>>>>,
    context_last_active: std::sync::Arc<tokio::sync::Mutex<HashMap<String, Instant>>>,
    delta_tx: std::sync::Arc<tokio::sync::Mutex<Option<mpsc::Sender<StateDelta>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WatchKey {
    context: String,
    kind: ResourceKind,
    namespace: Option<String>,
}

impl WatchKey {
    fn new(context: &str, kind: ResourceKind, namespace: Option<String>) -> Self {
        Self {
            context: context.to_string(),
            kind,
            namespace: if kind.is_namespaced() {
                namespace
            } else {
                None
            },
        }
    }
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

        let provider = Self {
            contexts,
            default_context,
            kubeconfig,
            readonly: options.readonly,
            warm_contexts: options.warm_contexts,
            warm_context_ttl: Duration::from_secs(options.warm_context_ttl_secs.max(1)),
            clients: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watched: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            context_last_active: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())),
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

    pub fn default_namespace_for_context(&self, context: &str) -> Option<String> {
        self.kubeconfig
            .contexts
            .iter()
            .find(|named| named.name == context)
            .and_then(|named| named.context.as_ref())
            .and_then(|ctx| ctx.namespace.clone())
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

    async fn replace_watch_plan(
        &self,
        context: &str,
        targets: &[WatchTarget],
    ) -> anyhow::Result<()> {
        if !self.contexts.iter().any(|ctx| ctx == context) {
            bail!("unknown context: {context}");
        }

        let tx = self
            .delta_tx
            .lock()
            .await
            .as_ref()
            .cloned()
            .context("resource provider is not started")?;
        let client = self.client_for_context(context).await?;
        let now = Instant::now();

        let warm_contexts = {
            let mut last_active = self.context_last_active.lock().await;
            last_active.insert(context.to_string(), now);
            let keep = select_warm_contexts(
                &last_active,
                context,
                now,
                self.warm_contexts,
                self.warm_context_ttl,
            );

            last_active.retain(|ctx, ts| {
                ctx == context
                    || (keep.contains(ctx) && now.duration_since(*ts) <= self.warm_context_ttl)
            });
            keep
        };

        let desired: HashSet<WatchKey> = targets
            .iter()
            .map(|target| WatchKey::new(context, target.kind, target.namespace.clone()))
            .collect();

        let mut watched = self.watched.lock().await;
        let existing_keys: Vec<WatchKey> = watched.keys().cloned().collect();
        for key in existing_keys {
            let keep_existing = if key.context == context {
                desired.contains(&key)
            } else {
                warm_contexts.contains(&key.context)
            };
            if !keep_existing && let Some(task) = watched.remove(&key) {
                task.abort();
            }
        }

        for key in desired {
            if watched.contains_key(&key) {
                continue;
            }
            let task = spawn_watch_for_kind(
                client.clone(),
                key.context.clone(),
                key.kind,
                key.namespace.clone(),
                tx.clone(),
            );
            watched.insert(key, task);
        }

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

fn select_warm_contexts(
    last_active: &HashMap<String, Instant>,
    active_context: &str,
    now: Instant,
    warm_contexts: usize,
    ttl: Duration,
) -> HashSet<String> {
    let mut inactive: Vec<(String, Instant)> = last_active
        .iter()
        .filter_map(|(ctx, ts)| {
            if ctx == active_context {
                None
            } else {
                Some((ctx.clone(), *ts))
            }
        })
        .collect();
    inactive.sort_by(|a, b| b.1.cmp(&a.1));

    inactive
        .into_iter()
        .filter(|(_, ts)| now.duration_since(*ts) <= ttl)
        .take(warm_contexts)
        .map(|(ctx, _)| ctx)
        .collect()
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

    async fn replace_resource(
        &self,
        key: &ResourceKey,
        manifest: serde_json::Value,
    ) -> Result<ActionResult, ActionError> {
        if self.readonly {
            return Err(ActionError::ReadOnly);
        }

        let client = self
            .client_for_context(&key.context)
            .await
            .map_err(|err| ActionError::Failed(err.to_string()))?;

        let spec = api_resource_spec_for_kind(key.kind).ok_or_else(|| {
            ActionError::Unsupported(format!("replace {} is not implemented yet", key.kind))
        })?;
        let gvk = GroupVersionKind::gvk(spec.group, spec.version, spec.kind);
        let mut ar = ApiResource::from_gvk(&gvk);
        ar.plural = spec.plural.to_string();
        let obj: DynamicObject = serde_json::from_value(manifest)
            .map_err(|err| ActionError::Failed(format!("invalid manifest: {err}")))?;

        if spec.namespaced {
            let namespace = key.namespace.as_deref().unwrap_or("default");
            let api: Api<DynamicObject> = Api::namespaced_with(client, namespace, &ar);
            api.replace(&key.name, &PostParams::default(), &obj)
                .await
                .map_err(map_kube_error)?;
        } else {
            let api: Api<DynamicObject> = Api::all_with(client, &ar);
            api.replace(&key.name, &PostParams::default(), &obj)
                .await
                .map_err(map_kube_error)?;
        }

        Ok(ActionResult {
            message: format!("{} {} updated", key.kind.short_name(), key.name),
        })
    }
}

struct ApiResourceSpec {
    group: &'static str,
    version: &'static str,
    kind: &'static str,
    plural: &'static str,
    namespaced: bool,
}

fn api_resource_spec_for_kind(kind: ResourceKind) -> Option<ApiResourceSpec> {
    Some(match kind {
        ResourceKind::Pods => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Pod",
            plural: "pods",
            namespaced: true,
        },
        ResourceKind::Deployments => ApiResourceSpec {
            group: "apps",
            version: "v1",
            kind: "Deployment",
            plural: "deployments",
            namespaced: true,
        },
        ResourceKind::ReplicaSets => ApiResourceSpec {
            group: "apps",
            version: "v1",
            kind: "ReplicaSet",
            plural: "replicasets",
            namespaced: true,
        },
        ResourceKind::StatefulSets => ApiResourceSpec {
            group: "apps",
            version: "v1",
            kind: "StatefulSet",
            plural: "statefulsets",
            namespaced: true,
        },
        ResourceKind::DaemonSets => ApiResourceSpec {
            group: "apps",
            version: "v1",
            kind: "DaemonSet",
            plural: "daemonsets",
            namespaced: true,
        },
        ResourceKind::Services => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Service",
            plural: "services",
            namespaced: true,
        },
        ResourceKind::Ingresses => ApiResourceSpec {
            group: "networking.k8s.io",
            version: "v1",
            kind: "Ingress",
            plural: "ingresses",
            namespaced: true,
        },
        ResourceKind::ConfigMaps => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "ConfigMap",
            plural: "configmaps",
            namespaced: true,
        },
        ResourceKind::Secrets => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Secret",
            plural: "secrets",
            namespaced: true,
        },
        ResourceKind::Jobs => ApiResourceSpec {
            group: "batch",
            version: "v1",
            kind: "Job",
            plural: "jobs",
            namespaced: true,
        },
        ResourceKind::CronJobs => ApiResourceSpec {
            group: "batch",
            version: "v1",
            kind: "CronJob",
            plural: "cronjobs",
            namespaced: true,
        },
        ResourceKind::PersistentVolumeClaims => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "PersistentVolumeClaim",
            plural: "persistentvolumeclaims",
            namespaced: true,
        },
        ResourceKind::PersistentVolumes => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "PersistentVolume",
            plural: "persistentvolumes",
            namespaced: false,
        },
        ResourceKind::Nodes => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Node",
            plural: "nodes",
            namespaced: false,
        },
        ResourceKind::Namespaces => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Namespace",
            plural: "namespaces",
            namespaced: false,
        },
        ResourceKind::Events => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "Event",
            plural: "events",
            namespaced: true,
        },
        ResourceKind::ServiceAccounts => ApiResourceSpec {
            group: "",
            version: "v1",
            kind: "ServiceAccount",
            plural: "serviceaccounts",
            namespaced: true,
        },
        ResourceKind::Roles => ApiResourceSpec {
            group: "rbac.authorization.k8s.io",
            version: "v1",
            kind: "Role",
            plural: "roles",
            namespaced: true,
        },
        ResourceKind::RoleBindings => ApiResourceSpec {
            group: "rbac.authorization.k8s.io",
            version: "v1",
            kind: "RoleBinding",
            plural: "rolebindings",
            namespaced: true,
        },
        ResourceKind::ClusterRoles => ApiResourceSpec {
            group: "rbac.authorization.k8s.io",
            version: "v1",
            kind: "ClusterRole",
            plural: "clusterroles",
            namespaced: false,
        },
        ResourceKind::ClusterRoleBindings => ApiResourceSpec {
            group: "rbac.authorization.k8s.io",
            version: "v1",
            kind: "ClusterRoleBinding",
            plural: "clusterrolebindings",
            namespaced: false,
        },
        ResourceKind::NetworkPolicies => ApiResourceSpec {
            group: "networking.k8s.io",
            version: "v1",
            kind: "NetworkPolicy",
            plural: "networkpolicies",
            namespaced: true,
        },
        ResourceKind::HorizontalPodAutoscalers => ApiResourceSpec {
            group: "autoscaling",
            version: "v2",
            kind: "HorizontalPodAutoscaler",
            plural: "horizontalpodautoscalers",
            namespaced: true,
        },
        ResourceKind::PodDisruptionBudgets => ApiResourceSpec {
            group: "policy",
            version: "v1",
            kind: "PodDisruptionBudget",
            plural: "poddisruptionbudgets",
            namespaced: true,
        },
    })
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
) -> JoinHandle<()>
where
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
    })
}

fn spawn_cluster_watch<K>(
    client: Client,
    context: String,
    kind: ResourceKind,
    tx: mpsc::Sender<StateDelta>,
) -> JoinHandle<()>
where
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
    })
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

        backoff = next_watch_backoff(backoff, loop_started.elapsed());

        sleep(backoff).await;
    }
}

fn next_watch_backoff(current: Duration, run_elapsed: Duration) -> Duration {
    if run_elapsed > Duration::from_secs(20) {
        Duration::from_millis(500)
    } else {
        (current * 2).min(Duration::from_secs(30))
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
        ResourceKind::Pods => extract_pod_status(raw),
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

fn extract_pod_status(raw: &serde_json::Value) -> String {
    if raw
        .pointer("/metadata/deletionTimestamp")
        .and_then(serde_json::Value::as_str)
        .is_some()
    {
        return "Terminating".to_string();
    }

    if let Some(reason) = raw
        .pointer("/status/containerStatuses")
        .and_then(serde_json::Value::as_array)
        .and_then(|statuses| {
            statuses.iter().find_map(|status| {
                status
                    .pointer("/state/waiting/reason")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        status
                            .pointer("/state/terminated/reason")
                            .and_then(serde_json::Value::as_str)
                    })
            })
        })
    {
        return reason.to_string();
    }

    raw.pointer("/status/reason")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            raw.pointer("/status/phase")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("Unknown")
        .to_string()
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
) -> JoinHandle<()> {
    match kind {
        ResourceKind::Pods => spawn_namespaced_watch::<Pod>(client, context, kind, namespace, tx),
        ResourceKind::Deployments => {
            spawn_namespaced_watch::<Deployment>(client, context, kind, namespace, tx)
        }
        ResourceKind::ReplicaSets => {
            spawn_namespaced_watch::<ReplicaSet>(client, context, kind, namespace, tx)
        }
        ResourceKind::StatefulSets => {
            spawn_namespaced_watch::<StatefulSet>(client, context, kind, namespace, tx)
        }
        ResourceKind::DaemonSets => {
            spawn_namespaced_watch::<DaemonSet>(client, context, kind, namespace, tx)
        }
        ResourceKind::Services => {
            spawn_namespaced_watch::<Service>(client, context, kind, namespace, tx)
        }
        ResourceKind::Ingresses => {
            spawn_namespaced_watch::<Ingress>(client, context, kind, namespace, tx)
        }
        ResourceKind::ConfigMaps => {
            spawn_namespaced_watch::<ConfigMap>(client, context, kind, namespace, tx)
        }
        ResourceKind::Secrets => {
            spawn_namespaced_watch::<Secret>(client, context, kind, namespace, tx)
        }
        ResourceKind::Jobs => spawn_namespaced_watch::<Job>(client, context, kind, namespace, tx),
        ResourceKind::CronJobs => {
            spawn_namespaced_watch::<CronJob>(client, context, kind, namespace, tx)
        }
        ResourceKind::PersistentVolumeClaims => {
            spawn_namespaced_watch::<PersistentVolumeClaim>(client, context, kind, namespace, tx)
        }
        ResourceKind::PersistentVolumes => {
            spawn_cluster_watch::<PersistentVolume>(client, context, kind, tx)
        }
        ResourceKind::Nodes => spawn_cluster_watch::<Node>(client, context, kind, tx),
        ResourceKind::Namespaces => spawn_cluster_watch::<Namespace>(client, context, kind, tx),
        ResourceKind::Events => {
            spawn_namespaced_watch::<KubeEvent>(client, context, kind, namespace, tx)
        }
        ResourceKind::ServiceAccounts => {
            spawn_namespaced_watch::<ServiceAccount>(client, context, kind, namespace, tx)
        }
        ResourceKind::Roles => spawn_namespaced_watch::<Role>(client, context, kind, namespace, tx),
        ResourceKind::RoleBindings => {
            spawn_namespaced_watch::<RoleBinding>(client, context, kind, namespace, tx)
        }
        ResourceKind::ClusterRoles => spawn_cluster_watch::<ClusterRole>(client, context, kind, tx),
        ResourceKind::ClusterRoleBindings => {
            spawn_cluster_watch::<ClusterRoleBinding>(client, context, kind, tx)
        }
        ResourceKind::NetworkPolicies => {
            spawn_namespaced_watch::<NetworkPolicy>(client, context, kind, namespace, tx)
        }
        ResourceKind::HorizontalPodAutoscalers => {
            spawn_namespaced_watch::<HorizontalPodAutoscaler>(client, context, kind, namespace, tx)
        }
        ResourceKind::PodDisruptionBudgets => {
            spawn_namespaced_watch::<PodDisruptionBudget>(client, context, kind, namespace, tx)
        }
    }
}

#[cfg(test)]
mod tests {
    use kube::{Error, error::Status};
    use kube_runtime::watcher;
    use serde_json::json;

    use super::{
        ActionError, KubeResourceProvider, api_resource_spec_for_kind, extract_status,
        is_forbidden_watcher_error, next_watch_backoff, select_warm_contexts,
    };
    use crate::{
        cluster::ActionExecutor,
        model::{ResourceKey, ResourceKind},
    };
    use kube::config::Kubeconfig;
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };
    use tokio::sync::Mutex;

    #[test]
    fn select_warm_contexts_respects_limit_and_ttl() {
        let now = Instant::now();
        let mut last_active = HashMap::new();
        last_active.insert("active".to_string(), now);
        last_active.insert("ctx-a".to_string(), now - Duration::from_secs(2));
        last_active.insert("ctx-b".to_string(), now - Duration::from_secs(5));
        last_active.insert("ctx-c".to_string(), now - Duration::from_secs(50));

        let keep = select_warm_contexts(&last_active, "active", now, 2, Duration::from_secs(20));
        assert_eq!(keep.len(), 2);
        assert!(keep.contains("ctx-a"));
        assert!(keep.contains("ctx-b"));
        assert!(!keep.contains("ctx-c"));
    }

    #[test]
    fn select_warm_contexts_bounded_under_many_contexts() {
        let now = Instant::now();
        let mut last_active = HashMap::new();
        last_active.insert("active".to_string(), now);
        for idx in 0..200 {
            last_active.insert(
                format!("ctx-{idx}"),
                now - Duration::from_secs(idx as u64 + 1),
            );
        }

        let keep = select_warm_contexts(&last_active, "active", now, 1, Duration::from_secs(120));
        assert!(keep.len() <= 1);
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn perf_select_warm_contexts_many_contexts() {
        let now = Instant::now();
        let mut last_active = HashMap::new();
        last_active.insert("active".to_string(), now);
        for idx in 0..5_000 {
            last_active.insert(
                format!("ctx-{idx}"),
                now - Duration::from_secs(idx as u64 + 1),
            );
        }

        let start = Instant::now();
        let mut total = 0usize;
        for _ in 0..10_000 {
            total = total.saturating_add(
                select_warm_contexts(&last_active, "active", now, 1, Duration::from_secs(30)).len(),
            );
        }
        let elapsed = start.elapsed();
        eprintln!(
            "[perf] select_warm_contexts total={} elapsed={:?} avg/op={:?}",
            total,
            elapsed,
            elapsed / 10_000
        );
    }

    #[test]
    fn watcher_forbidden_error_is_terminal() {
        let err = watcher::Error::WatchError(Box::new(Status {
            code: 403,
            message: "forbidden".to_string(),
            ..Status::default()
        }));
        assert!(is_forbidden_watcher_error(&err));
    }

    #[test]
    fn watcher_non_forbidden_error_is_retryable() {
        let err = watcher::Error::WatchError(Box::new(Status {
            code: 500,
            message: "internal server error".to_string(),
            ..Status::default()
        }));
        assert!(!is_forbidden_watcher_error(&err));
    }

    #[test]
    fn watcher_initial_list_forbidden_is_terminal() {
        let err = watcher::Error::InitialListFailed(Error::Api(Box::new(Status {
            code: 403,
            message: "rbac denied".to_string(),
            ..Status::default()
        })));
        assert!(is_forbidden_watcher_error(&err));
    }

    #[test]
    fn watch_backoff_resets_after_healthy_stream_duration() {
        let reset = next_watch_backoff(Duration::from_secs(8), Duration::from_secs(21));
        assert_eq!(reset, Duration::from_millis(500));
    }

    #[test]
    fn watch_backoff_grows_and_caps_on_fast_failures() {
        let mut current = Duration::from_millis(500);
        for _ in 0..10 {
            current = next_watch_backoff(current, Duration::from_secs(1));
        }
        assert_eq!(current, Duration::from_secs(30));
    }

    #[test]
    fn api_resource_spec_exists_for_all_supported_kinds() {
        for kind in ResourceKind::ORDERED {
            assert!(
                api_resource_spec_for_kind(kind).is_some(),
                "missing api resource spec for {:?}",
                kind
            );
        }
    }

    #[test]
    fn api_resource_spec_namespaced_flag_matches_kind_contract() {
        for kind in ResourceKind::ORDERED {
            let spec = api_resource_spec_for_kind(kind).expect("spec exists");
            assert_eq!(
                spec.namespaced,
                kind.is_namespaced(),
                "namespaced mismatch for {:?}",
                kind
            );
        }
    }

    #[test]
    fn pod_status_prefers_terminating_when_deletion_timestamp_present() {
        let raw = json!({
            "metadata": { "deletionTimestamp": "2026-03-08T14:12:00Z" },
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    { "state": { "waiting": { "reason": "CrashLoopBackOff" } } }
                ]
            }
        });

        let status = extract_status(ResourceKind::Pods, &raw);
        assert_eq!(status, "Terminating");
    }

    #[test]
    fn pod_status_uses_container_waiting_reason_when_available() {
        let raw = json!({
            "status": {
                "phase": "Running",
                "containerStatuses": [
                    { "state": { "waiting": { "reason": "ImagePullBackOff" } } }
                ]
            }
        });

        let status = extract_status(ResourceKind::Pods, &raw);
        assert_eq!(status, "ImagePullBackOff");
    }

    fn readonly_provider() -> KubeResourceProvider {
        KubeResourceProvider {
            contexts: vec!["ctx".to_string()],
            default_context: Some("ctx".to_string()),
            kubeconfig: Kubeconfig::default(),
            readonly: true,
            warm_contexts: 1,
            warm_context_ttl: Duration::from_secs(30),
            clients: std::sync::Arc::new(Mutex::new(HashMap::new())),
            watched: std::sync::Arc::new(Mutex::new(HashMap::new())),
            context_last_active: std::sync::Arc::new(Mutex::new(HashMap::new())),
            delta_tx: std::sync::Arc::new(Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn delete_resource_is_blocked_in_readonly_mode() {
        let provider = readonly_provider();
        let key = ResourceKey::new(
            "ctx",
            ResourceKind::Pods,
            Some("default".to_string()),
            "demo",
        );
        let err = provider
            .delete_resource(&key)
            .await
            .expect_err("readonly delete must fail");
        assert!(matches!(err, ActionError::ReadOnly));
    }

    #[tokio::test]
    async fn replace_resource_is_blocked_in_readonly_mode() {
        let provider = readonly_provider();
        let key = ResourceKey::new(
            "ctx",
            ResourceKind::Pods,
            Some("default".to_string()),
            "demo",
        );
        let err = provider
            .replace_resource(&key, serde_json::json!({}))
            .await
            .expect_err("readonly replace must fail");
        assert!(matches!(err, ActionError::ReadOnly));
    }
}
