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
    api::{DeleteParams, ListParams, LogParams, PostParams},
    config::{KubeConfigOptions, Kubeconfig},
    core::{ApiResource, DynamicObject, GroupVersionKind},
    discovery::{Discovery, Scope},
};
use kube_runtime::watcher::{self, Event};
use serde::Serialize;
use tokio::{sync::mpsc, task::JoinHandle, time::sleep};
use tracing::{error, warn};

use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

use super::{
    ActionError, ActionExecutor, ActionResult, DiscoveredResource, DynamicRow, EventRow, NodeUsage,
    PodLogEvent, PodLogRequest, PodLogStream, PodUsage, ResourceProvider, WatchTarget,
    extract::{extracted_for, usage_from_metrics},
};

/// Bounded one-shot dynamic list size (Phase 4.1 read-only browse).
const DYNAMIC_LIST_LIMIT: u32 = 500;

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

        // Optional eager auth/client warmup. Best-effort: a context that fails to authenticate
        // (e.g. an expired SSO token) must not abort startup — the other clusters stay usable and
        // the broken one surfaces its error when its watches run. Warmup just primes the cache.
        if options.all_contexts {
            let contexts = provider.contexts.clone();
            for context in contexts {
                if let Err(err) = provider.client_for_context(&context).await {
                    warn!(context = %context, error = %err, "context auth/client warmup failed");
                }
            }
        } else if let Some(context) = provider.default_context.clone()
            && let Err(err) = provider.client_for_context(&context).await
        {
            warn!(context = %context, error = %err, "default context auth/client warmup failed");
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
        // A context whose client can't be built (e.g. expired SSO / exec-auth failure) records a
        // per-context error for the UI instead of failing the call — other contexts keep working,
        // and a later retry recovers once credentials are refreshed.
        let client = match self.client_for_context(context).await {
            Ok(client) => client,
            Err(err) => {
                let _ = tx
                    .send(StateDelta::Error {
                        context: context.to_string(),
                        message: format!("auth/connect failed: {err}"),
                    })
                    .await;
                return Ok(());
            }
        };
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
        let contexts_before: HashSet<String> =
            watched.keys().map(|key| key.context.clone()).collect();
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

        // Contexts that lost all their watchers (dropped from active + warm) get their cached
        // entities evicted so store memory stays bounded (Phase 1.3).
        let contexts_after: HashSet<String> =
            watched.keys().map(|key| key.context.clone()).collect();
        drop(watched);
        for evicted in contexts_before.difference(&contexts_after) {
            if evicted != context {
                let _ = tx
                    .send(StateDelta::EvictContext {
                        context: evicted.clone(),
                    })
                    .await;
            }
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

    async fn get_object(&self, key: &ResourceKey) -> anyhow::Result<serde_json::Value> {
        let client = self.client_for_context(&key.context).await?;
        let spec = api_resource_spec_for_kind(key.kind)
            .with_context(|| format!("no API mapping for {}", key.kind))?;
        let gvk = GroupVersionKind::gvk(spec.group, spec.version, spec.kind);
        let mut ar = ApiResource::from_gvk(&gvk);
        ar.plural = spec.plural.to_string();

        let obj: DynamicObject = if spec.namespaced {
            let namespace = key.namespace.as_deref().unwrap_or("default");
            let api: Api<DynamicObject> = Api::namespaced_with(client, namespace, &ar);
            api.get(&key.name).await?
        } else {
            let api: Api<DynamicObject> = Api::all_with(client, &ar);
            api.get(&key.name).await?
        };

        Ok(serde_json::to_value(obj)?)
    }

    async fn discover(&self, context: &str) -> anyhow::Result<Vec<DiscoveredResource>> {
        let client = self.client_for_context(context).await?;
        let discovery = Discovery::new(client).run().await?;
        let mut out = Vec::new();
        for group in discovery.groups() {
            for (api_resource, caps) in group.recommended_resources() {
                out.push(DiscoveredResource {
                    api_resource,
                    namespaced: matches!(caps.scope, Scope::Namespaced),
                });
            }
        }
        out.sort_by(|a, b| a.full_name().cmp(&b.full_name()));
        out.dedup_by(|a, b| a.full_name() == b.full_name());
        Ok(out)
    }

    async fn list_dynamic(
        &self,
        context: &str,
        resource: &DiscoveredResource,
        namespace: Option<&str>,
    ) -> anyhow::Result<Vec<DynamicRow>> {
        let client = self.client_for_context(context).await?;
        let ar = &resource.api_resource;
        let api: Api<DynamicObject> = match (resource.namespaced, namespace) {
            (true, Some(ns)) => Api::namespaced_with(client, ns, ar),
            _ => Api::all_with(client, ar),
        };
        let list = api
            .list(&ListParams::default().limit(DYNAMIC_LIST_LIMIT))
            .await?;
        let rows = list
            .into_iter()
            .map(|obj| DynamicRow {
                namespace: obj.namespace(),
                age: obj
                    .creation_timestamp()
                    .map(|ts| DateTime::<Utc>::from(std::time::SystemTime::from(ts.0))),
                status: dynamic_status(&obj),
                name: obj.name_any(),
            })
            .collect();
        Ok(rows)
    }

    async fn get_dynamic(
        &self,
        context: &str,
        resource: &DiscoveredResource,
        namespace: Option<&str>,
        name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let client = self.client_for_context(context).await?;
        let ar = &resource.api_resource;
        let api: Api<DynamicObject> = match (resource.namespaced, namespace) {
            (true, Some(ns)) => Api::namespaced_with(client, ns, ar),
            (true, None) => Api::namespaced_with(client, "default", ar),
            (false, _) => Api::all_with(client, ar),
        };
        Ok(serde_json::to_value(api.get(name).await?)?)
    }

    async fn node_metrics(&self, context: &str) -> anyhow::Result<Vec<NodeUsage>> {
        let client = self.client_for_context(context).await?;
        let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "NodeMetrics");
        let mut ar = ApiResource::from_gvk(&gvk);
        ar.plural = "nodes".to_string();
        let api: Api<DynamicObject> = Api::all_with(client, &ar);
        let list = api.list(&ListParams::default()).await?;
        let rows = list
            .into_iter()
            .filter_map(|obj| {
                let usage = obj.data.get("usage")?;
                let (cpu_used_m, mem_used_b) = usage_from_metrics(usage)?;
                Some(NodeUsage {
                    cpu_used_m,
                    mem_used_b,
                })
            })
            .collect();
        Ok(rows)
    }

    async fn pod_metrics(
        &self,
        context: &str,
        namespace: Option<&str>,
    ) -> anyhow::Result<Vec<PodUsage>> {
        let client = self.client_for_context(context).await?;
        let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
        let mut ar = ApiResource::from_gvk(&gvk);
        ar.plural = "pods".to_string();
        let api: Api<DynamicObject> = match namespace {
            Some(ns) => Api::namespaced_with(client, ns, &ar),
            None => Api::all_with(client, &ar),
        };
        let list = api.list(&ListParams::default()).await?;
        let rows = list
            .into_iter()
            .filter_map(|obj| {
                let namespace = obj.namespace()?;
                let name = obj.name_any();
                // Sum usage across the pod's containers.
                let containers = obj.data.get("containers").and_then(|v| v.as_array())?;
                let (mut cpu_used_m, mut mem_used_b) = (0u64, 0u64);
                for container in containers {
                    if let Some(usage) = container.get("usage").and_then(usage_from_metrics) {
                        cpu_used_m = cpu_used_m.saturating_add(usage.0);
                        mem_used_b = mem_used_b.saturating_add(usage.1);
                    }
                }
                Some(PodUsage {
                    namespace,
                    name,
                    cpu_used_m,
                    mem_used_b,
                })
            })
            .collect();
        Ok(rows)
    }

    async fn events_for(
        &self,
        context: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> anyhow::Result<Vec<EventRow>> {
        let client = self.client_for_context(context).await?;
        let api: Api<KubeEvent> = match namespace {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        let params = ListParams::default().fields(&format!("involvedObject.name={name}"));
        let list = api.list(&params).await?;
        let mut rows: Vec<EventRow> = list
            .into_iter()
            .map(|event| {
                let last = event
                    .last_timestamp
                    .map(|t| DateTime::<Utc>::from(std::time::SystemTime::from(t.0)))
                    .or_else(|| {
                        event
                            .event_time
                            .map(|t| DateTime::<Utc>::from(std::time::SystemTime::from(t.0)))
                    });
                let source = event
                    .source
                    .and_then(|s| s.component)
                    .or(event.reporting_component)
                    .unwrap_or_default();
                EventRow {
                    type_: event.type_.unwrap_or_default(),
                    reason: event.reason.unwrap_or_default(),
                    message: event.message.unwrap_or_default(),
                    count: event.count.unwrap_or(1),
                    last,
                    source,
                }
            })
            .collect();
        rows.sort_by(|a, b| b.last.cmp(&a.last));
        Ok(rows)
    }
}

/// Best-effort status string for a dynamic object (no curated extractor for unknown kinds).
fn dynamic_status(obj: &DynamicObject) -> String {
    obj.data
        .pointer("/status/phase")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            obj.data
                .pointer("/status/conditions/0/type")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("-")
        .to_string()
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
            // (Re)list starting: mark the current set stale but keep showing it (no blank window).
            tx.send(StateDelta::RelistStart {
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
            // Initial list fully emitted: sweep entities not refreshed during this relist.
            tx.send(StateDelta::RelistEnd {
                context: context.to_string(),
                kind,
            })
            .await
            .map_err(|_| ())?;
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
    let columns = extract_columns(kind, &raw);
    // Extract the few scalar fields the all-rows hot paths need, then drop the full JSON.
    let extracted = extracted_for(kind, &raw);

    Some(ResourceEntity {
        key,
        status,
        age,
        labels,
        columns,
        extracted,
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

/// Produce the kind-specific column values for an entity, in the exact order declared by
/// [`ResourceKind::extra_columns`]. The lengths must match (enforced by `kind_columns_contract`).
fn extract_columns(kind: ResourceKind, raw: &serde_json::Value) -> Vec<String> {
    // Small readers over the raw JSON.
    let s = |p: &str| {
        raw.pointer(p)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let i = |p: &str| raw.pointer(p).and_then(serde_json::Value::as_i64);
    let int_cell = |p: &str| i(p).unwrap_or(0).to_string();
    // Count of keys in an object (ConfigMap/Secret `data`) or elements in an array.
    let count = |p: &str| match raw.pointer(p) {
        Some(serde_json::Value::Object(m)) => m.len().to_string(),
        Some(serde_json::Value::Array(a)) => a.len().to_string(),
        _ => "0".to_string(),
    };
    let dash = || "-".to_string();

    match kind {
        ResourceKind::Deployments => vec![
            int_cell("/status/updatedReplicas"),
            int_cell("/status/availableReplicas"),
        ],
        ResourceKind::ReplicaSets => vec![
            int_cell("/spec/replicas"),
            int_cell("/status/replicas"),
            int_cell("/status/readyReplicas"),
        ],
        ResourceKind::StatefulSets => vec![format!(
            "{}/{}",
            i("/status/readyReplicas").unwrap_or(0),
            i("/spec/replicas").unwrap_or(0)
        )],
        ResourceKind::DaemonSets => vec![
            int_cell("/status/desiredNumberScheduled"),
            int_cell("/status/numberReady"),
            int_cell("/status/numberAvailable"),
        ],
        ResourceKind::Services => vec![
            s("/spec/type").unwrap_or_else(|| "ClusterIP".to_string()),
            s("/spec/clusterIP").unwrap_or_else(dash),
            service_ports(raw),
        ],
        ResourceKind::Ingresses => vec![
            s("/spec/ingressClassName").unwrap_or_else(dash),
            ingress_hosts(raw),
            ingress_address(raw),
        ],
        ResourceKind::Jobs => vec![
            format!(
                "{}/{}",
                i("/status/succeeded").unwrap_or(0),
                i("/spec/completions")
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "1".to_string())
            ),
            job_duration(raw),
        ],
        ResourceKind::CronJobs => vec![
            s("/spec/schedule").unwrap_or_else(dash),
            raw.pointer("/spec/suspend")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                .to_string(),
            count("/status/active"),
        ],
        ResourceKind::ConfigMaps => vec![count("/data")],
        ResourceKind::Secrets => vec![
            s("/type").unwrap_or_else(|| "Opaque".to_string()),
            count("/data"),
        ],
        ResourceKind::Nodes => vec![
            node_roles(raw),
            s("/status/nodeInfo/kubeletVersion").unwrap_or_else(dash),
        ],
        ResourceKind::PersistentVolumeClaims => vec![
            s("/spec/volumeName").unwrap_or_else(dash),
            s("/status/capacity/storage").unwrap_or_else(dash),
            access_modes(raw),
            s("/spec/storageClassName").unwrap_or_else(dash),
        ],
        ResourceKind::PersistentVolumes => vec![
            s("/spec/capacity/storage").unwrap_or_else(dash),
            access_modes(raw),
            s("/spec/persistentVolumeReclaimPolicy").unwrap_or_else(dash),
            pv_claim(raw),
            s("/spec/storageClassName").unwrap_or_else(dash),
        ],
        ResourceKind::HorizontalPodAutoscalers => vec![
            format!(
                "{}/{}",
                s("/spec/scaleTargetRef/kind").unwrap_or_else(dash),
                s("/spec/scaleTargetRef/name").unwrap_or_else(dash)
            ),
            int_cell("/spec/minReplicas"),
            int_cell("/spec/maxReplicas"),
            int_cell("/status/currentReplicas"),
        ],
        ResourceKind::ServiceAccounts => vec![count("/secrets"), count("/imagePullSecrets")],
        ResourceKind::RoleBindings | ResourceKind::ClusterRoleBindings => {
            vec![role_ref(raw), subjects_summary(raw)]
        }
        ResourceKind::PodDisruptionBudgets => vec![
            int_or_str("/spec/minAvailable", raw),
            int_or_str("/spec/maxUnavailable", raw),
            int_cell("/status/disruptionsAllowed"),
        ],
        _ => vec![],
    }
}

/// `spec.ports[]` rendered as `port/proto` joined by commas (e.g. `80/TCP,443/TCP`).
fn service_ports(raw: &serde_json::Value) -> String {
    let Some(ports) = raw.pointer("/spec/ports").and_then(|v| v.as_array()) else {
        return "-".to_string();
    };
    let rendered: Vec<String> = ports
        .iter()
        .map(|p| {
            let port = p.pointer("/port").and_then(serde_json::Value::as_i64);
            let proto = p
                .pointer("/protocol")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("TCP");
            match port {
                Some(port) => format!("{port}/{proto}"),
                None => proto.to_string(),
            }
        })
        .collect();
    if rendered.is_empty() {
        "-".to_string()
    } else {
        rendered.join(",")
    }
}

/// Distinct `spec.rules[].host` values joined by commas.
fn ingress_hosts(raw: &serde_json::Value) -> String {
    let Some(rules) = raw.pointer("/spec/rules").and_then(|v| v.as_array()) else {
        return "-".to_string();
    };
    let hosts: Vec<&str> = rules
        .iter()
        .filter_map(|r| r.pointer("/host").and_then(serde_json::Value::as_str))
        .collect();
    if hosts.is_empty() {
        "*".to_string()
    } else {
        hosts.join(",")
    }
}

/// First `status.loadBalancer.ingress[]` ip or hostname.
fn ingress_address(raw: &serde_json::Value) -> String {
    raw.pointer("/status/loadBalancer/ingress")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|e| {
            e.pointer("/ip")
                .and_then(serde_json::Value::as_str)
                .or_else(|| e.pointer("/hostname").and_then(serde_json::Value::as_str))
        })
        .map(str::to_string)
        .unwrap_or_else(|| "-".to_string())
}

/// Job wall-clock duration from `status.startTime` to `status.completionTime` (or now).
fn job_duration(raw: &serde_json::Value) -> String {
    let parse = |p: &str| {
        raw.pointer(p)
            .and_then(serde_json::Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    let Some(start) = parse("/status/startTime") else {
        return "-".to_string();
    };
    let end = parse("/status/completionTime").unwrap_or_else(Utc::now);
    let secs = end.signed_duration_since(start).num_seconds().max(0);
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Role names from `node-role.kubernetes.io/<role>` labels, joined by commas.
fn node_roles(raw: &serde_json::Value) -> String {
    let Some(labels) = raw.pointer("/metadata/labels").and_then(|v| v.as_object()) else {
        return "<none>".to_string();
    };
    let roles: Vec<&str> = labels
        .keys()
        .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
        .filter(|r| !r.is_empty())
        .collect();
    if roles.is_empty() {
        "<none>".to_string()
    } else {
        roles.join(",")
    }
}

/// `spec.accessModes[]` abbreviated (RWO/ROX/RWX/RWOP) and joined.
fn access_modes(raw: &serde_json::Value) -> String {
    let Some(modes) = raw.pointer("/spec/accessModes").and_then(|v| v.as_array()) else {
        return "-".to_string();
    };
    let abbr: Vec<&str> = modes
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(|m| match m {
            "ReadWriteOnce" => "RWO",
            "ReadOnlyMany" => "ROX",
            "ReadWriteMany" => "RWX",
            "ReadWriteOncePod" => "RWOP",
            other => other,
        })
        .collect();
    if abbr.is_empty() {
        "-".to_string()
    } else {
        abbr.join(",")
    }
}

/// PV bound claim as `namespace/name`.
fn pv_claim(raw: &serde_json::Value) -> String {
    let ns = raw
        .pointer("/spec/claimRef/namespace")
        .and_then(serde_json::Value::as_str);
    let name = raw
        .pointer("/spec/claimRef/name")
        .and_then(serde_json::Value::as_str);
    match (ns, name) {
        (Some(ns), Some(name)) => format!("{ns}/{name}"),
        (None, Some(name)) => name.to_string(),
        _ => "-".to_string(),
    }
}

/// Role/ClusterRole referenced by a (Cluster)RoleBinding, as `kind/name` (e.g. `ClusterRole/view`).
fn role_ref(raw: &serde_json::Value) -> String {
    let kind = raw
        .pointer("/roleRef/kind")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    let name = raw
        .pointer("/roleRef/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    format!("{kind}/{name}")
}

/// Binding subjects as `abbrev:name` joined by commas (sa/u/g for ServiceAccount/User/Group).
fn subjects_summary(raw: &serde_json::Value) -> String {
    let Some(subjects) = raw.pointer("/subjects").and_then(|v| v.as_array()) else {
        return "-".to_string();
    };
    let parts: Vec<String> = subjects
        .iter()
        .map(|s| {
            let kind = s.pointer("/kind").and_then(serde_json::Value::as_str);
            let name = s
                .pointer("/name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let abbr = match kind {
                Some("ServiceAccount") => "sa",
                Some("User") => "u",
                Some("Group") => "g",
                Some(other) => other,
                None => "?",
            };
            format!("{abbr}:{name}")
        })
        .collect();
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}

/// A field that may be an integer or a percentage string (e.g. PDB `minAvailable`).
fn int_or_str(pointer: &str, raw: &serde_json::Value) -> String {
    match raw.pointer(pointer) {
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => "-".to_string(),
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
        ActionError, KubeResourceProvider, api_resource_spec_for_kind, extract_columns,
        extract_status, is_forbidden_watcher_error, next_watch_backoff, select_warm_contexts,
    };
    use crate::{
        cluster::{ActionExecutor, ResourceProvider, WatchTarget},
        model::{ResourceKey, ResourceKind, StateDelta},
    };
    use kube::config::Kubeconfig;
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    // Representative non-trivial pod (labels, annotations, 2 containers w/ env+resources, status).
    fn representative_pod_json() -> serde_json::Value {
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {
                "name": "api-7d9f8c6b5d-abcde", "namespace": "team-payments",
                "uid": "f1e2d3c4-b5a6-7890-1234-567890abcdef",
                "resourceVersion": "1048576", "creationTimestamp": "2026-06-20T10:00:00Z",
                "labels": {
                    "app": "api", "app.kubernetes.io/name": "api", "app.kubernetes.io/instance": "api-prod",
                    "app.kubernetes.io/version": "1.42.0", "pod-template-hash": "7d9f8c6b5d",
                    "team": "payments", "tier": "backend", "env": "prod"
                },
                "annotations": {
                    "kubectl.kubernetes.io/restartedAt": "2026-06-20T10:00:00Z",
                    "prometheus.io/scrape": "true", "prometheus.io/port": "9090",
                    "checksum/config": "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211",
                    "sidecar.istio.io/status": "{\"version\":\"1.20\",\"initContainers\":[\"istio-init\"],\"containers\":[\"istio-proxy\"]}"
                },
                "ownerReferences": [ { "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api-7d9f8c6b5d", "uid": "aabbccdd", "controller": true } ]
            },
            "spec": {
                "nodeName": "ip-10-0-12-34.eu-north-1.compute.internal",
                "serviceAccountName": "api",
                "containers": [
                    {
                        "name": "api", "image": "registry.example.com/payments/api:1.42.0",
                        "ports": [ { "containerPort": 8080, "name": "http" }, { "containerPort": 9090, "name": "metrics" } ],
                        "env": [
                            { "name": "LOG_LEVEL", "value": "info" }, { "name": "DB_HOST", "value": "aurora.internal" },
                            { "name": "DB_PORT", "value": "5432" }, { "name": "POD_IP", "valueFrom": { "fieldRef": { "fieldPath": "status.podIP" } } }
                        ],
                        "resources": { "requests": { "cpu": "250m", "memory": "256Mi" }, "limits": { "cpu": "1", "memory": "512Mi" } },
                        "volumeMounts": [ { "name": "config", "mountPath": "/etc/api" }, { "name": "kube-api-access", "mountPath": "/var/run/secrets/kubernetes.io/serviceaccount", "readOnly": true } ]
                    },
                    {
                        "name": "istio-proxy", "image": "docker.io/istio/proxyv2:1.20",
                        "resources": { "requests": { "cpu": "100m", "memory": "128Mi" }, "limits": { "cpu": "2", "memory": "1Gi" } },
                        "env": [ { "name": "PILOT_CERT_PROVIDER", "value": "istiod" } ]
                    }
                ]
            },
            "status": {
                "phase": "Running", "podIP": "10.0.12.200", "hostIP": "10.0.12.34", "startTime": "2026-06-20T10:00:01Z",
                "conditions": [
                    { "type": "Initialized", "status": "True", "lastTransitionTime": "2026-06-20T10:00:02Z" },
                    { "type": "Ready", "status": "True", "lastTransitionTime": "2026-06-20T10:00:10Z" },
                    { "type": "ContainersReady", "status": "True", "lastTransitionTime": "2026-06-20T10:00:10Z" },
                    { "type": "PodScheduled", "status": "True", "lastTransitionTime": "2026-06-20T10:00:00Z" }
                ],
                "containerStatuses": [
                    { "name": "api", "ready": true, "restartCount": 0, "image": "registry.example.com/payments/api:1.42.0", "started": true, "state": { "running": { "startedAt": "2026-06-20T10:00:05Z" } } },
                    { "name": "istio-proxy", "ready": true, "restartCount": 1, "image": "docker.io/istio/proxyv2:1.20", "started": true, "state": { "running": { "startedAt": "2026-06-20T10:00:06Z" } } }
                ]
            }
        })
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn perf_to_value_vs_dynamic_parse() {
        use k8s_openapi::api::core::v1::Pod;
        use kube::core::DynamicObject;

        let wire = serde_json::to_string(&representative_pod_json()).expect("serialize");
        let iters = 50_000u32;

        // Current ingest path: parse wire -> typed Pod, then serde_json::to_value(pod).
        let pod: Pod = serde_json::from_str(&wire).expect("pod parse");
        let start = Instant::now();
        let mut acc = 0usize;
        for _ in 0..iters {
            let v = serde_json::to_value(&pod).expect("to_value");
            acc = acc.wrapping_add(v.as_object().map(|o| o.len()).unwrap_or(0));
        }
        let to_value_per = start.elapsed() / iters;

        // Parse cost comparison: typed Pod vs DynamicObject (proposed ingest).
        let start = Instant::now();
        for _ in 0..iters {
            let p: Pod = serde_json::from_str(&wire).expect("pod parse");
            acc = acc.wrapping_add(p.metadata.name.is_some() as usize);
        }
        let pod_parse_per = start.elapsed() / iters;

        let start = Instant::now();
        for _ in 0..iters {
            let d: DynamicObject = serde_json::from_str(&wire).expect("dynamic parse");
            acc = acc.wrapping_add(d.metadata.name.is_some() as usize);
        }
        let dyn_parse_per = start.elapsed() / iters;

        std::hint::black_box(acc);
        eprintln!(
            "[perf] wire={}B  to_value/obj={:?}  pod_parse/obj={:?}  dyn_parse/obj={:?}",
            wire.len(),
            to_value_per,
            pod_parse_per,
            dyn_parse_per
        );
        eprintln!(
            "[perf] current ingest serde ≈ pod_parse+to_value = {:?}/obj  -> 10k = {:?}",
            pod_parse_per + to_value_per,
            (pod_parse_per + to_value_per) * 10_000
        );
        eprintln!(
            "[perf] proposed (dynamic) serde ≈ dyn_parse = {:?}/obj  -> 10k = {:?}",
            dyn_parse_per,
            dyn_parse_per * 10_000
        );
    }

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

    #[test]
    fn extract_columns_matches_declared_header_count_for_every_kind() {
        // The header (ResourceKind::extra_columns) and the per-row values (extract_columns) must
        // stay in lockstep; an empty object exercises every default branch.
        for kind in ResourceKind::ORDERED {
            let values = extract_columns(kind, &json!({}));
            assert_eq!(
                values.len(),
                kind.extra_columns().len(),
                "{kind} column count mismatch: {values:?} vs headers {:?}",
                kind.extra_columns()
            );
        }
    }

    #[test]
    fn extract_columns_renders_deployment_and_service_fields() {
        let deploy = json!({
            "spec": { "replicas": 3 },
            "status": { "readyReplicas": 2, "updatedReplicas": 3, "availableReplicas": 2 }
        });
        // headers: UP-TO-DATE, AVAILABLE
        assert_eq!(
            extract_columns(ResourceKind::Deployments, &deploy),
            vec!["3".to_string(), "2".to_string()]
        );

        let svc = json!({
            "spec": {
                "type": "LoadBalancer",
                "clusterIP": "10.0.0.5",
                "ports": [ { "port": 80, "protocol": "TCP" }, { "port": 443, "protocol": "TCP" } ]
            }
        });
        // headers: TYPE, CLUSTER-IP, PORTS
        assert_eq!(
            extract_columns(ResourceKind::Services, &svc),
            vec![
                "LoadBalancer".to_string(),
                "10.0.0.5".to_string(),
                "80/TCP,443/TCP".to_string()
            ]
        );
    }

    #[test]
    fn extract_columns_abbreviates_pvc_access_modes() {
        let pvc = json!({
            "spec": {
                "volumeName": "pv-001",
                "accessModes": ["ReadWriteOnce"],
                "storageClassName": "gp3"
            },
            "status": { "capacity": { "storage": "10Gi" } }
        });
        // headers: VOLUME, CAPACITY, ACCESS, STORAGECLASS
        assert_eq!(
            extract_columns(ResourceKind::PersistentVolumeClaims, &pvc),
            vec![
                "pv-001".to_string(),
                "10Gi".to_string(),
                "RWO".to_string(),
                "gp3".to_string()
            ]
        );
    }

    #[test]
    fn extract_columns_summarizes_rolebinding_role_and_subjects() {
        let rb = json!({
            "roleRef": { "kind": "ClusterRole", "name": "view" },
            "subjects": [
                { "kind": "ServiceAccount", "name": "build", "namespace": "ci" },
                { "kind": "User", "name": "alice" }
            ]
        });
        // headers: ROLE, SUBJECTS
        assert_eq!(
            extract_columns(ResourceKind::RoleBindings, &rb),
            vec![
                "ClusterRole/view".to_string(),
                "sa:build,u:alice".to_string()
            ]
        );
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
    async fn watch_plan_records_error_for_unauthorized_context_without_failing() {
        // The readonly provider's "ctx" has no entry in its (empty) kubeconfig, so building a
        // client fails — standing in for an expired-auth/unreachable context.
        let provider = readonly_provider();
        let (tx, mut rx) = mpsc::channel(8);
        provider.start(tx).await.expect("start");

        let targets = [WatchTarget {
            kind: ResourceKind::Pods,
            namespace: None,
        }];
        let result = provider.replace_watch_plan("ctx", &targets).await;
        assert!(result.is_ok(), "a bad context must not fail the call");

        match rx.try_recv() {
            Ok(StateDelta::Error { context, message }) => {
                assert_eq!(context, "ctx");
                assert!(message.contains("auth/connect failed"), "{message}");
            }
            other => panic!("expected a per-context Error delta, got {other:?}"),
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
