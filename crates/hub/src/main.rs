use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use peephole_core::{Plugin, Store, scheduler};
use peephole_hub::routes::router;
use peephole_hub::settings::Settings;
use peephole_hub::state::{self, Hub};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

const EVENT_BACKLOG: usize = 64;

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let settings = Settings::from_env()?;
    let cfg = settings.load_config(|k| std::env::var(k).ok())?;
    let plugins = cfg
        .plugins
        .iter()
        .map(|entry| {
            peephole_plugins::build(entry)
                .with_context(|| format!("plugin {:?} (kind {}) could not start", entry.name(), entry.kind))
        })
        .collect::<Result<Vec<Arc<dyn Plugin>>>>()?;
    let store = Store::new();
    let registry = state::open_registry(&settings, &cfg)?;
    let (events, receiver) = broadcast::channel(EVENT_BACKLOG);
    let holds = state::spawn_event_holds(registry.clone(), receiver);
    let intervals = state::intervals(&plugins);
    let scheduler = scheduler::run(plugins, store.clone(), events);
    let (pages, plugin_count) = (cfg.pages.len(), cfg.plugins.len());
    let app = router(Hub::new(&settings, cfg, store, registry, intervals, scheduler.triggers()));
    let listener = tokio::net::TcpListener::bind(settings.listen)
        .await
        .with_context(|| format!("binding PEEPHOLE_LISTEN {}", settings.listen))?;
    tracing::info!(addr = %settings.listen, pages, plugins = plugin_count, demo = settings.demo, "peephole hub listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await.context("serving HTTP")?;
    scheduler.abort();
    holds.abort();
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "listening for Ctrl-C failed");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "listening for SIGTERM failed");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
