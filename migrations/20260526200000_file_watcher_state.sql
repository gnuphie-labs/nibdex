-- nibdex Day 6 commit 4 — file_watcher daemon stats (D-6.3.3).
--
-- Singleton row (id=1) carries the running watcher's runtime state so a
-- separate `nibdex mcp` process can surface it via check().file_watcher.
-- stdio MCP mode itself does not run the watcher (D-6.4.1); the watcher
-- updates this row from `nibdex watch` (commit 4) and the future HTTP
-- daemon (commit 6).
--
-- Liveness rule: check() treats the row as "live" when last_heartbeat_ts is
-- within the last 60s. Stale rows surface as file_watcher: null so we never
-- claim a watcher is running when it has crashed.

CREATE TABLE file_watcher_state (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    started_at_ts            INTEGER NOT NULL,
    last_heartbeat_ts        INTEGER NOT NULL,
    last_event_ts            INTEGER,
    events_total             INTEGER NOT NULL DEFAULT 0,
    events_coalesced_total   INTEGER NOT NULL DEFAULT 0,
    subscriptions_json       TEXT NOT NULL
);
