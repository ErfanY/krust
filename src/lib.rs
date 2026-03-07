pub mod cli;
pub mod cluster;
pub mod config;
pub mod keymap;
pub mod model;
pub mod state;
pub mod ui;
pub mod view;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::{EnvFilter, fmt};

use crate::{
    cli::Cli,
    cluster::{ActionExecutor, KubeProviderOptions, KubeResourceProvider, ResourceProvider},
};

pub async fn run() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let app_config = config::AppConfig::load()?;
    let keymap = keymap::Keymap::load()?;

    let provider = Arc::new(
        KubeResourceProvider::new(KubeProviderOptions {
            kubeconfig: cli.kubeconfig.clone(),
            context: cli.context.clone(),
            all_contexts: cli.all_contexts,
            namespace: cli
                .namespace
                .clone()
                .or(app_config.runtime.namespace.clone()),
            readonly: cli.readonly,
            warm_contexts: app_config.runtime.warm_contexts,
            warm_context_ttl_secs: app_config.runtime.warm_context_ttl_secs,
        })
        .await
        .context("failed to initialize Kubernetes provider")?,
    );

    let contexts = provider.context_names().to_vec();
    let initial_context = cli
        .context
        .or(app_config.runtime.default_context)
        .or_else(|| provider.default_context().map(str::to_string))
        .or_else(|| contexts.first().cloned())
        .context("could not determine initial context")?;

    let initial_namespace = cli.namespace.or(app_config.runtime.namespace);

    let (tx, rx) = mpsc::channel(app_config.runtime.delta_channel_capacity);
    provider.start(tx).await?;

    let action_executor: Arc<dyn ActionExecutor> = provider.clone();
    let resource_provider: Arc<dyn ResourceProvider> = provider;

    ui::run(
        contexts,
        initial_context,
        initial_namespace,
        rx,
        action_executor,
        resource_provider,
        keymap,
        cli.readonly,
        app_config.runtime.fps_limit,
        app_config.ui.show_help,
    )
    .await
}

fn init_tracing() {
    if std::env::var_os("RUST_LOG").is_none() {
        return;
    }

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}
