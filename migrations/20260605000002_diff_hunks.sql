-- D1 SPIKE (docs/D1_SCOPE.md §10 "Reframe"): the change/content map — the second
-- artifact in the §10 split ("snapshot chunks for SEARCH + a diff/content map for
-- PROVENANCE"). Where `source_chunks` indexes code at its committed snapshot (and
-- stamps the file's LAST-touch commit — which the gear-4 eyeball proved breaks on
-- refactors/moves), `diff_hunks` indexes the **added lines of every commit's diff**,
-- each natively bound to the commit that introduced them. The atom is change+rationale.
--
-- Why this recovers the true author the snapshot loses: a moved line appears as an
-- ADDITION in BOTH the authoring commit and the later mover, so a content query
-- matches both — and the OLDEST matching change (min authored_at) is the author,
-- reachable straight through every mechanical move. (Pickaxe `git log -S` semantics,
-- precomputed into an index instead of probed per-query.) No reverse blame.
--
-- SPIKE: added lines only (the authoring signal); deletions/context not indexed yet.
-- `-C` copy-detection deliberately NOT used — oldest-wins resolves the author without
-- it; recorded as a lever if oldest-wins proves insufficient (D1_SCOPE §10 open).

PRAGMA foreign_keys = ON;

CREATE TABLE diff_hunks (
    id            INTEGER PRIMARY KEY,
    -- Provenance FK, resolved from commit_hash. SET NULL (not CASCADE): re-indexing
    -- commits must never delete the change-record. NULL when the commit isn't in
    -- `commit_entries` (e.g. `nibdex index` not run, or capped out).
    commit_id     INTEGER REFERENCES commit_entries(id) ON DELETE SET NULL,
    -- Carried inline too, so the probe needs no join to resolve the author:
    commit_hash   TEXT NOT NULL,
    authored_at   INTEGER NOT NULL,   -- the oldest-wins origin-resolution key
    summary       TEXT NOT NULL,      -- commit subject (%s) — the rationale, in brief
    file_path     TEXT NOT NULL,      -- new-side path (joins commit_entries.files_changed)
    new_start     INTEGER NOT NULL,   -- 1-based line where the added block lands in the new file
    added_body    TEXT NOT NULL       -- the '+' lines of one contiguous added block
);

CREATE INDEX idx_diff_hunks_commit     ON diff_hunks(commit_id);
CREATE INDEX idx_diff_hunks_authored   ON diff_hunks(authored_at);
CREATE INDEX idx_diff_hunks_file       ON diff_hunks(file_path);

-- search_index rows for diff hunks use source_table='diff_hunks', kind='diff' —
-- child table #6 in the same (source_table, rowid_ref) FTS5 mold. No FTS schema change.
