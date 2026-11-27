use crate::prompt::{build_repair_prompt, build_user_prompt, payload_files, system_prompt};
use core_contracts::{
    ArtifactPayload, ArtifactType, ContractError, EvolutionArchive, ExecutionNode, LlmClient,
    LlmRequest, LlmResponse, LlmTier, Mark, PolicyGrant, VerifierSpec,
};
use ledger_sqlite::SqliteLedgerStore;
use node_runtime::{apply_patch, files_sha256, LocalSandboxNode};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub run_id: String,
    pub islands: u32,
    pub iterations: u32,
    pub inspiration_k: usize,
    /// Consecutive non-improving rounds before escalating to `LlmTier::Strong`.
    pub stagnation_limit: u32,
    /// Every N-th round is a cross-island crossover round (Strong tier). 0 disables.
    pub cross_every: u32,
    /// Island 0 triggers `migrate_elites` every N of its rounds. 0 disables.
    pub migrate_every: u32,
    /// Max allowed holdout regression vs. the current champion before rollback.
    pub holdout_regression_threshold: f64,
    pub max_transient_retries: u32,
    pub max_output_tokens: u32,
    pub grant: PolicyGrant,
    pub train_verifier: VerifierSpec,
    pub holdout_verifier: VerifierSpec,
    /// Bench task + scoring description appended to the system prompt.
    pub task_spec: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunStats {
    pub iterations_started: u64,
    pub llm_requests: u64,
    pub llm_fast_requests: u64,
    pub llm_strong_requests: u64,
    pub llm_transient_retries: u64,
    pub patch_invalid: u64,
    pub patch_repaired: u64,
    pub patch_discarded: u64,
    pub policy_refusals: u64,
    pub eval_transient_retries: u64,
    pub candidates_recorded: u64,
    pub holdout_checks: u64,
    pub champions_accepted: u64,
    pub blacklisted: u64,
    pub migrations: u64,
    pub migrated_cells: u64,
    pub recoverable_errors: u64,
    pub duplicates_skipped: u64,
    pub budget_stop: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Champion {
    pub candidate_id: String,
    pub island_id: u32,
    pub train_score: f64,
    pub holdout_score: f64,
    pub holdout_passed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub stats: RunStats,
    pub champion: Option<Champion>,
    pub spent_micro_usd: i64,
}

pub struct EvolutionEngine {
    cfg: EngineConfig,
    store: SqliteLedgerStore,
    node: Arc<LocalSandboxNode>,
    llm: Arc<dyn LlmClient>,
    stop: AtomicBool,
    champion: tokio::sync::Mutex<Option<Champion>>,
    stats: Mutex<RunStats>,
    system_prompt: String,
    /// Content hashes of every artifact already submitted for evaluation (dedup).
    seen_artifacts: Mutex<HashSet<String>>,
}

/// Outcome of a call that may be retried on `NodeTransient`.
enum Retried<T> {
    Done(T),
    GaveUp,
}

const LOOP_NODE: &str = "evolve-loop";

impl EvolutionEngine {
    pub fn new(
        cfg: EngineConfig,
        store: SqliteLedgerStore,
        node: Arc<LocalSandboxNode>,
        llm: Arc<dyn LlmClient>,
    ) -> Arc<Self> {
        Arc::new(Self {
            system_prompt: system_prompt(cfg.task_spec.as_deref()),
            cfg,
            store,
            node,
            llm,
            stop: AtomicBool::new(false),
            champion: tokio::sync::Mutex::new(None),
            stats: Mutex::new(RunStats::default()),
            seen_artifacts: Mutex::new(HashSet::new()),
        })
    }

    fn stat(&self, f: impl FnOnce(&mut RunStats)) {
        if let Ok(mut s) = self.stats.lock() {
            f(&mut s);
        }
    }

    pub fn stats(&self) -> RunStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    async fn event(&self, event_type: &str, payload: serde_json::Value) -> Result<(), ContractError> {
        self.store
            .append_event(&self.cfg.run_id, LOOP_NODE, event_type, &payload.to_string())
            .await
    }

    /// Evaluates the seed on every island (train) and establishes the holdout baseline champion.
    pub async fn seed(&self, files: BTreeMap<String, String>) -> Result<String, ContractError> {
        let sha = self.node.register_snapshot(&files)?;
        self.mark_seen(&files_sha256(&files));
        let payload = ArtifactPayload::FileMap(files);
        for island in 0..self.cfg.islands {
            let cid = format!("{}-seed-i{island}", self.cfg.run_id);
            let mark = match self.eval_retry(&cid, &sha, "", &self.cfg.train_verifier).await? {
                Retried::Done(m) => m,
                Retried::GaveUp => return Err(ContractError::NodeTransient("seed evaluation kept failing".into())),
            };
            if !mark.passed {
                return Err(ContractError::Internal(format!(
                    "seed failed train verifier: {:?}",
                    mark.public_feedback
                )));
            }
            self.store
                .record_candidate(island, None, &sha, ArtifactType::CodeDiff, "", &payload, &mark)
                .await?;
            self.stat(|s| s.candidates_recorded += 1);

            if island == 0 {
                let hold = match self.eval_retry(&cid, &sha, "", &self.cfg.holdout_verifier).await? {
                    Retried::Done(m) => m,
                    Retried::GaveUp => return Err(ContractError::NodeTransient("seed holdout kept failing".into())),
                };
                Self::assert_no_leak(&hold)?;
                self.store
                    .record_candidate(island, None, &sha, ArtifactType::CodeDiff, "", &payload, &hold)
                    .await?;
                *self.champion.lock().await = Some(Champion {
                    candidate_id: cid.clone(),
                    island_id: island,
                    train_score: mark.primary_score,
                    holdout_score: hold.primary_score,
                    holdout_passed: hold.passed,
                });
            }
        }
        self.event("seed", serde_json::json!({"snapshot": sha, "islands": self.cfg.islands})).await?;
        Ok(sha)
    }

    pub async fn run(self: &Arc<Self>) -> Result<RunReport, ContractError> {
        let mut handles = Vec::new();
        for island in 0..self.cfg.islands {
            let engine = Arc::clone(self);
            handles.push(tokio::spawn(async move { engine.island_loop(island).await }));
        }
        let mut first_err = None;
        for h in handles {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(join) => {
                    first_err.get_or_insert(ContractError::Internal(format!("island task panicked: {join}")));
                }
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(RunReport {
            run_id: self.cfg.run_id.clone(),
            stats: self.stats(),
            champion: self.champion.lock().await.clone(),
            spent_micro_usd: core_contracts::BudgetLedger::total_spent(&self.store, &self.cfg.run_id).await?,
        })
    }

    async fn island_loop(self: Arc<Self>, island: u32) -> Result<(), ContractError> {
        let mut stagnation = 0u32;
        let mut island_best = f64::NEG_INFINITY;
        for iter in 0..self.cfg.iterations {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            self.stat(|s| s.iterations_started += 1);
            match self.iteration(island, iter, &mut stagnation, &mut island_best).await {
                Ok(()) => {}
                Err(ContractError::BudgetExceeded { projected, limit }) => {
                    // Graceful halt of ALL islands: no new reservations will be attempted.
                    let first = !self.stop.swap(true, Ordering::SeqCst);
                    self.stat(|s| s.budget_stop = true);
                    if first {
                        self.event("budget_stop", serde_json::json!({
                            "island": island, "iteration": iter, "projected": projected, "limit": limit
                        }))
                        .await?;
                    }
                    break;
                }
                Err(e @ ContractError::HoldoutLeakPrevented(_)) => {
                    self.stop.store(true, Ordering::SeqCst);
                    return Err(e);
                }
                Err(e) => {
                    self.stat(|s| s.recoverable_errors += 1);
                    self.event("error", serde_json::json!({
                        "island": island, "iteration": iter, "error": e.to_string()
                    }))
                    .await?;
                }
            }
            if island == 0 && self.cfg.migrate_every > 0 && (iter + 1).is_multiple_of(self.cfg.migrate_every) {
                let moved = self.store.migrate_elites().await?;
                self.stat(|s| {
                    s.migrations += 1;
                    s.migrated_cells += moved as u64;
                });
                self.event("migrate", serde_json::json!({"iteration": iter, "cells_updated": moved})).await?;
            }
        }
        Ok(())
    }

    async fn call_llm(&self, tier: LlmTier, user_prompt: &str) -> Result<Retried<LlmResponse>, ContractError> {
        for attempt in 0..=self.cfg.max_transient_retries {
            let req = LlmRequest {
                run_id: self.cfg.run_id.clone(),
                tier,
                system_prompt: self.system_prompt.clone(),
                user_prompt: user_prompt.to_string(),
                max_output_tokens: self.cfg.max_output_tokens,
                estimated_cost_micro_usd: 0,
            };
            self.stat(|s| {
                s.llm_requests += 1;
                match tier {
                    LlmTier::Fast => s.llm_fast_requests += 1,
                    LlmTier::Strong => s.llm_strong_requests += 1,
                }
            });
            match self.llm.complete(req).await {
                Ok(resp) => return Ok(Retried::Done(resp)),
                Err(ContractError::NodeTransient(_)) => {
                    // The router has already released the reservation; retry without penalty.
                    self.stat(|s| s.llm_transient_retries += 1);
                    tokio::time::sleep(Duration::from_millis(25 * (attempt as u64 + 1))).await;
                }
                Err(e) => return Err(e),
            }
        }
        self.event("llm_transient_exhausted", serde_json::json!({"tier": tier.as_str()})).await?;
        Ok(Retried::GaveUp)
    }

    async fn eval_retry(
        &self,
        candidate_id: &str,
        base_sha: &str,
        patch: &str,
        verifier: &VerifierSpec,
    ) -> Result<Retried<Mark>, ContractError> {
        for attempt in 0..=self.cfg.max_transient_retries {
            match self
                .node
                .eval_candidate(candidate_id, base_sha, patch, verifier, &self.cfg.grant)
                .await
            {
                Ok(m) => return Ok(Retried::Done(m)),
                Err(ContractError::NodeTransient(_)) => {
                    self.stat(|s| s.eval_transient_retries += 1);
                    tokio::time::sleep(Duration::from_millis(25 * (attempt as u64 + 1))).await;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Retried::GaveUp)
    }

    /// Returns true if `sha` was not seen before (and records it).
    fn mark_seen(&self, sha: &str) -> bool {
        self.seen_artifacts
            .lock()
            .map(|mut s| s.insert(sha.to_string()))
            .unwrap_or(true)
    }

    fn assert_no_leak(mark: &Mark) -> Result<(), ContractError> {
        if mark.is_holdout && mark.public_feedback.is_some() {
            return Err(ContractError::HoldoutLeakPrevented(format!(
                "node returned holdout feedback for {}",
                mark.candidate_id
            )));
        }
        Ok(())
    }

    async fn refuse(&self, island: u32, candidate_id: &str, reason: &str) -> Result<(), ContractError> {
        self.stat(|s| s.policy_refusals += 1);
        self.event("refusal", serde_json::json!({
            "island": island, "candidate_id": candidate_id, "reason": reason
        }))
        .await
    }

    async fn iteration(
        &self,
        island: u32,
        iter: u32,
        stagnation: &mut u32,
        island_best: &mut f64,
    ) -> Result<(), ContractError> {
        let ctx = self.store.sample_prompt_context(island, self.cfg.inspiration_k).await?;
        *island_best = island_best.max(ctx.parent.primary_score);

        let crossover = self.cfg.cross_every > 0
            && (iter + 1).is_multiple_of(self.cfg.cross_every)
            && ctx.inspirations.iter().any(|c| c.island_id != island);
        let tier = if *stagnation >= self.cfg.stagnation_limit || crossover {
            LlmTier::Strong
        } else {
            LlmTier::Fast
        };

        let parent_files = payload_files(&ctx.parent.payload);
        let parent_sha = self.node.register_snapshot(&parent_files)?;
        let candidate_id = format!(
            "{}-i{island}-g{iter}-{}",
            self.cfg.run_id,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let user_prompt = build_user_prompt(&ctx, crossover);

        let Retried::Done(resp) = self.call_llm(tier, &user_prompt).await? else {
            return Ok(());
        };
        let mut content = resp.content;
        let mut repaired = false;
        let files = loop {
            match apply_patch(&parent_files, &content) {
                Ok((files, _)) => break files,
                Err(ContractError::PatchInvalid(msg)) => {
                    self.stat(|s| s.patch_invalid += 1);
                    if repaired {
                        self.stat(|s| s.patch_discarded += 1);
                        *stagnation += 1;
                        self.event("patch_repair", serde_json::json!({
                            "island": island, "candidate_id": candidate_id, "outcome": "failed", "error": msg
                        }))
                        .await?;
                        self.event("patch_invalid", serde_json::json!({
                            "island": island, "candidate_id": candidate_id, "error": msg, "action": "discard"
                        }))
                        .await?;
                        return Ok(());
                    }
                    // Single-shot cheap format repair on the Fast tier.
                    repaired = true;
                    let repair = build_repair_prompt(&user_prompt, &content, &msg);
                    let Retried::Done(r) = self.call_llm(LlmTier::Fast, &repair).await? else {
                        return Ok(());
                    };
                    content = r.content;
                }
                Err(ContractError::PolicyViolation(msg)) => {
                    *stagnation += 1;
                    return self.refuse(island, &candidate_id, &msg).await;
                }
                Err(e) => return Err(e),
            }
        };
        if repaired {
            self.stat(|s| s.patch_repaired += 1);
            self.event("patch_repair", serde_json::json!({
                "island": island, "candidate_id": candidate_id, "outcome": "repaired"
            }))
            .await?;
        }
        // Skip re-evaluating content that any island already submitted (no verifier cost, no duplicate rows).
        let artifact_sha = files_sha256(&files);
        if !self.mark_seen(&artifact_sha) {
            self.stat(|s| s.duplicates_skipped += 1);
            *stagnation += 1;
            return self
                .event("duplicate", serde_json::json!({
                    "island": island, "candidate_id": candidate_id, "artifact_sha256": artifact_sha
                }))
                .await;
        }

        let mark = match self.eval_retry(&candidate_id, &parent_sha, &content, &self.cfg.train_verifier).await {
            Ok(Retried::Done(m)) => m,
            Ok(Retried::GaveUp) => return Ok(()),
            Err(ContractError::PolicyViolation(msg)) => {
                *stagnation += 1;
                return self.refuse(island, &candidate_id, &msg).await;
            }
            Err(ContractError::PatchInvalid(msg)) => {
                self.stat(|s| {
                    s.patch_invalid += 1;
                    s.patch_discarded += 1;
                });
                *stagnation += 1;
                return self
                    .event("patch_invalid", serde_json::json!({
                        "island": island, "candidate_id": candidate_id, "error": msg, "action": "discard"
                    }))
                    .await;
            }
            Err(e) => return Err(e),
        };

        let payload = ArtifactPayload::FileMap(files);
        self.store
            .record_candidate(island, Some(&ctx.parent.id), &parent_sha, ArtifactType::CodeDiff, &content, &payload, &mark)
            .await?;
        self.stat(|s| s.candidates_recorded += 1);

        if mark.passed && mark.primary_score > *island_best {
            *island_best = mark.primary_score;
            *stagnation = 0;
        } else {
            *stagnation += 1;
        }
        if tier == LlmTier::Strong && !crossover {
            // One Strong escalation per stagnation streak, then fall back to Fast exploration.
            *stagnation = 0;
        }

        if mark.passed {
            self.holdout_gate(island, &ctx.parent.id, &parent_sha, &content, &payload, &mark).await?;
        }
        Ok(())
    }

    /// Holdout final review for a new global-best train score; rollback + blacklist on failure.
    async fn holdout_gate(
        &self,
        island: u32,
        parent_id: &str,
        parent_sha: &str,
        patch: &str,
        payload: &ArtifactPayload,
        train: &Mark,
    ) -> Result<(), ContractError> {
        let mut champ = self.champion.lock().await;
        let best_train = champ.as_ref().map(|c| c.train_score).unwrap_or(f64::NEG_INFINITY);
        if train.primary_score <= best_train {
            return Ok(());
        }
        self.stat(|s| s.holdout_checks += 1);
        let cid = &train.candidate_id;

        let hold = match self.eval_retry(cid, parent_sha, patch, &self.cfg.holdout_verifier).await {
            Ok(Retried::Done(m)) => m,
            Ok(Retried::GaveUp) => return Ok(()),
            Err(ContractError::PolicyViolation(_)) => Mark {
                candidate_id: cid.clone(),
                verifier_id: self.cfg.holdout_verifier.verifier_id.clone(),
                passed: false,
                primary_score: 0.0,
                metrics: BTreeMap::new(),
                cell_key: "holdout_policy_violation".into(),
                is_holdout: true,
                artifact_sha256: train.artifact_sha256.clone(),
                public_feedback: None,
            },
            Err(e) => return Err(e),
        };
        Self::assert_no_leak(&hold)?;
        self.store
            .record_candidate(island, Some(parent_id), parent_sha, ArtifactType::CodeDiff, patch, payload, &hold)
            .await?;

        let baseline = champ.as_ref().map(|c| c.holdout_score).unwrap_or(f64::NEG_INFINITY);
        let accepted = hold.passed && hold.primary_score >= baseline - self.cfg.holdout_regression_threshold;
        if accepted {
            self.stat(|s| s.champions_accepted += 1);
            *champ = Some(Champion {
                candidate_id: cid.clone(),
                island_id: island,
                train_score: train.primary_score,
                holdout_score: hold.primary_score,
                holdout_passed: true,
            });
            // Only the decision bit is logged; holdout diagnostics never leave the node.
            self.event("champion", serde_json::json!({
                "candidate_id": cid, "island": island, "train_score": train.primary_score
            }))
            .await?;
        } else {
            self.stat(|s| s.blacklisted += 1);
            self.store
                .blacklist_candidate(&self.cfg.run_id, cid, "holdout gate failed or regressed beyond threshold")
                .await?;
        }
        Ok(())
    }
}
