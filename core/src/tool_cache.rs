//! Session-scoped cache of large tool outputs so digests / ingress caps can
//! shrink context without forcing an expensive re-run to recover the bytes.
//!
//! Keyed by `sha256(tool_name + "\0" + args_json)`. Read-only tools may be
//! restored on an identical re-call; bash is never restored (side effects).
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::collections::VecDeque;

/// Soft cap on cached entries (oldest evicted first).
const MAX_ENTRIES: usize = 64;
/// Soft cap on total cached bytes across all entries.
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

#[derive(Default)]
pub struct ToolOutputCache {
    map: HashMap<String, String>,
    order: VecDeque<String>,
    total_bytes: usize,
}

impl ToolOutputCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stable cache key for a tool call.
    pub fn key(tool: &str, args_json: &str) -> String {
        let mut h = Sha256::new();
        h.update(tool.as_bytes());
        h.update([0]);
        h.update(args_json.as_bytes());
        let dig = h.finalize();
        // 16 hex chars (64 bits) — enough for a session-local map.
        dig.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    /// Whether an identical re-call of `tool` may be served from cache.
    pub fn is_restorable(tool: &str) -> bool {
        matches!(
            tool,
            "read_file"
                | "grep"
                | "glob"
                | "list_dir"
                | "bulk_read"
                | "fetch"
                | "web_search"
                | "diagnostics"
                | "git_status"
                | "git_diff"
                | "git_log"
                | "git_show"
                | "todo_read"
                | "workspace_activity"
        )
    }

    pub fn store(&mut self, tool: &str, args_json: &str, output: &str) {
        if output.is_empty() {
            return;
        }
        let key = Self::key(tool, args_json);
        self.insert(key, output.to_string());
    }

    /// Store under an already-computed key (used when digesting from call_map).
    pub fn store_key(&mut self, key: String, output: &str) {
        if output.is_empty() || key.is_empty() {
            return;
        }
        self.insert(key, output.to_string());
    }

    pub fn get(&self, tool: &str, args_json: &str) -> Option<&str> {
        if !Self::is_restorable(tool) {
            return None;
        }
        let key = Self::key(tool, args_json);
        self.map.get(&key).map(|s| s.as_str())
    }

    /// Drop everything — called after destructive workspace mutations so a
    /// stale read/grep can't be restored over a changed tree.
    pub fn invalidate_all(&mut self) {
        self.map.clear();
        self.order.clear();
        self.total_bytes = 0;
    }

    fn insert(&mut self, key: String, output: String) {
        if let Some(old) = self.map.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.len());
            self.order.retain(|k| k != &key);
        }
        while self.map.len() >= MAX_ENTRIES
            || (self.total_bytes + output.len() > MAX_TOTAL_BYTES && !self.order.is_empty())
        {
            if let Some(evict) = self.order.pop_front() {
                if let Some(old) = self.map.remove(&evict) {
                    self.total_bytes = self.total_bytes.saturating_sub(old.len());
                }
            } else {
                break;
            }
        }
        // If a single entry alone exceeds the budget, still keep it (better
        // than losing the only recovery path for a huge read).
        self.total_bytes = self.total_bytes.saturating_add(output.len());
        self.order.push_back(key.clone());
        self.map.insert(key, output);
    }
}

/// Tools whose successful execution should wipe the restore cache (tree changed).
pub fn invalidates_cache(tool: &str) -> bool {
    matches!(
        tool,
        "write_file"
            | "edit"
            | "patch"
            | "bulk_write"
            | "bulk_edit"
            | "bash"
            | "delete"
            | "rename"
            | "mkdir"
            | "git_add"
            | "git_commit"
            | "git_push"
            | "git_pull"
            | "git_branch"
            | "bulk" // may contain writes
            | "snapshot_edit"
            | "ast_edit"
            | "collections" // index/add/remove mutate durable store
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_get_roundtrip() {
        let mut c = ToolOutputCache::new();
        c.store("grep", r#"{"pattern":"x"}"#, "a.txt:1:x");
        assert_eq!(c.get("grep", r#"{"pattern":"x"}"#), Some("a.txt:1:x"));
        assert!(c.get("bash", r#"{"command":"ls"}"#).is_none());
    }

    #[test]
    fn invalidate_clears() {
        let mut c = ToolOutputCache::new();
        c.store("read_file", r#"{"path":"a"}"#, "hello");
        c.invalidate_all();
        assert!(c.get("read_file", r#"{"path":"a"}"#).is_none());
    }

    #[test]
    fn key_stable() {
        assert_eq!(
            ToolOutputCache::key("grep", "{}"),
            ToolOutputCache::key("grep", "{}")
        );
        assert_ne!(
            ToolOutputCache::key("grep", "{}"),
            ToolOutputCache::key("read_file", "{}")
        );
    }
}
