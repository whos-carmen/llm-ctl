CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    name TEXT,
    created_at DOUBLE PRECISION NOT NULL,
    updated_at DOUBLE PRECISION NOT NULL,
    model TEXT,
    total_requests BIGINT NOT NULL DEFAULT 0,
    total_prompt_tokens BIGINT NOT NULL DEFAULT 0,
    total_completion_tokens BIGINT NOT NULL DEFAULT 0,
    total_cache_tokens BIGINT NOT NULL DEFAULT 0,
    total_prompt_ms DOUBLE PRECISION NOT NULL DEFAULT 0,
    total_completion_ms DOUBLE PRECISION NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS turns (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    turn_index BIGINT NOT NULL,
    timestamp DOUBLE PRECISION NOT NULL,
    request_model TEXT,
    request_messages JSONB,
    request_max_tokens INTEGER,
    request_temperature DOUBLE PRECISION,
    response_id TEXT,
    response_content TEXT,
    response_finish_reason TEXT,
    cache_n BIGINT NOT NULL DEFAULT 0,
    prompt_n BIGINT NOT NULL DEFAULT 0,
    prompt_ms DOUBLE PRECISION NOT NULL DEFAULT 0,
    prompt_per_second DOUBLE PRECISION NOT NULL DEFAULT 0,
    predicted_n BIGINT NOT NULL DEFAULT 0,
    predicted_ms DOUBLE PRECISION NOT NULL DEFAULT 0,
    predicted_per_second DOUBLE PRECISION NOT NULL DEFAULT 0,
    usage_prompt_tokens BIGINT NOT NULL DEFAULT 0,
    usage_completion_tokens BIGINT NOT NULL DEFAULT 0,
    usage_cached_tokens BIGINT NOT NULL DEFAULT 0,
    duration_ms DOUBLE PRECISION NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id);