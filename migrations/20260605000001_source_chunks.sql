-- D1: source/code chunks — the 5th corpus (DESIGN "the bet" / docs/D1_SCOPE.md).
-- The four knowledge layers (session, memory, design, commit) all point AT the
-- code; this table supplies the noun they join to. Each chunk is a fixed
-- line-window of a git-tracked file at its committed state, stamped with the
-- commit that last touched it so provenance rides WITH the code.
--
-- Shape mirrors the proven design_doc_sections mold (document_id + line range +
-- body) plus two D1 additions: `language` and the `last_commit_id` provenance FK.

PRAGMA foreign_keys = ON;

CREATE TABLE source_chunks (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    -- Denormalized from documents(path) on purpose: the commit<->code join in
    -- D1_SCOPE §G keys on source_chunks.path <-> commit_entries.files_changed,
    -- and find_code results carry the path per-chunk.
    path            TEXT NOT NULL,
    line_start      INTEGER NOT NULL,
    line_end        INTEGER NOT NULL,
    language        TEXT,
    body            TEXT NOT NULL,
    -- Provenance: the commit that last touched this chunk's file. SET NULL (not
    -- CASCADE) — re-indexing a commit must never delete the code it described.
    last_commit_id  INTEGER REFERENCES commit_entries(id) ON DELETE SET NULL
);

CREATE INDEX idx_source_chunks_document    ON source_chunks(document_id);
CREATE INDEX idx_source_chunks_path        ON source_chunks(path);
CREATE INDEX idx_source_chunks_last_commit ON source_chunks(last_commit_id);

-- search_index rows for source chunks use source_table='source_chunks',
-- kind='source'. No schema change to the FTS5 table — it already keys on
-- (source_table, rowid_ref), so source becomes child table #5 in the same mold.
