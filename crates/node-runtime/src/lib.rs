//! Two-stage isolated local execution node.
//!
//! Stage 1 (untrusted): materialize parent snapshot + SEARCH/REPLACE patch in a fresh
//! temp dir, run the optional build command under a hard wall-clock limit.
//! Stage 2 (verifier): copy the built artifact into a separate read-only directory and
//! run the verifier process from its own empty working directory. Holdout verifiers
//! have stderr discarded and `public_feedback` forced to `None`.

pub mod patch;
mod process;
mod sandbox;

pub use patch::{apply_patch, files_sha256, parse_patch, validate_rel_path, PatchBlock};
pub use sandbox::{LocalSandboxConfig, LocalSandboxNode};
