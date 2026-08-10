use crate::config::{Approval, PermissionRule};
use crate::tooling::ToolKind;
use crate::workspace;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

pub(crate) fn sanitized_approval_preview(args: &Value) -> String {
    fn sanitize(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "api_key"
                            | "authorization"
                            | "password"
                            | "access_token"
                            | "refresh_token"
                            | "id_token"
                            | "client_secret"
                    ) {
                        *value = Value::String("[REDACTED]".into());
                    } else {
                        sanitize(value);
                    }
                }
            }
            Value::Array(values) => values.iter_mut().for_each(sanitize),
            _ => {}
        }
    }

    let mut preview = args.clone();
    sanitize(&mut preview);
    let serialized = serde_json::to_string(&preview).unwrap_or_else(|_| "{}".into());
    serialized.chars().take(2_000).collect()
}

/// A persisted pattern must cover the current request without degenerating to
/// an unscoped wildcard. This is intentionally conservative; users can approve
/// a broader operation separately instead of silently widening one request.
pub(crate) fn approval_pattern_within_requested_scope(args: &Value, pattern: &str) -> bool {
    let target = args
        .get("path")
        .or_else(|| args.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let pattern = pattern.trim();
    if target.is_empty() || pattern.is_empty() || pattern == "*" {
        return false;
    }
    if pattern.split(['/', '\\']).any(|part| part == "..") {
        return false;
    }
    if pattern == target {
        return true;
    }
    let wildcard = pattern
        .char_indices()
        .find(|(_, ch)| matches!(ch, '*' | '?' | '['))
        .map(|(index, _)| index);
    let Some(wildcard) = wildcard else {
        return false;
    };
    let prefix = pattern[..wildcard].trim();
    !prefix.is_empty() && prefix != "/" && target.starts_with(prefix)
}

#[cfg(test)]
mod approval_scope_tests {
    use super::*;

    #[test]
    fn pattern_must_cover_request_without_global_wildcard() {
        let args = json!({"path":"src/runtime/mod.rs"});
        assert!(approval_pattern_within_requested_scope(
            &args,
            "src/runtime/*"
        ));
        assert!(approval_pattern_within_requested_scope(
            &args,
            "src/runtime/mod.rs"
        ));
        assert!(!approval_pattern_within_requested_scope(&args, "*"));
        assert!(!approval_pattern_within_requested_scope(&args, "docs/*"));
        assert!(!approval_pattern_within_requested_scope(&args, "../*"));
    }

    #[test]
    fn approval_preview_redacts_nested_secrets() {
        let preview = sanitized_approval_preview(
            &json!({"path":"x", "nested":{"password":"secret"}, "tokens_in":3}),
        );
        assert!(preview.contains("[REDACTED]"));
        assert!(!preview.contains("secret"));
        assert!(preview.contains("tokens_in"));
    }
}

/// Ask the TUI to approve a tool call; block until answered or aborted.
/// On "always", only the matched tool KIND is escalated (not the whole session).

pub(crate) fn tool_matches_rule(tool_name: &str, args: &Value, rule: &PermissionRule) -> bool {
    if !rule.tool_name.eq_ignore_ascii_case(tool_name) && rule.tool_name != "*" {
        return false;
    }
    if rule.rule_content.is_empty() || rule.rule_content == "*" {
        return true;
    }
    // Rule content matching: check against tool args.
    // For bash: match against the command string.
    // For write_file/edit: match against the path.
    // For grep/glob: match against the search pattern.
    // For WebFetch: match against URL domain.
    // Use glob-style matching with * wildcards.
    let candidate = match tool_name {
        "bash" => args.get("command").and_then(|v| v.as_str()).unwrap_or(""),
        "write_file" | "edit" | "patch" | "read_file" | "bulk_read" | "bulk_write"
        | "bulk_edit" | "delete" | "mkdir" => {
            args.get("path").and_then(|v| v.as_str()).unwrap_or("")
        }
        "rename" => args.get("from").and_then(|v| v.as_str()).unwrap_or(""),
        "grep" => args.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
        "glob" => args.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
        _ => "",
    };
    if candidate.is_empty() {
        return false;
    }
    star_match_rule(&rule.rule_content, candidate)
}

fn star_match_rule(pattern: &str, text: &str) -> bool {
    // Simple glob: * matches any sequence, ? matches one char.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            match p[i - 1] {
                '*' => dp[i][j] = dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i][j] = dp[i - 1][j - 1],
                c => dp[i][j] = dp[i - 1][j - 1] && c == t[j - 1],
            }
        }
    }
    dp[p.len()][t.len()]
}

/// If this tool call targets a restricted ("dangerous") path, return the
/// blocklist reason. The approval gate uses this so that — under
/// `Destructive`/`Always` — a restricted path (`.env`, `.git/**`, `.ssh/**`,
/// `id_rsa`, …) forces an approval prompt for BOTH reads and writes, instead
/// of the old unconditional hard block. Under `Never` the gate skips this
/// entirely, so ALL file restrictions are disabled.
///
/// `root` is the workspace root used to resolve symlinks: each path is first
/// checked against the blocklist in its RAW model-supplied form (catches a
/// literal `.env`/`.git` early), then — after `workspace::resolve` follows
/// symlinks to a canonical absolute path — checked AGAIN against the
/// canonical path's components. A symlink alias such as `linkdir -> .git`
/// makes `linkdir/config` pass the raw check (no `.git` in the literal
/// string) yet resolve to `<root>/.git/config`; the canonical re-check closes
/// that bypass, since the canonical path is what actually gets read/written.
/// If `resolve` fails (e.g. the path escapes the workspace) the raw-check
/// result stands unchanged.
///
/// Covers content-touching tools and file-targeted searches/collection indexing.
pub(crate) fn restricted_path_for_tool(
    name: &str,
    args: &Value,
    root: &std::path::Path,
) -> Option<String> {
    fn path_of(a: &Value) -> Option<&str> {
        a.get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    }
    // Check one path string: raw form first, then the symlink-resolved
    // canonical path. Both use the same blocklist; the canonical pass is what
    // defeats a symlink alias (linkdir -> .git) the raw pass can't see.
    fn check(raw: &str, root: &std::path::Path) -> Option<String> {
        if let Some(reason) = workspace::check_dangerous_path(raw) {
            return Some(reason);
        }
        let canon = workspace::resolve(root, raw).ok()?;
        // Reduce to a root-relative, forward-slash form so the same
        // component-glob logic (`.git/**`, `**/.ssh/**`, …) that checks the
        // raw string applies to the canonical path, cross-platform.
        let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let rel = canon.strip_prefix(&canon_root).unwrap_or(&canon);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        workspace::check_dangerous_path(&rel_str)
    }
    match name {
        "read_file" | "write_file" | "edit" | "patch" | "delete" | "mkdir" => {
            path_of(args).and_then(|raw| check(raw, root))
        }
        "grep" | "collections" => path_of(args).and_then(|raw| {
            let resolved = workspace::resolve(root, raw).ok()?;
            if resolved.is_file() {
                check(raw, root)
            } else {
                None
            }
        }),
        "rename" => {
            let from = args
                .get("from")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let to = args
                .get("to")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            from.and_then(|raw| check(raw, root))
                .or_else(|| to.and_then(|raw| check(raw, root)))
        }
        "bulk_read" => args
            .get("paths")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_str())
                    .find_map(|raw| check(raw, root))
            }),
        "bulk_write" => args
            .get("files")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("path").and_then(|v| v.as_str()))
                    .find_map(|raw| check(raw, root))
            }),
        "bulk_edit" => args
            .get("edits")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("path").and_then(|v| v.as_str()))
                    .find_map(|raw| check(raw, root))
            }),
        // `bulk`: recurse into inner calls — if ANY inner call targets a
        // restricted path, the whole bulk prompts (then approved calls proceed).
        "bulk" => args
            .get("calls")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|c| {
                    let n = c.get("name").and_then(|v| v.as_str())?;
                    let a = c.get("args")?;
                    restricted_path_for_tool(n, a, root)
                })
            }),
        _ => None,
    }
}

/// Build the user-message prompt for an `apply_skill` invocation: instructs
/// the model to read and follow the named skill, inlining the skill body (the
/// core reads it from disk so global skills under ~/.catalyst-code/skills
/// work despite read_file's path restriction), and appending an optional task.

/// Pure approval decision shared by foreground and delegated tool paths.
/// Permission-rule grants (`force_allow`/`escalated`) always win; explicit
/// force-ask is only honored when no grant exists.
pub fn approval_required(
    mode: &Approval,
    kind: ToolKind,
    restricted_path: bool,
    force_allow: bool,
    escalated: bool,
    force_ask: bool,
) -> bool {
    if force_allow || escalated {
        return false;
    }
    if force_ask {
        return true;
    }
    match mode {
        Approval::Never => false,
        Approval::Destructive => kind == ToolKind::Destructive || restricted_path,
        Approval::Always => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_modes_and_overrides_explicitly() {
        assert!(!approval_required(
            &Approval::Never,
            ToolKind::Destructive,
            true,
            false,
            false,
            false
        ));
        assert!(approval_required(
            &Approval::Destructive,
            ToolKind::ReadOnly,
            true,
            false,
            false,
            false
        ));
        assert!(approval_required(
            &Approval::Always,
            ToolKind::ReadOnly,
            false,
            false,
            false,
            false
        ));
        assert!(!approval_required(
            &Approval::Always,
            ToolKind::Destructive,
            true,
            true,
            false,
            true
        ));
    }
}
