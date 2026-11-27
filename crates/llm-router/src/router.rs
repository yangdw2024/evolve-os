use crate::config::{Pricing, RouterConfig};
use crate::dedup::PromptDeduper;
use crate::openai::OpenAiCompatClient;
use async_trait::async_trait;
use core_contracts::{
    BudgetLedger, ContractError, LlmClient, LlmRequest, LlmResponse, LlmTier, MicroUsd,
};
use std::sync::Arc;

/// A concrete model backend plus the pricing used to size its reservations.
#[derive(Clone)]
pub struct TierBackend {
    pub client: Arc<dyn LlmClient>,
    pub pricing: Pricing,
}

/// Budget-guarded tiered router implementing `LlmClient`.
///
/// Protocol per call (no check-then-call race):
/// 1. `reserve(max(request estimate, worst-case pricing estimate))` -> `BudgetExceeded` aborts here.
/// 2. `complete()` on the tier backend.
/// 3. success -> `settle(actual)`; any failure (incl. `NodeTransient`) -> `release`.
pub struct BudgetedLlmRouter {
    ledger: Arc<dyn BudgetLedger>,
    fast: TierBackend,
    strong: TierBackend,
    run_limit_micro_usd: MicroUsd,
    node_id: String,
    deduper: PromptDeduper,
}

impl BudgetedLlmRouter {
    pub fn new(
        ledger: Arc<dyn BudgetLedger>,
        fast: TierBackend,
        strong: TierBackend,
        run_limit_micro_usd: MicroUsd,
        deduper: PromptDeduper,
    ) -> Self {
        Self {
            ledger,
            fast,
            strong,
            run_limit_micro_usd,
            node_id: "llm-router".to_string(),
            deduper,
        }
    }

    pub fn with_node_id(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = node_id.into();
        self
    }

    /// Builds OpenAI-compatible HTTP backends for both tiers from config.
    pub fn from_config(
        cfg: &RouterConfig,
        ledger: Arc<dyn BudgetLedger>,
        run_limit_micro_usd: MicroUsd,
    ) -> Result<Self, ContractError> {
        let fast = OpenAiCompatClient::new(cfg.fast.clone())?;
        let strong = OpenAiCompatClient::new(cfg.strong.clone())?;
        Ok(Self::new(
            ledger,
            TierBackend {
                pricing: fast.pricing(),
                client: Arc::new(fast),
            },
            TierBackend {
                pricing: strong.pricing(),
                client: Arc::new(strong),
            },
            run_limit_micro_usd,
            PromptDeduper::new(&cfg.dedup),
        ))
    }

    pub fn run_limit_micro_usd(&self) -> MicroUsd {
        self.run_limit_micro_usd
    }

    fn backend(&self, tier: LlmTier) -> &TierBackend {
        match tier {
            LlmTier::Fast => &self.fast,
            LlmTier::Strong => &self.strong,
        }
    }
}

#[async_trait]
impl LlmClient for BudgetedLlmRouter {
    async fn complete(&self, mut req: LlmRequest) -> Result<LlmResponse, ContractError> {
        let (user_prompt, stats) = self.deduper.dedup(&req.system_prompt, &req.user_prompt);
        req.user_prompt = user_prompt;

        let tier = req.tier;
        let backend = self.backend(tier).clone();
        let prompt_bytes = req.system_prompt.len() + req.user_prompt.len();
        let estimate = req
            .estimated_cost_micro_usd
            .max(backend.pricing.estimate(prompt_bytes, req.max_output_tokens))
            .max(1);
        req.estimated_cost_micro_usd = estimate;

        // Phase 1: atomic reservation (BudgetExceeded propagates; backend is never called).
        let reservation = self
            .ledger
            .reserve(&req.run_id, &self.node_id, estimate, self.run_limit_micro_usd)
            .await?;

        // Phase 2: model call, then settle or release.
        match backend.client.complete(req).await {
            Ok(resp) => {
                let payload = serde_json::json!({
                    "model_id": resp.model_id,
                    "tier": tier.as_str(),
                    "reserved_micro_usd": estimate,
                    "dedup_chunks_omitted": stats.chunks_deduped,
                    "dedup_bytes_saved": stats.bytes_saved,
                    "prompt_truncated": stats.truncated,
                })
                .to_string();
                self.ledger
                    .settle(
                        &reservation,
                        &self.node_id,
                        Some(tier.as_str()),
                        resp.prompt_tokens,
                        resp.completion_tokens,
                        resp.actual_cost_micro_usd.max(0),
                        &payload,
                    )
                    .await?;
                Ok(resp)
            }
            Err(err) => {
                self.ledger.release(&reservation).await.map_err(|rel| {
                    ContractError::Internal(format!(
                        "failed to release reservation after `{err}`: {rel}"
                    ))
                })?;
                Err(err)
            }
        }
    }
}
