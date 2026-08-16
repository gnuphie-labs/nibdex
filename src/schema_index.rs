// SPDX-License-Identifier: MIT

//! The sixth corpus: the shape of the databases a workspace talks to.
//!
//! # Why
//!
//! Measured over 107 sessions in one large private workspace: **253 tool calls
//! did nothing but re-derive a database's shape**, across 62% of sessions, and
//! 27 more failed outright on a guessed column name. The same tables were
//! re-introspected twenty-odd times each. Schema changes monthly and gets
//! re-derived hundreds of times — the exact work a derived index should absorb.
//!
//! # A dump, not a connection
//!
//! nibdex reads a JSON dump the user produces; it holds no credentials and opens
//! no socket. `SECURITY.md` states there is no network egress, and keeping that
//! claim true is worth more than the freshness a live connection would buy. A
//! dump also treats Postgres and SQL Server identically, and routes through the
//! IP-domain machinery for free because it is just a file in a labelled tree.
//!
//! `nibdex schema-dump-query --dialect postgres` prints the query that produces
//! one, so making a dump is a single pipe with no new dependency.
//!
//! # Discovery by convention
//!
//! Any file matching [`SCHEMA_DUMP_SUFFIX`] anywhere in the workspace is a dump.
//! Nothing to configure and nothing to remember — the whole competition with
//! `grep` is that `grep` needs you to know nothing about it, so a corpus that
//! required a flag to work would lose before it started.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// Files ending in this are schema dumps. Distinctive enough that an ordinary
/// `schema.json` — a JSON Schema document, an API contract — is not swept in by
/// accident.
pub const SCHEMA_DUMP_SUFFIX: &str = ".nibdex-schema.json";

/// One column, in ordinal order.
///
/// `max_len` is carried because it is the field that produces silent bugs: a
/// value stored in 12 characters compared against one declared 40 never
/// matches, and reading the query does not reveal it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub data_type: String,
    #[serde(default)]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_len: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// One table, view or function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub name: String,
    #[serde(default = "default_object_type", rename = "type")]
    pub object_type: String,
    #[serde(default)]
    pub columns: Vec<Column>,
    /// View or function body. The reason views and functions are indexed at all:
    /// a predicate hidden inside a view is not greppable in the source tree, so
    /// it is invisible to every other tool the workspace has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

fn default_schema() -> String {
    "public".to_string()
}
fn default_object_type() -> String {
    "table".to_string()
}

/// A whole dump.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaDump {
    /// Logical name of the database this describes. The dump names itself
    /// rather than nibdex guessing from a filename, because one workspace
    /// commonly talks to several and the filename is the user's business.
    pub database: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// When the dump was taken, epoch seconds. Optional, and its absence is
    /// reported rather than defaulted to now — a freshness claim nibdex invented
    /// would be worse than no claim at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<i64>,
    pub objects: Vec<SchemaObject>,
}

/// Parse a dump, naming the file when it fails.
///
/// A malformed dump must be a loud error at index time, not a silently empty
/// corpus: an empty corpus and an absent one look identical to every caller
/// downstream, and telling those apart is the whole point of `corpus_empty`.
pub fn parse_dump(text: &str, path: &Path) -> Result<SchemaDump> {
    serde_json::from_str(text).with_context(|| format!("parse schema dump {}", path.display()))
}

/// Render one object as the text a caller reads AND the text FTS indexes.
///
/// Rendered once at index time rather than per query: the hook's path must stay
/// one indexed lookup plus one read, because it fires on every shell call.
///
/// The qualified name appears on the first line so that a search for either
/// `bids` or `sales.bids` reaches the row, and every column name is present as
/// its own token so a caller who half-remembers a column can find its table.
pub fn render_object(db: &str, o: &SchemaObject) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}.{}.{} ({})\n", db, o.schema, o.name, o.object_type));
    for c in &o.columns {
        s.push_str(&format!("  {} {}", c.name, c.data_type));
        if let Some(n) = c.max_len {
            s.push_str(&format!("({n})"));
        }
        // NOT NULL is the half worth stating outright. "nullable" is the common
        // case and adding it to every line would bury the constraint that
        // actually changes how a query must be written.
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        if let Some(d) = &c.default {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        s.push('\n');
    }
    if let Some(d) = &o.definition {
        s.push_str("  definition:\n");
        for line in d.lines() {
            s.push_str(&format!("    {line}\n"));
        }
    }
    s
}

/// Replace this document's objects with the dump's contents.
///
/// Delete-then-insert rather than upsert: a dropped table must DISAPPEAR. An
/// upsert would leave it in the index forever, and a schema index that keeps
/// answering for a table that no longer exists is worse than no schema index —
/// it is confidently wrong, which is the failure mode this whole project keeps
/// finding in its own instruments.
pub async fn persist_dump(pool: &SqlitePool, document_id: i64, dump: &SchemaDump) -> Result<usize> {
    sqlx::query(
        "DELETE FROM search_index WHERE source_table = 'schema_objects' AND rowid_ref IN \
         (SELECT id FROM schema_objects WHERE document_id = ?)",
    )
    .bind(document_id)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM schema_objects WHERE document_id = ?")
        .bind(document_id)
        .execute(pool)
        .await?;

    for o in &dump.objects {
        let body = render_object(&dump.database, o);
        let columns_json = serde_json::to_string(&o.columns)?;
        let row: (i64,) = sqlx::query_as(
            r#"
            INSERT INTO schema_objects
                (document_id, database_name, schema_name, object_name, object_type,
                 columns_json, definition, body)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(document_id)
        .bind(&dump.database)
        .bind(&o.schema)
        .bind(&o.name)
        .bind(&o.object_type)
        .bind(&columns_json)
        .bind(o.definition.as_deref())
        .bind(&body)
        .fetch_one(pool)
        .await?;

        sqlx::query(
            "INSERT INTO search_index (body, kind, rowid_ref, source_table) \
             VALUES (?, 'schema', ?, 'schema_objects')",
        )
        .bind(&body)
        .bind(row.0)
        .execute(pool)
        .await?;
    }
    Ok(dump.objects.len())
}

/// One object as the hook hands it back.
///
/// There is deliberately no `find_schema` MCP tool for this to feed — the hook is
/// the delivery path, because a new tool would be held deferred like the other
/// seven and get called as rarely. Naming an unbuilt function here would be the
/// same defect as the `check()` doc-string that described its orphan computation
/// as stubbed long after it worked: a protocol-adjacent comment asserting
/// something that is not true, which is exactly what a reader has no way to check.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaHit {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub object_type: String,
    pub body: String,
}

/// Split a SQL identifier into its optional schema and its bare name.
///
/// Callers write `bids`, `sales.bids` and `db.sales.bids` for one table. The
/// LAST component is always the object; anything immediately before it is the
/// namespace. Quotes and brackets are stripped because `[dbo].[Employees]` and
/// `"public"."bids"` are the same names wearing two dialects' punctuation.
pub fn split_qualified(raw: &str) -> (Option<String>, String) {
    let clean = |s: &str| s.trim_matches(|c| c == '"' || c == '[' || c == ']' || c == '`').to_string();
    let parts: Vec<&str> = raw.split('.').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => (None, String::new()),
        [name] => (None, clean(name)),
        [.., schema, name] => (Some(clean(schema)), clean(name)),
    }
}

/// Exact lookup by table name — the hook's path.
///
/// Deliberately NOT a search. This runs behind a hook that fires on every shell
/// call, so it must be an indexed equality probe; paying FTS here would tax the
/// one retrieval path that is currently free.
///
/// Case-insensitive, because SQL is: a caller writing `Employees` must reach the
/// row stored as `employees`. When the caller qualified the name, the namespace
/// is honoured — two schemas may hold a table of the same name, and answering
/// from the wrong one is the `find_code` cross-repo bug in a new costume.
pub async fn lookup_object(pool: &SqlitePool, raw: &str) -> Result<Vec<SchemaHit>> {
    let (schema, name) = split_qualified(raw);
    if name.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String, String, String, String)> = match &schema {
        Some(s) => {
            sqlx::query_as(
                "SELECT database_name, schema_name, object_name, object_type, body \
                 FROM schema_objects \
                 WHERE object_name = ? COLLATE NOCASE AND schema_name = ? COLLATE NOCASE",
            )
            .bind(&name)
            .bind(s)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT database_name, schema_name, object_name, object_type, body \
                 FROM schema_objects WHERE object_name = ? COLLATE NOCASE",
            )
            .bind(&name)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|(database, schema, name, object_type, body)| SchemaHit {
            database,
            schema,
            name,
            object_type,
            body,
        })
        .collect())
}

/// The introspection query for a dialect, emitting one JSON document in the
/// shape [`SchemaDump`] expects.
///
/// Shipped with the binary rather than written in the README, so producing a
/// dump needs no copy-paste from a web page and cannot drift from the parser.
pub fn dump_query(dialect: &str) -> Option<&'static str> {
    match dialect {
        "postgres" | "postgresql" | "pg" => Some(POSTGRES_DUMP_QUERY),
        "mssql" | "sqlserver" | "sql-server" => Some(MSSQL_DUMP_QUERY),
        _ => None,
    }
}

/// Postgres. Run with `psql -At -d <db> -f -` and redirect to a
/// `*.nibdex-schema.json` file inside the workspace.
///
/// Views and functions are included deliberately; a predicate inside a view body
/// is invisible to a source-tree grep.
const POSTGRES_DUMP_QUERY: &str = r#"-- nibdex schema dump (PostgreSQL)
--   psql -At -d YOURDB -f this.sql > db.nibdex-schema.json
SELECT json_build_object(
  'database', current_database(),
  'dialect', 'postgres',
  'generated_at', extract(epoch FROM now())::bigint,
  'objects', COALESCE((
    SELECT json_agg(o ORDER BY o->>'schema', o->>'name')
    FROM (
      SELECT json_build_object(
        'schema', c.table_schema,
        'name', c.table_name,
        'type', CASE t.table_type WHEN 'VIEW' THEN 'view' ELSE 'table' END,
        'columns', COALESCE((
          SELECT json_agg(json_build_object(
                   'name', k.column_name,
                   'type', k.data_type,
                   'nullable', k.is_nullable = 'YES',
                   'max_len', k.character_maximum_length,
                   'default', k.column_default
                 ) ORDER BY k.ordinal_position)
          FROM information_schema.columns k
          WHERE k.table_schema = c.table_schema AND k.table_name = c.table_name
        ), '[]'::json),
        'definition', v.view_definition
      ) AS o
      FROM (SELECT DISTINCT table_schema, table_name FROM information_schema.columns) c
      JOIN information_schema.tables t
        ON t.table_schema = c.table_schema AND t.table_name = c.table_name
      LEFT JOIN information_schema.views v
        ON v.table_schema = c.table_schema AND v.table_name = c.table_name
      WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema')
    ) s
  ), '[]'::json)
);
"#;

/// SQL Server. The invocation is fussier than the Postgres one and every part of
/// it was established by RUNNING it against two real databases, not by reading
/// the manual:
///
/// - **`-y 0` is required** — without it sqlcmd truncates the JSON value at the
///   default display width and the file is silently cut off mid-object.
/// - **`-h -1` and `-W` cannot accompany it.** Both are rejected outright:
///   *"the -h and the -y 0 options are mutually exclusive"*, and likewise for
///   `-W`. The recipe this comment used to give was `-h -1 -y 0 -W`, which sqlcmd
///   refuses to run at all — it had never been executed.
/// - **`SET NOCOUNT ON` is inside the query**, because sqlcmd otherwise appends
///   `(N rows affected)` and the output is then not JSON.
/// - **`tr -d '\n'` reassembles** the wrapped lines; sqlcmd wraps the single long
///   value across lines without altering it, so concatenation is lossless.
const MSSQL_DUMP_QUERY: &str = r#"-- nibdex schema dump (SQL Server)
--   sqlcmd -S SERVER -U USER -d YOURDB -y 0 -i this.sql | tr -d '\n' > db.nibdex-schema.json
SET NOCOUNT ON;
SELECT
  DB_NAME() AS [database],
  'mssql'   AS [dialect],
  DATEDIFF_BIG(SECOND, '1970-01-01', GETUTCDATE()) AS [generated_at],
  (SELECT
      s.name  AS [schema],
      o.name  AS [name],
      CASE o.type WHEN 'V' THEN 'view' ELSE 'table' END AS [type],
      m.definition AS [definition],
      (SELECT c.name                AS [name],
              ty.name               AS [type],
              CAST(c.is_nullable AS bit) AS [nullable],
              -- sys.columns.max_length is STORAGE BYTES, and it is populated for
              -- every type: -1 means MAX, 8 means a bigint occupies 8 bytes. Read
              -- as a width it renders id bigint(8), and doubles every Unicode
              -- string -- nvarchar(50) stores 100 bytes, so it printed
              -- nvarchar(100). A width is the one fact a caller acts on without
              -- checking, so a wrong one is worse than none.
              --
              -- Decided on the BASE SYSTEM type (st), not the declared one (ty).
              -- An alias type carries its own name: sysname is nvarchar(128) and
              -- reports max_length 256, so a name-matched rule leaves it blank --
              -- which is what testing against two real databases caught, and it
              -- is the same for any CREATE TYPE ... FROM nvarchar(n). Resolving
              -- to the base type handles every alias at once instead of listing
              -- them. The DECLARED name is still what gets reported as the type.
              CASE
                WHEN c.max_length = -1                        THEN NULL
                WHEN st.name IN ('nvarchar', 'nchar')         THEN c.max_length / 2
                WHEN st.name IN ('varchar', 'char',
                                 'varbinary', 'binary')       THEN c.max_length
                ELSE NULL
              END AS [max_len],
              OBJECT_DEFINITION(c.default_object_id) AS [default]
       FROM sys.columns c
       JOIN sys.types ty ON ty.user_type_id = c.user_type_id
       JOIN sys.types st ON st.user_type_id = ty.system_type_id AND st.is_user_defined = 0
       WHERE c.object_id = o.object_id
       ORDER BY c.column_id
       FOR JSON PATH) AS [columns]
   FROM sys.objects o
   JOIN sys.schemas s ON s.schema_id = o.schema_id
   LEFT JOIN sys.sql_modules m ON m.object_id = o.object_id
   WHERE o.type IN ('U', 'V')
   ORDER BY s.name, o.name
   FOR JSON PATH) AS [objects]
FOR JSON PATH, WITHOUT_ARRAY_WRAPPER;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL Server dump must not hand back STORAGE BYTES as a column width,
    /// and must carry a default like the Postgres one always has.
    ///
    /// `sys.columns.max_length` is bytes for every type, so `NULLIF(max_length,
    /// -1)` rendered `id bigint(8)`, `uniqueidentifier(16)`, and — the dangerous
    /// one — `nvarchar(50)` as `nvarchar(100)`, because a Unicode string stores
    /// two bytes per character. That is an authoritative-looking answer with a
    /// silently doubled width, injected unsolicited: exactly the confidently-wrong
    /// output this corpus was built to prevent.
    ///
    /// 🟢 DRIVEN AGAINST TWO REAL SQL SERVER DATABASES (2026-08-16), which is what
    /// this test can only approximate. Measured against the server's own
    /// `INFORMATION_SCHEMA.CHARACTER_MAXIMUM_LENGTH` over every table and view
    /// column: the OLD expression emitted a width for **1,089 columns that have no
    /// character width at all** and a **wrong** width for **701 more** — 1,790 of
    /// 1,951 columns, 92%, on one database; 1,632 of 1,762, 93%, on the other. The
    /// new expression: **0 mismatches on both.**
    ///
    /// That run also found the case reasoning had missed — `sysname`, the built-in
    /// alias for `nvarchar(128)`, reports `max_length` 256 under its own type name
    /// — which is why the rule now resolves the BASE SYSTEM type rather than
    /// matching declared names. This test pins the shape; the numbers above are
    /// the evidence.
    #[test]
    fn the_mssql_dump_query_converts_byte_widths_and_carries_a_default() {
        assert!(
            !MSSQL_DUMP_QUERY.contains("NULLIF(c.max_length"),
            "the raw byte width is back; nvarchar would double again"
        );
        assert!(
            MSSQL_DUMP_QUERY.contains("c.max_length / 2"),
            "nchar/nvarchar store 2 bytes per character and must be halved"
        );
        assert!(
            MSSQL_DUMP_QUERY.contains("st.user_type_id = ty.system_type_id"),
            "widths must be decided on the BASE system type, or every alias type \
             (sysname, and any CREATE TYPE ... FROM nvarchar) silently loses its width"
        );
        assert!(
            MSSQL_DUMP_QUERY.contains("st.name IN ('nvarchar', 'nchar')"),
            "the halving rule must read the base type, not the declared one"
        );
        assert!(
            MSSQL_DUMP_QUERY.contains("OBJECT_DEFINITION(c.default_object_id)"),
            "MSSQL selected no default at all, so render_object's DEFAULT branch \
             was dead for every SQL Server dump while Postgres carried one"
        );
        // Parity with the query that was right all along.
        assert!(POSTGRES_DUMP_QUERY.contains("k.column_default"));
        assert!(POSTGRES_DUMP_QUERY.contains("k.character_maximum_length"));
    }

    fn obj(name: &str, cols: &[(&str, &str, bool, Option<i64>)]) -> SchemaObject {
        SchemaObject {
            schema: "public".into(),
            name: name.into(),
            object_type: "table".into(),
            columns: cols
                .iter()
                .map(|(n, t, nullable, max_len)| Column {
                    name: (*n).into(),
                    data_type: (*t).into(),
                    nullable: *nullable,
                    max_len: *max_len,
                    default: None,
                })
                .collect(),
            definition: None,
        }
    }

    /// The rendering IS the answer a caller reads, so the two facts that produce
    /// silent bugs must survive it: the declared width, and NOT NULL.
    #[test]
    fn rendering_carries_width_and_not_null() {
        let o = obj(
            "orders",
            &[("id", "integer", false, None), ("code", "character varying", true, Some(12))],
        );
        let out = render_object("shopdb", &o);
        assert!(out.contains("shopdb.public.orders (table)"), "{out}");
        assert!(out.contains("id integer NOT NULL"), "{out}");
        // The width is the whole point: a 12-char column compared against a
        // 40-char value never matches, and nothing in the query reveals it.
        assert!(out.contains("code character varying(12)"), "{out}");
        // A nullable column must NOT be labelled NOT NULL.
        assert!(!out.contains("varying(12) NOT NULL"), "{out}");
    }

    /// A caller writes the same table three ways. All three must reach one row,
    /// and a namespace must not be mistaken for the object.
    #[test]
    fn qualified_names_split_into_namespace_and_object() {
        assert_eq!(split_qualified("bids"), (None, "bids".to_string()));
        assert_eq!(split_qualified("sales.bids"), (Some("sales".into()), "bids".into()));
        // Three parts: the middle is the namespace, the database prefix is not
        // the object. Getting this backwards answers for the wrong table.
        assert_eq!(split_qualified("mydb.sales.bids"), (Some("sales".into()), "bids".into()));
        assert_eq!(split_qualified("[dbo].[Employees]"), (Some("dbo".into()), "Employees".into()));
        assert_eq!(split_qualified("\"public\".\"bids\""), (Some("public".into()), "bids".into()));
        assert_eq!(split_qualified(""), (None, String::new()));
    }

    /// A dump with an unknown extra field must still parse — the producer is a
    /// SQL query a user may extend, and a corpus that refuses a superset of what
    /// it needs would break on the first person who added a column.
    #[test]
    fn parse_tolerates_extra_fields_and_defaults_the_optional_ones() {
        let text = r#"{
            "database": "shopdb",
            "objects": [
              {"name": "orders", "columns": [{"name": "id", "type": "int"}], "owner": "alice"}
            ],
            "extra_top_level": 1
        }"#;
        let d = parse_dump(text, Path::new("x.json")).expect("must tolerate extras");
        assert_eq!(d.database, "shopdb");
        assert_eq!(d.objects.len(), 1);
        assert_eq!(d.objects[0].schema, "public", "schema defaults");
        assert_eq!(d.objects[0].object_type, "table", "type defaults");
        // generated_at absent must stay absent. Defaulting it to now would be
        // nibdex inventing a freshness claim, which is worse than none.
        assert!(d.generated_at.is_none());
    }

    /// A malformed dump must fail loudly and name the file. A silently empty
    /// corpus is indistinguishable from an absent one to every caller.
    #[test]
    fn a_malformed_dump_is_an_error_naming_the_file() {
        let err = parse_dump("{ not json", Path::new("/w/db.nibdex-schema.json")).unwrap_err();
        assert!(format!("{err}").contains("db.nibdex-schema.json"), "{err}");
    }

    /// Both dialects must be reachable under the spellings people actually type,
    /// and an unknown dialect must be rejected rather than silently served the
    /// wrong engine's query.
    #[test]
    fn dump_query_resolves_known_dialects_only() {
        assert!(dump_query("postgres").unwrap().contains("information_schema.columns"));
        assert!(dump_query("pg").is_some());
        assert!(dump_query("mssql").unwrap().contains("sys.columns"));
        assert!(dump_query("sqlserver").is_some());
        assert!(dump_query("oracle").is_none());
        assert!(dump_query("").is_none());
    }
}
