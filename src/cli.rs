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
}
