-- A sixth corpus: the shape of the databases the workspace talks to.
--
-- WHY THIS EXISTS. Measured over 107 sessions in one large private workspace:
-- 253 tool calls did nothing but re-derive a database's shape (`information_
-- schema`, `sys.columns`), across 62% of sessions, and 27 more failed outright
-- on a column name that was guessed wrong. The same handful of tables were
-- re-introspected twenty-odd times each. Schema changes monthly and is
-- re-derived hundreds of times, which is the exact shape of work a derived
-- index is supposed to absorb.
--
-- The second-order benefit is worth more than the first: with types and widths
-- indexed, a mismatch is visible before it bites. A stored 12-character value
-- compared against a 40-character one silently never matches, and no amount of
-- reading the query finds that.
--
-- SOURCED FROM A DUMP FILE, NOT A CONNECTION. nibdex holds no credentials and
-- opens no socket — `SECURITY.md` states there is no network egress, and that
-- claim is worth more than the freshness a live connection would buy. A dump is
-- also the only shape that treats Postgres and SQL Server identically, and it
-- routes through the existing IP-domain machinery for free, because the file
-- lives in a labelled directory like any other artifact.
--
-- ONE ROW PER OBJECT, not per column. The caller's question is almost always
-- "what does this table look like", so the answer should be one row they can
-- read, not a join they must assemble. Columns ride as ordered JSON.
CREATE TABLE schema_objects (
    id              INTEGER PRIMARY KEY,
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    -- Logical database the dump came from. Free text: a workspace may talk to
    -- several, and the dump names itself rather than nibdex inferring it.
    database_name   TEXT NOT NULL,
    -- `public`, `dbo`, or a real namespace. Kept separate from `object_name`
    -- because callers write both `bids` and `sales.bids` for the same table,
    -- and a lookup must match either without string surgery at query time.
    schema_name     TEXT NOT NULL,
    object_name     TEXT NOT NULL,
    -- table | view | function. Views and functions matter more than they look:
    -- a predicate hidden inside a view body is not greppable in the source
    -- tree, so it is a blind spot for every tool the workspace already has.
    object_type     TEXT NOT NULL,
    -- Ordered [{name, type, nullable, max_len, default}]. JSON rather than a
    -- child table because nothing queries a single column in isolation, and a
    -- second table would turn the common read into a join for no gain.
    columns_json    TEXT NOT NULL,
    -- View/function body when the dump carries one; NULL for a plain table.
    definition      TEXT,
    -- The flattened rendering, which is both what FTS indexes and what a caller
    -- is shown. Stored rather than rebuilt per query so the hook's path stays
    -- one indexed lookup and one read.
    body            TEXT NOT NULL
);

-- The hook's path is an EXACT lookup on a table name pulled out of a SQL
-- statement, not a search — it fires on every shell call and must not pay FTS
-- for it. Case-insensitive because SQL is: a caller writing `Employees` must
-- reach the row stored as `employees`.
CREATE INDEX idx_schema_objects_name ON schema_objects(object_name COLLATE NOCASE);
CREATE INDEX idx_schema_objects_document ON schema_objects(document_id);
