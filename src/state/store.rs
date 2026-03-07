use std::collections::{HashMap, HashSet};

use crate::model::{ResourceEntity, ResourceKey, ResourceKind, StateDelta};

#[derive(Debug, Default)]
pub struct StateStore {
    entities: HashMap<ResourceKey, ResourceEntity>,
    context_kind_index: HashMap<(String, ResourceKind), HashSet<ResourceKey>>,
    namespaces_by_context: HashMap<String, HashSet<String>>,
    errors: HashMap<String, String>,
}

impl StateStore {
    pub fn apply(&mut self, delta: StateDelta) {
        match delta {
            StateDelta::Upsert(entity) => self.upsert(entity),
            StateDelta::Remove(key) => self.remove(&key),
            StateDelta::Reset { context, kind } => self.reset_kind(&context, kind),
            StateDelta::Error { context, message } => {
                self.errors.insert(context, message);
            }
        }
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
                let entity = self.entities.get(key)?;
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
        self.entities.get(key)
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

    fn upsert(&mut self, entity: ResourceEntity) {
        let key = entity.key.clone();

        if let Some(existing) = self.entities.insert(key.clone(), entity)
            && let Some(index) = self
                .context_kind_index
                .get_mut(&(existing.key.context.clone(), existing.key.kind))
        {
            index.remove(&existing.key);
        }

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

    fn reset_kind(&mut self, context: &str, kind: ResourceKind) {
        if let Some(keys) = self.context_kind_index.remove(&(context.to_string(), kind)) {
            for key in keys {
                self.entities.remove(&key);
            }
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
            raw: serde_json::json!({"name": name}),
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
}
