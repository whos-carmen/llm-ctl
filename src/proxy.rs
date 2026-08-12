//! Reverse proxy to the single llama-server worker, with per-turn capture into
//! Postgres (streaming + non-streaming) and model-must-be-active enforcement.

use crate::store::{Store, Turn};
use crate::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request};
use axum::response::Response as AxResponse;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Instant;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// Max request body we accept from clients (LLM prompts are well under this).
const MAX_BODY: usize = 10 * 1024 * 1024;
/// Max buffered SSE text before we drop the partial line (defensive).
const MAX_SSE_BUF: usize = 64 * 1024;

fn ok_json(v: Value) -> AxResponse {
    AxResponse::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn err_json(status: u16, msg: &str) -> AxResponse {
    AxResponse::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(json!({ "error": { "message": msg } }).to_string()))
        .unwrap()
}

fn worker_base(st: &AppState) -> String {
    format!("http://127.0.0.1:{}", st.cfg.worker.port)
}

fn filtered_headers(req: &Request<Body>) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in req.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if matches!(lk.as_str(), "host" | "transfer-encoding" | "content-length" | "connection") {
            continue;
        }
        headers.insert(k.clone(), v.clone());
    }
    headers
}

/// Fallback: status at "/", forward everything else; POST completions get
/// model enforcement + per-turn capture.
pub async fn fallback(State(st): State<AppState>, req: Request<Body>) -> AxResponse {
    let path = req.uri().path().to_string();
    if path == "/" {
        return ok_json(json!({
            "service": "llm-ctl",
            "status": "ok",
            "worker_base": worker_base(&st),
        }));
    }
    let is_completion = req.method() == Method::POST
        && (path.ends_with("/chat/completions") || path.ends_with("/completions"));
    if is_completion {
        handle_completion(st, req).await
    } else {
        forward(&st.http, &worker_base(&st), req).await
    }
}

/// Plain byte-for-byte forward (GETs, /v1/models, /health, /metrics, /slots...).
async fn forward(client: &reqwest::Client, base: &str, req: Request<Body>) -> AxResponse {
    let method = req.method().clone();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{}{}{}", base, req.uri().path(), query);
    let headers = filtered_headers(&req);
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return err_json(413, "request body too large"),
    };
    let upstream = match client.request(method, &url).headers(headers).body(body).send().await {
        Ok(r) => r,
        Err(e) => return err_json(502, &format!("upstream unreachable: {e}")),
    };
    build_response(upstream).await
}

async fn build_response(upstream: reqwest::Response) -> AxResponse {
    let status = upstream.status();
    let mut builder = AxResponse::builder().status(status);
    for (k, v) in upstream.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if matches!(lk.as_str(), "transfer-encoding" | "content-length" | "connection") {
            continue;
        }
        builder = builder.header(k, v);
    }
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| -> BoxErr { std::io::Error::other(e).into() }));
    builder.body(Body::from_stream(stream)).unwrap()
}

async fn handle_completion(st: AppState, req: Request<Body>) -> AxResponse {
    let path = req.uri().path().to_string();
    let session_header = req
        .headers()
        .get("x-session-id")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let headers = filtered_headers(&req);
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return err_json(413, "request body too large"),
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err_json(400, &format!("invalid JSON body: {e}")),
    };
    let req_model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let stream = parsed.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let messages = parsed.get("messages").cloned();
    let max_tokens = parsed.get("max_tokens").and_then(|v| v.as_i64());
    let temperature = parsed.get("temperature").and_then(|v| v.as_f64());

    // Model-must-be-active enforcement.
    let active = st.worker.lock().await.model_id.clone();
    let active = match active {
        Some(a) => a,
        None => return err_json(503, "no model loaded; start one first"),
    };
    if !req_model.is_empty() && req_model != active {
        return err_json(
            409,
            &format!("model '{req_model}' is not loaded; active model is '{active}'"),
        );
    }
    let model = if req_model.is_empty() { active } else { req_model };

    // Activity marker for idle-gating in the status bar.
    {
        let mut w = st.worker.lock().await;
        w.last_request_at = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        );
    }

    // Session binding: X-Session-Id header > body "user" > active-session hint
    // (pi extension reports its session id) > "active within 30 min" > new.
    let session_id = {
        let hint = st.active_session.lock().await.clone();
        let sid = session_header
            .or_else(|| {
                parsed
                    .get("user")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string())
            })
            .or(hint);
        match &st.store {
            Some(s) => match s.resolve_session(sid.as_deref(), &model).await {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(%e, "session resolve failed; skipping recording");
                    String::new()
                }
            },
            None => String::new(),
        }
    };

    let start = Instant::now();
    let url = format!("{}{}", worker_base(&st), path);
    let upstream = match st
        .http
        .request(Method::POST, &url)
        .headers(headers)
        .body(body.clone())
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return err_json(502, &format!("upstream unreachable: {e}")),
    };

    if stream {
        stream_and_record(upstream, st.store, st.worker, session_id, model, messages, max_tokens, temperature, start).await
    } else {
        let status = upstream.status();
        let mut out_headers = Vec::new();
        for (k, v) in upstream.headers() {
            let lk = k.as_str().to_ascii_lowercase();
            if !matches!(lk.as_str(), "transfer-encoding" | "content-length" | "connection") {
                out_headers.push((k.clone(), v.clone()));
            }
        }
        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(_) => return err_json(502, "upstream read failed"),
        };
        record_from_response(&st, session_id, &model, messages, max_tokens, temperature, &bytes, start).await;
        let mut builder = AxResponse::builder().status(status);
        for (k, v) in out_headers {
            builder = builder.header(k, v);
        }
        builder.body(Body::from(bytes)).unwrap()
    }
}

// --- Recording helpers -----------------------------------------------------

fn num(v: &Option<Value>, k: &str) -> f64 {
    v.as_ref().and_then(|x| x.get(k)).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
fn num_i(v: &Option<Value>, k: &str) -> i64 {
    v.as_ref().and_then(|x| x.get(k)).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn build_turn(
    session_id: &str,
    model: &str,
    messages: Option<Value>,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    id: Option<String>,
    content: Option<String>,
    finish: Option<String>,
    usage: &Option<Value>,
    timings: &Option<Value>,
    duration_ms: f64,
) -> Turn {
    let usage_prompt = num_i(usage, "prompt_tokens");
    let usage_completion = num_i(usage, "completion_tokens");
    let cached = usage
        .as_ref()
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    // Streaming chunks carry `timings` but not `usage`; fall back to timings.
    let prompt_tokens = if usage_prompt > 0 { usage_prompt } else { num_i(timings, "prompt_n") + num_i(timings, "cache_n") };
    let completion_tokens = if usage_completion > 0 { usage_completion } else { num_i(timings, "predicted_n") };
    let cached_tokens = if cached > 0 { cached } else { num_i(timings, "cache_n") };
    Turn {
        session_id: session_id.to_string(),
        request_model: model.to_string(),
        request_messages: messages,
        request_max_tokens: max_tokens,
        request_temperature: temperature,
        response_id: id,
        response_content: content,
        response_finish_reason: finish,
        cache_n: num_i(timings, "cache_n"),
        prompt_n: num_i(timings, "prompt_n"),
        prompt_ms: num(timings, "prompt_ms"),
        prompt_per_second: num(timings, "prompt_per_second"),
        predicted_n: num_i(timings, "predicted_n"),
        predicted_ms: num(timings, "predicted_ms"),
        predicted_per_second: num(timings, "predicted_per_second"),
        usage_prompt_tokens: prompt_tokens,
        usage_completion_tokens: completion_tokens,
        usage_cached_tokens: cached_tokens,
        duration_ms,
    }
}

async fn record_turn(
    store: &Option<Arc<Store>>,
    worker: &Arc<tokio::sync::Mutex<crate::supervisor::WorkerState>>,
    turn: &Turn,
) {
    if turn.session_id.is_empty() {
        return;
    }
    // Stash the turn's cache stats for the status bar.
    {
        let mut w = worker.lock().await;
        w.last_turn = Some(serde_json::json!({
            "cache_n": turn.cache_n,
            "prompt_n": turn.prompt_n,
            "predicted_n": turn.predicted_n,
        }));
    }
    match store {
        Some(s) => {
            if let Err(e) = s.record_turn(turn).await {
                tracing::warn!(%e, "record turn failed");
            }
        }
        None => tracing::debug!("store unavailable; turn not recorded"),
    }
}

async fn record_from_response(
    st: &AppState,
    session_id: String,
    model: &str,
    messages: Option<Value>,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    bytes: &axum::body::Bytes,
    start: Instant,
) {
    if session_id.is_empty() {
        return;
    }
    let v: Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return,
    };
    let usage = v.get("usage").cloned();
    let timings = v.get("timings").cloned();
    let ch0 = v.get("choices").and_then(|c| c.get(0));
    let content = ch0
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let finish = ch0.and_then(|c| c.get("finish_reason")).and_then(|x| x.as_str()).map(|s| s.to_string());
    let id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string());
    let turn = build_turn(
        &session_id, model, messages, max_tokens, temperature, id, content, finish,
        &usage, &timings, start.elapsed().as_secs_f64() * 1000.0,
    );
    record_turn(&st.store, &st.worker, &turn).await;
}

// --- Streaming -------------------------------------------------------------

/// Collect the final `usage`/`timings`/content from an SSE stream.
#[derive(Default)]
struct Collector {
    content: String,
    usage: Option<Value>,
    timings: Option<Value>,
    finish: Option<String>,
    id: Option<String>,
}

impl Collector {
    fn feed(&mut self, line: &str) {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data: ") else { return };
        if data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return };
        if let Some(id) = v.get("id").and_then(|x| x.as_str()) {
            self.id = Some(id.to_string());
        }
        if let Some(ch) = v.get("choices").and_then(|c| c.get(0)) {
            if let Some(d) = ch.get("delta").and_then(|d| d.get("content")).and_then(|x| x.as_str()) {
                self.content.push_str(d);
            }
            if let Some(fr) = ch.get("finish_reason").and_then(|x| x.as_str()) {
                self.finish = Some(fr.to_string());
            }
        }
        if let Some(u) = v.get("usage") {
            self.usage = Some(u.clone());
        }
        if let Some(t) = v.get("timings") {
            self.timings = Some(t.clone());
        }
    }
}

async fn stream_and_record(
    upstream: reqwest::Response,
    store: Option<Arc<Store>>,
    worker: Arc<tokio::sync::Mutex<crate::supervisor::WorkerState>>,
    session_id: String,
    model: String,
    messages: Option<Value>,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    start: Instant,
) -> AxResponse {
    let status = upstream.status();
    let mut builder = AxResponse::builder().status(status);
    for (k, v) in upstream.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if !matches!(lk.as_str(), "transfer-encoding" | "content-length" | "connection") {
            builder = builder.header(k, v);
        }
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, BoxErr>>(64);
    let collector = Arc::new(Mutex::new(Collector::default()));

    tokio::spawn(async move {
        let mut stream = upstream.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut client_gone = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    if !session_id.is_empty() {
                        buf.extend_from_slice(&b);
                        let mut col = collector.lock().unwrap();
                        // Split on newlines at the BYTE level so multi-byte UTF-8
                        // chars split across TCP chunks are not corrupted.
                        while let Some(pos) = buf.iter().position(|&x| x == b'\n') {
                            let line: Vec<u8> = buf.drain(..=pos).collect();
                            col.feed(&String::from_utf8_lossy(&line));
                        }
                        // Defensive: drop a giant partial line instead of growing forever.
                        if buf.len() > MAX_SSE_BUF {
                            buf.clear();
                        }
                    }
                    // Client disconnect: stop forwarding but keep draining so the
                    // completed turn is still recorded below.
                    if !client_gone && tx.send(Ok(b)).await.is_err() {
                        client_gone = true;
                    }
                }
                Err(e) => {
                    if !client_gone {
                        let _ = tx.send(Err(std::io::Error::other(e).into())).await;
                    }
                    break;
                }
            }
        }
        // Record the completed turn (guard dropped before the await).
        if !session_id.is_empty() {
            let turn = {
                let col = collector.lock().unwrap();
                build_turn(
                    &session_id,
                    &model,
                    messages,
                    max_tokens,
                    temperature,
                    col.id.clone(),
                    if col.content.is_empty() { None } else { Some(col.content.clone()) },
                    col.finish.clone(),
                    &col.usage,
                    &col.timings,
                    start.elapsed().as_secs_f64() * 1000.0,
                )
            };
            record_turn(&store, &worker, &turn).await;
        }
    });

    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    builder.body(body).unwrap()
}
