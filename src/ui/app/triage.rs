use super::*;

/// Flag a pod once its restarts reach this, even if it currently looks healthy (recovered crashloop).
const RESTART_ATTENTION: u32 = 3;
/// Cap triage rows so a melting-down cluster can't produce an unbounded list (sorted worst-first).
const TRIAGE_MAX_ROWS: usize = 500;

/// One row of the triage board: a pod that needs an operator's attention.
pub(super) struct TriageItem {
    pub severity: Severity,
    pub namespace: String,
    pub name: String,
    pub reason: String,
    pub restarts: u32,
    pub ready: bool,
    pub age: String,
}

impl App {
    /// Open the live triage board ("what needs attention"). `namespace` None = whole cluster.
    pub(super) fn show_triage_overlay(&mut self, namespace: Option<String>) {
        self.overlay = Some(Overlay::Triage {
            namespace,
            selected: 0,
        });
        self.status_line = "triage".to_string();
    }

    /// Compute the triage rows from current store state (rebuilt each frame), worst severity first
    /// then most restarts. Returns `(rows, total_before_cap)`.
    pub(super) fn triage_items(&self, namespace: Option<&str>) -> (Vec<TriageItem>, usize) {
        let context = self.current_tab().context.clone();
        let mut items: Vec<TriageItem> = self
            .store
            .list(&context, ResourceKind::Pods, namespace)
            .into_iter()
            .filter_map(|pod| triage_item(pod))
            .collect();
        // Worst first; within a severity, most restarts; then newest (likely the active incident).
        items.sort_by(|a, b| {
            severity_rank(b.severity)
                .cmp(&severity_rank(a.severity))
                .then(b.restarts.cmp(&a.restarts))
                .then(a.name.cmp(&b.name))
        });
        let total = items.len();
        items.truncate(TRIAGE_MAX_ROWS);
        (items, total)
    }

    /// Counts by severity over the full (pre-cap) item set, for the header summary.
    pub(super) fn triage_counts(items: &[TriageItem]) -> (usize, usize) {
        let err = items.iter().filter(|i| i.severity == Severity::Err).count();
        let warn = items
            .iter()
            .filter(|i| i.severity == Severity::Warn)
            .count();
        (err, warn)
    }
}

/// Build a triage row for a pod if it needs attention, else None.
fn triage_item(pod: &crate::model::ResourceEntity) -> Option<TriageItem> {
    let status = &pod.status;
    let ready = pod.extracted.ready;
    let restarts = pod.extracted.restarts;
    let status_severity = classify_status_severity(status);

    // The status string already flags a problem (CrashLoopBackOff, OOMKilled, Pending, …).
    if status_severity != Severity::Ok {
        return Some(item(pod, status_severity, status.clone()));
    }
    // Otherwise surface the two signals a plain phase ("Running") hides.
    if !ready {
        return Some(item(pod, Severity::Warn, "NotReady".to_string()));
    }
    if restarts >= RESTART_ATTENTION {
        return Some(item(pod, Severity::Warn, format!("Restarts×{restarts}")));
    }
    None // healthy: Running + Ready + few restarts
}

fn item(pod: &crate::model::ResourceEntity, severity: Severity, reason: String) -> TriageItem {
    TriageItem {
        severity,
        namespace: pod.key.namespace.clone().unwrap_or_else(|| "-".to_string()),
        name: pod.key.name.clone(),
        reason,
        restarts: pod.extracted.restarts,
        ready: pod.extracted.ready,
        age: short_age(pod.age),
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Err => 2,
        Severity::Warn => 1,
        Severity::Ok => 0,
    }
}
