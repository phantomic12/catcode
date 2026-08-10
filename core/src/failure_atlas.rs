//! Failure atlas — compact diagnostic signatures (spec §11 / §13).
//!
//! Stores hashed/truncated failure signatures for retrieval during error
//! recovery. Never stores full unbounded test logs. Fail-open.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

use crate::learning_store::{self, ProjectLearningPaths, MAX_FEEDBACK};

/// Compact diagnostic signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticSignature {
    pub class: String,
    pub signature: String,
    pub count: u32,
    pub last_seen: u64,
    #[serde(default)]
    pub resolved: bool,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn diagnostics_path(project_id: &str) -> std::path::PathBuf {
    ProjectLearningPaths::resolve(project_id)
        .root
        .join("diagnostics.jsonl")
}

/// Normalize a raw error string into a compact signature (bounded).
pub fn compact_signature(raw: &str) -> String {
    let s = raw.lines().take(3).collect::<Vec<_>>().join(" | ");
    // Collapse very long digit runs (addresses), keep short error codes.
    let mut out = String::new();
    let mut digit_run = 0;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digit_run += 1;
            if digit_run <= 5 {
                out.push(c);
            } else if digit_run == 6 {
                out.push('#');
            }
        } else {
            digit_run = 0;
            out.push(c);
        }
    }
    out.chars().take(240).collect()
}

/// True if output looks like a real diagnostic (compile/type/test error) worth
/// remembering, vs. a non-diagnostic failure (typo, missing file, exit 1 from
/// a grep). Keeps the failure atlas focused on recurring *technical* failures.
pub fn looks_like_diagnostic_output(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("error[")
        || l.contains("error:")
        || l.contains("error e")
        || l.contains("panicked")
        || l.contains("panic:")
        || l.contains("traceback")
        || l.contains("exception")
        || l.contains("test failed")
        || l.contains("tests failed")
        || l.contains("failed to compile")
        || l.contains("cannot find")
        || l.contains("undefined reference")
        || l.contains("does not compile")
        || l.contains("syntax error")
}

/// Record (or bump) a diagnostic signature for a project.
pub fn record_diagnostic(project_id: &str, class: &str, raw_or_sig: &str) {
    let sig = compact_signature(raw_or_sig);
    let path = diagnostics_path(project_id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = ProjectLearningPaths::resolve(project_id).ensure();

    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"));
    let mut rows: Vec<DiagnosticSignature> = learning_store::read_jsonl(&path);
    if let Some(existing) = rows
        .iter_mut()
        .find(|r| r.class == class && r.signature == sig)
    {
        existing.count = existing.count.saturating_add(1);
        existing.last_seen = now_secs();
        existing.resolved = false;
    } else {
        rows.push(DiagnosticSignature {
            class: class.to_string(),
            signature: sig,
            count: 1,
            last_seen: now_secs(),
            resolved: false,
        });
    }
    let mut body = String::new();
    for r in &rows {
        if let Ok(line) = serde_json::to_string(r) {
            body.push_str(&line);
            body.push_str("\n");
        }
    }
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Match diagnostics by class substring or signature text.
pub fn match_diagnostics(
    project_id: &str,
    class_or_text: &str,
    limit: usize,
) -> Vec<DiagnosticSignature> {
    let q = class_or_text.to_lowercase();
    let mut rows = learning_store::read_jsonl::<DiagnosticSignature>(&diagnostics_path(project_id));
    rows.retain(|r| {
        r.class.to_lowercase().contains(&q)
            || r.signature.to_lowercase().contains(&q)
            || q.is_empty()
    });
    rows.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_seen.cmp(&a.last_seen)));
    rows.truncate(limit);
    rows
}

/// Mark matching signatures resolved.
pub fn mark_resolved(project_id: &str, class: &str) {
    let path = diagnostics_path(project_id);
    let mut rows: Vec<DiagnosticSignature> = learning_store::read_jsonl(&path);
    let mut changed = false;
    for r in &mut rows {
        if r.class == class {
            r.resolved = true;
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let _ = std::fs::write(&path, "");
    for r in &rows {
        learning_store::append_jsonl(&path, r, MAX_FEEDBACK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning_store::{learning_test_serial, override_learning_root};

    #[test]
    fn record_and_match() {
        let _ls = learning_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("fail-{}-{}", std::process::id(), now_secs()));
        let _ = std::fs::remove_dir_all(&root);
        let _g = override_learning_root(root);
        record_diagnostic("project-x", "cargo-test", "error[E0308]: mismatched types");
        record_diagnostic("project-x", "cargo-test", "error[E0308]: mismatched types");
        let hits = match_diagnostics("project-x", "E0308", 5);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].count, 2);
    }

    #[test]
    fn compact_bounds_length() {
        let long = "x".repeat(1000);
        assert!(compact_signature(&long).len() <= 240);
    }
}
