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
    /// Kind-specific column values, in [`crate::model::ResourceKind::extra_columns`] order.
    pub columns: Vec<String>,
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

        let filter = Filter::parse(&request.filter);
        entities.retain(|entity| filter.matches(entity));

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

/// A parsed table filter (k9s-style). Compiled once per projection, applied per row.
///
/// Syntax:
/// - empty                  → match everything
/// - `!<rest>`              → invert the result of `<rest>`
/// - `k=v`, `k==v`, `k!=v`  → label selector (comma-separated requirements, all must hold)
/// - anything else          → case-insensitive substring over name/namespace/status/columns
#[derive(Debug, Clone)]
enum Filter {
    All,
    Substring { needle: String, negate: bool },
    Labels { reqs: Vec<LabelReq>, negate: bool },
}

#[derive(Debug, Clone)]
struct LabelReq {
    key: String,
    equal: bool,
    value: String,
}

impl Filter {
    fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Filter::All;
        }
        let (negate, body) = match trimmed.strip_prefix('!') {
            Some(rest) => (true, rest.trim()),
            None => (false, trimmed),
        };
        if body.is_empty() {
            // a lone "!" filters nothing useful — treat as match-all
            return Filter::All;
        }
        if let Some(reqs) = parse_label_selector(body) {
            return Filter::Labels { reqs, negate };
        }
        Filter::Substring {
            needle: body.to_lowercase(),
            negate,
        }
    }

    fn matches(&self, entity: &crate::model::ResourceEntity) -> bool {
        match self {
            Filter::All => true,
            Filter::Substring { needle, negate } => substring_match(entity, needle) ^ negate,
            Filter::Labels { reqs, negate } => {
                reqs.iter().all(|req| req.matches(&entity.labels)) ^ negate
            }
        }
    }
}

impl LabelReq {
    fn matches(&self, labels: &[(String, String)]) -> bool {
        let found = labels.iter().find(|(k, _)| k == &self.key);
        match (self.equal, found) {
            (true, Some((_, v))) => v == &self.value,
            (true, None) => false,
            // `!=` holds when the label is absent or has a different value (k9s semantics)
            (false, Some((_, v))) => v != &self.value,
            (false, None) => true,
        }
    }
}

/// Parse `k=v,k2!=v2` into label requirements. Returns None if the text isn't a label selector
/// (so it falls through to substring matching).
fn parse_label_selector(body: &str) -> Option<Vec<LabelReq>> {
    let mut reqs = Vec::new();
    for part in body.split(',') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once("!=") {
            reqs.push(LabelReq {
                key: valid_label_key(k)?,
                equal: false,
                value: v.trim().to_string(),
            });
        } else if let Some((k, v)) = part.split_once("==").or_else(|| part.split_once('=')) {
            reqs.push(LabelReq {
                key: valid_label_key(k)?,
                equal: true,
                value: v.trim().to_string(),
            });
        } else {
            return None;
        }
    }
    if reqs.is_empty() { None } else { Some(reqs) }
}

/// Accept a plausible Kubernetes label key (so a stray `=` in a search term doesn't masquerade
/// as a selector). Returns the trimmed key, or None if it doesn't look like a label key.
fn valid_label_key(raw: &str) -> Option<String> {
    let key = raw.trim();
    if key.is_empty() {
        return None;
    }
    let ok = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if ok { Some(key.to_string()) } else { None }
}

/// Case-insensitive substring match across the fields shown in the table.
fn substring_match(entity: &crate::model::ResourceEntity, needle_lower: &str) -> bool {
    let contains = |field: &str| field.to_lowercase().contains(needle_lower);
    contains(&entity.key.name)
        || entity
            .key
            .namespace
            .as_deref()
            .is_some_and(|ns| contains(ns))
        || contains(&entity.status)
        || entity.columns.iter().any(|c| contains(c))
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
        columns: entity.columns.clone(),
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
            columns: vec![format!("{name}-summary")],
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

    fn put_labeled(store: &mut StateStore, name: &str, labels: &[(&str, &str)]) {
        store.apply(StateDelta::Upsert(ResourceEntity {
            key: ResourceKey::new("ctx", ResourceKind::Pods, Some("ns".to_string()), name),
            status: "Running".to_string(),
            age: Some(Utc::now()),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            columns: vec![],
            extracted: Default::default(),
        }));
    }

    fn project_names(store: &StateStore, filter: &str) -> Vec<String> {
        SimpleViewProjector
            .project(
                store,
                &ViewRequest {
                    context: "ctx".to_string(),
                    kind: ResourceKind::Pods,
                    namespace: None,
                    filter: filter.to_string(),
                    sort: SortColumn::Name,
                    descending: false,
                },
            )
            .order
            .iter()
            .map(|key| key.name.clone())
            .collect()
    }

    #[test]
    fn filter_label_selector_matches_labels_not_substring() {
        let mut store = StateStore::default();
        put_labeled(&mut store, "api", &[("app", "api"), ("tier", "backend")]);
        put_labeled(&mut store, "web", &[("app", "web"), ("tier", "frontend")]);

        assert_eq!(project_names(&store, "app=api"), vec!["api"]);
        assert_eq!(project_names(&store, "app==api"), vec!["api"]);
        assert_eq!(project_names(&store, "app!=api"), vec!["web"]);
        // all comma-separated requirements must hold
        assert_eq!(project_names(&store, "app=api,tier=backend"), vec!["api"]);
        assert!(project_names(&store, "app=api,tier=frontend").is_empty());
        // label key absent => `!=` holds, `=` does not
        assert_eq!(project_names(&store, "missing!=x").len(), 2);
        assert!(project_names(&store, "missing=x").is_empty());
    }

    #[test]
    fn filter_inverse_excludes_matches() {
        let mut store = StateStore::default();
        put_labeled(&mut store, "api", &[]);
        put_labeled(&mut store, "worker", &[]);
        assert_eq!(project_names(&store, "!work"), vec!["api"]);
        assert_eq!(project_names(&store, "!app=api"), vec!["api", "worker"]); // no app label on either
    }

    #[test]
    fn filter_non_selector_terms_use_substring() {
        let mut store = StateStore::default();
        put_labeled(&mut store, "api-server", &[]);
        put_labeled(&mut store, "worker", &[]);
        // "ap" is not a selector (no '='), so it substring-matches the name
        assert_eq!(project_names(&store, "ap"), vec!["api-server"]);
        // empty filter matches all
        assert_eq!(project_names(&store, "").len(), 2);
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
