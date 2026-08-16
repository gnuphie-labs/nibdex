-- Per-session retrieval activity — the DENOMINATOR for nibdex's own usefulness.
--
-- Every existing instrument in nibdex (the JSONL sink, the cost ledger,
-- `check().cost_savings`) fires only when a nibdex tool is CALLED. That makes
-- them survivorship-biased by construction: they can report savings in
-- proportion to use, and they are structurally blind to non-use. A day on which
-- nibdex was never called records nothing at all, which reads as a quiet day
-- rather than as a total miss.
--
-- This table supplies what those instruments cannot see: how much retrieval work
-- happened in a session, against how much of it went to nibdex. It is derived
-- from the transcripts the session pass already reads, so it costs no new I/O.
--
-- COUNTS ONLY. No commands, no paths, no queries, no prose — the columns are
-- integers and an opaque session id. Nothing here can carry workspace content.
CREATE TABLE session_activity (
    session_id       TEXT PRIMARY KEY,
    -- Unix seconds of the earliest event seen for this session.
    first_seen       INTEGER NOT NULL,
    -- Tool calls that were RETRIEVAL: the built-in search tools, plus shell
    -- commands that are a search (grep/rg/find/ack/ag). Counting only the
    -- dedicated search tools badly understates the competition, because most
    -- retrieval in practice is a `grep` inside a shell call.
    retrieval_calls  INTEGER NOT NULL DEFAULT 0,
    -- Tool calls that went to a nibdex MCP tool.
    nibdex_calls     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_session_activity_first_seen ON session_activity(first_seen DESC);
