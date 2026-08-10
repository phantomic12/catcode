use crate::*;

const DIGEST_MIN_BYTES: usize = 256;
/// Soft reclaim leaves more history than full compaction while still creating
/// useful runway. It is a target after the 70% default trigger, not a trigger.
const SOFT_DIGEST_TARGET_FRACTION: f32 = 0.60;
/// Post-compaction target: reclaim until the conversation fits under this
/// fraction of the context window (was 0.50 — left too little runway).
const POST_COMPACT_BUDGET_FRACTION: f32 = 0.35;

/// One shared, model-aware policy for every automatic context rewrite. The
/// configured percentages remain user-facing intent, while `hard_limit`
/// reserves enough room for a likely response plus protocol/tokenizer drift.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ContextPolicy {
    pub digest_threshold: u64,
    pub compact_threshold: u64,
    pub hard_limit: u64,
    pub response_reserve: u64,
    pub safety_margin: u64,
}

pub(crate) fn context_policy(
    messages: &[Message],
    context_window: u64,
    max_output_tokens: u32,
    compact_at: f32,
    digest_at: f32,
) -> ContextPolicy {
    if context_window == 0 {
        return ContextPolicy {
            digest_threshold: 0,
            compact_threshold: 0,
            hard_limit: 0,
            response_reserve: 0,
            safety_margin: 0,
        };
    }

    // Start with 5% output runway, then grow it when recent assistant replies
    // demonstrate that this session needs more. Bound it by both model metadata
    // and 25% of the window so an advertised 128k output cap does not force a
    // healthy 400k context to compact around 50–60%.
    let base_reserve = ((context_window as f64 * 0.05) as u64)
        .max(512)
        .min(context_window / 10);
    let observed = messages
        .iter()
        .rev()
        .filter(|m| m.is_assistant())
        .take(4)
        .map(estimate_message_tokens)
        .max()
        .unwrap_or(0)
        .saturating_mul(2);
    let metadata_cap = if max_output_tokens == 0 {
        context_window / 4
    } else {
        (max_output_tokens as u64).min(context_window / 4)
    };
    let response_reserve = base_reserve
        .max(observed)
        .min(metadata_cap.max(base_reserve));
    let safety_margin = ((context_window as f64 * 0.02) as u64)
        .max(256)
        .min(context_window / 20);
    let hard_limit = context_window
        .saturating_sub(response_reserve)
        .saturating_sub(safety_margin);
    let configured_compact = (context_window as f64 * compact_at as f64).round() as u64;
    let compact_threshold = configured_compact.min(hard_limit);
    let digest_threshold = if digest_at <= 0.0 {
        0
    } else {
        ((context_window as f64 * digest_at as f64).round() as u64).min(compact_threshold)
    };
    ContextPolicy {
        digest_threshold,
        compact_threshold,
        hard_limit,
        response_reserve,
        safety_margin,
    }
}

pub(crate) fn utilization_pct(tokens: u64, context_window: u64) -> u64 {
    if context_window == 0 {
        0
    } else {
        ((tokens as f64 / context_window as f64) * 100.0).round() as u64
    }
}

pub(crate) fn should_auto_digest(auto_compact: bool, est: u64, policy: ContextPolicy) -> bool {
    auto_compact && policy.digest_threshold > 0 && est > policy.digest_threshold
}

pub(crate) fn should_auto_compact(
    auto_compact: bool,
    est: u64,
    message_count: usize,
    policy: ContextPolicy,
) -> bool {
    auto_compact && est > policy.compact_threshold && (est > policy.hard_limit || message_count > 4)
}

/// Index where the soft-digest keep-window begins: walk backward accumulating
/// tokens until `keep_budget` (20% of context, floored at 4k) is exceeded,
/// always keeping at least `SOFT_DIGEST_MIN_KEEP` messages.
pub(crate) fn soft_digest_keep_start(messages: &[Message], context_window: u64) -> usize {
    let n = messages.len();
    if n <= SOFT_DIGEST_MIN_KEEP {
        return 0;
    }
    let budget = ((context_window as f32 * SOFT_DIGEST_KEEP_FRACTION) as u64).max(4_000);
    let mut acc: u64 = 0;
    let mut start = n;
    for i in (0..n).rev() {
        let t = estimate_message_tokens(&messages[i]);
        if i < n.saturating_sub(SOFT_DIGEST_MIN_KEEP) && acc + t > budget {
            break;
        }
        acc += t;
        start = i;
    }
    start
}

/// Soft-digest path used by the main loop and subagents: collapse stale large
/// tool results AND oversized tool-call arguments outside the token-budgeted
/// keep window, then budget-reclaim any remaining oversized call args / results
/// that still push the conversation over the soft reclaim target.
/// When `cache` is provided, restorable tool outputs are stored before they are
/// replaced so "re-run identical call to restore" is truthful.
/// Returns total items changed.
pub fn soft_digest_conversation(
    messages: &mut Vec<Message>,
    context_window: u64,
    cache: Option<&mut tool_cache::ToolOutputCache>,
) -> usize {
    // Prefill cache from oversized tool results we're about to digest so a
    // later identical re-call can restore without re-executing.
    if let Some(cache) = cache {
        cache_tool_results_before_digest(messages, cache);
    }
    let keep_start = soft_digest_keep_start(messages, context_window);
    let mut changed = 0usize;
    let n = messages.len();
    if n <= SOFT_DIGEST_MIN_KEEP {
        // Few-but-huge: digest oversized tool results but keep the most recent
        // one so the model does not lose the content it just fetched (CORE_REVIEW).
        changed += digest_stale_tool_results(messages, 1);
        changed += digest_stale_call_args(messages, n);
    } else if keep_start > 0 {
        let keep = n.saturating_sub(keep_start);
        changed += digest_stale_tool_results(messages, keep);
        changed += digest_stale_call_args(messages, keep_start);
    }
    // else: keep_start==0 but n > MIN_KEEP means the whole history still fits
    // in the keep budget — don't collapse recent results; leave reclaim to
    // digest_to_budget below.
    // Soft budget reclaim: the default trigger is 70% and the target is 60%,
    // leaving useful runway without making this lightweight path resemble a
    // full compaction.
    let soft_budget = ((context_window as f32) * SOFT_DIGEST_TARGET_FRACTION) as u64;
    changed += digest_to_budget(messages, soft_budget);
    changed
}

/// Store restorable tool outputs into the digest cache before they are
/// collapsed, so identical re-calls can restore after soft digest / compact.
pub(crate) fn cache_tool_results_before_digest(
    messages: &[Message],
    cache: &mut tool_cache::ToolOutputCache,
) {
    let mut call_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for m in messages {
        if let Some(calls) = m.tool_calls() {
            for tc in calls {
                if !tc.id.is_empty() {
                    call_map.insert(
                        tc.id.clone(),
                        (tc.function.name.clone(), tc.function.arguments.clone()),
                    );
                }
            }
        }
    }
    for m in messages {
        if !m.is_tool() {
            continue;
        }
        let content = match m.content_text() {
            Some(c) if !c.starts_with("[digested:") && c.len() > DIGEST_MIN_BYTES => c,
            _ => continue,
        };
        let id = m.tool_call_id().unwrap_or("");
        let Some((name, args)) = call_map.get(id) else {
            continue;
        };
        if tool_cache::ToolOutputCache::is_restorable(name) {
            cache.store(name, args, content);
        }
    }
}

/// Collapse stale, large `role: "tool"` results into a one-line digest so they
/// stop being re-sent verbatim on every turn. Only tool messages older than the
/// last `keep` messages are eligible, and only if their content exceeds
/// `DIGEST_MIN_BYTES`. Already-digested results are skipped (idempotent). The
/// tool_call_id and role are preserved so orphaned-call sanitization and the
/// model's tool-call/result pairing stay intact. Returns the count digested.
#[allow(clippy::ptr_arg)]
pub fn digest_stale_tool_results(messages: &mut Vec<Message>, keep: usize) -> usize {
    if messages.len() <= keep {
        return 0;
    }
    // Build tool_call_id -> (tool_name, args_json) from assistant tool_calls so
    // the digest records WHAT was read/run, not just the size.
    let mut call_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for m in messages.iter() {
        if !m.is_assistant() {
            continue;
        }
        if let Some(calls) = m.tool_calls() {
            for tc in calls {
                if tc.id.is_empty() {
                    continue;
                }
                call_map.insert(
                    tc.id.clone(),
                    (tc.function.name.clone(), tc.function.arguments.clone()),
                );
            }
        }
    }
    let digest_to = messages.len().saturating_sub(keep);
    let mut changed = 0usize;
    for m in messages[..digest_to].iter_mut() {
        if !m.is_tool() {
            continue;
        }
        let content = match m.content_text() {
            Some(c) => c,
            None => continue,
        };
        if content.starts_with("[digested:") || content.len() <= DIGEST_MIN_BYTES {
            continue;
        }
        let id = m.tool_call_id().unwrap_or("").to_string();
        let (name, args_json) = call_map.get(&id).cloned().unwrap_or_default();
        let lines = content.lines().count();
        let digest = make_digest(&name, &args_json, content.len(), lines);
        if let Message::Tool {
            ref mut content, ..
        } = m
        {
            *content = digest;
            changed += 1;
        }
    }
    changed
}

/// Soft-path reclaim for oversized assistant `tool_call.arguments` (write_file
/// content, edit payloads, …). Unlike `digest_oversized_call_args` (budget-driven
/// at compact time), this digests every eligible call at or before `until_idx`
/// so large write/edit args stop riding every turn well before the 90% compact.
pub(crate) fn digest_stale_call_args(messages: &mut [Message], until_idx: usize) -> usize {
    let end = until_idx.min(messages.len());
    let mut changed = 0usize;
    for m in messages.iter_mut().take(end) {
        if let Message::Assistant {
            ref mut tool_calls, ..
        } = m
        {
            if let Some(calls) = tool_calls.as_mut() {
                for tc in calls.iter_mut() {
                    if tc.function.arguments.len() <= 2048 {
                        continue;
                    }
                    if let Some(digested) =
                        digest_call_args_field(&tc.function.name, &tc.function.arguments)
                    {
                        tc.function.arguments = digested;
                        changed += 1;
                    }
                }
            }
        }
    }
    changed
}

/// Reclaim oversized assistant `tool_call.arguments` (H3): the tool-result
/// digest (`digest_to_budget`'s main loop) only collapses `role:"tool"`
/// messages, so a huge NON-tool message — an assistant tool_call whose
/// `arguments` JSON embeds a large payload (a `write_file`'s `content`, an
/// `edit`'s `edits`, a `patch`'s diff) — survives compaction untouched. If it
/// lands in the kept tail and alone approaches the window, the next request
/// is oversized → HTTP 400 that repeats every turn. This replaces that payload
/// field with a one-line digest (keeping id/name + valid JSON) oldest-first
/// until `messages` fits `budget`. Returns the count digested.
pub(crate) fn digest_oversized_call_args(messages: &mut [Message], budget: u64) -> usize {
    if estimate_messages_tokens(messages) <= budget {
        return 0;
    }
    let mut changed = 0usize;
    for i in 0..messages.len() {
        if estimate_messages_tokens(messages) <= budget {
            break;
        }
        // Borrow this one message mutably (the immutable budget-check borrow
        // above has already ended at the `;`).
        if let Message::Assistant {
            ref mut tool_calls, ..
        } = messages[i]
        {
            if let Some(calls) = tool_calls.as_mut() {
                for tc in calls.iter_mut() {
                    if tc.function.arguments.len() <= 2048 {
                        continue;
                    }
                    if let Some(digested) =
                        digest_call_args_field(&tc.function.name, &tc.function.arguments)
                    {
                        tc.function.arguments = digested;
                        changed += 1;
                    }
                }
            }
        }
    }
    changed
}

/// If a tool-call's `arguments` JSON embeds a large payload field, replace just
/// that field with a one-line digest, keeping the rest of the args + valid
/// JSON. Returns the new arguments string, or `None` when there is nothing to
/// trim (unknown tool, missing field, or the field is already small).
pub(crate) fn digest_call_args_field(tool: &str, args_json: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(args_json).ok()?;
    let field = match tool {
        "write_file" => "content",
        "edit" | "bulk_edit" => "edits",
        "patch" => "patch",
        "bulk_write" => "files",
        _ => return None,
    };
    let obj = v.as_object_mut()?;
    let cur = obj.get(field)?;
    let cur_len = serde_json::to_string(cur).map(|s| s.len()).unwrap_or(0);
    if cur_len <= 2048 {
        return None;
    }
    let digest = format!(
        "[digested: {} `{}` was {} bytes — re-run to regenerate]",
        tool, field, cur_len
    );
    obj.insert(field.to_string(), Value::String(digest));
    Some(serde_json::to_string(&v).unwrap_or_else(|_| args_json.to_string()))
}

/// Last-resort token reclaim for compaction: collapse oversized `role:"tool"`
/// results into one-line digests until `messages` fits under `budget` tokens.
/// Unlike `digest_stale_tool_results` (which only touches results older than a
/// keep-window and bails on small conversations), this digests ANY eligible
/// tool result — including recent ones — oldest-first, stopping as soon as the
/// budget is met so the most recent results stay verbatim when possible.
///
/// This is what makes compaction effective when a few huge tool results (large
/// file reads, verbose command output) dominate the context: dropping old
/// turns can't reclaim enough because the bulk lives in the kept tail, but
/// collapsing those results to a one-liner (with a re-run hint) drops 100k+
/// tokens at a time. `tool_call_id` + `role` are preserved, so tool-call/result
/// pairing and orphan-sanitization stay intact. Returns the count digested.
pub(crate) fn digest_to_budget(messages: &mut [Message], budget: u64) -> usize {
    if estimate_messages_tokens(messages) <= budget {
        return 0;
    }
    // tool_call_id -> (tool_name, args_json) from assistant tool_calls, so the
    // digest records WHAT was read/run, not just the size.
    let mut call_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for m in messages.iter() {
        if !m.is_assistant() {
            continue;
        }
        if let Some(calls) = m.tool_calls() {
            for tc in calls {
                if tc.id.is_empty() {
                    continue;
                }
                call_map.insert(
                    tc.id.clone(),
                    (tc.function.name.clone(), tc.function.arguments.clone()),
                );
            }
        }
    }
    // Walk oldest-first, collapsing oversized tool results until the budget is
    // met. Recent results are processed last and so stay verbatim when earlier
    // digests already reached the budget.
    let mut changed = 0usize;
    for i in 0..messages.len() {
        if estimate_messages_tokens(messages) <= budget {
            break;
        }
        if !messages[i].is_tool() {
            continue;
        }
        let content = match messages[i].content_text() {
            Some(c) => c,
            None => continue,
        };
        if content.starts_with("[digested:") || content.len() <= DIGEST_MIN_BYTES {
            continue;
        }
        let id = messages[i].tool_call_id().unwrap_or("").to_string();
        let (name, args_json) = call_map.get(&id).cloned().unwrap_or_default();
        let lines = content.lines().count();
        let digest = make_digest(&name, &args_json, content.len(), lines);
        if let Message::Tool {
            ref mut content, ..
        } = messages[i]
        {
            *content = digest;
            changed += 1;
        }
    }
    // Also reclaim huge NON-tool messages: an assistant tool_call.arguments
    // (e.g. a write_file of a large file, whose content lives in the args JSON)
    // is never touched by the tool-result digest above, so a single such
    // message in the kept tail can keep the request oversized → HTTP 400 that
    // repeats every turn. Replace the large payload field with a one-line
    // digest (id/name kept) so the model still sees the call shape.
    changed += digest_oversized_call_args(messages, budget);
    changed
}

/// back to the content: the tool name, its key argument, and the size/line
/// count. The suffix tells the model how to recover the full output.
pub(crate) fn make_digest(tool: &str, args_json: &str, len: usize, lines: usize) -> String {
    let args: Value = serde_json::from_str(args_json).unwrap_or(json!({}));
    let get = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let what = match tool {
        "read_file" => {
            if lines > 0 {
                format!(
                    "read_file {:?} ({} lines, {} bytes)",
                    get("path"),
                    lines,
                    len
                )
            } else {
                format!("read_file {:?} ({} bytes)", get("path"), len)
            }
        }
        "bulk_read" => format!("bulk_read ({} bytes)", len),
        "bash" => format!(
            "bash {:?} ({} bytes)",
            truncate_str(get("command"), 80),
            len
        ),
        "grep" => format!(
            "grep {:?} ({} bytes)",
            truncate_str(get("pattern"), 80),
            len
        ),
        "glob" => format!(
            "glob {:?} ({} bytes)",
            truncate_str(get("pattern"), 80),
            len
        ),
        "diagnostics" => format!("diagnostics ({} bytes)", len),
        other => format!("{} ({} bytes)", other, len),
    };
    let how = if tool == "bash" {
        "re-run if needed"
    } else if tool_cache::ToolOutputCache::is_restorable(tool) {
        "re-run identical call to restore (cached if available)"
    } else {
        "re-run to recover full output"
    };
    format!("[digested: {what} — {how}]")
}

/// Cap a freshly produced tool result before it enters the conversation.
/// Oversized outputs are smart-truncated (head-error salvage + tail) rather than
/// immediately digested to a one-liner — the model still sees useful content on
/// first ingress. Soft digest later collapses stale results further. Full bytes
/// remain in `tool_output_cache` for identical re-calls (themselves restore-capped).
pub(crate) const INGRESS_MAX_BYTES: usize = 24 * 1024;
/// Cap applied when restoring a cached tool output so a re-call after digest
/// cannot re-bloat the conversation with the full payload.
pub(crate) const RESTORE_MAX_BYTES: usize = 16 * 1024;

pub(crate) fn apply_ingress_cap(tool: &str, args_json: &str, output: String) -> String {
    let _ = (tool, args_json); // kept for call-site symmetry / future per-tool caps
    if output.len() <= INGRESS_MAX_BYTES || output.starts_with("[digested:") {
        return output;
    }
    tools::smart_truncate(&output, INGRESS_MAX_BYTES)
}

pub(crate) fn apply_restore_cap(output: &str) -> String {
    if output.len() <= RESTORE_MAX_BYTES {
        return format!("[restored from digest cache]\n{output}");
    }
    let truncated = tools::smart_truncate(output, RESTORE_MAX_BYTES);
    format!(
        "[restored from digest cache — truncated to {RESTORE_MAX_BYTES} bytes; \
         re-call with a narrower offset/limit if you need another slice]\n{truncated}"
    )
}

/// Find an earlier undigested tool result for the same tool name + args JSON.
/// Used to avoid duplicating large read/grep output when the model re-issues
/// an identical call while the prior result is still verbatim in history.
pub(crate) fn find_duplicate_tool_result(
    messages: &[Message],
    tool: &str,
    args_json: &str,
) -> Option<(String, String)> {
    if !tool_cache::ToolOutputCache::is_restorable(tool) {
        return None;
    }
    let mut call_map: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for m in messages {
        if let Some(calls) = m.tool_calls() {
            for tc in calls {
                call_map.insert(
                    tc.id.clone(),
                    (tc.function.name.clone(), tc.function.arguments.clone()),
                );
            }
        }
    }
    for m in messages.iter().rev() {
        if !m.is_tool() {
            continue;
        }
        let id = m.tool_call_id()?.to_string();
        let (name, args) = call_map.get(&id)?;
        if name != tool || args != args_json {
            continue;
        }
        let content = m.content_text()?;
        if content.starts_with("[digested:")
            || content.starts_with("[duplicate of tool_call_id")
            || content.starts_with("[restored from digest cache]")
        {
            continue;
        }
        if content.len() <= DIGEST_MIN_BYTES {
            continue;
        }
        let preview = trunc_chars(content, 400);
        return Some((id, preview));
    }
    None
}

/// Truncate a string to `n` chars at a char boundary, appending an ellipsis.
pub(crate) fn truncate_str(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

pub(crate) fn trunc_chars(s: &str, n: usize) -> String {
    truncate_str(s, n)
}

/// Enable deferred tool schemas for the rest of this session (and this turn's
/// subsequent model rounds). Core tools are always available; `load_tools` is
/// how the model opts into git/fetch/bulk/diagnostics/spawn/etc.

pub(crate) fn token_budget_tail_start(messages: &[Message], context_window: u64) -> usize {
    const MIN_TAIL: usize = 6;
    const TAIL_FRACTION: f32 = 0.25;
    let n = messages.len();
    if n <= MIN_TAIL {
        return 0;
    }
    let budget = ((context_window as f32 * TAIL_FRACTION) as u64).max(6_000);
    let mut acc: u64 = 0;
    let mut start = n;
    for i in (0..n).rev() {
        let t = estimate_message_tokens(&messages[i]);
        // Always keep the most recent MIN_TAIL messages; only enforce the
        // budget on older ones so a single giant tool result can't shrink the
        // tail to nothing.
        if i < n.saturating_sub(MIN_TAIL) && acc + t > budget {
            break;
        }
        acc += t;
        start = i;
    }
    start
}

/// Naive compaction fallback: keep the system prompt + a token-budgeted tail
/// verbatim, drop the middle with a marker. `context_window` sizes the tail
/// (0/unset → the 6k floor). Used when summarization is disabled or unavailable.
/// Hint the system allocator to release freed heap pages back to the OS.
///
/// Rust's default allocator on glibc Linux does NOT eagerly return freed memory:
/// a turn clones the conversation several times (lock-across-await forces
/// clones) and compaction drops the old copies, but the freed bytes stay in
/// malloc's arenas, so RSS creeps up over a long session and never falls back
/// (the "starts at 11M, now 27M" symptom). `malloc_trim(0)` releases the free
/// top-of-arena pages. Called once per turn — negligible vs a multi-second
/// model turn. No-op on non-glibc targets (musl/macOS/Windows return freed
/// memory to the OS far more eagerly on their own).
#[cfg(all(unix, target_env = "gnu"))]
pub(crate) fn trim_heap() {
    extern "C" {
        fn malloc_trim(pad: usize) -> std::os::raw::c_int;
    }
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(unix, target_env = "gnu")))]
pub(crate) fn trim_heap() {}

/// Build a compact manifest of the tool calls made in the turns being
/// dropped by full compaction, so the model knows what it can re-run to restore
/// full outputs from the digest cache (soft-digest stores restorable outputs
/// keyed by tool name + args before collapsing them). Without this, a
/// compacted agent has no idea which calls existed in the dropped middle and
/// cannot re-fetch their results.
///
/// Returns an empty string when there were no tool calls to list.
pub(crate) fn build_compaction_manifest(to_summarize: &[Message]) -> String {
    const MAX_ENTRIES: usize = 30;
    const MAX_CHARS: usize = 1_500;
    const LABEL_CAP: usize = 80;

    fn key_arg(args: &str) -> String {
        let v: serde_json::Value = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        let obj = match v.as_object() {
            Some(o) => o,
            None => return String::new(),
        };
        for f in ["path", "command", "pattern", "query", "task", "url"] {
            if let Some(k) = obj.get(f) {
                let s = match k {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return s.chars().take(60).collect::<String>();
            }
        }
        String::new()
    }

    let mut entries: Vec<String> = Vec::new();
    for m in to_summarize {
        if let Some(calls) = m.tool_calls() {
            for tc in calls {
                if tc.function.name == "finish" {
                    continue;
                }
                let key = key_arg(&tc.function.arguments);
                let label = if key.is_empty() {
                    tc.function.name.clone()
                } else {
                    format!("{}({})", tc.function.name, key)
                };
                let capped: String = label.chars().take(LABEL_CAP).collect();
                entries.push(capped);
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
            }
        }
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    if entries.is_empty() {
        return String::new();
    }
    let mut manifest = String::from(
        "\n\n[Compacted tool calls — re-run any of these to restore its full output from the digest cache]:\n",
    );
    manifest.push_str(&entries.join(", "));
    if manifest.chars().count() > MAX_CHARS {
        let kept: String = manifest.chars().take(MAX_CHARS).collect();
        manifest = format!("{kept}…");
    }
    manifest
}

pub fn compact_conversation(messages: &mut Vec<Message>, context_window: u64) {
    if messages.len() <= 2 {
        return;
    }
    let system = messages[0].clone();
    let tail_start = token_budget_tail_start(messages, context_window).max(1);
    let tail: Vec<Message> = messages[tail_start..].to_vec();
    let mut compacted = vec![system];
    compacted.push(Message::system("[Earlier conversation history was compacted to fit the context window. Tool results from prior turns were dropped.]"));
    compacted.extend(tail);
    // The kept tail can still hold the bulk of the tokens when a few tool
    // results are huge (large file reads, verbose command output). Dropping old
    // turns reclaims nothing there; collapse those oversized results into
    // one-line digests until the conversation fits under half the window.
    let budget = ((context_window as f32) * POST_COMPACT_BUDGET_FRACTION) as u64;
    digest_to_budget(&mut compacted, budget);
    *messages = compacted;
}

/// Compact a conversation by summarizing older turns into one system message,
/// keeping the system prompt + a token-budgeted tail verbatim. Falls back to
/// the naive drop-oldest (`compact_conversation`) when summarization is
/// disabled and not forced, or when there's too little middle to summarize. On
/// summary failure, degrades to a drop-oldest marker so the turn still
/// proceeds. `force_summarize` overrides `summarize_on_compact=false` — used by
/// the 95% hard cap where naive drop-oldest may not reclaim enough.
pub async fn compact_with_summary(
    client: &reqwest::Client,
    cfg: &Config,
    provider: &ResolvedProvider,
    model: &str,
    messages: &mut Vec<Message>,
    cancel: &CancellationToken,
    force_summarize: bool,
    context_window: u64,
    instructions: Option<&str>,
    memory_provider: Option<&plugins::PluginMemoryProviderConfig>,
) -> usize {
    // Returns the character count of the produced summary system message (0
    // when no summary was generated — naive drop-oldest fallback or a
    // failed/too-small summarize). Surfaced on the `compacted` event so the
    // TUI can show how big the rolling summary is.
    if messages.len() <= 2 {
        return 0;
    }
    if !cfg.summarize_on_compact && !force_summarize {
        compact_conversation(messages, context_window);
        return 0;
    }
    let tail_start = token_budget_tail_start(messages, context_window).max(1);
    if tail_start <= 1 {
        compact_conversation(messages, context_window);
        return 0;
    }
    let to_summarize: Vec<Message> = messages[1..tail_start].to_vec();
    let kept: Vec<Message> = messages[tail_start..].to_vec();
    // Pre-digest the middle so the summarize HTTP call itself stays small, then
    // one combined summarize+facts call (avoids a second full pass).
    let mut for_summary = to_summarize.clone();
    // Digest with a throwaway cache so restorable tool outputs are captured
    // before collapse — the summary manifest's "re-run to restore" claim is
    // only honest when something was cached (CORE_REVIEW). Callers that own
    // the live ToolOutputCache should prefer compact paths that pass it;
    // here we at least avoid promising restore from thin air mid-summarize.
    let mut digest_cache = crate::tool_cache::ToolOutputCache::new();
    let _ = soft_digest_conversation(&mut for_summary, context_window, Some(&mut digest_cache));
    let combined = provider::summarize_and_extract(
        client,
        provider,
        model,
        &for_summary,
        cancel,
        instructions,
    )
    .await;
    let mut summary_chars = 0usize;
    let mut compacted = vec![messages[0].clone()];
    if let Some((s, facts)) = combined {
        // Append a compaction manifest: a compact list of the tool calls made in
        // the dropped middle turns, so the model knows what it can re-run to
        // restore full outputs from the digest cache (soft-digest stored the
        // restorable outputs before collapsing them). Without this a compacted
        // agent has no record of which calls existed in the dropped history.
        let manifest = build_compaction_manifest(&to_summarize);
        let content = if manifest.is_empty() {
            format!("[Summary of earlier turns]\n{s}")
        } else {
            format!("[Summary of earlier turns]\n{s}{manifest}")
        };
        summary_chars = content.chars().count();
        compacted.push(Message::system(content));
        // Session memory extraction: persist durable facts so future sessions inherit
        // project knowledge. Best-effort; never blocks compaction. Facts ACCUMULATE
        // across compactions (append, not overwrite) so early-session facts survive,
        // with a rolling byte cap so the file stays bounded.
        if cfg.summarize_on_compact {
            if let Some(facts) = facts {
                if let Some(mp) = memory_provider {
                    let args = json!({
                        "name": "session-extract",
                        "content": facts,
                        "type": "session",
                        "description": "auto-extracted durable facts (accumulated on compaction)",
                        "cap_bytes": 16384,
                    });
                    let _ = plugins::execute_memory_provider(
                        mp,
                        "compact_append",
                        &args,
                        &cfg.workspace.display().to_string(),
                        "",
                    )
                    .await;
                } else {
                    let _ = memory::append_memory(
                        &cfg.workspace,
                        "session-extract",
                        &facts,
                        "session",
                        "auto-extracted durable facts (accumulated on compaction)",
                        16_384,
                    );
                }
            }
        }
    } else {
        compacted.push(Message::system("[Earlier conversation history was compacted to fit the context window. Tool results from prior turns were dropped; summarization was unavailable.]"));
    }
    // The kept tail can still hold the bulk of the tokens when a few recent
    // tool results are huge. Collapse them so the compacted conversation
    // actually fits the window instead of no-op'ing back to its original size.
    compacted.extend(kept);
    let budget = ((context_window as f32) * POST_COMPACT_BUDGET_FRACTION) as u64;
    digest_to_budget(&mut compacted, budget);
    *messages = compacted;
    summary_chars
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_tool_calls_from_dropped_turns() {
        let dropped = vec![
            Message::assistant_tool_calls(vec![ToolCall {
                id: "c1".into(),
                typ: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                },
            }]),
            Message::assistant_tool_calls(vec![ToolCall {
                id: "c2".into(),
                typ: "function".into(),
                function: FunctionCall {
                    name: "grep".into(),
                    arguments: r#"{"pattern":"TODO","head_limit":10}"#.into(),
                },
            }]),
        ];
        let m = build_compaction_manifest(&dropped);
        assert!(m.contains("read_file(src/main.rs)"));
        assert!(m.contains("grep(TODO)"));
        assert!(m.contains("re-run any of these"));
    }

    #[test]
    fn manifest_skips_finish_and_empty_when_no_calls() {
        // Only a finish call → no manifest.
        let dropped = vec![Message::assistant_tool_calls(vec![ToolCall {
            id: "f1".into(),
            typ: "function".into(),
            function: FunctionCall {
                name: "finish".into(),
                arguments: "{}".into(),
            },
        }])];
        let m = build_compaction_manifest(&dropped);
        assert!(m.is_empty());
    }

    #[test]
    fn manifest_caps_entries_and_total_chars() {
        // 50 tool calls → only 30 listed, total capped.
        let mut dropped = Vec::new();
        for i in 0..50 {
            dropped.push(Message::assistant_tool_calls(vec![ToolCall {
                id: format!("c{i}"),
                typ: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: format!(r#"{{"path":"file_{i}.rs"}}"#),
                },
            }]));
        }
        let m = build_compaction_manifest(&dropped);
        // 30 entries max → file_29 present, file_49 absent.
        assert!(m.contains("file_29.rs"));
        assert!(!m.contains("file_49.rs"));
        // Total under a reasonable bound (cap + ellipsis + header).
        assert!(m.chars().count() < 2000);
    }
}
