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
            .acquire_timeout(std::time::Duration::from_secs(5))
            .max_lifetime(Some(std::time::Duration::from_secs(30 * 60)))
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

    /// Session id: client-supplied, else a brand-new session. No implicit
    /// "recent session" reuse — sessions are bound only by the client's
    /// explicit id (X-Session-Id header or the pi /api/session-active hint).
    /// Untrusted client ids are validated (sanitized charset/len); invalid ones
    /// fall back to a fresh generated id rather than being stored verbatim.
    pub async fn resolve_session(&self, client_id: Option<&str>, model: &str) -> Result<String> {
        let sid = client_id
            .map(str::trim)
            .filter(|s| valid_session_id(s))
            .map(String::from)
            .unwrap_or_else(gen_session_id);
        sqlx::query(
            "INSERT INTO sessions (id, created_at, updated_at, model) \
             VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2) \
             ON CONFLICT (id) DO UPDATE SET model = excluded.model, updated_at = excluded.updated_at",
        )
        .bind(&sid)
        .bind(model)
        .execute(&self.pool)
        .await?;
        Ok(sid)
    }

    /// Persist one turn atomically: session row, turn insert (unique index on
    /// (session_id, turn_index)), and rollup - all in one transaction.
    pub async fn record_turn(&self, t: &Turn) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // Serialize concurrent inserts for the same session so turn_index can't
        // collide (single-active proxy usually prevents this; this hardens
        // record_fake_turn / future endpoints). Advisory lock releases at commit.
        let key = session_lock_key(&t.session_id);
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(key)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO sessions (id, created_at, updated_at, model) \
             VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2) \
             ON CONFLICT (id) DO UPDATE SET model = excluded.model, updated_at = excluded.updated_at",
        )
        .bind(&t.session_id)
        .bind(&t.request_model)
        .execute(&mut *tx)
        .await?;

        let idx: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(turn_index), -1) + 1 FROM turns WHERE session_id = $1",
        )
        .bind(&t.session_id)
        .fetch_one(&mut *tx)
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
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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

/// Client-supplied session ids are treated as untrusted text: allow a compact
/// printable subset (for DB + HTML safety) and a sane length cap. Invalid ids
/// fall back to a generated one rather than being stored verbatim.
fn valid_session_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Stable 63-bit hash of a session id for pg_advisory_xact_lock.
fn session_lock_key(sid: &str) -> i64 {
    // FNV-1a 64-bit, masked to i64::MAX.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sid.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & i64::MAX as u64) as i64
}
