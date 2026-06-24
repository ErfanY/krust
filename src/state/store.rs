use std::collections::{HashMap, HashSet};

use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

/// Stored entity plus the relist generation it was last seen in. On reconnect we bump the
/// generation, refresh still-present objects to the new generation as their applies arrive, and
/// sweep anything left at an older generation — so the previous list stays visible the whole time
/// instead of blanking.
#[derive(Debug)]
struct EntitySlot {
    entity: ResourceEntity,
    generation: u64,
}

#[derive(Debug, Default)]
pub struct StateStore {
    entities: HashMap<ResourceKey, EntitySlot>,
    context_kind_index: HashMap<(String, ResourceKind), HashSet<ResourceKey>>,
    namespaces_by_context: HashMap<String, HashSet<String>>,
    current_generation: HashMap<(String, ResourceKind), u64>,
    errors: HashMap<String, String>,
    revision: u64,
}

impl StateStore {
    pub fn apply(&mut self, delta: StateDelta) {
        match delta {
            StateDelta::Upsert(entity) => self.upsert(entity),
            StateDelta::Remove(key) => self.remove(&key),
            StateDelta::RelistStart { context, kind } => self.relist_start(&context, kind),
            StateDelta::RelistEnd { context, kind } => self.relist_end(&context, kind),
            StateDelta::EvictContext { context } => self.evict_context(&context),
            StateDelta::Error { context, message } => {
                self.errors.insert(context, message);
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn list(
        &self,
        context: &str,
        kind: ResourceKind,
        namespace: Option<&str>,
    ) -> Vec<&ResourceEntity> {
        let Some(keys) = self.context_kind_index.get(&(context.to_string(), kind)) else {
            return Vec::new();
        };

        keys.iter()
            .filter_map(|key| {
                let entity = &self.entities.get(key)?.entity;
                if let Some(target_ns) = namespace
                    && entity.key.namespace.as_deref() != Some(target_ns)
                {
                    return None;
                }
                Some(entity)
            })
            .collect()
    }

    pub fn get(&self, key: &ResourceKey) -> Option<&ResourceEntity> {
        self.entities.get(key).map(|slot| &slot.entity)
    }

    pub fn count(&self, context: &str, kind: ResourceKind, namespace: Option<&str>) -> usize {
        let Some(keys) = self.context_kind_index.get(&(context.to_string(), kind)) else {
            return 0;
        };

        if let Some(target_ns) = namespace {
            keys.iter()
                .filter(|key| key.namespace.as_deref() == Some(target_ns))
                .count()
        } else {
            keys.len()
        }
    }

    pub fn contexts(&self) -> Vec<String> {
        let mut contexts: Vec<String> = self
            .context_kind_index
            .keys()
            .map(|(context, _)| context.clone())
            .collect();
        contexts.sort();
        contexts.dedup();
        contexts
    }

    pub fn namespaces(&self, context: &str) -> Vec<String> {
        let Some(namespaces) = self.namespaces_by_context.get(context) else {
            return Vec::new();
        };
        let mut out: Vec<String> = namespaces.iter().cloned().collect();
        out.sort();
        out
    }

    pub fn error_for_context(&self, context: &str) -> Option<&str> {
        self.errors.get(context).map(String::as_str)
    }

    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn upsert(&mut self, entity: ResourceEntity) {
        let key = entity.key.clone();
        // Stamp with the active generation for this (context, kind) so a relist sweep keeps it.
        let generation = self
            .current_generation
            .get(&(key.context.clone(), key.kind))
            .copied()
            .unwrap_or(0);

        // Re-inserting the same key into the index HashSet is a no-op, so no removal needed.
        self.entities
            .insert(key.clone(), EntitySlot { entity, generation });

        if let Some(ns) = key.namespace.clone() {
            self.namespaces_by_context
                .entry(key.context.clone())
                .or_default()
                .insert(ns);
        }

        self.context_kind_index
            .entry((key.context.clone(), key.kind))
            .or_default()
            .insert(key);
    }

    fn remove(&mut self, key: &ResourceKey) {
        self.entities.remove(key);
        if let Some(index) = self
            .context_kind_index
            .get_mut(&(key.context.clone(), key.kind))
        {
            index.remove(key);
        }
    }

    /// Drop everything cached for a context whose watchers were stopped (Phase 1.3). Bounds store
    /// memory to the active + warm contexts instead of accumulating every context ever visited.
    fn evict_context(&mut self, context: &str) {
        let kinds: Vec<ResourceKind> = self
            .context_kind_index
            .keys()
            .filter(|(ctx, _)| ctx == context)
            .map(|(_, kind)| *kind)
            .collect();
        for kind in kinds {
            if let Some(keys) = self.context_kind_index.remove(&(context.to_string(), kind)) {
                for key in keys {
                    self.entities.remove(&key);
                }
            }
            self.current_generation.remove(&(context.to_string(), kind));
        }
        self.namespaces_by_context.remove(context);
    }

    /// Begin a (re)list for a kind: bump its generation. Existing entities keep their old
    /// generation and stay visible until the matching `relist_end` sweeps whatever wasn't refreshed.
    fn relist_start(&mut self, context: &str, kind: ResourceKind) {
        *self
            .current_generation
            .entry((context.to_string(), kind))
            .or_insert(0) += 1;
    }

    /// Finish a (re)list: drop entities still at an older generation (objects that disappeared
    /// while disconnected and were absent from the fresh list).
    fn relist_end(&mut self, context: &str, kind: ResourceKind) {
        let generation = self
            .current_generation
            .get(&(context.to_string(), kind))
            .copied()
            .unwrap_or(0);
        let Some(keys) = self.context_kind_index.get(&(context.to_string(), kind)) else {
            return;
        };
        let stale: Vec<ResourceKey> = keys
            .iter()
            .filter(|key| {
                self.entities
                    .get(*key)
                    .map(|slot| slot.generation < generation)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for key in stale {
            self.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

    fn mk_entity(context: &str, namespace: Option<&str>, name: &str) -> ResourceEntity {
        ResourceEntity {
            key: ResourceKey::new(
                context,
                ResourceKind::Pods,
                namespace.map(str::to_string),
                name,
            ),
            status: "Running".to_string(),
            age: Some(Utc::now()),
            labels: vec![("app".to_string(), "demo".to_string())],
            summary: "test".to_string(),
            extracted: Default::default(),
        }
    }

    #[test]
    fn upsert_and_filter_namespace() {
        let mut store = StateStore::default();
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("a"), "pod-a")));
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("b"), "pod-b")));

        let a = store.list("dev", ResourceKind::Pods, Some("a"));
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key.name, "pod-a");
    }

    #[test]
    fn remove_entity() {
        let mut store = StateStore::default();
        let entity = mk_entity("dev", Some("a"), "pod-a");
        let key = entity.key.clone();
        store.apply(StateDelta::Upsert(entity));
        store.apply(StateDelta::Remove(key.clone()));

        assert!(store.get(&key).is_none());
    }

    #[test]
    fn evict_context_drops_only_that_context() {
        let mut store = StateStore::default();
        store.apply(StateDelta::Upsert(mk_entity("ctx-a", Some("ns"), "pod-a")));
        store.apply(StateDelta::Upsert(mk_entity("ctx-b", Some("ns"), "pod-b")));
        assert_eq!(store.entity_count(), 2);

        store.apply(StateDelta::EvictContext {
            context: "ctx-a".to_string(),
        });

        assert_eq!(store.entity_count(), 1);
        assert!(store.list("ctx-a", ResourceKind::Pods, None).is_empty());
        assert_eq!(store.list("ctx-b", ResourceKind::Pods, None).len(), 1);
        assert!(store.namespaces("ctx-a").is_empty());
    }

    #[test]
    fn relist_keeps_data_visible_and_sweeps_only_gone_objects() {
        let mut store = StateStore::default();
        // Initial list: two pods.
        store.apply(StateDelta::RelistStart {
            context: "dev".to_string(),
            kind: ResourceKind::Pods,
        });
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("a"), "pod-a")));
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("a"), "pod-b")));
        store.apply(StateDelta::RelistEnd {
            context: "dev".to_string(),
            kind: ResourceKind::Pods,
        });
        assert_eq!(store.list("dev", ResourceKind::Pods, None).len(), 2);

        // Reconnect/relist: pod-a is still present, pod-b is gone, pod-c is new.
        store.apply(StateDelta::RelistStart {
            context: "dev".to_string(),
            kind: ResourceKind::Pods,
        });
        // Mid-relist the previous set must remain fully visible (no blank window).
        assert_eq!(store.list("dev", ResourceKind::Pods, None).len(), 2);
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("a"), "pod-a")));
        store.apply(StateDelta::Upsert(mk_entity("dev", Some("a"), "pod-c")));
        // Still 3 visible until the relist ends (pod-b not yet swept).
        assert_eq!(store.list("dev", ResourceKind::Pods, None).len(), 3);
        store.apply(StateDelta::RelistEnd {
            context: "dev".to_string(),
            kind: ResourceKind::Pods,
        });

        let names: std::collections::HashSet<String> = store
            .list("dev", ResourceKind::Pods, None)
            .into_iter()
            .map(|e| e.key.name.clone())
            .collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains("pod-a"));
        assert!(names.contains("pod-c"));
        assert!(!names.contains("pod-b"));
    }
}
