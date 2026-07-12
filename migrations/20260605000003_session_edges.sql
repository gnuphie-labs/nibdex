-- D1 SPIKE (docs/D1_SCOPE.md §10 "Session-coverage probe", gear 7): the RAW
-- TRANSCRIPT session→code map — the session leg of provenance the curated
-- CLAUDE.md note structurally loses. The probe (D1_SCOPE §10) proved a perfect
-- CLAUDE.md path-parser recovers only ~25% of a session's edit-edges because the
-- summary names code by *concept, not path* ("a `build` block on `check()`"); the
-- transcript carries every edge LOSSLESSLY, timestamped, and branch-anchored.
--
-- Each row is one Edit/Write tool-call from a `~/.claude/projects/<slug>/*.jsonl`
-- transcript: the file it touched (the "change"), the nearest preceding assistant
-- text (the "rationale"), and the per-line `gitBranch` + `cwd` + `timestamp` that
-- are the §1 commit join key, present for free. This is the §7 "change+rationale"
-- atom NATIVELY (tool_use = change, adjacent text = rationale) — the same object
-- as a commit (diff + message), as §7 predicted.
--
-- SPIKE SCOPE: we index the EDGE (which file, when, why), NOT the verbatim diff
-- content (old_string/new_string/Write body). Three reasons: (a) the code content
-- already lives in `diff_hunks`/`source_chunks` from git; (b) it keeps volume down
-- (21 transcripts × 1–2.7 MB here); (c) it sidesteps the worst IP-scrub concern —
-- only the user's own rationale prose + paths land here. (External export still
-- owes rationale-scrubbing; that is METRICS_EXPORT_SPEC's job, not this index's.)
--
-- Read-edges (the 14 `Read` targets a session builds against) are bonus design
-- context only the transcript has (D1_SCOPE §10 corollary 3) — recorded as a lever,
-- NOT indexed in this gear (write-edges are the authoring signal; Reads are next).

PRAGMA foreign_keys = ON;

CREATE TABLE session_edges (
    id            INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL,      -- the transcript's sessionId (session identity)
    message_uuid  TEXT NOT NULL,      -- the assistant message uuid that carried the edge
    tool          TEXT NOT NULL,      -- 'Edit' | 'Write'
    file_path     TEXT NOT NULL,      -- repo-relative path the edit targeted (cwd-stripped)
    repo_path     TEXT,               -- the cwd at edit time (the repo-root candidate)
    git_branch    TEXT,               -- per-line gitBranch — the commit join key
    edited_at     INTEGER NOT NULL,   -- epoch (the §1 commit-binding key)
    rationale     TEXT NOT NULL,      -- nearest preceding assistant text (the "why")
    -- Best-effort session→commit binding: the OLDEST commit on this repo that
    -- captured this file at/after `edited_at` (the next commit that committed the
    -- edit). SET NULL (not CASCADE) — re-indexing a commit must never delete the
    -- session record. NULL when no such commit is indexed (binding precision is a
    -- D1_SCOPE §10 unknown this gear is built to settle).
    commit_id     INTEGER REFERENCES commit_entries(id) ON DELETE SET NULL
);

CREATE INDEX idx_session_edges_session ON session_edges(session_id);
CREATE INDEX idx_session_edges_file    ON session_edges(file_path);
CREATE INDEX idx_session_edges_time    ON session_edges(edited_at);
CREATE INDEX idx_session_edges_commit  ON session_edges(commit_id);

-- search_index rows for session edges use source_table='session_edges',
-- kind='session_edge' — child table #7 in the same (source_table, rowid_ref)
-- FTS5 mold. Body = rationale + file_path, so a query finds the design-thread
-- BY ITS REASONING ("the build provenance work") or by the file it touched. No
-- FTS schema change.
