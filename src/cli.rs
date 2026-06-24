use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "krust",
    version,
    about = "High-performance Kubernetes terminal navigator"
)]
pub struct Cli {
    /// Kubernetes context to open initially.
    #[arg(long)]
    pub context: Option<String>,

    /// Namespace to scope namespaced resources initially.
    #[arg(long)]
    pub namespace: Option<String>,

    /// Start in read-only mode.
    #[arg(long, default_value_t = false)]
    pub readonly: bool,

    /// Explicit kubeconfig path.
    #[arg(long)]
    pub kubeconfig: Option<PathBuf>,

    /// Eagerly initialize auth/client for all kubeconfig contexts.
    #[arg(long, default_value_t = false)]
    pub all_contexts: bool,

    /// Run a headless performance benchmark against the initial context, print a report, then exit.
    #[arg(long, hide = true, default_value_t = false)]
    pub bench: bool,

    /// Bench: iterations per measured operation.
    #[arg(long, hide = true, default_value_t = 100)]
    pub bench_iters: usize,

    /// Bench: seconds to let watchers settle / initial-sync before measuring.
    #[arg(long, hide = true, default_value_t = 20)]
    pub bench_settle_secs: u64,

    /// Bench: warm this many contexts in sequence and report store size/RSS (Phase 1.3 memory test).
    #[arg(long, hide = true, default_value_t = 1)]
    pub bench_contexts: usize,

    /// Soak: run the data plane for this many seconds under live churn, sampling RSS/entities/fds
    /// to detect leaks/unbounded growth, then exit. 0 disables (Phase 5 soak).
    #[arg(long, hide = true, default_value_t = 0)]
    pub soak_secs: u64,

    /// Soak: seconds between samples.
    #[arg(long, hide = true, default_value_t = 15)]
    pub soak_sample_secs: u64,

    /// Print the discovered API resources for the initial context, then exit (Phase 4.1 check).
    #[arg(long, hide = true, default_value_t = false)]
    pub discover: bool,

    /// With --discover: also list (and describe the first object of) this resource (plural or kind).
    #[arg(long, hide = true)]
    pub discover_resource: Option<String>,

    /// Print live cluster usage from metrics.k8s.io for the initial context, then exit (4.2 check).
    #[arg(long, hide = true, default_value_t = false)]
    pub metrics: bool,
}
