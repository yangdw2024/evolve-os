PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

-- 1. Append-only event & cost ledger (single source of truth; integer MicroUsd)
CREATE TABLE IF NOT EXISTS ledger_events (
    seq_id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id            TEXT NOT NULL,
    node_id           TEXT NOT NULL,
    event_type        TEXT NOT NULL, -- 'llm_call' | 'exec' | 'verify' | 'refusal' | 'rollback' | 'migrate'
    model_tier        TEXT,          -- 'fast' | 'strong'
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    cost_micro_usd    INTEGER NOT NULL DEFAULT 0,
    payload_json      TEXT NOT NULL,
    created_at_ms     INTEGER NOT NULL
);

-- 2. Two-phase atomic budget reservations preventing concurrent multi-island overspend
CREATE TABLE IF NOT EXISTS budget_reservations (
    reservation_id     TEXT PRIMARY KEY,
    run_id             TEXT NOT NULL,
    node_id            TEXT NOT NULL,
    reserved_micro_usd INTEGER NOT NULL CHECK (reserved_micro_usd >= 0),
    status             TEXT NOT NULL CHECK (status IN ('pending', 'settled', 'released')),
    created_at_ms      INTEGER NOT NULL,
    updated_at_ms      INTEGER NOT NULL
);

-- 3. Content-addressed artifact blob store (supports Stage 1 SingleFile and Stage 2 FileMap/SkillBundle)
CREATE TABLE IF NOT EXISTS artifact_blobs (
    sha256        TEXT PRIMARY KEY,
    media_type    TEXT NOT NULL, -- 'text/plain' | 'application/vnd.evolve.filemap+json'
    content       BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL
);

-- 4. Evolutionary lineage nodes (NO UNIQUE on blob_sha256 so multiple islands can converge independently)
CREATE TABLE IF NOT EXISTS candidates (
    id              TEXT PRIMARY KEY,
    island_id       INTEGER NOT NULL,
    parent_id       TEXT REFERENCES candidates(id),
    base_commit_sha TEXT NOT NULL,
    artifact_type   TEXT NOT NULL CHECK (artifact_type IN ('code_diff', 'skill_bundle', 'prompt_policy')),
    diff_content    TEXT NOT NULL,
    blob_sha256     TEXT NOT NULL REFERENCES artifact_blobs(sha256),
    status          TEXT NOT NULL CHECK (status IN ('active', 'archived', 'blacklisted')),
    created_at_ms   INTEGER NOT NULL
);

-- 5. Deterministic Verifier Marks (strict physical Holdout feedback guard at DB layer)
CREATE TABLE IF NOT EXISTS marks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    candidate_id    TEXT NOT NULL REFERENCES candidates(id),
    verifier_id     TEXT NOT NULL,
    is_holdout      INTEGER NOT NULL DEFAULT 0 CHECK (is_holdout IN (0, 1)),
    passed          INTEGER NOT NULL CHECK (passed IN (0, 1)),
    primary_score   REAL NOT NULL,
    cell_key        TEXT NOT NULL,
    metrics_json    TEXT NOT NULL,
    public_feedback TEXT,
    created_at_ms   INTEGER NOT NULL,
    CHECK (is_holdout = 0 OR public_feedback IS NULL)
);

-- 6. Rebuildable MAP-Elites island grid projection (updated ONLY from non-holdout passing Marks)
CREATE TABLE IF NOT EXISTS map_elites_grid (
    island_id     INTEGER NOT NULL,
    cell_key      TEXT NOT NULL,
    candidate_id  TEXT NOT NULL REFERENCES candidates(id),
    primary_score REAL NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (island_id, cell_key)
);

CREATE INDEX IF NOT EXISTS idx_ledger_run ON ledger_events(run_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_reservations_run_status ON budget_reservations(run_id, status);
CREATE INDEX IF NOT EXISTS idx_candidates_island ON candidates(island_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_marks_cand_holdout ON marks(candidate_id, is_holdout, created_at_ms);
