use crate::types::MicroUsd;
use thiserror::Error;

/// Typed domain errors for the frozen contract layer.
///
/// Outer evolution loops dispatch recovery strategies strictly by variant:
/// - `PatchInvalid` -> cheap discard or single-shot format repair
/// - `PolicyViolation` -> record refusal / penalize or blacklist candidate
/// - `BudgetExceeded` -> halt or park the evolution run immediately
/// - `NodeTransient` -> retry on another execution node without penalizing the candidate
/// - `HoldoutLeakPrevented` -> invariant violation; abort write immediately
#[derive(Debug, Error)]
pub enum ContractError {
    #[error("patch apply or format failed: {0}")]
    PatchInvalid(String),

    #[error("policy violation or sandbox timeout: {0}")]
    PolicyViolation(String),

    #[error("budget hard limit reached: requested/projected {projected}, limit {limit}")]
    BudgetExceeded { projected: MicroUsd, limit: MicroUsd },

    #[error("transient node or transport failure (retryable, do not penalize candidate): {0}")]
    NodeTransient(String),

    #[error("holdout isolation invariant violated: {0}")]
    HoldoutLeakPrevented(String),

    #[error("resource not found: {0}")]
    NotFound(String),

    #[error("storage or internal error: {0}")]
    Internal(String),
}
