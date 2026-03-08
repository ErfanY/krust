use crate::model::ResourceKind;

pub(crate) enum ResourceAlias {
    Supported(ResourceKind),
    Unsupported(&'static str),
    Unknown,
}

pub(crate) fn command_names() -> &'static [&'static str] {
    &[
        "ctx",
        "context",
        "contexts",
        "ctxs",
        "ns",
        "namespace",
        "all",
        "0",
        "kind",
        "resources",
        "res",
        "aliases",
        "clear",
        "clear-filter",
        "fmt",
        "format",
        "yaml",
        "yml",
        "json",
        "c",
        "container",
        "containers",
        "sources",
        "src",
        "pause",
        "resume",
        "edit",
        "tail",
        "copy",
        "yank",
        "dump",
        "help",
        "?",
        "quit",
        "exit",
        "q",
        "pulse",
        "pulses",
        "pu",
        "xray",
        "popeye",
        "pop",
        "plugins",
        "plugin",
        "screendump",
        "sd",
    ]
}

pub(crate) fn resource_alias_names() -> &'static [&'static str] {
    &[
        "po",
        "pod",
        "pods",
        "deploy",
        "dp",
        "deployment",
        "deployments",
        "rs",
        "replicaset",
        "replicasets",
        "sts",
        "statefulset",
        "statefulsets",
        "ds",
        "daemonset",
        "daemonsets",
        "svc",
        "service",
        "services",
        "ing",
        "ingress",
        "ingresses",
        "cm",
        "configmap",
        "configmaps",
        "sec",
        "secret",
        "secrets",
        "job",
        "jobs",
        "cj",
        "cronjob",
        "cronjobs",
        "pvc",
        "pvcs",
        "claim",
        "claims",
        "pv",
        "pvs",
        "no",
        "node",
        "nodes",
        "ns",
        "namespace",
        "namespaces",
        "ev",
        "event",
        "events",
        "sa",
        "serviceaccount",
        "serviceaccounts",
        "role",
        "roles",
        "rb",
        "rolebinding",
        "rolebindings",
        "crole",
        "clusterrole",
        "clusterroles",
        "crb",
        "clusterrolebinding",
        "clusterrolebindings",
        "netpol",
        "np",
        "networkpolicy",
        "networkpolicies",
        "hpa",
        "hpas",
        "pdb",
        "pdbs",
    ]
}

pub(crate) fn parse_resource_alias(token: &str) -> ResourceAlias {
    let normalized = token.to_ascii_lowercase();
    match normalized.as_str() {
        "pods" | "pod" | "po" => ResourceAlias::Supported(ResourceKind::Pods),
        "deployments" | "deployment" | "deploy" | "dp" => {
            ResourceAlias::Supported(ResourceKind::Deployments)
        }
        "replicasets" | "replicaset" | "rs" => ResourceAlias::Supported(ResourceKind::ReplicaSets),
        "statefulsets" | "statefulset" | "sts" => {
            ResourceAlias::Supported(ResourceKind::StatefulSets)
        }
        "daemonsets" | "daemonset" | "ds" => ResourceAlias::Supported(ResourceKind::DaemonSets),
        "services" | "service" | "svc" | "svcs" => ResourceAlias::Supported(ResourceKind::Services),
        "ingresses" | "ingress" | "ing" => ResourceAlias::Supported(ResourceKind::Ingresses),
        "configmaps" | "configmap" | "cm" => ResourceAlias::Supported(ResourceKind::ConfigMaps),
        "secrets" | "secret" | "sec" | "se" => ResourceAlias::Supported(ResourceKind::Secrets),
        "jobs" | "job" => ResourceAlias::Supported(ResourceKind::Jobs),
        "cronjobs" | "cronjob" | "cj" => ResourceAlias::Supported(ResourceKind::CronJobs),
        "pvcs"
        | "pvc"
        | "persistentvolumeclaim"
        | "persistentvolumeclaims"
        | "claim"
        | "claims" => ResourceAlias::Supported(ResourceKind::PersistentVolumeClaims),
        "pvs" | "pv" | "persistentvolume" | "persistentvolumes" => {
            ResourceAlias::Supported(ResourceKind::PersistentVolumes)
        }
        "nodes" | "node" | "no" => ResourceAlias::Supported(ResourceKind::Nodes),
        "namespaces" | "namespace" | "ns" => ResourceAlias::Supported(ResourceKind::Namespaces),
        "events" | "event" | "ev" => ResourceAlias::Supported(ResourceKind::Events),
        "serviceaccounts" | "serviceaccount" | "sa" => {
            ResourceAlias::Supported(ResourceKind::ServiceAccounts)
        }
        "roles" | "role" => ResourceAlias::Supported(ResourceKind::Roles),
        "rolebindings" | "rolebinding" | "rb" => {
            ResourceAlias::Supported(ResourceKind::RoleBindings)
        }
        "clusterroles" | "clusterrole" | "crole" => {
            ResourceAlias::Supported(ResourceKind::ClusterRoles)
        }
        "clusterrolebindings" | "clusterrolebinding" | "crb" => {
            ResourceAlias::Supported(ResourceKind::ClusterRoleBindings)
        }
        "networkpolicies" | "networkpolicy" | "netpol" | "np" => {
            ResourceAlias::Supported(ResourceKind::NetworkPolicies)
        }
        "hpas" | "hpa" | "horizontalpodautoscaler" | "horizontalpodautoscalers" => {
            ResourceAlias::Supported(ResourceKind::HorizontalPodAutoscalers)
        }
        "pdbs" | "pdb" | "poddisruptionbudget" | "poddisruptionbudgets" => {
            ResourceAlias::Supported(ResourceKind::PodDisruptionBudgets)
        }
        "all" | "*" => ResourceAlias::Unsupported("all resources"),
        "api" | "apis" | "apiservice" | "apiservices" => ResourceAlias::Unsupported("API services"),
        "crd" | "crds" | "customresourcedefinition" | "customresourcedefinitions" => {
            ResourceAlias::Unsupported("CustomResourceDefinitions")
        }
        "cr" | "customresources" => ResourceAlias::Unsupported("generic custom resources"),
        "ep" | "endpoint" | "endpoints" => ResourceAlias::Unsupported("Endpoints"),
        "eps" | "endpointslice" | "endpointslices" => ResourceAlias::Unsupported("EndpointSlices"),
        "rc" | "replicationcontroller" | "replicationcontrollers" => {
            ResourceAlias::Unsupported("ReplicationControllers")
        }
        "cs" | "componentstatus" | "componentstatuses" => {
            ResourceAlias::Unsupported("ComponentStatuses")
        }
        "csr" | "certificatesigningrequest" | "certificatesigningrequests" => {
            ResourceAlias::Unsupported("CertificateSigningRequests")
        }
        "sc" | "storageclass" | "storageclasses" => ResourceAlias::Unsupported("StorageClasses"),
        "ingclass" | "ingressclass" | "ingressclasses" => {
            ResourceAlias::Unsupported("IngressClasses")
        }
        "limits" | "limitrange" | "limitranges" | "lr" => ResourceAlias::Unsupported("LimitRanges"),
        "quota" | "resourcequota" | "resourcequotas" | "rq" => {
            ResourceAlias::Unsupported("ResourceQuotas")
        }
        "pc" | "priorityclass" | "priorityclasses" => ResourceAlias::Unsupported("PriorityClasses"),
        "runtimeclass" | "runtimeclasses" => ResourceAlias::Unsupported("RuntimeClasses"),
        "lease" | "leases" => ResourceAlias::Unsupported("Leases"),
        "va" | "volumeattachment" | "volumeattachments" => {
            ResourceAlias::Unsupported("VolumeAttachments")
        }
        "pt" | "podtemplate" | "podtemplates" => ResourceAlias::Unsupported("PodTemplates"),
        "mwc" | "mutatingwebhookconfiguration" | "mutatingwebhookconfigurations" => {
            ResourceAlias::Unsupported("MutatingWebhookConfigurations")
        }
        "vwc" | "validatingwebhookconfiguration" | "validatingwebhookconfigurations" => {
            ResourceAlias::Unsupported("ValidatingWebhookConfigurations")
        }
        _ => ResourceAlias::Unknown,
    }
}
