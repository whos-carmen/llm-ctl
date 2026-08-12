-- Unique (session_id, turn_index): guards against duplicate turn indices
-- from concurrent record_turn calls.
CREATE UNIQUE INDEX IF NOT EXISTS idx_turns_session_turn ON turns (session_id, turn_index);
