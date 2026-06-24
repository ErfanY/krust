use chrono::{DateTime, Utc};

use crate::{
    model::{ResourceKey, ResourceKind, SortColumn},
    state::StateStore,
};

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Ordered, filtered view of a resource list. Holds only row *identities* (keys); display rows are
/// materialized on demand for the visible window so per-frame render cost is O(visible), not
/// O(total). The full ordered list is still needed for sort/filter/count/selection bounds.
#[derive(Debug, Clone, Default)]
pub struct ViewModel {
    pub order: Vec<ResourceKey>,
}

impl ViewModel {
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn key(&self, index: usize) -> Option<&ResourceKey> {
        self.order.get(index)
    }
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
            entities.retain(|entity| row_matches(entity, &needle));
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

        ViewModel {
            order: entities
                .into_iter()
                .map(|entity| entity.key.clone())
                .collect(),
        }
    }
}

/// Case-insensitive substring match across the fields shown in the table, without allocating a
/// combined haystack string per row.
fn row_matches(entity: &crate::model::ResourceEntity, needle_lower: &str) -> bool {
    let contains = |field: &str| field.to_lowercase().contains(needle_lower);
    contains(&entity.key.name)
        || entity
            .key
            .namespace
            .as_deref()
            .is_some_and(|ns| contains(ns))
        || contains(&entity.status)
        || contains(&entity.summary)
}

/// Build the display row for a single key (visible-window materialization). Returns None if the
/// entity is no longer in the store (e.g. removed between projection and render).
pub fn materialize_row(store: &StateStore, key: &ResourceKey) -> Option<ViewRow> {
    let entity = store.get(key)?;
    Some(ViewRow {
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
    use std::time::Instant;

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
            extracted: Default::default(),
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

        assert_eq!(vm.len(), 1);
        let row = materialize_row(&store, vm.key(0).expect("one match")).expect("row materializes");
        assert_eq!(row.name, "worker");
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn perf_project_large_active_view() {
        let mut store = StateStore::default();
        for idx in 0..10_000 {
            put(
                &mut store,
                "ctx",
                if idx % 2 == 0 { "team-a" } else { "team-b" },
                &format!("pod-{idx:05}"),
                if idx % 7 == 0 { "Pending" } else { "Running" },
            );
        }

        let projector = SimpleViewProjector;
        let request = ViewRequest {
            context: "ctx".to_string(),
            kind: ResourceKind::Pods,
            namespace: None,
            filter: "pod-09".to_string(),
            sort: SortColumn::Name,
            descending: false,
        };

        let start = Instant::now();
        let mut rows = 0usize;
        for _ in 0..30 {
            rows = projector.project(&store, &request).len();
        }
        let elapsed = start.elapsed();
        eprintln!(
            "[perf] project_large_active_view rows={} total={:?} avg={:?}",
            rows,
            elapsed,
            elapsed / 30
        );
    }
}
