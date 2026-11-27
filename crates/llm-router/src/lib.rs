//! Mode-A in-process LLM routing with a hard two-phase budget guard.
//!
//! Every call goes strictly through
//! `BudgetLedger::reserve -> LlmClient::complete -> settle` (or `release` on failure),
//! so concurrent islands can never overspend via check-then-call races.

pub mod config;
pub mod dedup;
pub mod openai;
pub mod router;

pub use config::{
    ApiProtocol, DedupConfig, InputType, ModelCatalogEntry, Pricing, ProviderConfig, ReasoningVia,
    RetryPolicy, RouterConfig, TierModelConfig,
};
pub use dedup::{DedupStats, PromptDeduper};
pub use openai::OpenAiCompatClient;
pub use router::{BudgetedLlmRouter, TierBackend};

#[cfg(test)]
mod tests;
