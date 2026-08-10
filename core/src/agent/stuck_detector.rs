//! Self-correcting stuck detection — a sliding-window detector that recognizes
//! when the agent is repeating the same tool call without making filesystem
//! progress, and injects a steering nudge so the model self-corrects.
//!
//! This is deliberately NOT a max-turn cap. The loop runs to completion; the
//! detector only observes and nudges. When the model is spinning (re-reading the
//! same file, re-grepping the same pattern, re-listing the same dir) the nudge
//! makes it aware of the repetition and suggests concrete alternatives. A
//! non-stuck agent never sees a nudge.

use crate::tooling::policy::classify;
use crate::tooling::ToolKind;
use serde_json::Value;

/// Number of consecutive identical non-mutating calls that trip the detector.
/// Three identical reads/greps with no write in between is an unambiguous spin.
const STUCK_THRESHOLD: usize = 3;

/// A normalized signature for a tool call: the tool name plus the single most
/// identifying argument (path / pattern / command / task / …), ignoring
/// incidental fields like line offsets, flags, or limit counts. This makes
/// "similar" calls collapse to the same signature — e.g. `read_file` on the same
/// path with different `offset`/`limit`, or `grep` with the same `pattern` but
/// different `head_limit`, are treated as identical.
fn tool_signature(name: &str, args: &str) -> String {
    // `finish` is a completion signal, not work — never counts toward stuck.
    if name == "finish" {
        return "finish".to_string();
    }
    let v: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return format!("{name}|<unparseable>"),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return name.to_string(),
    };
    // Tool-specific identifying field priority. For search tools the pattern
    // is the real key (the path/glob is just a scope); for file tools the path
    // dominates; bash by command; etc.
    let priority: &[&str] = match name {
        "grep" | "glob" => &["pattern"],
        "bash" => &["command"],
        "subagent" => &["task"],
        "read_file" | "write_file" | "edit" | "patch" | "delete" | "rename" | "mkdir" => {
            &["path", "from"]
        }
        "collections" => &["collection", "action", "query"],
        "memory" => &["action", "name", "id"],
        "fetch" => &["url"],
        _ => &[
            "path",
            "from",
            "command",
            "pattern",
            "query",
            "task",
            "url",
            "collection",
            "id",
            "name",
        ],
    };
    let key = priority
        .iter()
        .find_map(|f| obj.get(*f))
        .map(|k| {
            let s = match k {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            // Cap so a giant command/task blob doesn't dominate the signature.
            s.chars().take(200).collect::<String>()
        })
        .unwrap_or_default();
    format!("{name}|{key}")
}

/// Whether a tool mutates the workspace (files / shell side effects). Reuses the
/// existing fail-closed policy classification: anything not ReadOnly is treated
/// as a mutation for stuck purposes.
fn is_mutating_tool(name: &str) -> bool {
    classify(name) != ToolKind::ReadOnly
}

/// Self-correcting stuck detector. Maintains a sliding window of recent tool
/// call signatures and whether each was a mutation. When the last
/// `STUCK_THRESHOLD` signatures are identical AND none of them mutated the
/// filesystem, the agent is spinning and a steering nudge is produced.
pub(crate) struct StuckDetector {
    recent: Vec<String>,
    recent_mutating: Vec<bool>,
    /// How many nudges have fired for the *current* repetition episode. Each
    /// nudge raises the re-trigger threshold by 2 so persistent loops get
    /// progressively sparser reminders rather than a nudge every 3 calls.
    nudge_count: usize,
}

impl StuckDetector {
    pub(crate) fn new() -> Self {
        Self {
            recent: Vec::with_capacity(STUCK_THRESHOLD + 4),
            recent_mutating: Vec::with_capacity(STUCK_THRESHOLD + 4),
            nudge_count: 0,
        }
    }

    /// Record a tool call that is about to run (or just ran). The signature is
    /// derived from `name` + `args`; mutation status from the policy classify.
    pub(crate) fn record(&mut self, name: &str, args: &str) {
        let sig = tool_signature(name, args);
        let mutating = is_mutating_tool(name);
        // `finish` never counts as a repetition step — it ends the episode.
        if sig == "finish" {
            self.recent.clear();
            self.recent_mutating.clear();
            self.nudge_count = 0;
            return;
        }
        // Progress (mutation) or a *different* signature than the current
        // spin ends the stuck episode — reset escalation so a later
        // independent spin starts at threshold 3 again (CORE_REVIEW).
        // Do NOT reset when the window is empty (post-nudge): that would
        // wipe the escalated threshold before the next spin re-triggers.
        if mutating {
            self.nudge_count = 0;
        } else if let Some(last) = self.recent.last() {
            if last != &sig {
                self.nudge_count = 0;
            }
        }
        self.recent.push(sig);
        self.recent_mutating.push(mutating);
        // Keep the window bounded — we only ever inspect the last few entries.
        if self.recent.len() > STUCK_THRESHOLD + 4 {
            let drop = self.recent.len() - (STUCK_THRESHOLD + 4);
            self.recent.drain(0..drop);
            self.recent_mutating.drain(0..drop);
        }
    }

    /// Inspect the window and return a steering nudge if the agent appears stuck.
    /// When a nudge is returned the window is reset so the model gets a fresh
    /// chance to self-correct before the next nudge. Returns `None` when healthy.
    pub(crate) fn check_and_nudge(&mut self) -> Option<String> {
        let n = self.recent.len();
        if n < STUCK_THRESHOLD {
            return None;
        }
        // Escalating threshold: first nudge at STUCK_THRESHOLD, then +2 per prior
        // nudge in this episode (3, 5, 7, …). Prevents nudge spam while still
        // catching loops that refuse to self-correct.
        let threshold = STUCK_THRESHOLD + self.nudge_count * 2;
        if n < threshold {
            return None;
        }
        let tail = &self.recent[n - threshold..];
        let tail_mut = &self.recent_mutating[n - threshold..];
        let first = &tail[0];
        // All `threshold` signatures identical?
        if !tail.iter().all(|s| s == first) {
            return None;
        }
        // None of them mutated the filesystem (pure spin — re-read/re-grep/etc).
        if tail_mut.iter().any(|m| *m) {
            return None;
        }
        // A mutating call anywhere in the broader recent window breaks the
        // episode — the agent made progress, so reset escalation.
        let nudge = self.build_nudge(first, threshold);
        self.nudge_count += 1;
        // Reset the window so the model gets `threshold` fresh calls before the
        // next nudge (escalated).
        self.recent.clear();
        self.recent_mutating.clear();
        Some(nudge)
    }

    fn build_nudge(&self, sig: &str, hits: usize) -> String {
        let (tool, key) = sig.split_once('|').unwrap_or((sig, ""));
        let key_desc = if key.is_empty() {
            String::new()
        } else {
            format!(" (key args: {})", key)
        };
        format!(
            "⚠ You appear to be stuck in a repetition loop: you have called `{tool}`{key_desc} \
             {hits} times in a row without making any filesystem changes. Re-running an \
             identical read-only call will return the same result you already have — it will not \
             change anything. Self-correct now by choosing one of:\n\
             1. Use the result you already have from the earlier call(s) instead of re-fetching.\n\
             2. Try a different tool or approach to make progress toward your goal.\n\
             3. Re-read your original goal and verify whether the task is actually complete.\n\
             4. If the task is done, call `finish` with a summary.\n\
             Do NOT repeat the same call again."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> StuckDetector {
        StuckDetector::new()
    }

    #[test]
    fn signature_normalizes_incidental_fields() {
        // read_file same path, different offset/limit → same signature.
        let a = tool_signature(
            "read_file",
            r#"{"path":"src/main.rs","offset":1,"limit":50}"#,
        );
        let b = tool_signature(
            "read_file",
            r#"{"path":"src/main.rs","offset":100,"limit":200}"#,
        );
        assert_eq!(a, b);
        // Different path → different signature.
        let c = tool_signature("read_file", r#"{"path":"src/lib.rs"}"#);
        assert_ne!(a, c);
    }

    #[test]
    fn signature_uses_command_for_bash() {
        let a = tool_signature("bash", r#"{"command":"ls -la"}"#);
        assert!(a.starts_with("bash|ls -la"));
    }

    #[test]
    fn signature_uses_pattern_for_grep() {
        let a = tool_signature("grep", r#"{"pattern":"TODO","head_limit":10}"#);
        let b = tool_signature("grep", r#"{"pattern":"TODO","head_limit":50}"#);
        assert_eq!(a, b);
        assert!(a.starts_with("grep|TODO"));
    }

    #[test]
    fn finish_never_counts_toward_repetition() {
        let mut d = det();
        d.record("read_file", r#"{"path":"x.rs"}"#);
        d.record("finish", "{}");
        d.record("read_file", r#"{"path":"x.rs"}"#);
        d.record("finish", "{}");
        d.record("read_file", r#"{"path":"x.rs"}"#);
        // Only 3 read_file calls but interspersed with finish — the window has
        // 5 entries with mixed signatures, so no nudge yet.
        assert!(d.check_and_nudge().is_none());
    }

    #[test]
    fn three_identical_reads_trigger_nudge() {
        let mut d = det();
        d.record("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(d.check_and_nudge().is_none());
        d.record("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(d.check_and_nudge().is_none());
        d.record("read_file", r#"{"path":"src/main.rs"}"#);
        let nudge = d
            .check_and_nudge()
            .expect("should nudge after 3 identical reads");
        assert!(nudge.contains("stuck in a repetition loop"));
        assert!(nudge.contains("read_file"));
        assert!(nudge.contains("src/main.rs"));
    }

    #[test]
    fn mutation_between_reads_prevents_nudge() {
        let mut d = det();
        d.record("read_file", r#"{"path":"x.rs"}"#);
        d.record("edit", r#"{"path":"x.rs","edits":[]}"#); // mutation
        d.record("read_file", r#"{"path":"x.rs"}"#);
        d.record("edit", r#"{"path":"x.rs","edits":[]}"#);
        d.record("read_file", r#"{"path":"x.rs"}"#);
        // Mutations interspersed → not stuck (the agent is making progress).
        assert!(d.check_and_nudge().is_none());
    }

    #[test]
    fn nudge_resets_window_then_re_triggers_at_escalated_threshold() {
        let mut d = det();
        for _ in 0..3 {
            d.record("grep", r#"{"pattern":"foo"}"#);
        }
        let n1 = d.check_and_nudge();
        assert!(n1.is_some(), "first nudge at threshold 3");
        // After reset, 3 more identical calls should NOT re-trigger (threshold
        // escalated to 5 after one nudge).
        for _ in 0..3 {
            d.record("grep", r#"{"pattern":"foo"}"#);
        }
        assert!(
            d.check_and_nudge().is_none(),
            "no nudge before escalated threshold"
        );
        // Two more → 5 total since reset → re-trigger.
        for _ in 0..2 {
            d.record("grep", r#"{"pattern":"foo"}"#);
        }
        assert!(
            d.check_and_nudge().is_some(),
            "second nudge at escalated threshold 5"
        );
    }

    #[test]
    fn different_calls_do_not_trigger() {
        let mut d = det();
        d.record("read_file", r#"{"path":"a.rs"}"#);
        d.record("read_file", r#"{"path":"b.rs"}"#);
        d.record("read_file", r#"{"path":"c.rs"}"#);
        assert!(d.check_and_nudge().is_none());
    }

    #[test]
    fn is_mutating_classifies_correctly() {
        assert!(!is_mutating_tool("read_file"));
        assert!(!is_mutating_tool("grep"));
        assert!(!is_mutating_tool("glob"));
        assert!(is_mutating_tool("write_file"));
        assert!(is_mutating_tool("edit"));
        assert!(is_mutating_tool("bash"));
        // Unknown → fail-closed destructive → mutating.
        assert!(is_mutating_tool("totally_unknown_tool"));
    }

    #[test]
    fn similar_grep_calls_collapse_to_same_signature() {
        // Same pattern, different path-scope and flags → same signature key.
        let a = tool_signature("grep", r#"{"pattern":"fn main","path":"src"}"#);
        let b = tool_signature("grep", r#"{"pattern":"fn main","glob":"*.rs"}"#);
        assert_eq!(a, b);
    }
}
