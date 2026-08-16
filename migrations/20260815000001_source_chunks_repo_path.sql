-- Which repo a source chunk belongs to.
--
-- `source_chunks.path` is REPO-RELATIVE by design (it joins to
-- `commit_entries.files_changed`), so on any multi-repo index a `find_code` hit
-- came back as `src/mcp/query.rs` with nothing saying which tree that is. The
-- caller cannot open the file, and on a corpus where several repos share a
-- layout — 18 repos of mostly Rust, say — the same path names a different file
-- in each. This is also what forced the `nibdex hook` scope check to fall back
-- on tail-matching a directory name, which admits any repo's `src/`.
--
-- Stored rather than derived. It IS derivable two ways today — from
-- `commit_entries.repo_path` via the provenance join, or by subtracting the
-- relative path from `documents.path` — and both agree on 1530/1530 chunks of
-- the live index. Neither is good enough to build a filter on: provenance is
-- NULL for a repo with no commits, and the path arithmetic cannot use an index,
-- so `WHERE substr(...) = ?` scans. A stored column is indexable, survives a
-- repo with no commit history, and stops the schema depending on the unstated
-- invariant that `documents.path` always ends with `source_chunks.path`.
--
-- Nullable: rows written before this migration keep NULL until reindexed, and
-- a NULL must read as "unknown", never as "no repo". Existing rows are NOT
-- backfilled here — a backfill would have to guess, and this corpus is rebuilt
-- from source cheaply.
ALTER TABLE source_chunks ADD COLUMN repo_path TEXT;

CREATE INDEX idx_source_chunks_repo_path ON source_chunks(repo_path);
