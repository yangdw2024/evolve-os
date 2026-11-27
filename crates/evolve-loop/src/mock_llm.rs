//! Deterministic offline mutator for `--mock-llm` runs on the toy bench.
//!
//! It deliberately emits a mix of good, neutral, harmful and *adversarial* outputs so
//! every error path of the loop is exercised: improvements, malformed text
//! (`PatchInvalid` -> repair), stale SEARCH text, an infinite loop (`PolicyViolation`),
//! an overfitted lookup table (train 1.0 / holdout fail -> blacklist), and periodic
//! simulated 503s (`NodeTransient` -> release + retry).

use crate::prompt::{PARENT_FILE_HEADER, REPAIR_TAG};
use async_trait::async_trait;
use core_contracts::{ContractError, LlmClient, LlmRequest, LlmResponse, LlmTier};
use llm_router::Pricing;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct MockToyMutator {
    counter: AtomicU64,
    fast_pricing: Pricing,
    strong_pricing: Pricing,
    /// Every N-th call fails with `NodeTransient` (0 disables).
    transient_every: u64,
}

impl MockToyMutator {
    pub fn new(fast_pricing: Pricing, strong_pricing: Pricing, transient_every: u64) -> Self {
        Self {
            counter: AtomicU64::new(0),
            fast_pricing,
            strong_pricing,
            transient_every,
        }
    }

    pub fn calls(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }
}

fn extract_parent_solution(prompt: &str) -> String {
    let header = format!("{PARENT_FILE_HEADER}solution.py\n");
    let Some(start) = prompt.find(&header) else { return String::new() };
    let rest = &prompt[start + header.len()..];
    let Some(fence_end) = rest.find('\n') else { return String::new() };
    let body = &rest[fence_end + 1..];
    match body.find("\n```") {
        Some(end) => body[..end].to_string(),
        None => String::new(),
    }
}

fn block(search: &str, replace: &str) -> String {
    format!("<<<<<<< SEARCH solution.py\n{search}\n=======\n{replace}\n>>>>>>> REPLACE\n")
}

fn return_line(parent: &str) -> Option<String> {
    parent
        .lines()
        .find(|l| l.trim_start().starts_with("return "))
        .map(str::to_string)
}

fn smart_improve(parent: &str, n: u64) -> String {
    let Some(ret) = return_line(parent) else {
        return "I could not find the parent code.".into();
    };
    let indent: String = ret.chars().take_while(|c| c.is_whitespace()).collect();
    match ret.trim() {
        "return n % 3 == 0" => block(&ret, &format!("{indent}return n % 3 == 0 or n % 5 == 0")),
        "return n % 5 == 0" => block(&ret, &format!("{indent}return n % 3 == 0 or n % 5 == 0")),
        "return n % 3 == 0 or n % 5 == 0" => block(&ret, &format!("{indent}return (n % 3 == 0) != (n % 5 == 0)")),
        _ => annotate(&ret, &indent, n),
    }
}

fn annotate(ret: &str, indent: &str, n: u64) -> String {
    block(ret, &format!("{indent}# mutation {n}: annotated, behaviour unchanged\n{ret}"))
}

fn overfit_lookup(ret: &str, indent: &str) -> String {
    let positives: Vec<String> = (1..=200)
        .filter(|n| (n % 3 == 0) != (n % 5 == 0))
        .map(|n| n.to_string())
        .collect();
    block(ret, &format!("{indent}return n in {{{}}}", positives.join(", ")))
}

impl MockToyMutator {
    fn mutate(&self, n: u64, parent: &str, tier: LlmTier) -> String {
        let Some(ret) = return_line(parent) else {
            return "no parent code found".into();
        };
        let indent: String = ret.chars().take_while(|c| c.is_whitespace()).collect();
        if tier == LlmTier::Strong {
            return smart_improve(parent, n);
        }
        match n % 10 {
            0 | 9 => smart_improve(parent, n),
            1 => overfit_lookup(&ret, &indent),
            2 => "Idea: maybe also check divisibility by five? (no patch blocks here)".into(),
            3 => smart_improve(parent, n),
            4 => block(&ret, &format!("{indent}while True:\n{indent}    pass")),
            5 => block(&format!("{indent}return n % 7 == 0"), &format!("{indent}return False")),
            6 => annotate(&ret, &indent, n),
            7 => block(&ret, &format!("{indent}_pad_{n} = n\n{indent}_pad2_{n} = n\n{indent}_pad3_{n} = n\n{ret}")),
            _ => {
                if ret.trim() == "return n % 3 == 0" {
                    block(&ret, &format!("{indent}return n % 5 == 0"))
                } else {
                    annotate(&ret, &indent, n)
                }
            }
        }
    }
}

#[async_trait]
impl LlmClient for MockToyMutator {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, ContractError> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        if self.transient_every > 0 && (n + 1).is_multiple_of(self.transient_every) {
            return Err(ContractError::NodeTransient("mock llm: simulated HTTP 503".into()));
        }
        let parent = extract_parent_solution(&req.user_prompt);
        let content = if req.user_prompt.contains(REPAIR_TAG) {
            smart_improve(&parent, n)
        } else {
            self.mutate(n, &parent, req.tier)
        };
        let pricing = match req.tier {
            LlmTier::Fast => self.fast_pricing,
            LlmTier::Strong => self.strong_pricing,
        };
        let prompt_tokens = Pricing::estimate_tokens(req.system_prompt.len() + req.user_prompt.len());
        let completion_tokens = Pricing::estimate_tokens(content.len()).min(req.max_output_tokens as u64);
        Ok(LlmResponse {
            content,
            model_id: format!("mock-{}", req.tier.as_str()),
            prompt_tokens,
            completion_tokens,
            actual_cost_micro_usd: pricing.cost(prompt_tokens, completion_tokens),
        })
    }
}
