mod config;
mod db;
mod pve;
mod proxy;
mod reap;
mod store;
mod supervisor;

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<config::Config>,
    pub worker: Arc<Mutex<supervisor::WorkerState>>,
}

async fn health(State(st): State<AppState>) -> Json<serde_json::Value> {
    let (model_id, pid, status) = {
        let w = st.worker.lock().await;
        (w.model_id.clone(), w.pid, format!("{:?}", w.status))
    };
    let pve = pve::list_containers(&st.cfg).await.is_ok();
    let postgres = db::probe(&st.cfg.db.pgurl).await.is_ok();
    Json(json!({
        "service": "llm-ctl",
        "status": "ok",
        "worker": { "model": model_id, "pid": pid, "status": status },
        "cross_tier": { "pve_reachable": pve, "postgres_reachable": postgres },
    }))
}

async fn handle_pve(State(st): State<AppState>) -> Response {
    match pve::list_containers(&st.cfg).await {
        Ok(cts) => Json(cts).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_db(State(st): State<AppState>) -> Response {
    let store = match store::Store::connect(&st.cfg.db.pgurl).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "postgres_reachable": false, "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let res = match store.migrate().await {
        Ok(()) => store.ping().await,
        Err(e) => Err(e),
    };
    match res {
        Ok(()) => Json(json!({ "postgres_reachable": true })).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "postgres_reachable": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_session_test(State(st): State<AppState>) -> Response {
    let store = match store::Store::connect(&st.cfg.db.pgurl).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    if let Err(e) = store.migrate().await {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    match store
        .record_fake_turn("m3-test-session", "lfm2.5-8b")
        .await
    {
        Ok(n) => Json(json!({ "session": "m3-test-session", "turns": n })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,llm_ctl=debug".into()),
        )
        .init();

    let cfg_path = std::env::var("LLM_CTL_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let cfg = Arc::new(config::Config::load(&cfg_path)?);
    tracing::info!(listen = ?cfg.listen, "config loaded from {cfg_path}");

    // Sole ownership: reclaim any llama-server not started by us.
    let reaped = reap::reap_legacy_llama();
    tracing::info!(reaped = ?reaped, "reclaimed llama-server ownership");

    let worker = Arc::new(Mutex::new(supervisor::WorkerState::new()));
    let sup = supervisor::Supervisor::new(cfg.worker.clone(), cfg.llama.binary.clone(), worker.clone());
    if let Some(model) = cfg.models.iter().find(|m| m.autostart) {
        sup.spawn(model, "127.0.0.1").await?;
    } else {
        tracing::warn!("no autostart model configured; worker not started");
    }

    let state = AppState {
        cfg: cfg.clone(),
        worker,
    };
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/pve", get(handle_pve))
        .route("/api/db", get(handle_db))
        .route("/api/session-test", get(handle_session_test))
        .fallback(proxy::fallback)
        .with_state(state);

    let addr = format!("{}:{}", cfg.listen.host, cfg.listen.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "llm-ctl listening");
    axum::serve(listener, app).await?;
    Ok(())
}
