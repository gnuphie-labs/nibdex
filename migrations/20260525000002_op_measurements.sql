-- nibdex perf ledger (Day 3).
-- One row per measured operation: indexer passes today, MCP tool calls Day 6.
-- Shape is generic enough that the rmcp tool handlers can reuse the same Op helper.

CREATE TABLE op_measurements (
    id              INTEGER PRIMARY KEY,
    op_name         TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    rss_delta_bytes INTEGER,
    rss_after_bytes INTEGER,
    rows_in         INTEGER,
    rows_out        INTEGER,
    error           TEXT,
    extra_json      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_op_measurements_name_started_at ON op_measurements(op_name, started_at DESC);
