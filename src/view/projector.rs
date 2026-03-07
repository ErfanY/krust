use chrono::{DateTime, Utc};

use crate::{
    model::{ResourceKey, ResourceKind, SortColumn},
    state::StateStore,
};

#[derive(Debug, Clone)]
pub struct ViewRequest {
    pub context: String,
    pub kind: ResourceKind,
    pub namespace: Option<String>,
    pub filter: String,
    pub sort: SortColumn,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct ViewRow {
    pub key: ResourceKey,
    pub namespace: String,
    pub name: String,
    pub status: String,
    pub age: String,
    pub summary: String,
}

#[derive(Debug, Clone, Default)]
pub struct ViewModel {
    pub rows: Vec<ViewRow>,
}

pub trait ViewProjector: Send + Sync {
    fn project(&self, store: &StateStore, request: &ViewRequest) -> ViewModel;
}

#[derive(Debug, Default)]
pub struct SimpleViewProjector;

impl ViewProjector for SimpleViewProjector {
    fn project(&self, store: &StateStore, request: &ViewRequest) -> ViewModel {
        let namespace_filter = request.namespace.as_deref();
        let mut entities = store.list(&request.context, request.kind, namespace_filter);
        let needle = request.filter.to_lowercase();

        if !needle.is_empty() {
            entities.retain(|entity| {
                let ns = entity.key.namespace.as_deref().unwrap_or("");
                let haystack = format!(
                    "{} {} {} {}",
                    entity.key.name, ns, entity.status, entity.summary
                )
                .to_lowercase();
                haystack.contains(&needle)
            });
        }

        entities.sort_by(|a, b| {
            let ord = match request.sort {
                SortColumn::Name => a.key.name.cmp(&b.key.name),
                SortColumn::Namespace => a.key.namespace.cmp(&b.key.namespace),
                SortColumn::Status => a.status.cmp(&b.status),
                SortColumn::Age => a.age.cmp(&b.age),
            };
            if request.descending {
                ord.reverse()
            } else {
                ord
            }
        });

        let rows = entities
            .into_iter()
            .map(|entity| ViewRow {
                key: entity.key.clone(),
                namespace: entity
                    .key
                    .namespace
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
                name: entity.key.name.clone(),
                status: entity.status.clone(),
                age: human_age(entity.age),
                summary: entity.summary.clone(),
            })
            .collect();

        ViewModel { rows }
    }
}

fn human_age(when: Option<DateTime<Utc>>) -> String {
    let Some(when) = when else {
        return "-".to_string();
    };
    let delta = Utc::now().signed_duration_since(when);
    if delta.num_days() > 0 {
        return format!("{}d", delta.num_days());
    }
    if delta.num_hours() > 0 {
        return format!("{}h", delta.num_hours());
    }
    if delta.num_minutes() > 0 {
        return format!("{}m", delta.num_minutes());
    }
    format!("{}s", delta.num_seconds().max(0))
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;
    use crate::{
        model::{ResourceEntity, ResourceKey, ResourceKind, SortColumn, StateDelta},
        state::StateStore,
    };

    fn put(store: &mut StateStore, context: &str, ns: &str, name: &str, status: &str) {
        store.apply(StateDelta::Upsert(ResourceEntity {
            key: ResourceKey::new(context, ResourceKind::Pods, Some(ns.to_string()), name),
            status: status.to_string(),
            age: Some(Utc::now() - Duration::minutes(5)),
            labels: vec![],
            summary: format!("{name}-summary"),
            raw: serde_json::json!({ "name": name }),
        }));
    }

    #[test]
    fn filters_and_sorts() {
        let mut store = StateStore::default();
        put(&mut store, "ctx", "team-a", "api", "Running");
        put(&mut store, "ctx", "team-b", "worker", "Pending");

        let projector = SimpleViewProjector;
        let vm = projector.project(
            &store,
            &ViewRequest {
                context: "ctx".to_string(),
                kind: ResourceKind::Pods,
                namespace: None,
                filter: "work".to_string(),
                sort: SortColumn::Name,
                descending: false,
            },
        );

        assert_eq!(vm.rows.len(), 1);
        assert_eq!(vm.rows[0].name, "worker");
    }
}
