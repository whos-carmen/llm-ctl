//! Session/cache store on the ops tier (PostgreSQL on CT `201`).

use anyhow::Result;
use rand::Rng;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use sqlx::PgPool;

/// One recorded request/response turn (matches `migrations/0001_init.sql`).
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub session_id: String,
    pub request_model: String,
    pub request_messages: Option<serde_json::Value>,
    pub request_max_tokens: Option<i64>,
    pub request_temperature: Option<f64>,
    pub response_id: Option<String>,
    pub response_content: Option<String>,
    pub response_finish_reason: Option<String>,
    pub cache_n: i64,
    pub prompt_n: i64,
    pub prompt_ms: f64,
    pub prompt_per_second: f64,
    pub predicted_n: i64,
    pub predicted_ms: f64,
    pub predicted_per_second: f64,
    pub usage_prompt_tokens: i64,
    pub usage_completion_tokens: i64,
    pub usage_cached_tokens: i64,
    pub duration_ms: f64,
}

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}

impl Store {
    pub async fn connect(pgurl: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(pgurl)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Session id: client-supplied, else reuse the latest session active in the
    /// last 30 minutes, else create a new one.
    pub async fn resolve_session(&self, client_id: Option<&str>, model: &str) -> Result<String> {
        if let Some(cid) = client_id.map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(
                "INSERT INTO sessions (id, created_at, updated_at, model) \
                 VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2) \
                 ON CONFLICT (id) DO UPDATE SET updated_at = excluded.updated_at",
            )
            .bind(cid)
            .bind(model)
            .execute(&self.pool)
            .await?;
            return Ok(cid.to_string());
        }
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM sessions \
             WHERE updated_at > extract(epoch from now()) - 1800 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some((sid,)) = row {
            sqlx::query("UPDATE sessions SET updated_at = extract(epoch from now()) WHERE id = $1")
                .bind(&sid)
                .execute(&self.pool)
                .await?;
            return Ok(sid);
        }
        let sid = gen_session_id();
        sqlx::query(
            "INSERT INTO sessions (id, created_at, updated_at, model) \
             VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2)",
        )
        .bind(&sid)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(sid)
    }

    /// Persist one turn: ensure the session row, insert the turn, roll up totals.
    pub async fn record_turn(&self, t: &Turn) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, created_at, updated_at, model) \
             VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2) \
             ON CONFLICT (id) DO UPDATE SET updated_at = excluded.updated_at",
        )
        .bind(&t.session_id)
        .bind(&t.request_model)
        .execute(&self.pool)
        .await?;

        let idx: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(turn_index), -1) + 1 FROM turns WHERE session_id = $1",
        )
        .bind(&t.session_id)
        .fetch_one(&self.pool)
        .await?;

        sqlx::query(
            "INSERT INTO turns (
                session_id, turn_index, timestamp, request_model, request_messages,
                request_max_tokens, request_temperature, response_id, response_content,
                response_finish_reason, cache_n, prompt_n, prompt_ms, prompt_per_second,
                predicted_n, predicted_ms, predicted_per_second,
                usage_prompt_tokens, usage_completion_tokens, usage_cached_tokens, duration_ms
             ) VALUES ($1,$2, extract(epoch from now()), $3,$4,$5,$6,$7,$8,$9,
                       $10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)",
        )
        .bind(&t.session_id)
        .bind(idx.0)
        .bind(&t.request_model)
        .bind(t.request_messages.as_ref().map(|v| Json(v.clone())))
        .bind(t.request_max_tokens)
        .bind(t.request_temperature)
        .bind(&t.response_id)
        .bind(&t.response_content)
        .bind(&t.response_finish_reason)
        .bind(t.cache_n)
        .bind(t.prompt_n)
        .bind(t.prompt_ms)
        .bind(t.prompt_per_second)
        .bind(t.predicted_n)
        .bind(t.predicted_ms)
        .bind(t.predicted_per_second)
        .bind(t.usage_prompt_tokens)
        .bind(t.usage_completion_tokens)
        .bind(t.usage_cached_tokens)
        .bind(t.duration_ms)
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "UPDATE sessions SET
                total_requests = total_requests + 1,
                total_prompt_tokens = total_prompt_tokens + $1,
                total_completion_tokens = total_completion_tokens + $2,
                total_cache_tokens = total_cache_tokens + $3,
                total_prompt_ms = total_prompt_ms + $4,
                total_completion_ms = total_completion_ms + $5
             WHERE id = $6",
        )
        .bind(t.usage_prompt_tokens)
        .bind(t.usage_completion_tokens)
        .bind(t.usage_cached_tokens)
        .bind(t.prompt_ms)
        .bind(t.predicted_ms)
        .bind(&t.session_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Session list with rollups - the "serve cache info to clients" surface.
    pub async fn list_sessions(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let rows: Vec<(String, Option<String>, i64, i64, i64, i64, f64, f64)> = sqlx::query_as(
            "SELECT id, model, total_requests, total_prompt_tokens, total_completion_tokens,
                    total_cache_tokens, total_prompt_ms, total_completion_ms
             FROM sessions ORDER BY updated_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, model, reqs, pt, ct, cache, pms, cms)| {
                serde_json::json!({
                    "id": id,
                    "model": model,
                    "requests": reqs,
                    "prompt_tokens": pt,
                    "completion_tokens": ct,
                    "cache_tokens": cache,
                    "cache_hit_pct": cache_hit_pct(cache, pt),
                    "prompt_ms": pms,
                    "completion_ms": cms,
                })
            })
            .collect())
    }

    /// M3 leftover: seed a fake turn + count (used by /api/session-test).
    pub async fn record_fake_turn(&self, session_id: &str, model: &str) -> Result<i64> {
        self.record_turn(&Turn {
            session_id: session_id.to_string(),
            request_model: model.to_string(),
            cache_n: 7,
            prompt_n: 3,
            prompt_per_second: 55.3,
            predicted_per_second: 243.2,
            predicted_n: 40,
            duration_ms: 435.57,
            ..Default::default()
        })
        .await?;
        self.count_turns(session_id).await
    }

    pub async fn count_turns(&self, session_id: &str) -> Result<i64> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM turns WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }
}

fn cache_hit_pct(cache: i64, prompt: i64) -> f64 {
    let total = cache + prompt;
    if total > 0 {
        (cache as f64 / total as f64) * 100.0
    } else {
        0.0
    }
}

fn gen_session_id() -> String {
    format!("{:012x}", rand::thread_rng().gen::<u64>())
}
