use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::TcpListener;
use tracing::{error, info};

use crate::metrics::AppMetrics;

#[derive(Clone)]
struct AppState {
    metrics: Arc<AppMetrics>,
}

pub async fn serve(addr: SocketAddr, metrics: Arc<AppMetrics>) -> anyhow::Result<()> {
    let state = AppState { metrics };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/", get(root_handler))
        .with_state(state);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "HTTP server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn metrics_handler(State(state): State<AppState>) -> Response {
    match state.metrics.encode() {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(e) => {
            error!(error = %e, "failed to encode metrics");
            (StatusCode::INTERNAL_SERVER_ERROR, "encode failure").into_response()
        }
    }
}

async fn root_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "solaredge_exporter — metrics at /metrics\n",
    )
        .into_response()
}
