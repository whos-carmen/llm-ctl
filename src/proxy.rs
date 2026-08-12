//! Reverse proxy: forward unmatched HTTP to the llama-server worker.

use crate::AppState;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::response::Response as AxResponse;
use futures_util::StreamExt;

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

fn ok_json(value: serde_json::Value) -> AxResponse {
    AxResponse::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn err_json(status: u16, msg: &str) -> AxResponse {
    AxResponse::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "error": { "message": msg } }).to_string(),
        ))
        .unwrap()
}

/// Fallback handler: serves a small status at "/", proxies everything else.
pub async fn fallback(State(st): State<AppState>, req: Request<Body>) -> AxResponse {
    let path = req.uri().path().to_string();
    if path == "/" {
        return ok_json(serde_json::json!({
            "service": "llm-ctl",
            "status": "ok",
            "worker_base": format!("http://127.0.0.1:{}", st.cfg.worker.port),
        }));
    }

    let base = format!("http://127.0.0.1:{}", st.cfg.worker.port);
    forward(&base, req).await
}

/// Forward one request to `base` (method, headers, body) and stream the reply.
async fn forward(base: &str, req: Request<Body>) -> AxResponse {
    let method = req.method().clone();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let url = format!("{}{}{}", base, req.uri().path(), query);

    // Build request headers, dropping hop-by-hop ones.
    let mut headers = reqwest::header::HeaderMap::new();
    for (k, v) in req.headers() {
        let lk = k.as_str().to_ascii_lowercase();
        if matches!(lk.as_str(), "host" | "transfer-encoding" | "content-length" | "connection") {
            continue;
        }
        headers.insert(k.clone(), v.clone());
    }

    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => return err_json(500, "failed to read request body"),
    };

    let client = match reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .build()
    {
        Ok(c) => c,
        Err(_) => return err_json(500, "failed to build http client"),
    };

    let upstream = match client
        .request(method, &url)
        .headers(headers)
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(%e, %url, "upstream request failed");
            return err_json(
                502,
                &format!("upstream {} unreachable: {e}", url.split("//").nth(1).unwrap_or(&url)),
            );
        }
    };

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
    let body = Body::from_stream(stream);
    builder.body(body).unwrap()
}
