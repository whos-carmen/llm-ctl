//! Session/cache store on the ops tier (PostgreSQL on CT `201`).

use anyhow::Result;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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

    /// Seed a fake turn + read back the count (M3 end-to-end proof).
    pub async fn record_fake_turn(&self, session_id: &str, model: &str) -> Result<i64> {
        sqlx::query(
            "INSERT INTO sessions (id, created_at, updated_at, model) \
             VALUES ($1, extract(epoch from now()), extract(epoch from now()), $2) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(session_id)
        .bind(model)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO turns (session_id, turn_index, timestamp, request_model, cache_n, prompt_n, \
             prompt_per_second, predicted_per_second, duration_ms) \
             VALUES ($1, 0, extract(epoch from now()), $2, 7, 3, 55.3, 243.2, 435.57)",
        )
        .bind(session_id)
        .bind(model)
        .execute(&self.pool)
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