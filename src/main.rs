mod config;
mod metrics;
mod monitoring_api;
mod portal;
mod scrape;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt};

use crate::config::Config;
use crate::metrics::AppMetrics;
use crate::monitoring_api::MonitoringApiClient;
use crate::portal::{Credentials, PortalClient, Secret, flatten_layout};

#[derive(Parser, Debug)]
#[command(
    name = "solaredge_exporter",
    about = "Prometheus exporter for SolarEdge per-optimizer metrics"
)]
struct Cli {
    /// Path to config.toml.
    #[arg(short, long)]
    config: PathBuf,
}

struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let tz = jiff::tz::TimeZone::system();
        let now = jiff::Timestamp::now().to_zoned(tz);
        write!(w, "{}", now.strftime("%Y-%m-%d %H:%M:%S"))
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_timer(LocalTime).with_env_filter(filter).init();
}

fn load_credentials() -> Result<Credentials> {
    let username = std::env::var("SOLAREDGE_USERNAME")
        .map_err(|_| anyhow::anyhow!("SOLAREDGE_USERNAME env var is required but not set"))?;
    let password = std::env::var("SOLAREDGE_PASSWORD")
        .map_err(|_| anyhow::anyhow!("SOLAREDGE_PASSWORD env var is required but not set"))?;
    if username.is_empty() || password.is_empty() {
        bail!("SOLAREDGE_USERNAME and SOLAREDGE_PASSWORD must be non-empty");
    }
    Ok(Credentials {
        username,
        password: Secret::new(password),
    })
}

fn load_api_key() -> Result<Secret> {
    let key = std::env::var("SOLAREDGE_API_KEY")
        .map_err(|_| anyhow::anyhow!("SOLAREDGE_API_KEY env var is required but not set"))?;
    if key.is_empty() {
        bail!("SOLAREDGE_API_KEY must be non-empty");
    }
    Ok(Secret::new(key))
}

fn log_layout_tree(optimizers: &[portal::FlatOptimizer]) {
    info!(
        count = optimizers.len(),
        "discovered optimizers from layout"
    );
    for opt in optimizers {
        info!(
            inverter = %opt.inverter_serial,
            inverter_name = %opt.inverter_display_name,
            optimizer = %opt.serial_number,
            display_name = %opt.display_name,
            reporter_id = opt.reporter_id,
            "optimizer"
        );
    }
    if optimizers.is_empty() {
        warn!(
            "no optimizers found in layout — check that the site_id is correct and the user has access"
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let config =
        Config::load(&cli.config).with_context(|| format!("loading {}", cli.config.display()))?;
    let config = Arc::new(config);
    info!(
        site_id = config.site_id,
        listen = %config.server.listen,
        refresh_secs = config.refresh.optimizer_seconds,
        fields = config.fields.len(),
        "config loaded"
    );

    let creds = load_credentials()?;
    info!(username = %creds.username, "credentials loaded from environment");
    let api_key = load_api_key()?;
    info!("SOLAREDGE_API_KEY loaded from environment");

    let client = Arc::new(PortalClient::new(config.site_id, creds)?);
    let monitoring_client = Arc::new(MonitoringApiClient::new(
        config.site_id,
        api_key,
        config.monitoring_api.state_file.clone(),
    )?);

    // One-shot layout fetch. Fail loudly — there's no useful work without it.
    info!("fetching site layout");
    let layout = client.fetch_layout().await.context("fetch_layout failed")?;
    let optimizers = Arc::new(flatten_layout(&layout));
    log_layout_tree(&optimizers);

    let metrics = Arc::new(AppMetrics::new());

    // Seed the persistent grid-charging counter from disk state before any
    // scrape task or HTTP server sees the registry.
    monitoring_api::scrape::seed_counter_from_state(&monitoring_client, &metrics);

    // Portal refresh task (per-optimizer telemetry, 15 min default)
    {
        let client = client.clone();
        let config = config.clone();
        let optimizers = optimizers.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            scrape::run(client, config, optimizers, metrics).await;
        });
    }

    // Public Monitoring API refresh task (battery + meter + site-PV counters, 30 min default)
    {
        let client = monitoring_client.clone();
        let config = config.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            monitoring_api::scrape::run(client, config, metrics).await;
        });
    }

    // Serve metrics. Ctrl+C exits.
    let addr = config.server.listen;
    tokio::select! {
        res = server::serve(addr, metrics.clone()) => {
            if let Err(e) = res {
                error!(error = %e, "HTTP server failed");
                return Err(e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl+C, shutting down");
        }
    }
    Ok(())
}
