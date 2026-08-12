mod config;
mod db;
mod download;
mod pve;
mod proxy;
mod reap;
mod rebuild;
mod store;
mod supervisor;

use std::sync::Arc;

use axum::extract::{Path, State};
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
    pub sup: Arc<supervisor::Supervisor>,
    pub store: Option<Arc<store::Store>>,
    pub models: Arc<Mutex<Vec<config::Model>>>,
    pub download: Arc<download::DownloadManager>,
    pub rebuild: Arc<rebuild::RebuildManager>,
    pub http: reqwest::Client,
}

async fn health(State(st): State<AppState>) -> Json<serde_json::Value> {
    let (model_id, pid, status, slots, metrics) = {
        let w = st.worker.lock().await;
        (
            w.model_id.clone(),
            w.pid,
            format!("{:?}", w.status),
            w.last_slots.clone(),
            w.last_metrics.clone(),
        )
    };
    let pve = tokio::time::timeout(std::time::Duration::from_secs(1), pve::list_containers(&st.cfg))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    let postgres = db::probe(&st.cfg.db.pgurl).await.is_ok();
    Json(json!({
        "service": "llm-ctl",
        "status": "ok",
        "worker": { "model": model_id, "pid": pid, "status": status, "slots": slots, "metrics": metrics },
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

async fn handle_models(State(st): State<AppState>) -> Response {
    let ms = st.models.lock().await;
    Json(st.sup.list(&ms).await).into_response()
}

async fn handle_model_start(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let model = {
        let ms = st.models.lock().await;
        ms.iter().find(|m| m.id == id).cloned()
    };
    let model = match model {
        Some(m) => m,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("model '{id}' not found") })),
            )
                .into_response();
        }
    };
    match st.sup.start_model(&model, "127.0.0.1").await {
        Ok(()) => Json(json!({ "ok": true, "model": model.id })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_model_stop(State(st): State<AppState>) -> Response {
    match st.sup.stop().await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_hf_download(State(st): State<AppState>, Json(req): Json<download::HfDownloadReq>) -> Response {
    match st.download.start(&req).await {
        Ok(()) => Json(json!({ "ok": true, "repo": req.repo })).into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn handle_hf_status(State(st): State<AppState>) -> Response {
    Json(st.download.job().await).into_response()
}

async fn handle_rebuild_start(State(st): State<AppState>) -> Response {
    match st.rebuild.start().await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

async fn handle_rebuild_status(State(st): State<AppState>) -> Response {
    Json(st.rebuild.job().await).into_response()
}

async fn handle_sessions(State(st): State<AppState>) -> Response {
    match &st.store {
        Some(s) => match s.list_sessions(20).await {
            Ok(v) => Json(json!({ "sessions": v })).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response(),
        },
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "store unavailable" })),
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
    let sup = Arc::new(supervisor::Supervisor::new(cfg.worker.clone(), cfg.llama.binary.clone(), worker.clone()));
    if let Some(model) = cfg.models.iter().find(|m| m.autostart) {
        sup.spawn(model, "127.0.0.1").await?;
    } else {
        tracing::warn!("no autostart model configured; worker not started");
    }

    // Supervisor poller: health (Starting -> Ready / Crashed), restart on
    // crash, + /slots + /metrics.
    {
        let sup = sup.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                tick.tick().await;
                sup.poll().await;
                sup.maybe_restart().await;
                sup.poll_slots().await;
            }
        });
    }

    // Session store (Postgres on the ops tier). Non-fatal if unavailable.
    let store = match store::Store::connect(&cfg.db.pgurl).await {
        Ok(s) => {
            if let Err(e) = s.migrate().await {
                tracing::warn!(%e, "db migrate failed");
            }
            Some(Arc::new(s))
        }
        Err(e) => {
            tracing::warn!(%e, "db unavailable at startup; turns won't be recorded");
            None
        }
    };

    // Runtime model registry (config models + HF auto-registered).
    let models = Arc::new(Mutex::new(cfg.models.clone()));
    let download = Arc::new(download::DownloadManager::new(cfg.hf.clone(), models.clone()));
    let rebuild = Arc::new(rebuild::RebuildManager::new(cfg.llama.clone()));

    let state = AppState {
        cfg: cfg.clone(),
        worker,
        sup,
        store,
        models,
        download,
        rebuild,
        http: reqwest::Client::new(),
    };
    let app = Router::new()
        .route("/api/health", get(health))
        .route("/api/pve", get(handle_pve))
        .route("/api/db", get(handle_db))
        .route("/api/session-test", get(handle_session_test))
        .route("/api/sessions", get(handle_sessions))
        .route("/api/models", get(handle_models))
        .route("/api/models/:id/start", axum::routing::post(handle_model_start))
        .route("/api/models/stop", axum::routing::post(handle_model_stop))
        .route("/api/hf/download", axum::routing::post(handle_hf_download).get(handle_hf_status))
        .route("/api/rebuild", axum::routing::post(handle_rebuild_start).get(handle_rebuild_status))
        .fallback(proxy::fallback)
        .with_state(state);

    let addr = format!("{}:{}", cfg.listen.host, cfg.listen.port);
    let listener = TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "llm-ctl listening");
    axum::serve(listener, app).await?;
    Ok(())
}
