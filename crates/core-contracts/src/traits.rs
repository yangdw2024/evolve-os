use crate::error::ContractError;
use crate::types::{
    ArtifactPayload, ArtifactType, BudgetReservation, LlmRequest, LlmResponse, Mark, MicroUsd,
    PolicyGrant, PromptContext, VerifierSpec,
};
use async_trait::async_trait;

/// Abstract execution node hiding local sandbox vs. remote SSH/gRPC GPU nodes.
///
/// Implementations MUST enforce two-stage isolation:
/// 1. Apply patch and build/run candidate inside an untrusted restricted sandbox.
/// 2. Execute `verifier` in an isolated environment where candidate code cannot read
///    or tamper with verifier fixtures (especially when `verifier.is_holdout == true`).
#[async_trait]
pub trait ExecutionNode: Send + Sync {
    fn node_id(&self) -> &str;

    async fn eval_candidate(
        &self,
        candidate_id: &str,
        base_commit_sha: &str,
        patch_payload: &str,
        verifier: &VerifierSpec,
        grant: &PolicyGrant,
    ) -> Result<Mark, ContractError>;
}

/// MAP-Elites + island population archive backed by content-addressed blobs.
#[async_trait]
pub trait EvolutionArchive: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn record_candidate(
        &self,
        island_id: u32,
        parent_id: Option<&str>,
        base_commit_sha: &str,
        artifact_type: ArtifactType,
        diff_payload: &str,
        materialized_payload: &ArtifactPayload,
        mark: &Mark,
    ) -> Result<(), ContractError>;

    /// Samples one parent from `island_id` and up to `inspiration_k` diverse elites
    /// from distinct cells or sibling islands.
    async fn sample_prompt_context(
        &self,
        island_id: u32,
        inspiration_k: usize,
    ) -> Result<PromptContext, ContractError>;

    /// Migrates top elites across islands, returning the number of grid cells updated.
    async fn migrate_elites(&self) -> Result<usize, ContractError>;
}

/// Two-phase atomic budget ledger preventing concurrent multi-island overspend.
#[async_trait]
pub trait BudgetLedger: Send + Sync {
    /// Atomically checks `(spent + active_reservations + estimated) <= run_limit`
    /// and records a pending reservation.
    async fn reserve(
        &self,
        run_id: &str,
        node_id: &str,
        estimated_micro_usd: MicroUsd,
        run_limit_micro_usd: MicroUsd,
    ) -> Result<BudgetReservation, ContractError>;

    /// Finalizes a reservation with actual spent micro-USD and appends an immutable ledger event.
    #[allow(clippy::too_many_arguments)]
    async fn settle(
        &self,
        reservation: &BudgetReservation,
        node_id: &str,
        model_tier: Option<&str>,
        prompt_tokens: u64,
        completion_tokens: u64,
        actual_micro_usd: MicroUsd,
        payload_json: &str,
    ) -> Result<(), ContractError>;

    /// Releases an unused reservation (e.g., on transient transport failure).
    async fn release(&self, reservation: &BudgetReservation) -> Result<(), ContractError>;

    /// Returns total settled micro-USD spent for `run_id`.
    async fn total_spent(&self, run_id: &str) -> Result<MicroUsd, ContractError>;
}

/// Mode-A lightweight in-process LLM client trait.
/// Can later be backed by an HTTP proxy if Mode-B sandboxed CLI agents are introduced.
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ContractError>;
}
