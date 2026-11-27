//! Asynchronous multi-island evolution outer loop.

pub mod engine;
pub mod mock_llm;
pub mod money;
pub mod prompt;
pub mod setup;

pub use engine::{Champion, EngineConfig, EvolutionEngine, RunReport, RunStats};
pub use mock_llm::MockToyMutator;
