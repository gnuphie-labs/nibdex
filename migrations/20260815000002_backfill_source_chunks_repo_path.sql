-- Backfill `source_chunks.repo_path` for rows indexed before the column existed.
--
-- WITHOUT this, upgrading is a silent trap. `nibdex index` is incremental — the
-- write-amplification fix skips any file whose content hash is unchanged — so a
-- reindex after upgrading rewrites nothing and leaves every existing chunk NULL.
-- The command reports success. The new `repo` filter then matches zero rows on a
-- fully-populated index, which is precisely the confident-wrong-answer this
-- codebase keeps having to fix. Observed for real: a post-migration `nibdex
-- index` over a 1537-chunk corpus left 1537 rows NULL and said "Indexed 85
-- documents".
--
-- The value is DERIVED, not guessed. `documents.path` is absolute and
-- `source_chunks.path` is repo-relative to the same file, so the repo root is
-- the difference between them. Checked against the independent derivation
-- (`commit_entries.repo_path` via the provenance join) on a live 6-repo index:
-- the two agree on 1530/1530 chunks. The `LIKE` guard means any row that does
-- NOT satisfy the containment invariant is left NULL rather than given a wrong
-- answer — self-heal what is derivable, stay silent about what is not.
--
-- Idempotent (`WHERE repo_path IS NULL`), so it is a no-op on an index built by
-- a stamping extractor.
UPDATE source_chunks
SET repo_path = (
        SELECT substr(d.path, 1, length(d.path) - length(source_chunks.path) - 1)
        FROM documents d
        WHERE d.id = source_chunks.document_id
    )
WHERE repo_path IS NULL
  AND EXISTS (
        SELECT 1 FROM documents d
        WHERE d.id = source_chunks.document_id
          AND d.path LIKE '%/' || source_chunks.path
    );
