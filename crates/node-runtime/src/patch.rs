//! Structured SEARCH/REPLACE patch protocol.
//!
//! ```text
//! <<<<<<< SEARCH relative/path.py
//! exact existing text (must occur exactly once)
//! =======
//! replacement text
//! >>>>>>> REPLACE
//! ```
//! An empty SEARCH section creates a new file (the file must not exist yet).
//! Text outside blocks (prose, code fences) is ignored.

use core_contracts::ContractError;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SEARCH_MARKER: &str = "<<<<<<< SEARCH";
pub const DIVIDER: &str = "=======";
pub const REPLACE_MARKER: &str = ">>>>>>> REPLACE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchBlock {
    pub path: String,
    pub search: String,
    pub replace: String,
}

fn invalid(msg: impl Into<String>) -> ContractError {
    ContractError::PatchInvalid(msg.into())
}

pub fn parse_patch(text: &str) -> Result<Vec<PatchBlock>, ContractError> {
    let text = text.replace("\r\n", "\n");
    let mut lines = text.lines().enumerate();
    let mut blocks = Vec::new();

    while let Some((ln, line)) = lines.next() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(SEARCH_MARKER) {
            let path = rest.trim().to_string();
            if path.is_empty() {
                return Err(invalid(format!(
                    "line {}: SEARCH marker must be followed by a relative file path",
                    ln + 1
                )));
            }
            let mut search = Vec::new();
            let mut divider_found = false;
            for (ln2, l) in lines.by_ref() {
                let t = l.trim_end();
                if t == DIVIDER {
                    divider_found = true;
                    break;
                }
                if t.trim_start().starts_with(SEARCH_MARKER) || t.trim_start().starts_with(REPLACE_MARKER) {
                    return Err(invalid(format!("line {}: unexpected marker inside SEARCH section", ln2 + 1)));
                }
                search.push(l);
            }
            if !divider_found {
                return Err(invalid(format!("block starting at line {}: missing `=======` divider", ln + 1)));
            }
            let mut replace = Vec::new();
            let mut end_found = false;
            for (ln2, l) in lines.by_ref() {
                let t = l.trim_end();
                if t.trim_start() == REPLACE_MARKER {
                    end_found = true;
                    break;
                }
                if t.trim_start().starts_with(SEARCH_MARKER) || t == DIVIDER {
                    return Err(invalid(format!("line {}: unexpected marker inside REPLACE section", ln2 + 1)));
                }
                replace.push(l);
            }
            if !end_found {
                return Err(invalid(format!("block starting at line {}: missing `>>>>>>> REPLACE` terminator", ln + 1)));
            }
            blocks.push(PatchBlock {
                path,
                search: search.join("\n"),
                replace: replace.join("\n"),
            });
        } else if trimmed.starts_with(REPLACE_MARKER) {
            return Err(invalid(format!("line {}: stray `>>>>>>> REPLACE` without SEARCH", ln + 1)));
        }
    }

    if blocks.is_empty() {
        return Err(invalid("no SEARCH/REPLACE blocks found in model output"));
    }
    Ok(blocks)
}

/// Normalizes and validates a candidate-supplied relative path.
/// Absolute paths, drive prefixes and `..` traversal are policy violations.
pub fn validate_rel_path(path: &str) -> Result<String, ContractError> {
    let norm = path.trim().replace('\\', "/");
    let violation = |why: &str| ContractError::PolicyViolation(format!("path `{path}` rejected: {why}"));
    if norm.is_empty() {
        return Err(violation("empty"));
    }
    if norm.starts_with('/') || norm.contains(':') {
        return Err(violation("absolute or drive-qualified path"));
    }
    let mut parts = Vec::new();
    for comp in norm.split('/') {
        match comp {
            "" | "." => continue,
            ".." => return Err(violation("parent traversal")),
            c => parts.push(c),
        }
    }
    if parts.is_empty() {
        return Err(violation("empty"));
    }
    Ok(parts.join("/"))
}

/// Applies a patch to an in-memory file map. Returns new files and the touched paths.
pub fn apply_patch(
    base: &BTreeMap<String, String>,
    patch: &str,
) -> Result<(BTreeMap<String, String>, Vec<String>), ContractError> {
    let blocks = parse_patch(patch)?;
    let mut files = base.clone();
    let mut touched = Vec::new();

    for (idx, block) in blocks.iter().enumerate() {
        let path = validate_rel_path(&block.path)?;
        if block.search.trim().is_empty() {
            if files.contains_key(&path) {
                return Err(invalid(format!(
                    "block {}: empty SEARCH may only create a new file, but `{path}` exists",
                    idx + 1
                )));
            }
            let mut content = block.replace.clone();
            if !content.ends_with('\n') {
                content.push('\n');
            }
            files.insert(path.clone(), content);
        } else {
            let current = files
                .get(&path)
                .ok_or_else(|| invalid(format!("block {}: target file `{path}` not found", idx + 1)))?
                .replace("\r\n", "\n");
            let count = current.matches(block.search.as_str()).count();
            if count == 0 {
                return Err(invalid(format!(
                    "block {}: SEARCH text not found in `{path}` (must match exactly)",
                    idx + 1
                )));
            }
            if count > 1 {
                return Err(invalid(format!(
                    "block {}: SEARCH text is ambiguous in `{path}` ({count} matches)",
                    idx + 1
                )));
            }
            let updated = current.replacen(block.search.as_str(), &block.replace, 1);
            files.insert(path.clone(), updated);
        }
        if !touched.contains(&path) {
            touched.push(path);
        }
    }
    Ok((files, touched))
}

/// Deterministic content hash of a file map (sorted paths).
pub fn files_sha256(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (path, content) in files {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        hasher.update(content.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("solution.py".into(), "def f(n):\n    return n % 3 == 0\n".into());
        m
    }

    #[test]
    fn applies_single_block_with_surrounding_prose() {
        let patch = "Sure! Here you go:\n```\n<<<<<<< SEARCH solution.py\n    return n % 3 == 0\n=======\n    return n % 5 == 0\n>>>>>>> REPLACE\n```\n";
        let (files, touched) = apply_patch(&base(), patch).unwrap();
        assert_eq!(files["solution.py"], "def f(n):\n    return n % 5 == 0\n");
        assert_eq!(touched, vec!["solution.py".to_string()]);
    }

    #[test]
    fn crlf_and_new_file_creation() {
        let patch = "<<<<<<< SEARCH solution.py\r\n    return n % 3 == 0\r\n=======\r\n    return True\r\n>>>>>>> REPLACE\r\n<<<<<<< SEARCH helpers/util.py\n=======\nX = 1\n>>>>>>> REPLACE";
        let (files, touched) = apply_patch(&base(), patch).unwrap();
        assert!(files["solution.py"].contains("return True"));
        assert_eq!(files["helpers/util.py"], "X = 1\n");
        assert_eq!(touched.len(), 2);
    }

    #[test]
    fn malformed_and_mismatched_patches_are_patch_invalid() {
        let cases = [
            "just prose, no blocks",
            "<<<<<<< SEARCH solution.py\nreturn\n>>>>>>> REPLACE",
            "<<<<<<< SEARCH solution.py\nreturn\n=======\nx",
            "<<<<<<< SEARCH\nreturn\n=======\nx\n>>>>>>> REPLACE",
            "<<<<<<< SEARCH solution.py\n    return n % 7 == 0\n=======\nx\n>>>>>>> REPLACE",
            "<<<<<<< SEARCH missing.py\nfoo\n=======\nx\n>>>>>>> REPLACE",
            "<<<<<<< SEARCH solution.py\n=======\nx\n>>>>>>> REPLACE",
            ">>>>>>> REPLACE",
        ];
        for c in cases {
            let err = apply_patch(&base(), c).unwrap_err();
            assert!(matches!(err, ContractError::PatchInvalid(_)), "{c:?} -> {err:?}");
        }
    }

    #[test]
    fn ambiguous_search_is_rejected() {
        let mut b = base();
        b.insert("solution.py".into(), "x = 1\nx = 1\n".into());
        let err = apply_patch(&b, "<<<<<<< SEARCH solution.py\nx = 1\n=======\nx = 2\n>>>>>>> REPLACE").unwrap_err();
        assert!(matches!(err, ContractError::PatchInvalid(m) if m.contains("ambiguous")));
    }

    #[test]
    fn traversal_and_absolute_paths_are_policy_violations() {
        for p in ["../evil.py", "/etc/passwd", "C:/Windows/x.py", "a/../../b.py"] {
            let patch = format!("<<<<<<< SEARCH {p}\n=======\nx\n>>>>>>> REPLACE");
            let err = apply_patch(&base(), &patch).unwrap_err();
            assert!(matches!(err, ContractError::PolicyViolation(_)), "{p} -> {err:?}");
        }
        assert_eq!(validate_rel_path("./a\\b.py").unwrap(), "a/b.py");
    }

    #[test]
    fn files_sha_is_order_independent_and_content_sensitive() {
        let a = base();
        let mut b = base();
        assert_eq!(files_sha256(&a), files_sha256(&b));
        b.insert("solution.py".into(), "changed".into());
        assert_ne!(files_sha256(&a), files_sha256(&b));
    }
}
