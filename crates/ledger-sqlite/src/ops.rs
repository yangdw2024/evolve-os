//! Inherent (non-trait) operational helpers used by the evolve loop:
//! audit events, holdout-driven blacklisting/rollback, and DB consistency audit.
//! These do NOT change any frozen `core-contracts` trait.

use crate::store::{map_sqlite_err, now_unix_ms, SqliteLedgerStore};
use core_contracts::{ContractError, MicroUsd};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditReport {
    pub event_counts: BTreeMap<String, i64>,
    pub candidates_total: i64,
    pub candidates_blacklisted: i64,
    pub marks_train: i64,
    pub marks_holdout: i64,
    pub holdout_marks_with_feedback: i64,
    pub grid_rows: i64,
    pub grid_rows_by_island: BTreeMap<i64, i64>,
    pub pending_reservations: i64,
    pub settled_reservations: i64,
    pub released_reservations: i64,
    pub llm_calls: i64,
    pub llm_spent_micro_usd: MicroUsd,
    /// Per-model call stats from `llm_call` payloads: model -> (calls, prompt_tok, completion_tok, cost).
    pub llm_by_model: BTreeMap<String, (i64, i64, i64, i64)>,
    pub patch_repairs_ok: i64,
    pub patch_repairs_failed: i64,
    /// Chronological train-mark curve for this run's candidates.
    pub score_curve: Vec<CurvePoint>,
    pub violations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurvePoint {
    pub seq: i64,
    pub candidate_id: String,
    pub passed: bool,
    pub primary_score: f64,
    pub best_so_far: f64,
    pub status: String,
}

fn count(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<i64, ContractError> {
    conn.query_row(sql, p, |r| r.get(0)).map_err(map_sqlite_err)
}

impl SqliteLedgerStore {
    /// Appends a zero-cost audit event (`refusal`, `rollback`, `migrate`, `patch_invalid`, ...).
    pub async fn append_event(
        &self,
        run_id: &str,
        node_id: &str,
        event_type: &str,
        payload_json: &str,
    ) -> Result<(), ContractError> {
        let (run_id, node_id, event_type, payload) = (
            run_id.to_string(),
            node_id.to_string(),
            event_type.to_string(),
            payload_json.to_string(),
        );
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO ledger_events (run_id, node_id, event_type, cost_micro_usd, payload_json, created_at_ms)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5)",
                params![run_id, node_id, event_type, payload, now_unix_ms()],
            )
            .map_err(map_sqlite_err)?;
            Ok(())
        })
        .await
    }

    pub async fn candidate_status(&self, candidate_id: &str) -> Result<Option<String>, ContractError> {
        let id = candidate_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row("SELECT status FROM candidates WHERE id = ?1", params![id], |r| r.get(0))
                .optional()
                .map_err(map_sqlite_err)
        })
        .await
    }

    /// Atomically blacklists a candidate, removes it from every MAP-Elites cell it occupies
    /// (including migrated copies) and restores each cell's best remaining *active* native
    /// elite from non-holdout passing marks. Returns the number of grid cells touched.
    pub async fn blacklist_candidate(
        &self,
        run_id: &str,
        candidate_id: &str,
        reason: &str,
    ) -> Result<usize, ContractError> {
        let (run_id, cand, reason) = (run_id.to_string(), candidate_id.to_string(), reason.to_string());
        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;
            let now = now_unix_ms();
            let updated = tx
                .execute("UPDATE candidates SET status = 'blacklisted' WHERE id = ?1", params![cand])
                .map_err(map_sqlite_err)?;
            if updated == 0 {
                return Err(ContractError::NotFound(format!("candidate {cand} not found")));
            }

            let cells: Vec<(i64, String)> = {
                let mut stmt = tx
                    .prepare("SELECT island_id, cell_key FROM map_elites_grid WHERE candidate_id = ?1")
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map(params![cand], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(map_sqlite_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite_err)?;
                rows
            };
            tx.execute("DELETE FROM map_elites_grid WHERE candidate_id = ?1", params![cand])
                .map_err(map_sqlite_err)?;

            let mut restored = Vec::new();
            for (island, cell) in &cells {
                let best: Option<(String, f64)> = tx
                    .query_row(
                        "SELECT m.candidate_id, m.primary_score
                         FROM marks m JOIN candidates c ON c.id = m.candidate_id
                         WHERE c.island_id = ?1 AND m.cell_key = ?2 AND m.is_holdout = 0
                           AND m.passed = 1 AND c.status = 'active'
                         ORDER BY m.primary_score DESC, m.id ASC LIMIT 1",
                        params![island, cell],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;
                if let Some((winner, score)) = best {
                    tx.execute(
                        "INSERT INTO map_elites_grid (island_id, cell_key, candidate_id, primary_score, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![island, cell, winner, score, now],
                    )
                    .map_err(map_sqlite_err)?;
                    restored.push(serde_json::json!({"island": island, "cell": cell, "restored": winner}));
                } else {
                    restored.push(serde_json::json!({"island": island, "cell": cell, "restored": null}));
                }
            }

            let payload = serde_json::json!({
                "candidate_id": cand,
                "reason": reason,
                "cells": restored,
            })
            .to_string();
            tx.execute(
                "INSERT INTO ledger_events (run_id, node_id, event_type, cost_micro_usd, payload_json, created_at_ms)
                 VALUES (?1, 'evolve-loop', 'rollback', 0, ?2, ?3)",
                params![run_id, payload, now],
            )
            .map_err(map_sqlite_err)?;
            tx.commit().map_err(map_sqlite_err)?;
            Ok(cells.len())
        })
        .await
    }

    /// Cross-table consistency audit. `violations` is empty iff all invariants hold.
    pub async fn audit(&self, run_id: &str, run_limit: Option<MicroUsd>) -> Result<AuditReport, ContractError> {
        let run_id = run_id.to_string();
        self.with_conn(move |conn| {
            let mut r = AuditReport::default();
            {
                let mut stmt = conn
                    .prepare("SELECT event_type, COUNT(*) FROM ledger_events GROUP BY event_type ORDER BY event_type")
                    .map_err(map_sqlite_err)?;
                for row in stmt
                    .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                    .map_err(map_sqlite_err)?
                {
                    let (k, v) = row.map_err(map_sqlite_err)?;
                    r.event_counts.insert(k, v);
                }
                let mut stmt = conn
                    .prepare("SELECT island_id, COUNT(*) FROM map_elites_grid GROUP BY island_id ORDER BY island_id")
                    .map_err(map_sqlite_err)?;
                for row in stmt
                    .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
                    .map_err(map_sqlite_err)?
                {
                    let (k, v) = row.map_err(map_sqlite_err)?;
                    r.grid_rows_by_island.insert(k, v);
                }
            }
            r.candidates_total = count(conn, "SELECT COUNT(*) FROM candidates", [])?;
            r.candidates_blacklisted = count(conn, "SELECT COUNT(*) FROM candidates WHERE status = 'blacklisted'", [])?;
            r.marks_train = count(conn, "SELECT COUNT(*) FROM marks WHERE is_holdout = 0", [])?;
            r.marks_holdout = count(conn, "SELECT COUNT(*) FROM marks WHERE is_holdout = 1", [])?;
            r.holdout_marks_with_feedback =
                count(conn, "SELECT COUNT(*) FROM marks WHERE is_holdout = 1 AND public_feedback IS NOT NULL", [])?;
            r.grid_rows = count(conn, "SELECT COUNT(*) FROM map_elites_grid", [])?;
            r.pending_reservations = count(conn, "SELECT COUNT(*) FROM budget_reservations WHERE run_id = ?1 AND status = 'pending'", params![run_id])?;
            r.settled_reservations = count(conn, "SELECT COUNT(*) FROM budget_reservations WHERE run_id = ?1 AND status = 'settled'", params![run_id])?;
            r.released_reservations = count(conn, "SELECT COUNT(*) FROM budget_reservations WHERE run_id = ?1 AND status = 'released'", params![run_id])?;
            r.llm_calls = count(conn, "SELECT COUNT(*) FROM ledger_events WHERE run_id = ?1 AND event_type = 'llm_call'", params![run_id])?;
            r.llm_spent_micro_usd = count(conn, "SELECT COALESCE(SUM(cost_micro_usd), 0) FROM ledger_events WHERE run_id = ?1", params![run_id])?;
            r.patch_repairs_ok = count(conn, "SELECT COUNT(*) FROM ledger_events WHERE run_id = ?1 AND event_type = 'patch_repair' AND json_extract(payload_json, '$.outcome') = 'repaired'", params![run_id])?;
            r.patch_repairs_failed = count(conn, "SELECT COUNT(*) FROM ledger_events WHERE run_id = ?1 AND event_type = 'patch_repair' AND json_extract(payload_json, '$.outcome') = 'failed'", params![run_id])?;
            {
                let mut stmt = conn
                    .prepare(
                        "SELECT COALESCE(json_extract(payload_json, '$.model_id'), '?'), COUNT(*),
                                SUM(prompt_tokens), SUM(completion_tokens), SUM(cost_micro_usd)
                         FROM ledger_events WHERE run_id = ?1 AND event_type = 'llm_call'
                         GROUP BY 1 ORDER BY 1",
                    )
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map(params![run_id], |row| {
                        Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
                    })
                    .map_err(map_sqlite_err)?;
                for row in rows {
                    let (k, v) = row.map_err(map_sqlite_err)?;
                    r.llm_by_model.insert(k, v);
                }
                let mut stmt = conn
                    .prepare(
                        "SELECT m.id, m.candidate_id, m.passed, m.primary_score, c.status
                         FROM marks m JOIN candidates c ON c.id = m.candidate_id
                         WHERE m.is_holdout = 0 AND m.candidate_id LIKE ?1 || '-%'
                         ORDER BY m.id",
                    )
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map(params![run_id], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, f64>(3)?, row.get::<_, String>(4)?))
                    })
                    .map_err(map_sqlite_err)?;
                let mut best = f64::NEG_INFINITY;
                for row in rows {
                    let (seq, cid, passed, score, status) = row.map_err(map_sqlite_err)?;
                    if passed == 1 && status == "active" {
                        best = best.max(score);
                    }
                    r.score_curve.push(CurvePoint {
                        seq,
                        candidate_id: cid,
                        passed: passed == 1,
                        primary_score: score,
                        best_so_far: if best.is_finite() { best } else { 0.0 },
                        status,
                    });
                }
            }

            let mut v = Vec::new();
            if r.holdout_marks_with_feedback > 0 {
                v.push(format!("{} holdout marks carry public_feedback", r.holdout_marks_with_feedback));
            }
            let bad_status = count(conn,
                "SELECT COUNT(*) FROM map_elites_grid g JOIN candidates c ON c.id = g.candidate_id WHERE c.status != 'active'", [])?;
            if bad_status > 0 {
                v.push(format!("{bad_status} grid rows reference non-active (blacklisted) candidates"));
            }
            let unbacked = count(conn,
                "SELECT COUNT(*) FROM map_elites_grid g WHERE NOT EXISTS (
                    SELECT 1 FROM marks m WHERE m.candidate_id = g.candidate_id AND m.cell_key = g.cell_key
                      AND m.is_holdout = 0 AND m.passed = 1 AND m.primary_score = g.primary_score)", [])?;
            if unbacked > 0 {
                v.push(format!("{unbacked} grid rows are not backed by a passing non-holdout mark"));
            }
            let verify_events = r.event_counts.get("verify").copied().unwrap_or(0);
            if verify_events != r.marks_train + r.marks_holdout {
                v.push(format!("verify events ({verify_events}) != marks ({})", r.marks_train + r.marks_holdout));
            }
            let blk_no_holdout = count(conn,
                "SELECT COUNT(*) FROM candidates c WHERE c.status = 'blacklisted' AND NOT EXISTS (
                    SELECT 1 FROM marks m WHERE m.candidate_id = c.id AND m.is_holdout = 1)", [])?;
            if blk_no_holdout > 0 {
                v.push(format!("{blk_no_holdout} blacklisted candidates lack a holdout mark"));
            }
            let rollbacks = r.event_counts.get("rollback").copied().unwrap_or(0);
            if rollbacks != r.candidates_blacklisted {
                v.push(format!("rollback events ({rollbacks}) != blacklisted candidates ({})", r.candidates_blacklisted));
            }
            if r.pending_reservations > 0 {
                v.push(format!("{} budget reservations still pending", r.pending_reservations));
            }
            if r.llm_calls != r.settled_reservations {
                v.push(format!("llm_call events ({}) != settled reservations ({})", r.llm_calls, r.settled_reservations));
            }
            if let Some(limit) = run_limit {
                if r.llm_spent_micro_usd > limit {
                    v.push(format!("spent {} > limit {limit} micro-USD", r.llm_spent_micro_usd));
                }
            }
            r.violations = v;
            Ok(r)
        })
        .await
    }
}
