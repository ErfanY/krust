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
}
