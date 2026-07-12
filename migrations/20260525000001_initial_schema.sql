-- nibdex initial schema (DESIGN §5.2).
-- Six typed tables + FTS5 virtual table. SQLite. Day 2.

PRAGMA foreign_keys = ON;

-- Raw unit of indexed file-based content (CLAUDE.md, memory files, design docs).
-- Git commits are NOT files in this sense; they live in commit_entries directly.
CREATE TABLE documents (
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,
    kind            TEXT NOT NULL,
    content_hash    TEXT NOT NULL,
    mtime           INTEGER NOT NULL,
    indexed_at      INTEGER NOT NULL
);

-- Session-history entries extracted from CLAUDE.md.
-- One row per "### #NNN" entry under "Recent session history".
CREATE TABLE session_entries (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    session_number  INTEGER NOT NULL,
    entry_date      TEXT,
    body            TEXT NOT NULL,
    files_touched   TEXT,
    todos_mentioned TEXT,
    decisions_made  TEXT,
    UNIQUE(session_number)
);

-- Git commit entries from one or more repositories under the workspace.
CREATE TABLE commit_entries (
    id              INTEGER PRIMARY KEY,
    repo_path       TEXT NOT NULL,
    commit_hash     TEXT NOT NULL,
    parent_hashes   TEXT,
    author_email    TEXT,
    author_name     TEXT,
    authored_at     INTEGER NOT NULL,
    committed_at    INTEGER NOT NULL,
    message_summary TEXT NOT NULL,
    message_body    TEXT,
    files_changed   TEXT,
    branch_refs     TEXT,
    UNIQUE(repo_path, commit_hash)
);
CREATE INDEX idx_commit_repo_authored_at ON commit_entries(repo_path, authored_at DESC);

-- Per-repo indexer cursor for incremental git extraction.
CREATE TABLE indexed_repos (
    repo_path           TEXT PRIMARY KEY,
    last_indexed_oid    TEXT NOT NULL,
    is_shallow          INTEGER NOT NULL DEFAULT 0,
    commit_count        INTEGER NOT NULL DEFAULT 0,
    last_indexed_at     INTEGER NOT NULL
);

-- Memory entries from ~/.claude/projects/.../memory/*.md.
CREATE TABLE memory_entries (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    memory_type     TEXT NOT NULL,
    description     TEXT,
    body            TEXT NOT NULL,
    UNIQUE(name)
);

-- Design-doc sections from docs/design/*.md.
CREATE TABLE design_doc_sections (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    heading_path    TEXT NOT NULL,
    line_start      INTEGER NOT NULL,
    line_end        INTEGER NOT NULL,
    body            TEXT NOT NULL
);

-- FTS5 virtual table — single retrieval surface across all content kinds.
CREATE VIRTUAL TABLE search_index USING fts5(
    body,
    kind UNINDEXED,
    rowid_ref UNINDEXED,
    source_table UNINDEXED
);
