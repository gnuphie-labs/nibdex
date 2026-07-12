-- Pre-flip hardening (docs/SESSION_SCOPE_DESIGN.md §5): make `session_edges`
-- indexing an ADDITIVE MERGE instead of wipe-and-rebuild.
--
-- The spike's `index_sessions` wiped every row on each run, then rebuilt from
-- whatever transcripts still existed. Claude Code transcripts are retention-
-- bounded (~30d default, then deleted), so re-indexing permanently ERASED every
-- edge whose transcript had rotated out — destroying exactly the session history
-- the tool exists to preserve. This adds a dedupe key so `INSERT OR IGNORE` is
-- idempotent and re-indexing only ever GROWS the corpus.
--
-- The dedupe identity of an edge = (which assistant message, which Edit/Write
-- within that message). `message_uuid` is globally unique per assistant message,
-- so this dedups correctly across re-index and across a resumed transcript that
-- preserves uuids. Edges from a uuid-less message are skipped at ingest (they
-- cannot be safely deduped), not stored with an empty key.
--
-- ONE-TIME final wipe: the pre-existing spike rows carry no ordinal (they would
-- all default to 0 and collide under the new UNIQUE index), and `session_edges`
-- is a derived cache — the next `index-sessions` repopulates it with ordinals.
-- This is the LAST destructive pass; every run after it is a merge.

PRAGMA foreign_keys = ON;

DELETE FROM search_index WHERE source_table = 'session_edges';
DELETE FROM session_edges;

ALTER TABLE session_edges ADD COLUMN edge_ordinal INTEGER NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX idx_session_edges_dedupe
    ON session_edges(message_uuid, edge_ordinal);
