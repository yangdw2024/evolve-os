use crate::store::{decode_payload, encode_payload, map_sqlite_err, now_unix_ms, SqliteLedgerStore};
use async_trait::async_trait;
use core_contracts::{
    ArtifactPayload, ArtifactType, CandidateSnapshot, ContractError, EvolutionArchive, Mark,
    PromptContext,
};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::BTreeMap;

#[async_trait]
impl EvolutionArchive for SqliteLedgerStore {
    async fn record_candidate(
        &self,
        island_id: u32,
        parent_id: Option<&str>,
        base_commit_sha: &str,
        artifact_type: ArtifactType,
        diff_payload: &str,
        materialized_payload: &ArtifactPayload,
        mark: &Mark,
    ) -> Result<(), ContractError> {
        // Hard invariant: Holdout marks may NEVER carry diagnostic feedback.
        if mark.is_holdout && mark.public_feedback.is_some() {
            return Err(ContractError::HoldoutLeakPrevented(format!(
                "holdout mark for candidate {} attempted to attach public_feedback",
                mark.candidate_id
            )));
        }

        let (blob_bytes, media_type, blob_sha256) = encode_payload(materialized_payload)?;
        let parent_id = parent_id.map(str::to_string);
        let base_commit_sha = base_commit_sha.to_string();
        let diff_payload = diff_payload.to_string();
        let mark = mark.clone();

        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;
            let now = now_unix_ms();

            // 1. Deduplicate content in artifact_blobs by SHA-256.
            tx.execute(
                "INSERT OR IGNORE INTO artifact_blobs (sha256, media_type, content, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![&blob_sha256, media_type, &blob_bytes, now],
            )
            .map_err(map_sqlite_err)?;

            // 2. Insert lineage node (OR IGNORE allows recording follow-up holdout marks for an existing candidate).
            tx.execute(
                "INSERT OR IGNORE INTO candidates
                 (id, island_id, parent_id, base_commit_sha, artifact_type, diff_content, blob_sha256, status, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8)",
                params![
                    &mark.candidate_id,
                    island_id as i64,
                    parent_id.as_deref(),
                    &base_commit_sha,
                    artifact_type.as_str(),
                    &diff_payload,
                    &blob_sha256,
                    now
                ],
            )
            .map_err(map_sqlite_err)?;

            // 3. Record deterministic Verifier Mark.
            let metrics_json = serde_json::to_string(&mark.metrics)
                .map_err(|e| ContractError::Internal(format!("serialize metrics: {e}")))?;

            tx.execute(
                "INSERT INTO marks
                 (candidate_id, verifier_id, is_holdout, passed, primary_score, cell_key, metrics_json, public_feedback, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    &mark.candidate_id,
                    &mark.verifier_id,
                    if mark.is_holdout { 1i64 } else { 0i64 },
                    if mark.passed { 1i64 } else { 0i64 },
                    mark.primary_score,
                    &mark.cell_key,
                    &metrics_json,
                    mark.public_feedback.as_deref(),
                    now
                ],
            )
            .map_err(map_sqlite_err)?;

            // 4. Update MAP-Elites grid ONLY on non-holdout passing marks (zero Holdout side-channel).
            if !mark.is_holdout && mark.passed {
                let existing_score: Option<f64> = tx
                    .query_row(
                        "SELECT primary_score FROM map_elites_grid
                         WHERE island_id = ?1 AND cell_key = ?2",
                        params![island_id as i64, &mark.cell_key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;

                let should_replace = match existing_score {
                    None => true,
                    Some(prev) => mark.primary_score > prev,
                };

                if should_replace {
                    tx.execute(
                        "INSERT INTO map_elites_grid (island_id, cell_key, candidate_id, primary_score, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(island_id, cell_key) DO UPDATE SET
                           candidate_id = excluded.candidate_id,
                           primary_score = excluded.primary_score,
                           updated_at_ms = excluded.updated_at_ms",
                        params![
                            island_id as i64,
                            &mark.cell_key,
                            &mark.candidate_id,
                            mark.primary_score,
                            now
                        ],
                    )
                    .map_err(map_sqlite_err)?;
                }
            }

            // 5. Append audit event to immutable Movement Ledger.
            let audit_payload = serde_json::json!({
                "candidate_id": mark.candidate_id,
                "island_id": island_id,
                "verifier_id": mark.verifier_id,
                "is_holdout": mark.is_holdout,
                "passed": mark.passed,
                "primary_score": mark.primary_score,
                "cell_key": mark.cell_key,
                "blob_sha256": blob_sha256,
            })
            .to_string();

            tx.execute(
                "INSERT INTO ledger_events
                 (run_id, node_id, event_type, cost_micro_usd, payload_json, created_at_ms)
                 VALUES (?1, 'archive', 'verify', 0, ?2, ?3)",
                params![&mark.candidate_id, &audit_payload, now],
            )
            .map_err(map_sqlite_err)?;

            tx.commit().map_err(map_sqlite_err)?;
            Ok(())
        })
        .await
    }

    async fn sample_prompt_context(
        &self,
        island_id: u32,
        inspiration_k: usize,
    ) -> Result<PromptContext, ContractError> {
        self.with_conn(move |conn| {
            // Fetch all elites in the requested island sorted descending by primary_score.
            let mut stmt = conn
                .prepare(
                    "SELECT candidate_id, cell_key, primary_score
                     FROM map_elites_grid
                     WHERE island_id = ?1
                     ORDER BY primary_score DESC",
                )
                .map_err(map_sqlite_err)?;

            let island_elites: Vec<(String, String, f64)> = stmt
                .query_map(params![island_id as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(map_sqlite_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_err)?;

            if island_elites.is_empty() {
                return Err(ContractError::NotFound(format!(
                    "no elites found in map_elites_grid for island {island_id}"
                )));
            }

            let mut rng = StdRng::from_entropy();
            // Exploitation-leaning parent selection: 70% top elite, 30% uniform random elite in island.
            let parent_idx = if island_elites.len() == 1 || rng.gen_bool(0.7) {
                0
            } else {
                rng.gen_range(0..island_elites.len())
            };
            let (parent_id, parent_cell, _) = &island_elites[parent_idx];
            let parent = hydrate_snapshot(conn, parent_id, parent_cell)?;

            // Collect diverse inspiration candidates from other cells in this island and sibling islands.
            let mut insp_stmt = conn
                .prepare(
                    "SELECT DISTINCT candidate_id, cell_key
                     FROM map_elites_grid
                     WHERE candidate_id != ?1",
                )
                .map_err(map_sqlite_err)?;

            let mut pool: Vec<(String, String)> = insp_stmt
                .query_map(params![parent_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(map_sqlite_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite_err)?;

            pool.shuffle(&mut rng);
            let mut inspirations = Vec::with_capacity(inspiration_k.min(pool.len()));
            for (cand_id, cell_key) in pool.into_iter().take(inspiration_k) {
                inspirations.push(hydrate_snapshot(conn, &cand_id, &cell_key)?);
            }

            Ok(PromptContext {
                parent,
                inspirations,
            })
        })
        .await
    }

    async fn migrate_elites(&self) -> Result<usize, ContractError> {
        self.with_conn(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite_err)?;

            let islands: Vec<i64> = {
                let mut stmt = tx
                    .prepare("SELECT DISTINCT island_id FROM map_elites_grid ORDER BY island_id ASC")
                    .map_err(map_sqlite_err)?;
                let rows = stmt
                    .query_map([], |row| row.get(0))
                    .map_err(map_sqlite_err)?
                    .collect::<Result<Vec<i64>, _>>()
                    .map_err(map_sqlite_err)?;
                rows
            };

            if islands.len() < 2 {
                tx.commit().map_err(map_sqlite_err)?;
                return Ok(0);
            }

            // Snapshot top elite per island first so ring migration is simultaneous.
            let mut migrants: Vec<(i64, String, String, f64)> = Vec::new();
            for (idx, &src_island) in islands.iter().enumerate() {
                let dst_island = islands[(idx + 1) % islands.len()];
                let top: Option<(String, String, f64)> = tx
                    .query_row(
                        "SELECT cell_key, candidate_id, primary_score
                         FROM map_elites_grid
                         WHERE island_id = ?1
                         ORDER BY primary_score DESC
                         LIMIT 1",
                        params![src_island],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;

                if let Some((cell_key, cand_id, score)) = top {
                    migrants.push((dst_island, cell_key, cand_id, score));
                }
            }

            let now = now_unix_ms();
            let mut updated_count = 0usize;

            for (dst_island, cell_key, cand_id, score) in migrants {
                let dst_score: Option<f64> = tx
                    .query_row(
                        "SELECT primary_score FROM map_elites_grid
                         WHERE island_id = ?1 AND cell_key = ?2",
                        params![dst_island, &cell_key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;

                if dst_score.map(|prev| score > prev).unwrap_or(true) {
                    tx.execute(
                        "INSERT INTO map_elites_grid (island_id, cell_key, candidate_id, primary_score, updated_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(island_id, cell_key) DO UPDATE SET
                           candidate_id = excluded.candidate_id,
                           primary_score = excluded.primary_score,
                           updated_at_ms = excluded.updated_at_ms",
                        params![dst_island, &cell_key, &cand_id, score, now],
                    )
                    .map_err(map_sqlite_err)?;
                    updated_count += 1;
                }
            }

            tx.commit().map_err(map_sqlite_err)?;
            Ok(updated_count)
        })
        .await
    }
}

/// Hydrates a `CandidateSnapshot` strictly from non-holdout marks (`is_holdout = 0`).
fn hydrate_snapshot(
    conn: &Connection,
    candidate_id: &str,
    cell_key: &str,
) -> Result<CandidateSnapshot, ContractError> {
    let (island_id, base_commit_sha, artifact_type_str, media_type, blob_bytes): (
        i64,
        String,
        String,
        String,
        Vec<u8>,
    ) = conn
        .query_row(
            "SELECT c.island_id, c.base_commit_sha, c.artifact_type, b.media_type, b.content
             FROM candidates c
             JOIN artifact_blobs b ON c.blob_sha256 = b.sha256
             WHERE c.id = ?1",
            params![candidate_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .map_err(map_sqlite_err)?;

    let (primary_score, metrics_json, last_public_feedback): (f64, String, Option<String>) = conn
        .query_row(
            "SELECT primary_score, metrics_json, public_feedback
             FROM marks
             WHERE candidate_id = ?1 AND is_holdout = 0
             ORDER BY id DESC
             LIMIT 1",
            params![candidate_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(map_sqlite_err)?;

    let artifact_type = ArtifactType::from_str(&artifact_type_str).ok_or_else(|| {
        ContractError::Internal(format!("unknown artifact_type: {artifact_type_str}"))
    })?;
    let payload = decode_payload(&media_type, &blob_bytes)?;
    let metrics: BTreeMap<String, f64> = serde_json::from_str(&metrics_json)
        .map_err(|e| ContractError::Internal(format!("deserialize metrics: {e}")))?;

    Ok(CandidateSnapshot {
        id: candidate_id.to_string(),
        island_id: island_id as u32,
        base_commit_sha,
        artifact_type,
        payload,
        primary_score,
        metrics,
        cell_key: cell_key.to_string(),
        last_public_feedback,
    })
}
