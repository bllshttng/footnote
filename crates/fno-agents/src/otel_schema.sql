CREATE TABLE IF NOT EXISTS api_requests (
    dedupe_key TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    ts TEXT,
    model TEXT,
    cost_usd_micros INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    skill_name TEXT,
    plugin_name TEXT,
    agent_name TEXT
);

CREATE TABLE IF NOT EXISTS otel_events (
    dedupe_key TEXT PRIMARY KEY NOT NULL,
    event_name TEXT NOT NULL,
    ts TEXT,
    session_id TEXT,
    prompt_id TEXT,
    attributes TEXT NOT NULL CHECK (json_valid(attributes)),
    resource TEXT NOT NULL CHECK (json_valid(resource))
);

CREATE INDEX IF NOT EXISTS api_requests_session_time ON api_requests(session_id, ts);
CREATE INDEX IF NOT EXISTS otel_events_session_time ON otel_events(session_id, ts);
