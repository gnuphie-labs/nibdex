-- nibdex §8.4 Layer 2 — cost-savings ledger (D-8.4).
--
-- Append-only per-emission row. One INSERT per successful tool call that
-- carries Layer-2 fields (i.e. calibration model is loaded + tool entry
-- resolves). The check() rollup in CheckResult.cost_savings reads this
-- table via three WHERE ts >= ? aggregates (1d/7d/30d windows, D-8.5).
--
-- `tokens_saved_p50` is SIGNED (INTEGER, not INTEGER CHECK >= 0) because
-- `result_tokens > counterfactual_tokens_p50` is a real outcome — a
-- high-volume query can legitimately produce more text than the
-- counterfactual human-orientation pass would have read. Truncating to
-- zero would lose that honesty signal (D-8.4).
--
-- `calibration_model_version` is stamped per row so retroactive
-- recalibration is reproducible: when the model bumps from v0.1 to v0.2,
-- historical rows keep their v0.1 stamps and check() aggregates can
-- partition by version cleanly.

CREATE TABLE cost_ledger_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    tool TEXT NOT NULL,
    result_tokens INTEGER NOT NULL,
    counterfactual_tokens_p50 INTEGER NOT NULL,
    counterfactual_tokens_p95 INTEGER NOT NULL,
    tokens_saved_p50 INTEGER NOT NULL,
    dollars_saved_p50_usd REAL NOT NULL,
    wall_ms INTEGER NOT NULL,
    counterfactual_wall_ms_p50 INTEGER NOT NULL,
    calibration_model_version TEXT NOT NULL
);

CREATE INDEX idx_cost_ledger_events_ts ON cost_ledger_events(ts);
CREATE INDEX idx_cost_ledger_events_tool ON cost_ledger_events(tool);
