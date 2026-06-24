//! Ingest-time extraction: pull the few scalar fields the all-rows hot paths need out of a
//! resource's JSON, so the full object never has to be retained per entity. The transient JSON
//! `Value` produced by the watch handler is fed here once and then dropped.

use serde_json::Value;

use crate::model::{Extracted, NodeCapacity, OwnerRef, PodResources, ResourceKind};

/// Build the compact per-row payload for a freshly observed object.
pub(crate) fn extracted_for(kind: ResourceKind, raw: &Value) -> Extracted {
    Extracted {
        node_name: match kind {
            ResourceKind::Pods => raw
                .pointer("/spec/nodeName")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        },
        containers: match kind {
            ResourceKind::Pods => container_names(raw),
            _ => Vec::new(),
        },
        owners: owner_refs(raw),
        pod_resources: match kind {
            ResourceKind::Pods => Some(pod_resources(raw)),
            _ => None,
        },
        node_capacity: match kind {
            ResourceKind::Nodes => Some(node_capacity(raw)),
            _ => None,
        },
    }
}

fn container_names(raw: &Value) -> Vec<String> {
    raw.pointer("/spec/containers")
        .and_then(Value::as_array)
        .map(|containers| {
            containers
                .iter()
                .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn owner_refs(raw: &Value) -> Vec<OwnerRef> {
    raw.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .map(|owners| {
            owners
                .iter()
                .filter_map(|owner| {
                    let kind = owner.get("kind").and_then(Value::as_str)?;
                    let name = owner.get("name").and_then(Value::as_str)?;
                    Some(OwnerRef {
                        kind: kind.to_string(),
                        name: name.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn pod_resources(raw: &Value) -> PodResources {
    let spec = raw.get("spec").and_then(Value::as_object);

    let mut cpu_req = 0u64;
    let mut cpu_lim = 0u64;
    let mut mem_req = 0u64;
    let mut mem_lim = 0u64;
    let mut init_cpu_req = 0u64;
    let mut init_cpu_lim = 0u64;
    let mut init_mem_req = 0u64;
    let mut init_mem_lim = 0u64;

    if let Some(spec) = spec {
        if let Some(containers) = spec.get("containers").and_then(Value::as_array) {
            for c in containers {
                if let Some(resources) = c.get("resources").and_then(Value::as_object) {
                    cpu_req = cpu_req.saturating_add(cpu(resources, "requests"));
                    cpu_lim = cpu_lim.saturating_add(cpu(resources, "limits"));
                    mem_req = mem_req.saturating_add(mem(resources, "requests"));
                    mem_lim = mem_lim.saturating_add(mem(resources, "limits"));
                }
            }
        }

        if let Some(containers) = spec.get("initContainers").and_then(Value::as_array) {
            for c in containers {
                if let Some(resources) = c.get("resources").and_then(Value::as_object) {
                    init_cpu_req = init_cpu_req.max(cpu(resources, "requests"));
                    init_cpu_lim = init_cpu_lim.max(cpu(resources, "limits"));
                    init_mem_req = init_mem_req.max(mem(resources, "requests"));
                    init_mem_lim = init_mem_lim.max(mem(resources, "limits"));
                }
            }
        }

        if let Some(overhead) = spec.get("overhead").and_then(Value::as_object) {
            if let Some(c) = overhead.get("cpu").and_then(Value::as_str) {
                let v = parse_cpu_millicores(c);
                cpu_req = cpu_req.saturating_add(v);
                cpu_lim = cpu_lim.saturating_add(v);
            }
            if let Some(m) = overhead.get("memory").and_then(Value::as_str) {
                let v = parse_bytes_quantity(m);
                mem_req = mem_req.saturating_add(v);
                mem_lim = mem_lim.saturating_add(v);
            }
        }
    }

    PodResources {
        cpu_request_m: cpu_req.saturating_add(init_cpu_req),
        cpu_limit_m: cpu_lim.saturating_add(init_cpu_lim),
        mem_request_b: mem_req.saturating_add(init_mem_req),
        mem_limit_b: mem_lim.saturating_add(init_mem_lim),
    }
}

fn node_capacity(raw: &Value) -> NodeCapacity {
    let unschedulable = raw
        .pointer("/spec/unschedulable")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let ready = raw
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions.iter().find_map(|cond| {
                let cond_type = cond.get("type").and_then(Value::as_str)?;
                if cond_type != "Ready" {
                    return None;
                }
                cond.get("status")
                    .and_then(Value::as_str)
                    .map(|status| status.eq_ignore_ascii_case("true"))
            })
        })
        .unwrap_or(false);

    NodeCapacity {
        ready,
        unschedulable,
        cpu_alloc_m: raw
            .pointer("/status/allocatable/cpu")
            .and_then(Value::as_str)
            .map(parse_cpu_millicores)
            .unwrap_or(0),
        mem_alloc_b: raw
            .pointer("/status/allocatable/memory")
            .and_then(Value::as_str)
            .map(parse_bytes_quantity)
            .unwrap_or(0),
        pod_alloc: raw
            .pointer("/status/allocatable/pods")
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
    }
}

fn cpu(resources: &serde_json::Map<String, Value>, field: &str) -> u64 {
    resources
        .get(field)
        .and_then(Value::as_object)
        .and_then(|m| m.get("cpu"))
        .and_then(Value::as_str)
        .map(parse_cpu_millicores)
        .unwrap_or(0)
}

fn mem(resources: &serde_json::Map<String, Value>, field: &str) -> u64 {
    resources
        .get(field)
        .and_then(Value::as_object)
        .and_then(|m| m.get("memory"))
        .and_then(Value::as_str)
        .map(parse_bytes_quantity)
        .unwrap_or(0)
}

/// Parse a `metrics.k8s.io` usage object (`{cpu, memory}`) into (millicores, bytes). Used for both
/// NodeMetrics (`/usage`) and summed PodMetrics container usage. Returns None if neither is present.
pub(crate) fn usage_from_metrics(usage: &Value) -> Option<(u64, u64)> {
    let cpu = usage
        .get("cpu")
        .and_then(Value::as_str)
        .map(parse_cpu_millicores);
    let mem = usage
        .get("memory")
        .and_then(Value::as_str)
        .map(parse_bytes_quantity);
    match (cpu, mem) {
        (None, None) => None,
        (c, m) => Some((c.unwrap_or(0), m.unwrap_or(0))),
    }
}

pub(crate) fn parse_cpu_millicores(value: &str) -> u64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }
    if let Some(v) = value.strip_suffix('n') {
        return parse_decimal_to_u64(v, 1.0 / 1_000_000.0);
    }
    if let Some(v) = value.strip_suffix('u') {
        return parse_decimal_to_u64(v, 1.0 / 1000.0);
    }
    if let Some(v) = value.strip_suffix('m') {
        return parse_decimal_to_u64(v, 1.0);
    }
    parse_decimal_to_u64(value, 1000.0)
}

pub(crate) fn parse_bytes_quantity(value: &str) -> u64 {
    let value = value.trim();
    if value.is_empty() {
        return 0;
    }
    let split = value
        .char_indices()
        .find(|(_, ch)| !ch.is_ascii_digit() && *ch != '.' && *ch != '-' && *ch != '+')
        .map(|(idx, _)| idx)
        .unwrap_or(value.len());
    let (num, suffix) = value.split_at(split);

    let multiplier = match suffix {
        "" => 1.0,
        "n" => 1.0 / 1_000_000_000.0,
        "u" => 1.0 / 1_000_000.0,
        "m" => 1.0 / 1000.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0_f64.powi(2),
        "Gi" => 1024.0_f64.powi(3),
        "Ti" => 1024.0_f64.powi(4),
        "Pi" => 1024.0_f64.powi(5),
        "Ei" => 1024.0_f64.powi(6),
        "K" => 1000.0,
        "M" => 1000.0_f64.powi(2),
        "G" => 1000.0_f64.powi(3),
        "T" => 1000.0_f64.powi(4),
        "P" => 1000.0_f64.powi(5),
        "E" => 1000.0_f64.powi(6),
        _ => 1.0,
    };
    parse_decimal_to_u64(num, multiplier)
}

fn parse_decimal_to_u64(value: &str, multiplier: f64) -> u64 {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .map(|v| (v * multiplier).max(0.0).round() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_pod_fields() {
        let raw = json!({
            "metadata": { "ownerReferences": [ { "kind": "ReplicaSet", "name": "rs-1" } ] },
            "spec": {
                "nodeName": "node-a",
                "containers": [
                    { "name": "main", "resources": { "requests": { "cpu": "100m", "memory": "64Mi" } } },
                    { "name": "side", "resources": { "requests": { "cpu": "50m", "memory": "32Mi" } } }
                ]
            }
        });
        let e = extracted_for(ResourceKind::Pods, &raw);
        assert_eq!(e.node_name.as_deref(), Some("node-a"));
        assert_eq!(e.containers, vec!["main".to_string(), "side".to_string()]);
        assert!(e.owned_by("ReplicaSet", "rs-1"));
        let r = e.pod_resources.expect("pod resources");
        assert_eq!(r.cpu_request_m, 150);
        assert_eq!(r.mem_request_b, (64 + 32) * 1024 * 1024);
    }

    #[test]
    fn parses_real_metrics_usage_units() {
        // formats seen on a real cluster: nanocores + Ki memory
        let usage = json!({ "cpu": "429753714n", "memory": "10872076Ki" });
        let (cpu_m, mem_b) = usage_from_metrics(&usage).expect("usage parses");
        assert_eq!(cpu_m, 430); // ~0.43 cores
        assert_eq!(mem_b, 10872076u64 * 1024); // ~10.4 GiB
        assert!(usage_from_metrics(&json!({})).is_none());
    }

    #[test]
    fn extracts_node_capacity() {
        let raw = json!({
            "status": {
                "conditions": [ { "type": "Ready", "status": "True" } ],
                "allocatable": { "cpu": "4", "memory": "8Gi", "pods": "110" }
            }
        });
        let e = extracted_for(ResourceKind::Nodes, &raw);
        let cap = e.node_capacity.expect("node capacity");
        assert!(cap.ready);
        assert_eq!(cap.cpu_alloc_m, 4000);
        assert_eq!(cap.mem_alloc_b, 8 * 1024 * 1024 * 1024);
        assert_eq!(cap.pod_alloc, 110);
    }
}
