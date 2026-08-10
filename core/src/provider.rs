// Multi-provider chat client. The internal conversation is always kept in
// OpenAI chat-completions shape (role:"tool", assistant `tool_calls`, ...)
// because every other layer (compaction, sanitization, subagents, session
// persistence) understands that shape. Translation to/from other wire
// protocols (Anthropic Messages API) happens only at the HTTP boundary,// driven by the active `ResolvedProvider`'s `kind`. Streams SSE chunks; emits
// delta/thinking/tool_call events. Retries on transient HTTP errors with
// exponential backoff (honors Retry-After).
use crate::config::{ProviderKind, ResolvedProvider};
use crate::logging::{self, estimate_tokens, TurnTimer};
use crate::message::{self, Message};
#[cfg(test)]
use crate::protocol::ModelInfo;
use crate::protocol::{emit, Event};
use crate::providers::adapter::{
    is_non_retryable_provider_message, is_retryable_http_error, malformed_response,
    ProviderAdapter, ProviderProtocol, ProviderRequest,
};
pub use crate::providers::discovery::*;
use crate::providers::registry::{adapter_for, protocol_for};
use crate::providers::sse::{SseDecoder, SseFrame};
use crate::providers::streaming::{
    append_stream_tool_args, ensure_stream_tool_slot, NormalizedStreamEvent,
};
pub use crate::providers::usage::*;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
pub const DEFAULT_BASE_URL: &str = "https://api.code.umans.ai/v1";
pub(crate) const MODELS_INFO_PATH: &str = "/models/info";
/// Standard OpenAI `/models` list endpoint (first-party OpenAI + Gemini's
/// OpenAI-compatible shim). Used as a fallback when `/models/info` (Umans)
/// isn't served by the endpoint.
pub(crate) const OPENAI_MODELS_PATH: &str = "/models";
const CHAT_PATH: &str = "/chat/completions";
/// Anthropic Messages API requires an `anthropic-version` header.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Identity betas Anthropic's gateway expects for Claude subscription (OAuth)
/// Bearer tokens on the Messages API. Plugin OAuth providers that use Claude
/// Pro/Max should also send matching UA / x-app via plugin headers.
pub(crate) const CLAUDE_OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
pub(crate) const CLAUDE_OAUTH_USER_AGENT: &str = "claude-cli/2.1.160";
pub(crate) const CLAUDE_OAUTH_X_APP: &str = "cli";
/// Anthropic endpoints: `{base_url}/messages` and `{base_url}/models`
/// (base_url conventionally ends in `/v1`, e.g. `https://api.anthropic.com/v1`).
const ANTHROPIC_MESSAGES_PATH: &str = "/messages";
pub(crate) const ANTHROPIC_MODELS_PATH: &str = "/models";

/// True if the base URL points at an Umans endpoint. Umans accepts extra
/// fields (reasoning_effort, reasoning_content replay) that vanilla OpenAI
/// servers reject with a 400 — gate those on this check.
pub fn is_umans(base_url: &str) -> bool {
    // Parse the HOST so a look-alike such as `https://api.umans.ai.evil.com/v1`
    // (host `api.umans.ai.evil.com`) is NOT mistaken for Umans. A naive
    // `contains("umans.ai")` substring match would enable Umans-only wire
    // fields (reasoning_effort / reasoning_content) on the wrong endpoint and
    // trigger 400s. Match `umans.ai` exactly or as a parent domain (subdomain).
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "umans.ai" || host.ends_with(".umans.ai")
}

/// True if the base URL points at a Kimi / Moonshot endpoint
/// (`api.kimi.com` coding subscription or `api.moonshot.ai` platform API).
/// Kimi accepts non-standard fields — top-level `thinking` / `reasoning_effort`
/// and streams `reasoning_content` — that vanilla OpenAI servers reject.
pub fn is_kimi(base_url: &str) -> bool {
    let host = endpoint_host(base_url);
    host == "api.kimi.com" || host == "api.moonshot.ai"
}

/// True if the base URL points at DeepSeek's official API endpoint
/// (`https://api.deepseek.com`). DeepSeek's OpenAI-compatible API accepts the
/// vendor `thinking` and `reasoning_effort` fields and streams
/// `reasoning_content`; those fields are host-gated so ordinary OpenAI
/// compatible endpoints never receive them accidentally.
pub fn is_deepseek(base_url: &str) -> bool {
    endpoint_host(base_url) == "api.deepseek.com"
}

/// True if the base URL points at Zhipu / Z.ai OpenAI-compatible endpoints.
/// Exact hosts only — no suffix matching that would accept lookalikes.
pub fn is_zhipu(base_url: &str) -> bool {
    let host = endpoint_host(base_url);
    host == "api.z.ai" || host == "open.bigmodel.cn" || host == "open.bigmodel.com"
}

/// True if the base URL points at MiniMax Anthropic/OpenAI-compatible hosts.
/// Exact hosts only — no suffix matching that would accept lookalikes.
pub fn is_minimax(base_url: &str) -> bool {
    let host = endpoint_host(base_url);
    host == "api.minimax.io" || host == "api.minimaxi.com"
}

fn endpoint_host(base_url: &str) -> String {
    base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// True only for the loopback Catalyst Cursor SDK sidecar. The dedicated path
/// is intentional: it lets the OpenAI-compatible transport preserve streamed
/// SDK thinking text without enabling non-standard fields for arbitrary local
/// OpenAI servers.
pub fn is_cursor_bridge(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    let loopback = match url.host_str().unwrap_or("").to_ascii_lowercase().as_str() {
        "localhost" | "127.0.0.1" | "::1" | "[::1]" => true,
        _ => false,
    };
    loopback && url.path().trim_end_matches('/') == "/cursor/v1"
}

/// The reasoning levels offered when a model advertises none of its own
/// (and as the fallback set the TUI cycles through).
pub const DEFAULT_THINKING_LEVELS: &[&str] = &["low", "medium", "high"];

/// Resolve a requested reasoning effort against a model's advertised thinking
/// levels. If the model declares no levels (empty slice) the request passes
/// through unchanged. If it does, an unsupported effort is clamped to the
/// closest preferred level (high → medium → low → … → first listed) so the
/// model never receives an effort it can't handle (e.g. GLM only takes "high").
/// Comparison is case-insensitive; the returned string preserves the model's
/// own casing so the wire field matches what the endpoint expects.
pub fn resolve_effort(requested: &str, levels: &[String]) -> String {
    if levels.is_empty() {
        return requested.to_string();
    }
    if let Some(hit) = levels.iter().find(|l| l.eq_ignore_ascii_case(requested)) {
        return hit.clone();
    }
    for pref in ["high", "medium", "low", "xhigh", "max", "minimal", "none"] {
        if let Some(hit) = levels.iter().find(|l| l.eq_ignore_ascii_case(pref)) {
            return hit.clone();
        }
    }
    levels[0].clone()
}

/// Hard cap on a single summarize request's user payload. Larger middles are
/// map-reduced in chunks so the summarize call itself never blows the model
/// context (which used to make compaction fall back to an empty drop marker).
const MAX_SUMMARY_INPUT_CHARS: usize = 100_000;
/// Per-tool-result char budget inside the summarize payload (after digesting
/// oversized results). Keeps path/command signal without re-sending 48KB dumps.
const SUMMARY_TOOL_RESULT_CHARS: usize = 1_500;
/// Max tokens for the combined summary+facts reply.
const SUMMARY_MAX_TOKENS: u32 = 3072;

/// Truncate `s` at a char boundary, appending an ellipsis when cut.
fn trunc_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// Build a compact, image-stripped string of a message for the summarization
/// prompt. Re-serializing a multimodal message verbatim would POST megabytes
/// of base64 image data to the model (costly, and it can blow the summary
/// request's own context); image parts are replaced with a short placeholder.
/// Oversized tool results and write/edit payloads are truncated so a tool-heavy
/// middle can still be summarized instead of failing the HTTP call.
fn message_for_summary(m: &Message) -> String {
    let v: Value = m.into();
    let mut clean = v;
    if let Some(arr) = clean.get_mut("content").and_then(|v| v.as_array_mut()) {
        for part in arr.iter_mut() {
            if part.get("type").and_then(|v| v.as_str()) == Some("image_url") {
                *part = json!({ "type": "text", "text": "[image omitted in summary]" });
            }
        }
    }
    // Truncate large tool-result content strings.
    if clean.get("role").and_then(|r| r.as_str()) == Some("tool") {
        if let Some(c) = clean.get("content").and_then(|c| c.as_str()) {
            if c.len() > SUMMARY_TOOL_RESULT_CHARS {
                let head = trunc_chars(c, SUMMARY_TOOL_RESULT_CHARS / 2);
                let tail = {
                    let chars: Vec<char> = c.chars().collect();
                    let n = SUMMARY_TOOL_RESULT_CHARS / 2;
                    if chars.len() > n {
                        chars[chars.len() - n..].iter().collect::<String>()
                    } else {
                        String::new()
                    }
                };
                clean["content"] = json!(format!(
                    "{head}\n…[truncated {} chars for summary]…\n{tail}",
                    c.len()
                ));
            }
        }
    }
    // Truncate huge tool-call argument payloads (write_file content, etc.).
    if let Some(calls) = clean.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        for tc in calls.iter_mut() {
            if let Some(args) = tc
                .pointer_mut("/function/arguments")
                .and_then(|a| a.as_str().map(|s| s.to_string()))
            {
                if args.len() > SUMMARY_TOOL_RESULT_CHARS {
                    *tc.pointer_mut("/function/arguments").unwrap() =
                        json!(trunc_chars(&args, SUMMARY_TOOL_RESULT_CHARS));
                }
            }
        }
    }
    serde_json::to_string(&clean).unwrap_or_default()
}

/// Serialize messages for a summarize call, then split into char-budgeted chunks
/// so each HTTP request stays under `MAX_SUMMARY_INPUT_CHARS`.
fn summary_payload_chunks(messages: &[Message]) -> Vec<String> {
    let parts: Vec<String> = messages.iter().map(message_for_summary).collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for p in parts {
        if !cur.is_empty() && cur.len() + 1 + p.len() > MAX_SUMMARY_INPUT_CHARS {
            chunks.push(std::mem::take(&mut cur));
        }
        if p.len() > MAX_SUMMARY_INPUT_CHARS {
            // A single message still oversized after truncation — hard-slice it.
            let mut offset = 0;
            let bytes = p.as_bytes();
            while offset < bytes.len() {
                let mut end = (offset + MAX_SUMMARY_INPUT_CHARS).min(bytes.len());
                while end > offset && !p.is_char_boundary(end) {
                    end -= 1;
                }
                if end == offset {
                    break;
                }
                chunks.push(p[offset..end].to_string());
                offset = end;
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(&p);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

fn summary_system_prompt(instructions: Option<&str>) -> String {
    const BASE_SYS: &str = "Summarize the following conversation turns in structured format. Preserve: decisions made, file paths touched, the user's goal, and any unresolved errors.\n\nAlso extract durable project facts worth remembering across future sessions (conventions, structure, key decisions, gotchas). If none, put the single word none under <facts>.\n\nUse this exact format:\n<summary>\n 1. Primary Request and Intent\n 2. Key Technical Concepts\n 3. Files and Code Sections\n 4. Errors and Fixes\n 5. Problem Solving\n 6. All User Messages\n 7. Pending Tasks\n 8. Current Work\n 9. Optional Next Step\n</summary>\n<facts>\n- fact one\n- fact two\n</facts>";
    match instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!(
            "{BASE_SYS}\n\nThe user provided the following guidance for what to preserve in this summary — honor it above the default priorities:\n{extra}"
        ),
        None => BASE_SYS.to_string(),
    }
}

/// Parse a combined summarize+facts reply into `(summary, optional_facts)`.
fn parse_summary_and_facts(raw: &str) -> (String, Option<String>) {
    let trimmed = raw.trim();
    let facts = {
        let lower = trimmed.to_ascii_lowercase();
        if let Some(start) = lower.find("<facts>") {
            let after = start + "<facts>".len();
            let end = lower[after..]
                .find("</facts>")
                .map(|i| after + i)
                .unwrap_or(trimmed.len());
            let body = trimmed[after..end].trim();
            if body.is_empty() || body.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(body.to_string())
            }
        } else {
            None
        }
    };
    let summary = {
        let lower = trimmed.to_ascii_lowercase();
        if let Some(start) = lower.find("<summary>") {
            let after = start + "<summary>".len();
            let end = lower[after..]
                .find("</summary>")
                .map(|i| after + i)
                .unwrap_or_else(|| {
                    lower[after..]
                        .find("<facts>")
                        .map(|i| after + i)
                        .unwrap_or(trimmed.len())
                });
            trimmed[after..end].trim().to_string()
        } else if let Some(facts_at) = lower.find("<facts>") {
            trimmed[..facts_at].trim().to_string()
        } else {
            trimmed.to_string()
        }
    };
    (summary, facts)
}

/// Summarize a slice of messages into one system message. Used by context
/// compaction so dropped turns become a short recap instead of vanishing.
/// Non-streaming, cheap; returns None on any failure (caller keeps the
/// naive drop-oldest fallback). Protocol-agnostic: branches on the provider's
/// `kind` (OpenAI chat-completions vs Anthropic Messages).
///
/// Oversized middles are truncated per-message and map-reduced in chunks so the
/// summarize HTTP call itself rarely fails from context overflow.
#[allow(dead_code)] // convenience wrapper: production uses summarize_and_extract;
                    // retained as API + exercised by the mock tests below
pub async fn summarize(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    messages: &[Message],
    cancel: &CancellationToken,
    instructions: Option<&str>,
) -> Option<String> {
    summarize_and_extract(client, provider, model, messages, cancel, instructions)
        .await
        .map(|(s, _)| s)
}

/// One-shot summarize + durable-fact extraction (single model call). Returns
/// `(summary, facts)` where facts is `None` when the model reported nothing
/// durable. Prefer this over separate `summarize` + `extract_facts` calls.
pub async fn summarize_and_extract(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    messages: &[Message],
    cancel: &CancellationToken,
    instructions: Option<&str>,
) -> Option<(String, Option<String>)> {
    let sys = summary_system_prompt(instructions);
    let chunks = summary_payload_chunks(messages);
    if chunks.len() == 1 {
        let raw = complete_text(
            client,
            provider,
            model,
            &sys,
            &chunks[0],
            SUMMARY_MAX_TOKENS,
            cancel,
        )
        .await?;
        return Some(parse_summary_and_facts(&raw));
    }
    // Map-reduce: summarize each chunk, then merge.
    let mut partials: Vec<String> = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_sys = format!(
            "{sys}\n\nThis is partial chunk {} of {}. Summarize only this chunk; a later merge will combine them.",
            i + 1,
            chunks.len()
        );
        let part = complete_text(
            client,
            provider,
            model,
            &chunk_sys,
            chunk,
            SUMMARY_MAX_TOKENS,
            cancel,
        )
        .await?;
        partials.push(part);
    }
    let merge_user = {
        let joined = partials.join("\n\n---\n\n");
        if joined.len() <= MAX_SUMMARY_INPUT_CHARS {
            joined
        } else {
            // Hierarchical reduce would be nicer; hard-cap keeps the merge call
            // from itself blowing the model context (which used to make compact
            // fall back to an empty drop marker).
            let mut out = String::new();
            for p in &partials {
                if out.len() + p.len() + 8 > MAX_SUMMARY_INPUT_CHARS {
                    break;
                }
                if !out.is_empty() {
                    out.push_str("\n\n---\n\n");
                }
                out.push_str(p);
            }
            if out.is_empty() {
                trunc_chars(&joined, MAX_SUMMARY_INPUT_CHARS)
            } else {
                out
            }
        }
    };
    let merge_sys = format!(
        "{sys}\n\nBelow are partial summaries of earlier conversation chunks. Merge them into one final <summary> and one <facts> block. Deduplicate; prefer later info when they conflict."
    );
    let raw = complete_text(
        client,
        provider,
        model,
        &merge_sys,
        &merge_user,
        SUMMARY_MAX_TOKENS,
        cancel,
    )
    .await?;
    Some(parse_summary_and_facts(&raw))
}

/// Extract durable facts worth remembering across future sessions from a slice of
/// the conversation. Best-effort (returns None on any failure, or if there is
/// nothing durable). Used by the session memory extraction hook on compaction.
/// Prefer [`summarize_and_extract`] when a summary is also needed (one call).
/// Protocol-agnostic: branches on the provider's `kind`.
#[allow(dead_code)] // convenience wrapper over summarize_and_extract; kept for API + tests
pub async fn extract_facts(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    messages: &[Message],
    cancel: &CancellationToken,
) -> Option<String> {
    summarize_and_extract(client, provider, model, messages, cancel, None)
        .await
        .and_then(|(_, facts)| facts)
}

/// One-shot text completion (no tools, no streaming). Returns the model's text
/// reply. Branches on provider kind so callers (summarize/extract_facts) stay
/// protocol-agnostic. `max_tokens` caps the reply (Anthropic requires it;
/// OpenAI servers ignore/apply it tolerantly).
pub(crate) async fn complete_text(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    cancel: &CancellationToken,
) -> Option<String> {
    match provider.kind {
        ProviderKind::OpenAI => {
            openai_complete(client, provider, model, system, user, max_tokens, cancel).await
        }
        ProviderKind::Anthropic => {
            anthropic_complete(client, provider, model, system, user, max_tokens, cancel).await
        }
    }
}

async fn openai_complete(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    cancel: &CancellationToken,
) -> Option<String> {
    let body = json!({
        "model": model,
        "stream": false,
        "max_tokens": max_tokens.max(256),
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    let url = format!("{}{CHAT_PATH}", provider.base_url);
    // Never send `Authorization: Bearer ` with an empty token — some proxies
    // treat that as present-but-invalid and return 401 instead of anonymous.
    let mut req = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(120));
    if let Some(k) = provider.api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    // Honor provider.headers on the non-stream helper too (OpenRouter Referer,
    // custom gateway keys, etc.). Without this, compaction/summary calls drop
    // headers the streaming path always sends.
    for (k, v) in &provider.headers {
        req = req.header(k, v);
    }
    let resp = tokio::select! {
        r = req.send() => r.ok()?,
        _ = cancel.cancelled() => return None,
    };
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
}

async fn anthropic_complete(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    cancel: &CancellationToken,
) -> Option<String> {
    let messages: Vec<Message> = vec![Message::system(system), Message::user(user)];
    // Same floor as the streaming adapter: Anthropic rejects max_tokens:0, and
    // a 1-token floor would silently truncate compact/summary replies.
    let mut body =
        message::build_anthropic_request(&messages, &[], "none", &[], max_tokens.max(256));
    body["model"] = json!(model);
    let url = format!("{}{ANTHROPIC_MESSAGES_PATH}", provider.base_url);
    let mut req = client
        .post(&url)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&body)
        .timeout(Duration::from_secs(120));
    // Mirror send_anthropic_request auth: OAuth subscription tokens need Bearer
    // + the claude-code identity beta; API keys use x-api-key. Never attach an
    // empty credential header (Some("") used to do that and 401'd).
    let key = anthropic_nonempty_key(provider.api_key.as_deref());
    let mut set_primary_auth = false;
    if provider.oauth {
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
            set_primary_auth = true;
        }
        req = req.header("anthropic-beta", CLAUDE_OAUTH_BETA);
    } else if let Some(k) = key {
        req = req.header("x-api-key", k);
        set_primary_auth = true;
    }
    for (k, v) in &provider.headers {
        if set_primary_auth && is_anthropic_primary_auth_header(k) {
            continue;
        }
        req = req.header(k, v);
    }
    let resp = tokio::select! {
        r = req.send() => r.ok()?,
        _ = cancel.cancelled() => return None,
    };
    if !resp.status().is_success() {
        return None;
    }
    let v: Value = resp.json().await.ok()?;
    // content is an array of blocks; return the first text block's text.
    v.get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                (b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .then(|| b.get("text").and_then(|t| t.as_str()).map(String::from))
                    .flatten()
            })
        })
}

/// True when `k` is a primary Anthropic auth header name (case-insensitive).
fn is_anthropic_primary_auth_header(k: &str) -> bool {
    k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-api-key")
}

/// Pick the non-empty trimmed Anthropic credential, if any.
fn anthropic_nonempty_key(api_key: Option<&str>) -> Option<&str> {
    api_key.map(str::trim).filter(|k| !k.is_empty())
}

/// Sanitize orphaned tool_calls: ensure every tool_calls entry has a matching
/// tool result message. Context compaction can drop tool results while keeping
/// the assistant message that made the call, causing a 400. Mirrors the Umans
/// extension's before_provider_request handler.
/// Also verifies that the sanitizer doesn't leave behind a broken conversation
/// (validate that every assistant with tool_calls has corresponding tool results).
#[allow(clippy::ptr_arg)]
pub fn sanitize_orphaned_tool_calls(messages: &mut Vec<Message>) -> usize {
    // Number of fixes applied (orphaned results dropped + synthetic results
    // inserted). Callers persist only when this is non-zero, so clean turns pay
    // just the scan with no session rewrite.
    // All tool_call ids emitted by any assistant message in the kept history.
    let call_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| {
            if m.is_assistant() {
                m.tool_calls()
            } else {
                None
            }
        })
        .flatten()
        .map(|tc| tc.id.clone())
        .collect();

    // All tool_call ids that currently have a matching `role:"tool"` result.
    let result_ids: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|m| {
            if m.is_tool() {
                m.tool_call_id().map(String::from)
            } else {
                None
            }
        })
        .collect();

    // Drop orphaned RESULTS: a `tool` message whose `tool_call_id` is not
    // emitted by any remaining assistant `tool_calls`. Compaction can keep a
    // tool result while dropping (or summarizing) the assistant call that
    // requested it — OpenAI APIs then reject the orphaned `tool` message with a
    // 400 that bricks the turn (and persists into the next). This is the
    // symmetric fix to the orphaned-CALL handling below.
    let before = messages.len();
    messages.retain(|m| {
        if m.is_tool() {
            m.tool_call_id()
                .map(|id| call_ids.contains(id))
                .unwrap_or(false)
        } else {
            true
        }
    });
    let dropped_results = before - messages.len();

    // Insert synthetic results for orphaned CALLS (assistant tool_calls with no
    // matching tool message). Computed against the original result_ids — the
    // retain above only removed results that had no matching call, so the set
    // of calls-with-results is unchanged.
    let orphaned: Vec<String> = call_ids
        .iter()
        .filter(|id| !result_ids.contains(*id))
        .cloned()
        .collect();
    if orphaned.is_empty() {
        return dropped_results;
    }

    // Insert synthetic tool results right after the assistant message that made each call.
    // For `finish`, never tell the model to "re-issue" — that makes the next user
    // turn ignore the new prompt and call finish again. Use the same completion
    // text the live finish path emits.
    let mut inserted = 0;
    let mut i = 0;
    while i < messages.len() {
        let is_assistant_with_calls =
            messages[i].is_assistant() && messages[i].tool_calls().is_some();
        if !is_assistant_with_calls {
            i += 1;
            continue;
        }
        let calls: Vec<(String, String)> = messages[i]
            .tool_calls()
            .unwrap()
            .iter()
            .filter(|tc| orphaned.contains(&tc.id))
            .map(|tc| (tc.id.clone(), tc.function.name.clone()))
            .collect();
        let insert_at = i + 1;
        for (k, (id, name)) in calls.iter().enumerate() {
            let body = if name == "finish" {
                crate::tools::FINISH_MESSAGE
            } else {
                "[tool result was lost — this call did not complete (the turn may have been aborted or its result dropped during context compaction). Re-issue the tool call if still needed.]"
            };
            messages.insert(insert_at + k, Message::tool(id, body));
            inserted += 1;
        }
        i = insert_at + calls.len();
    }
    dropped_results + inserted
}

/// Read a token count from a usage field, tolerating the integer, float, and
/// string encodings different OpenAI-compatible servers emit. `as_u64` alone
/// misses floats (some proxies serialize counts as `100.0`) and quoted numbers,
/// which silently drops the context budget to zero.
/// Sanitize tool-call `arguments`: ensure every assistant tool_call's
/// `arguments` field is a valid JSON string. Some models (notably the GLM
/// family) occasionally emit malformed `arguments` for long, quote-heavy
/// commands wrapped inside `bulk`'s nested JSON. When such a message is
/// replayed in the conversation history, the API rejects the whole request
/// with "Assistant tool call function.arguments must be valid JSON", which
/// then repeats on every subsequent turn and bricks the session. This
/// replaces any malformed `arguments` (and any non-string `arguments`) with
/// the valid string `"{}"` so the history is always API-valid; the matching
/// tool dispatch already returned an actionable error to the model. Returns
/// the number of tool calls fixed.
#[allow(clippy::ptr_arg)]
pub fn sanitize_tool_call_arguments(messages: &mut Vec<Message>) -> usize {
    let mut fixed = 0;
    for m in messages.iter_mut() {
        if !m.is_assistant() {
            continue;
        }
        // Get mutable access to tool_calls via the Message enum
        let calls = match m {
            Message::Assistant {
                tool_calls: Some(ref mut tc),
                ..
            } => tc,
            _ => continue,
        };
        for tc in calls.iter_mut() {
            let malformed = serde_json::from_str::<Value>(&tc.function.arguments).is_err();
            if malformed {
                tc.function.arguments = "{}".to_string();
                fixed += 1;
            }
        }
    }
    fixed
}

#[cfg(test)]
fn token_count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().map(|number| number as u64))
        .or_else(|| value.as_str()?.trim().parse().ok())
}

/// Detect the provider failure mode where a model writes a DSML function call
/// into hidden reasoning instead of returning structured `tool_calls`.
///
/// Keep this deliberately narrow: reasoning is untrusted model output and must
/// never become executable merely because it resembles a tool invocation. The
/// caller uses this only to reject/retry an otherwise empty completion.
fn reasoning_contains_dsml_tool_call(reasoning: &str) -> bool {
    let lower = reasoning.to_ascii_lowercase();
    lower.contains("<｜dsml｜invoke")
        || lower.contains("<|dsml|invoke")
        || lower.contains("dsml｜tool_calls")
        || lower.contains("dsml|tool_calls")
}

/// Add a one-shot recovery instruction without persisting it in conversation
/// history. Inserting a system message at the front is accepted by the broadest
/// set of OpenAI-compatible providers and leaves the original user turn intact.
fn add_structured_tool_call_recovery_instruction(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    messages.insert(
        0,
        json!({
            "role": "system",
            "content": "Protocol recovery: your previous response had no visible assistant content and no structured tool call. Continue the task; do not end with reasoning alone. If a tool is needed, return it through the API's structured tool_calls field. If the task is complete, return a visible final response and use the finish tool when available. Do not write DSML, XML, or tool-call syntax in normal content."
        }),
    );
}

fn parse_dsml_tag_attributes(
    tag: &str,
    expected_kind: &str,
) -> Result<serde_json::Map<String, Value>, String> {
    let Some(mut rest) = tag.strip_prefix(expected_kind) else {
        return Err(format!("expected DSML {expected_kind} tag"));
    };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Err(format!("invalid DSML {expected_kind} tag"));
    }
    let mut attrs = serde_json::Map::new();
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let Some(eq) = rest.find('=') else {
            return Err(format!("invalid DSML {expected_kind} attribute"));
        };
        let key = &rest[..eq];
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("invalid DSML {expected_kind} attribute name"));
        }
        rest = &rest[eq + 1..];
        let Some(quoted) = rest.strip_prefix('"') else {
            return Err(format!("DSML {expected_kind} attributes must be quoted"));
        };
        let Some(end_quote) = quoted.find('"') else {
            return Err(format!("unterminated DSML {expected_kind} attribute"));
        };
        if attrs
            .insert(key.to_string(), json!(&quoted[..end_quote]))
            .is_some()
        {
            return Err(format!("duplicate DSML {expected_kind} attribute '{key}'"));
        }
        rest = &quoted[end_quote + 1..];
    }
    Ok(attrs)
}

fn recovered_dsml_call_id(index: usize) -> String {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("call_dsml_{nanos:x}_{sequence:x}_{index:x}")
}

/// Recover the model's DSML wire format into ordinary, untrusted structured
/// calls. The returned calls still go through the normal JSON parsing, tool
/// implementation validation, approval gates, sandbox, and dispatch path.
fn parse_reasoning_dsml_tool_calls(
    reasoning: &str,
    registered_tools: &[Value],
) -> Result<Option<Vec<ToolAccum>>, String> {
    if !reasoning_contains_dsml_tool_call(reasoning) {
        return Ok(None);
    }

    // Some model templates use ASCII bars while others use full-width bars.
    let normalized = reasoning.replace("|DSML|", "｜DSML｜");
    const WRAPPER_OPEN: &str = "<｜DSML｜tool_calls>";
    const WRAPPER_CLOSE: &str = "</｜DSML｜tool_calls>";
    const INVOKE_OPEN: &str = "<｜DSML｜invoke";
    const INVOKE_CLOSE: &str = "</｜DSML｜invoke>";
    const PARAM_OPEN: &str = "<｜DSML｜parameter";
    const PARAM_CLOSE: &str = "</｜DSML｜parameter>";

    let start = normalized
        .rfind(WRAPPER_OPEN)
        .ok_or_else(|| "missing DSML tool_calls opening tag".to_string())?;
    let after_open = &normalized[start + WRAPPER_OPEN.len()..];
    let close = after_open
        .find(WRAPPER_CLOSE)
        .ok_or_else(|| "missing DSML tool_calls closing tag".to_string())?;
    if !after_open[close + WRAPPER_CLOSE.len()..].trim().is_empty() {
        return Err("unexpected text after DSML tool_calls".into());
    }
    let mut body = &after_open[..close];
    let mut calls = Vec::new();

    while !body.trim_start().is_empty() {
        body = body.trim_start();
        if !body.starts_with(INVOKE_OPEN) {
            return Err("unexpected content inside DSML tool_calls".into());
        }
        let open_end = body
            .find('>')
            .ok_or_else(|| "unterminated DSML invoke tag".to_string())?;
        let invoke_tag = &body["<｜DSML｜".len()..open_end];
        let mut invoke_attrs = parse_dsml_tag_attributes(invoke_tag, "invoke")?;
        if invoke_attrs.len() != 1 {
            return Err("DSML invoke must contain only a name attribute".into());
        }
        let name = invoke_attrs
            .remove("name")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "DSML invoke has no valid tool name".to_string())?;
        let registered = registered_tools.iter().any(|tool| {
            tool.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(name.as_str())
        });
        if !registered {
            return Err(format!("DSML requested unavailable tool '{name}'"));
        }

        let after_invoke_open = &body[open_end + 1..];
        let invoke_close = after_invoke_open
            .find(INVOKE_CLOSE)
            .ok_or_else(|| "missing DSML invoke closing tag".to_string())?;
        let mut params_text = &after_invoke_open[..invoke_close];
        let mut args = serde_json::Map::new();
        while !params_text.trim_start().is_empty() {
            params_text = params_text.trim_start();
            if !params_text.starts_with(PARAM_OPEN) {
                return Err("unexpected content inside DSML invoke".into());
            }
            let param_open_end = params_text
                .find('>')
                .ok_or_else(|| "unterminated DSML parameter tag".to_string())?;
            let param_tag = &params_text["<｜DSML｜".len()..param_open_end];
            let mut param_attrs = parse_dsml_tag_attributes(param_tag, "parameter")?;
            if param_attrs.len() != 2 {
                return Err("DSML parameter requires only name and string attributes".into());
            }
            let param_name = param_attrs
                .remove("name")
                .and_then(|v| v.as_str().map(str::to_string))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "DSML parameter has no valid name".to_string())?;
            let string_mode = param_attrs
                .remove("string")
                .and_then(|v| v.as_str().map(str::to_string))
                .ok_or_else(|| "DSML parameter has no string mode".to_string())?;
            let after_param_open = &params_text[param_open_end + 1..];
            let param_close = after_param_open
                .find(PARAM_CLOSE)
                .ok_or_else(|| "missing DSML parameter closing tag".to_string())?;
            let raw_value = &after_param_open[..param_close];
            let value = match string_mode.as_str() {
                "true" => Value::String(raw_value.to_string()),
                "false" => serde_json::from_str(raw_value)
                    .map_err(|e| format!("invalid JSON in DSML parameter '{param_name}': {e}"))?,
                _ => return Err("DSML parameter string mode must be true or false".into()),
            };
            if args.insert(param_name.clone(), value).is_some() {
                return Err(format!("duplicate DSML parameter '{param_name}'"));
            }
            params_text = &after_param_open[param_close + PARAM_CLOSE.len()..];
        }

        if calls.len() >= 32 {
            return Err("too many recovered DSML tool calls".into());
        }
        calls.push(ToolAccum {
            id: recovered_dsml_call_id(calls.len()),
            name,
            args: Value::Object(args).to_string(),
        });
        body = &after_invoke_open[invoke_close + INVOKE_CLOSE.len()..];
    }
    if calls.is_empty() {
        return Err("DSML tool_calls wrapper contained no invocations".into());
    }
    Ok(Some(calls))
}

/// One streamed assistant turn. Emits `thinking`/`delta`/`tool_call` events as it goes.
/// Retries the initial POST on 429/5xx with exponential backoff (honors Retry-After).
/// Returns the finalized assistant message, finish_reason, and (in/out) token counts.
pub async fn stream_turn(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    idle_timeout_secs: u64,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    prompt_est: u64,
    quiet: bool,
) -> Result<(Value, String, u64, u64, u64), String> {
    timer.begin_provider_call();
    let adapter = adapter_for(provider);
    if !adapter.capabilities().streaming {
        return Err(format!(
            "provider adapter '{}' does not support streaming",
            adapter.id()
        ));
    }
    match protocol_for(provider) {
        ProviderProtocol::GoogleCodeAssist => {
            stream_turn_gemini(
                client,
                provider,
                idle_timeout_secs,
                model,
                messages,
                tools,
                reasoning_effort,
                thinking_levels,
                max_tokens,
                cancel,
                timer,
                prompt_est,
                quiet,
            )
            .await
        }
        ProviderProtocol::CodexResponses => {
            stream_turn_codex(
                client,
                provider,
                idle_timeout_secs,
                model,
                messages,
                tools,
                reasoning_effort,
                thinking_levels,
                max_tokens,
                cancel,
                timer,
                prompt_est,
                quiet,
            )
            .await
        }
        ProviderProtocol::OpenAiChat => {
            stream_turn_openai(
                client,
                provider,
                idle_timeout_secs,
                model,
                messages,
                tools,
                reasoning_effort,
                thinking_levels,
                max_tokens,
                cancel,
                timer,
                prompt_est,
                quiet,
            )
            .await
        }
        ProviderProtocol::AnthropicMessages => {
            stream_turn_anthropic(
                client,
                provider,
                idle_timeout_secs,
                model,
                messages,
                tools,
                reasoning_effort,
                thinking_levels,
                max_tokens,
                cancel,
                timer,
                prompt_est,
                quiet,
            )
            .await
        }
    }
}

async fn stream_turn_codex(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    idle_timeout_secs: u64,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    prompt_est: u64,
    quiet: bool,
) -> Result<(Value, String, u64, u64, u64), String> {
    let api_key = provider.api_key.as_deref().unwrap_or("");
    let adapter = adapter_for(provider);
    let built = adapter.build_request(&ProviderRequest {
        provider,
        model,
        messages,
        tools,
        reasoning_effort,
        thinking_levels,
        // Codex Responses uses max_output_tokens; the adapter omits the field
        // when this is 0 (never puts max_output_tokens:0 on the wire).
        max_tokens,
    })?;
    let body = built.body;
    let url = built.url;
    let resp = send_with_retry(
        client,
        &url,
        api_key,
        &provider.headers,
        &body,
        cancel,
        adapter_for(provider),
    )
    .await?;
    let mut stream = resp.bytes_stream();
    let mut decoder = SseDecoder::default();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<ToolAccum> = Vec::new();
    let mut tokens_in = 0;
    let mut tokens_out = 0;
    let mut cached_tokens = 0;
    let mut finish_reason = String::new();
    let idle = Duration::from_secs(idle_timeout_secs.max(10));
    let mut last_stats: Option<Instant> = None;
    let mut terminal_event = false;

    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(idle, stream.next()) => c.map_err(|_| format!("stream idle timeout ({}s with no data)", idle_timeout_secs))?,
            _ = cancel.cancelled() => return Err("aborted".into()),
        };
        let Some(chunk) = chunk else { break };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let detail = fmt_chain(&error);
                if is_recoverable_truncated_chunked_body(&detail, &decoder, terminal_event) {
                    break;
                }
                return Err(format!("stream read: {detail}"));
            }
        };
        for frame in decoder.push(&chunk) {
            let obj = match frame {
                SseFrame::Json { value, .. } => value,
                SseFrame::Done => {
                    terminal_event = true;
                    continue;
                }
                SseFrame::Malformed { preview, .. } => {
                    return Err(format!("malformed provider SSE frame: {preview}"));
                }
            };
            terminal_event |= is_terminal_stream_object(&obj);
            for event in adapter.decode_stream_event(&obj) {
                terminal_event |= is_terminal_stream_event(&event);
                apply_codex_event(
                    event,
                    &mut content,
                    &mut reasoning,
                    &mut calls,
                    &mut tokens_in,
                    &mut tokens_out,
                    &mut cached_tokens,
                    &mut finish_reason,
                    timer,
                    quiet,
                )?;
            }
            if !quiet && (!content.is_empty() || !reasoning.is_empty()) {
                let now = Instant::now();
                if last_stats
                    .map(|t| now.duration_since(t).as_millis() >= 400)
                    .unwrap_or(true)
                {
                    last_stats = Some(now);
                    let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
                    let live_ctx = prompt_est.saturating_add(est_out);
                    let mut ev = Event::new("metrics")
                        .with("tokens_in", json!(live_ctx))
                        .with("tokens_out", json!(est_out))
                        .with("cached_tokens", json!(cached_tokens));
                    if let Some(ttft) = timer
                        .first_token
                        .map(|t| t.duration_since(timer.start).as_millis() as u64)
                    {
                        ev = ev.with("ttft_ms", json!(ttft));
                    }
                    if let Some(tps) = timer.live_tps_estimate(est_out) {
                        ev = ev.with("tps_est", json!(tps));
                    }
                    emit(&ev);
                }
            }
        }
    }
    for frame in decoder.finish() {
        let value = match frame {
            SseFrame::Json { value, .. } => value,
            SseFrame::Done => continue,
            SseFrame::Malformed { preview, .. } => {
                return Err(format!(
                    "malformed provider SSE frame at disconnect: {preview}"
                ));
            }
        };
        for event in adapter.decode_stream_event(&value) {
            apply_codex_event(
                event,
                &mut content,
                &mut reasoning,
                &mut calls,
                &mut tokens_in,
                &mut tokens_out,
                &mut cached_tokens,
                &mut finish_reason,
                timer,
                quiet,
            )?;
        }
    }
    timer.end_call(
        tokens_out,
        estimate_tokens(&content) + estimate_tokens(&reasoning),
    );
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.args }
            })
        })
        .collect();
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), json!("assistant"));
    if !tool_calls.is_empty() {
        // Keep any streamed prose alongside tool_calls so the next turn's
        // Responses input still has the assistant narration (responses_input
        // now preserves content+calls).
        if content.is_empty() {
            msg.insert("content".into(), Value::Null);
        } else {
            msg.insert("content".into(), json!(content));
        }
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    } else {
        msg.insert("content".into(), json!(content));
    }
    // Prefer an explicit stream finish reason (e.g. length from
    // response.incomplete). Fall back to tool_calls/stop heuristics.
    let reason = if !finish_reason.is_empty() {
        finish_reason
    } else if calls.is_empty() {
        "stop".into()
    } else {
        "tool_calls".into()
    };
    Ok((
        Value::Object(msg),
        reason,
        tokens_in,
        tokens_out,
        cached_tokens,
    ))
}

#[allow(clippy::too_many_arguments)]
fn apply_codex_event(
    event: NormalizedStreamEvent,
    content: &mut String,
    reasoning: &mut String,
    calls: &mut Vec<ToolAccum>,
    tokens_in: &mut u64,
    tokens_out: &mut u64,
    cached_tokens: &mut u64,
    finish_reason: &mut String,
    timer: &mut TurnTimer,
    quiet: bool,
) -> Result<(), String> {
    match event {
        NormalizedStreamEvent::TextDelta(text) => {
            if let Some(delta) = append_stream_fragment(content, &text, false) {
                if content.len() == delta.len() {
                    timer.mark_first_token();
                }
                if !quiet {
                    emit(&Event::new("delta").with("text", json!(delta)));
                }
            }
        }
        NormalizedStreamEvent::ReasoningDelta(text) => {
            if let Some(delta) = append_stream_fragment(reasoning, &text, false) {
                if reasoning.len() == delta.len() {
                    timer.mark_first_token();
                }
                if !quiet {
                    emit(&Event::new("thinking").with("text", json!(delta)));
                }
            }
        }
        NormalizedStreamEvent::ToolCallStart(delta) => {
            timer.mark_first_token();
            if calls.len() >= crate::providers::streaming::MAX_STREAM_TOOL_CALL_INDEX {
                return Err(format!(
                    "stream tool_call count exceeds cap ({})",
                    crate::providers::streaming::MAX_STREAM_TOOL_CALL_INDEX
                ));
            }
            let index = calls.len();
            let args = delta.arguments.unwrap_or_else(|| "{}".into());
            let mut total_args: usize = calls.iter().map(|c| c.args.len()).sum();
            let mut acc_args = String::new();
            append_stream_tool_args(&mut total_args, &mut acc_args, &args)?;
            let call = ToolAccum {
                id: delta.id.unwrap_or_default(),
                name: delta.name.unwrap_or_default(),
                args: acc_args,
            };
            if !quiet {
                emit(
                    &Event::new("tool_call_start")
                        .with("id", json!(call.id))
                        .with("index", json!(index)),
                );
                emit(
                    &Event::new("tool_call_name")
                        .with("index", json!(index))
                        .with("name", json!(call.name)),
                );
                emit(
                    &Event::new("tool_call_args")
                        .with("index", json!(index))
                        .with("args", json!(call.args)),
                );
            }
            calls.push(call);
        }
        NormalizedStreamEvent::Usage {
            input_tokens,
            output_tokens,
            cached_tokens: cached,
        } => {
            if let Some(value) = input_tokens {
                *tokens_in = value;
            }
            if let Some(value) = output_tokens {
                *tokens_out = value;
            }
            if let Some(value) = cached {
                *cached_tokens = value;
            }
        }
        NormalizedStreamEvent::FinishReason(reason) => {
            *finish_reason = reason;
        }
        NormalizedStreamEvent::FatalError(message)
        | NormalizedStreamEvent::RetryableError(message) => return Err(message),
        NormalizedStreamEvent::ToolCallDelta(_)
        | NormalizedStreamEvent::ToolCallComplete { .. } => {}
    }
    Ok(())
}

/// OpenAI-compatible streaming turn. Emits the same delta/thinking/tool_call
/// events and returns the same (assistant_msg, finish_reason, tokens) tuple
/// as the Anthropic path, so the caller is protocol-agnostic.
async fn stream_turn_openai(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    idle_timeout_secs: u64,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    prompt_est: u64,
    quiet: bool,
) -> Result<(Value, String, u64, u64, u64), String> {
    // reasoning_effort + reasoning_content replay are host-gated. Vanilla
    // OpenAI-compatible servers may reject non-standard fields with 400.
    let base_url = &provider.base_url;
    let umans = is_umans(base_url);
    let cursor_bridge = is_cursor_bridge(base_url);
    let kimi = is_kimi(base_url);
    let deepseek = is_deepseek(base_url);
    let zhipu = is_zhipu(base_url);
    let minimax_host = is_minimax(base_url);
    let minimax_model = is_minimax_model(model);
    // MiniMax first-party and proxy gateways both stream cumulative content
    // snapshots (and/or embed <think> tags). De-cumulate the raw wire text
    // before demux so snapshots are not re-parsed as fresh incremental tokens.
    let minimax_cumulative = minimax_host || minimax_model;
    // These hosts/models stream or demux reasoning and need it replayed on
    // subsequent tool turns. MiniMax-by-model covers proxy gateways.
    let supports_reasoning_content =
        umans || cursor_bridge || kimi || deepseek || zhipu || minimax_host || minimax_model;
    let api_key = provider.api_key.as_deref().unwrap_or("");
    let adapter = adapter_for(provider);
    let built = adapter.build_request(&ProviderRequest {
        provider,
        model,
        messages,
        tools,
        reasoning_effort,
        thinking_levels,
        // Pass the model-advertised budget when known. The OpenAI adapter
        // omits the field when this is 0 (never puts max_tokens:0 on the wire).
        max_tokens,
    })?;
    if !quiet {
        for notice in &built.notices {
            emit(&Event::new("info").with("message", json!(notice)));
        }
    }
    let mut body = built.body;
    let url = built.url;
    let tools_sorted = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // ponytail: retry the stream on transient cut-offs. Before the first token
    // we retry any failure. After partial deltas we still retry classified
    // transport/gateway failures (body drop, 5xx/504 stream frames) and ask
    // the TUI to discard the partial so a successful retry cannot duplicate
    // visible output. Non-retryable failures (malformed SSE, auth) stay fatal.
    let max_attempts = 3u32;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut think_demux = ThinkTagDemux::default();
    // Raw cumulative content from the provider, before <think> demux. Only
    // advanced when minimax_cumulative; otherwise unused.
    let mut wire_text = String::new();
    let mut tool_calls: Vec<ToolAccum> = Vec::new();
    let mut tool_args_bytes: usize = 0;
    let mut finish_reason = String::new();
    let mut tokens_in: u64 = 0;
    let mut tokens_out: u64 = 0;
    // ponytail: cached_tokens comes from usage.prompt_tokens_details.cached_tokens
    // (OpenAI/Z.AI implicit prefix caching). Surfaced so the harness can confirm
    // prefix-cache hits and diagnose busts — the request shape is already stable,
    // this just makes the hit visible.
    let mut cached_tokens: u64 = 0;
    // A malformed reasoning-only DSML call is an upstream protocol violation,
    // not an executable tool call. Retry it once with an explicit wire-format
    // reminder; a repeated violation becomes a visible error instead of a
    // silent, content-empty assistant completion.
    let mut protocol_retried = false;
    // Per-chunk idle timeout: if no bytes arrive for this long mid-stream, abort.
    // Configurable because reasoning models can think >60s before the first token.
    let idle = Duration::from_secs(idle_timeout_secs.max(10));

    // Live stats: the prompt's token count drives the footer's context budget
    // while output streams in (the real `usage` chunk at stream end then
    // overwrites it with exact values). The caller passes the best pre-stream
    // estimate — grounded on the endpoint's last real `prompt_tokens` when one
    // is available, else a char/4 of the whole prompt — so the live percentage
    // tracks reality instead of a whole-conversation char/4 guess.
    let est_prompt = prompt_est;
    let mut last_stats: Option<Instant> = None;

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = send_with_retry(
            client,
            &url,
            api_key,
            &provider.headers,
            &body,
            cancel,
            adapter,
        )
        .await?;
        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut emitted = false;
        let mut terminal_event = false;
        let mut err: Option<String> = None;

        'read_stream: loop {
            let chunk = tokio::select! {
                c = tokio::time::timeout(idle, stream.next()) => match c {
                    Ok(x) => x,
                    Err(_) => { err = Some(format!("stream idle timeout ({}s with no data)", idle_timeout_secs)); break; }
                },
                _ = cancel.cancelled() => return Err("aborted".into()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let detail = fmt_chain(&e);
                    if !is_recoverable_truncated_chunked_body(&detail, &decoder, terminal_event) {
                        err = Some(format!("stream read: {detail}"));
                    }
                    break;
                }
            };
            for frame in decoder.push(&chunk) {
                let obj = match frame {
                    SseFrame::Json { value, .. } => value,
                    SseFrame::Done => {
                        terminal_event = true;
                        continue;
                    }
                    SseFrame::Malformed { preview, .. } => {
                        err = Some(malformed_response(&preview).message);
                        break 'read_stream;
                    }
                };
                terminal_event |= is_terminal_stream_object(&obj);

                for event in adapter.decode_stream_event(&obj) {
                    terminal_event |= is_terminal_stream_event(&event);
                    match event {
                        NormalizedStreamEvent::TextDelta(text) => {
                            // 1) De-cumulate raw wire text for MiniMax/proxies.
                            // 2) Demux <think>…</think> into thinking vs content.
                            // 3) After demux, pieces are pure increments.
                            let to_demux = if minimax_cumulative {
                                match append_stream_fragment(&mut wire_text, &text, true) {
                                    Some(suffix) => suffix,
                                    None => continue,
                                }
                            } else {
                                text
                            };
                            let pieces = think_demux.push(&to_demux);
                            for piece in pieces {
                                match piece {
                                    ThinkPiece::Text(t) => {
                                        if let Some(delta) =
                                            append_stream_fragment(&mut content, &t, false)
                                        {
                                            if content.len() == delta.len() {
                                                timer.mark_first_token();
                                            }
                                            if !quiet {
                                                emitted = true;
                                                emit(
                                                    &Event::new("delta").with("text", json!(delta)),
                                                );
                                            }
                                        }
                                    }
                                    ThinkPiece::Thinking(t) => {
                                        if let Some(delta) =
                                            append_stream_fragment(&mut reasoning, &t, false)
                                        {
                                            if reasoning.len() == delta.len() {
                                                timer.mark_first_token();
                                            }
                                            if !quiet {
                                                emitted = true;
                                                emit(
                                                    &Event::new("thinking")
                                                        .with("text", json!(delta)),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        NormalizedStreamEvent::ReasoningDelta(text) => {
                            if let Some(delta) =
                                append_stream_fragment(&mut reasoning, &text, minimax_cumulative)
                            {
                                if reasoning.len() == delta.len() {
                                    timer.mark_first_token();
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(&Event::new("thinking").with("text", json!(delta)));
                                }
                            }
                        }
                        NormalizedStreamEvent::ToolCallStart(delta)
                        | NormalizedStreamEvent::ToolCallDelta(delta) => {
                            timer.mark_first_token();
                            let idx = delta.index;
                            // H3: bound index growth + total args bytes so a
                            // malicious high index cannot allocate multi-GB.
                            if let Err(e) = ensure_stream_tool_slot(&mut tool_calls, idx) {
                                err = Some(e);
                                break 'read_stream;
                            }
                            let acc = &mut tool_calls[idx];
                            if let Some(id) = delta.id {
                                if acc.id.is_empty() {
                                    acc.id.clone_from(&id);
                                    if !quiet {
                                        emitted = true;
                                        emit(
                                            &Event::new("tool_call_start")
                                                .with("id", json!(id))
                                                .with("index", json!(idx)),
                                        );
                                    }
                                }
                            }
                            if let Some(name) = delta.name {
                                if acc.name.is_empty() {
                                    acc.name.clone_from(&name);
                                    if !quiet {
                                        emitted = true;
                                        emit(
                                            &Event::new("tool_call_name")
                                                .with("index", json!(idx))
                                                .with("name", json!(name)),
                                        );
                                    }
                                }
                            }
                            if let Some(arguments) = delta.arguments {
                                if let Err(e) = append_stream_tool_args(
                                    &mut tool_args_bytes,
                                    &mut acc.args,
                                    &arguments,
                                ) {
                                    err = Some(e);
                                    break 'read_stream;
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(
                                        &Event::new("tool_call_args")
                                            .with("index", json!(idx))
                                            .with("args", json!(arguments)),
                                    );
                                }
                            }
                        }
                        NormalizedStreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens: cached,
                        } => {
                            if let Some(value) = input_tokens {
                                tokens_in = value;
                            }
                            if let Some(value) = output_tokens {
                                tokens_out = value;
                            }
                            if let Some(value) = cached {
                                cached_tokens = value;
                            }
                        }
                        NormalizedStreamEvent::FinishReason(reason) => finish_reason = reason,
                        NormalizedStreamEvent::ToolCallComplete { .. } => {}
                        NormalizedStreamEvent::RetryableError(detail)
                        | NormalizedStreamEvent::FatalError(detail) => {
                            err = Some(format!("provider stream error: {detail}"));
                            break 'read_stream;
                        }
                    }
                }
            }

            // Logical completion: terminal frame observed AND nothing left
            // buffered in the decoder. Without this gate, providers that emit
            // `finish_reason:stop` + `usage` but never send `[DONE]` and hold the
            // connection open (e.g. mmx/MiniMax-M3 over 9router) make catcode
            // sit on the per-chunk idle timeout (10s+) which the user reads as
            // "infinitely loading". Break early so the stream resolves in
            // <100ms after the terminal chunk. `decoder.finish()` drains any
            // half-line into a final frame before we exit.
            if stream_logically_complete(terminal_event, &decoder) {
                for frame in decoder.finish() {
                    let obj = match frame {
                        SseFrame::Json { value, .. } => value,
                        SseFrame::Done => continue,
                        SseFrame::Malformed { preview, .. } => {
                            err = Some(malformed_response(&preview).message);
                            break 'read_stream;
                        }
                    };
                    for event in adapter.decode_stream_event(&obj) {
                        match event {
                            NormalizedStreamEvent::Usage { .. } => {}
                            NormalizedStreamEvent::FinishReason(reason) => {
                                if finish_reason.is_empty() {
                                    finish_reason = reason;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                break;
            }

            // Live footer stats: emit a metrics event at most every ~400ms so the
            // TUI's context + approximate in-flight TPS move during the turn.
            // `tps_est` is explicitly marked approximate by the TUI; the final
            // `tps` still uses provider-reported usage only.
            if !quiet && (!content.is_empty() || !reasoning.is_empty()) {
                let now = Instant::now();
                let due = last_stats
                    .map(|t| now.duration_since(t) >= Duration::from_millis(400))
                    .unwrap_or(true);
                if due {
                    last_stats = Some(now);
                    let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
                    let live_ctx = est_prompt.saturating_add(est_out);
                    let mut ev = Event::new("metrics")
                        .with("tokens_in", json!(live_ctx))
                        .with("tokens_out", json!(est_out));
                    if let Some(ttft) = timer
                        .first_token
                        .map(|t| t.duration_since(timer.start).as_millis() as u64)
                    {
                        ev = ev.with("ttft_ms", json!(ttft));
                    }
                    if let Some(tps) = timer.live_tps_estimate(est_out) {
                        ev = ev.with("tps_est", json!(tps));
                    }
                    emit(&ev);
                }
            }
        }

        if err.is_none() {
            if content.trim().is_empty() && tool_calls.is_empty() {
                let protocol_issue = match parse_reasoning_dsml_tool_calls(
                    &reasoning,
                    &tools_sorted,
                ) {
                    Ok(Some(recovered)) => {
                        tool_calls = recovered;
                        finish_reason = "tool_calls".into();
                        // Do not replay the malformed channel content on the
                        // next provider request. The recovered structured call
                        // is the canonical persisted representation.
                        reasoning.clear();
                        if !quiet {
                            emit(&Event::new("info").with(
                                "message",
                                json!("recovered a provider tool call from reasoning DSML; applying normal validation and approval checks"),
                            ));
                        }
                        None
                    }
                    Ok(None) if reasoning.trim().is_empty() => {
                        Some("an entirely empty completion".to_string())
                    }
                    Ok(None) => Some("a reasoning-only completion".to_string()),
                    Err(parse_error) => Some(format!(
                        "invalid reasoning DSML that could not be validated: {parse_error}"
                    )),
                };
                if let Some(issue) = protocol_issue {
                    if protocol_retried {
                        return Err(format!(
                            "provider returned {issue}; retry also produced no usable assistant response"
                        ));
                    }
                    protocol_retried = true;
                    if !quiet {
                        emit(&Event::new("info").with(
                            "message",
                            json!(format!(
                                "provider returned {issue}; retrying once and requiring either a structured tool call or visible response"
                            )),
                        ));
                    }
                    add_structured_tool_call_recovery_instruction(&mut body);
                    content.clear();
                    wire_text.clear();
                    think_demux = ThinkTagDemux::default();
                    reasoning.clear();
                    tool_calls.clear();
                    tool_args_bytes = 0;
                    finish_reason.clear();
                    tokens_in = 0;
                    tokens_out = 0;
                    cached_tokens = 0;
                    timer.call_first_token = None;
                    continue;
                }
            }
            break; // stream completed cleanly
        }
        let msg = err.unwrap();
        // Retry transient stream failures even after partial tokens: the TUI
        // discards the partial on `discard_partial` so a successful retry cannot
        // duplicate visible output. Non-retryable failures (malformed SSE,
        // auth) still fail immediately.
        if !should_retry_stream_attempt(&msg, emitted, attempt, max_attempts) {
            return Err(msg);
        }
        let backoff = backoff_ms(attempt, None);
        let reason = if emitted {
            "retryable stream error after partial output"
        } else {
            "stream error before first token"
        };
        emit_stream_retry(attempt, reason, backoff, emitted);
        // Reset accumulators for the fresh attempt.
        content.clear();
        reasoning.clear();
        wire_text.clear();
        think_demux = ThinkTagDemux::default();
        tool_calls.clear();
        tool_args_bytes = 0;
        finish_reason.clear();
        tokens_in = 0;
        tokens_out = 0;
        cached_tokens = 0;
        timer.call_first_token = None;
        sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
    }

    // Flush any partial think-tag held across the last chunk.
    for piece in think_demux.finish() {
        match piece {
            ThinkPiece::Text(t) => {
                let _ = append_stream_fragment(&mut content, &t, false);
            }
            ThinkPiece::Thinking(t) => {
                let _ = append_stream_fragment(&mut reasoning, &t, false);
            }
        }
    }
    // Belt-and-suspenders: drop any residual think tags that escaped demux
    // (cumulative edge cases / model-emitted bare closers).
    sanitize_assistant_content(&mut content);

    // Fold this call's generation time + output tokens into the turn totals so
    // finalize() computes TPS over generation time only (excluding tool-call
    // wait and prefill). est_out is the char/4 fallback numerator when the
    // endpoint omits usage.
    let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
    timer.end_call(tokens_out, est_out);

    // Build the assistant message. OpenAI requires content null when tool_calls
    // present and empty. reasoning_content is retained for hosts/models that
    // need multi-turn thinking replay (incl. MiniMax-by-model on proxies).
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if content.is_empty() {
            Value::Null
        } else {
            json!(content)
        },
    );
    if supports_reasoning_content && !reasoning.is_empty() {
        msg.insert("reasoning_content".into(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        let arr: Vec<Value> = tool_calls
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "arguments": if t.args.is_empty() { "{}".to_string() } else { t.args.clone() }
                    }
                })
            })
            .collect();
        msg.insert("tool_calls".into(), json!(arr));
    }

    Ok((
        Value::Object(msg),
        finish_reason,
        tokens_in,
        tokens_out,
        cached_tokens,
    ))
}

// ===========================================================================
// Antigravity / Code Assist API (daily-cloudcode-pa / cloudcode-pa)
// ===========================================================================
//
// When a user signs in via the Antigravity OAuth flow, the OAuth token is for
// the Code Assist / Antigravity gateway — NOT for generativelanguage.googleapis.com
// (which only accepts API keys). The gateway uses the native Google GenAI wire
// format (not OpenAI-compatible), so we need our own message converter, request
// builder, and SSE response parser. Gemini 3 + Claude-via-Antigravity ride the
// same path.

/// Stream a turn through the Antigravity / Code Assist API (native GenAI
/// wire format). This is the OAuth path for Gemini 3 + Claude-via-Antigravity
/// — `generativelanguage.googleapis.com` only accepts API keys; the OAuth
/// token authenticates against the daily/prod `cloudcode-pa` gateways.
async fn stream_turn_gemini(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    idle_timeout_secs: u64,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    prompt_est: u64,
    quiet: bool,
) -> Result<(Value, String, u64, u64, u64), String> {
    let api_key = provider.api_key.as_deref().unwrap_or("");
    let max_attempts = 3u32;
    let adapter = adapter_for(provider);
    // Pass model-advertised thinking levels so the Google adapter can clamp
    // thinkingLevel/thinkingBudget (gemini-3 pro only accepts low|high).
    let built = adapter.build_request(&ProviderRequest {
        provider,
        model,
        messages,
        tools,
        reasoning_effort,
        thinking_levels,
        max_tokens,
    })?;
    if !quiet {
        for notice in &built.notices {
            emit(&Event::new("info").with("message", json!(notice)));
        }
    }
    let request = built.body;
    let url = built.url;
    let idle = Duration::from_secs(idle_timeout_secs.max(5));
    let est_prompt = prompt_est;
    let mut last_stats: Option<Instant> = None;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut genai_tool_calls: Vec<(String, Value)> = Vec::new(); // (name, args)
    let mut finish_reason = String::new();
    let mut tokens_in: u64 = 0;
    let mut tokens_out: u64 = 0;
    let mut cached_tokens: u64 = 0;
    let mut emitted = false;

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = send_with_retry(
            client,
            &url,
            api_key,
            &provider.headers,
            &request,
            cancel,
            adapter_for(provider),
        )
        .await?;
        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut terminal_event = false;
        let mut err: Option<String> = None;

        'read_stream: loop {
            let chunk = tokio::select! {
                c = tokio::time::timeout(idle, stream.next()) => match c {
                    Ok(x) => x,
                    Err(_) => { err = Some(format!("stream idle timeout ({}s with no data)", idle_timeout_secs)); break; }
                },
                _ = cancel.cancelled() => return Err("aborted".into()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let detail = fmt_chain(&e);
                    if !is_recoverable_truncated_chunked_body(&detail, &decoder, terminal_event) {
                        err = Some(format!("stream read: {detail}"));
                    }
                    break;
                }
            };
            for frame in decoder.push(&chunk) {
                let obj = match frame {
                    SseFrame::Json { value, .. } => value,
                    SseFrame::Done => {
                        terminal_event = true;
                        continue;
                    }
                    SseFrame::Malformed { preview, .. } => {
                        err = Some(malformed_response(&preview).message);
                        break 'read_stream;
                    }
                };
                terminal_event |= is_terminal_stream_object(&obj);

                for event in adapter.decode_stream_event(&obj) {
                    terminal_event |= is_terminal_stream_event(&event);
                    match event {
                        NormalizedStreamEvent::TextDelta(text) => {
                            if let Some(delta) = append_stream_fragment(&mut content, &text, false)
                            {
                                if content.len() == delta.len() {
                                    timer.mark_first_token();
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(&Event::new("delta").with("text", json!(delta)));
                                }
                            }
                        }
                        NormalizedStreamEvent::ReasoningDelta(text) => {
                            if let Some(delta) =
                                append_stream_fragment(&mut reasoning, &text, false)
                            {
                                if reasoning.len() == delta.len() {
                                    timer.mark_first_token();
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(&Event::new("thinking").with("text", json!(delta)));
                                }
                            }
                        }
                        NormalizedStreamEvent::ToolCallStart(delta) => {
                            timer.mark_first_token();
                            let name = delta.name.unwrap_or_default();
                            let args_raw = delta.arguments.unwrap_or_else(|| "{}".into());
                            if genai_tool_calls.len()
                                >= crate::providers::streaming::MAX_STREAM_TOOL_CALL_INDEX
                            {
                                err = Some(format!(
                                    "stream tool_call count exceeds cap ({})",
                                    crate::providers::streaming::MAX_STREAM_TOOL_CALL_INDEX
                                ));
                                break 'read_stream;
                            }
                            let mut total_args: usize = genai_tool_calls
                                .iter()
                                .map(|(_, a)| a.to_string().len())
                                .sum();
                            let mut acc = String::new();
                            if let Err(e) =
                                append_stream_tool_args(&mut total_args, &mut acc, &args_raw)
                            {
                                err = Some(e);
                                break 'read_stream;
                            }
                            let args: Value =
                                serde_json::from_str(&acc).unwrap_or_else(|_| json!({}));
                            let index = genai_tool_calls.len();
                            genai_tool_calls.push((name.clone(), args.clone()));
                            if !quiet {
                                emitted = true;
                                emit(
                                    &Event::new("tool_call_name")
                                        .with("index", json!(index))
                                        .with("name", json!(name)),
                                );
                                emit(
                                    &Event::new("tool_call_args")
                                        .with("index", json!(index))
                                        .with("args", json!(args.to_string())),
                                );
                            }
                        }
                        NormalizedStreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens: cached,
                        } => {
                            if let Some(value) = input_tokens {
                                tokens_in = value;
                            }
                            if let Some(value) = output_tokens {
                                tokens_out = value;
                            }
                            if let Some(value) = cached {
                                cached_tokens = value;
                            }
                        }
                        NormalizedStreamEvent::FinishReason(reason) => finish_reason = reason,
                        NormalizedStreamEvent::FatalError(message) => {
                            err = Some(message);
                            break;
                        }
                        NormalizedStreamEvent::RetryableError(message) => {
                            err = Some(message);
                            break;
                        }
                        NormalizedStreamEvent::ToolCallDelta(_)
                        | NormalizedStreamEvent::ToolCallComplete { .. } => {}
                    }
                }

                // Same early-break as the OpenAI path: a provider that sends
                // `message_stop` / `response.completed` and holds the connection
                // open (no further events) would otherwise wait for the idle
                // timeout. Drain any buffered half-line and exit.
                if stream_logically_complete(terminal_event, &decoder) {
                    for frame in decoder.finish() {
                        let obj = match frame {
                            SseFrame::Json { value, .. } => value,
                            SseFrame::Done => continue,
                            SseFrame::Malformed { preview, .. } => {
                                err = Some(malformed_response(&preview).message);
                                break;
                            }
                        };
                        for event in adapter.decode_stream_event(&obj) {
                            if let NormalizedStreamEvent::FinishReason(reason) = event {
                                if finish_reason.is_empty() {
                                    finish_reason = reason;
                                }
                            }
                        }
                    }
                    break 'read_stream;
                }

                // Live footer stats.
                if !quiet && (!content.is_empty() || !reasoning.is_empty()) {
                    let now = Instant::now();
                    let due = last_stats
                        .map(|t| now.duration_since(t) >= Duration::from_millis(400))
                        .unwrap_or(true);
                    if due {
                        last_stats = Some(now);
                        let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
                        let live_ctx = est_prompt.saturating_add(est_out);
                        let mut ev = Event::new("metrics")
                            .with("tokens_in", json!(live_ctx))
                            .with("tokens_out", json!(est_out));
                        if let Some(ttft) = timer
                            .first_token
                            .map(|t| t.duration_since(timer.start).as_millis() as u64)
                        {
                            ev = ev.with("ttft_ms", json!(ttft));
                        }
                        if let Some(tps) = timer.live_tps_estimate(est_out) {
                            ev = ev.with("tps_est", json!(tps));
                        }
                        emit(&ev);
                    }
                }
            }
        }

        if err.is_none() {
            break;
        }
        let msg = err.unwrap();
        if !should_retry_stream_attempt(&msg, emitted, attempt, max_attempts) {
            return Err(msg);
        }
        let backoff = backoff_ms(attempt, None);
        let reason = if emitted {
            "retryable stream error after partial output"
        } else {
            "stream error before first token"
        };
        emit_stream_retry(attempt, reason, backoff, emitted);
        content.clear();
        reasoning.clear();
        genai_tool_calls.clear();
        finish_reason.clear();
        tokens_in = 0;
        tokens_out = 0;
        cached_tokens = 0;
        // Gemini path tracks emitted outside the attempt loop; reset so a
        // successful retry starts clean and a second failure reclassifies.
        emitted = false;
        timer.call_first_token = None;
        sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
    }

    let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
    timer.end_call(tokens_out, est_out);

    // Build the assistant message in OpenAI shape (the rest of the harness
    // expects OpenAI-format messages).
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if content.is_empty() {
            Value::Null
        } else {
            json!(content)
        },
    );
    if !reasoning.is_empty() {
        msg.insert("reasoning_content".into(), json!(reasoning));
    }
    if !genai_tool_calls.is_empty() {
        let arr: Vec<Value> = genai_tool_calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| {
                json!({
                    "id": format!("call_{i}"),
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": args.to_string(),
                    }
                })
            })
            .collect();
        msg.insert("tool_calls".into(), json!(arr));
    }

    // Map GenAI finish reasons to OpenAI finish reasons.
    let finish = match finish_reason.as_str() {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" => "content_filter",
        _ => "stop",
    };

    Ok((
        Value::Object(msg),
        finish.to_string(),
        tokens_in,
        tokens_out,
        cached_tokens,
    ))
}

/// HMAC-SHA256 (RFC 2104) over `payload` with `key`, hex-encoded.
/// Implemented with sha2 so we don't need an extra `hmac` crate.
fn hmac_sha256_hex(key: &[u8], payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(BLOCK, 0);
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(payload);
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner.finalize());
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// iFlow chat requests require per-request HMAC headers (session-id,
/// x-iflow-timestamp, x-iflow-signature) matching 9router's IFlowExecutor.
fn iflow_signed_headers(api_key: &str, headers: &[(String, String)]) -> Vec<(String, String)> {
    use rand::RngCore;
    let mut uuid = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut uuid);
    // RFC 4122 variant bits for a random UUID (v4-ish).
    uuid[6] = (uuid[6] & 0x0f) | 0x40;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    let session_id = format!(
        "session-{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5], uuid[6], uuid[7],
        uuid[8], uuid[9], uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15]
    );
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let user_agent = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("iFlow-Cli");
    let payload = format!("{user_agent}:{session_id}:{timestamp}");
    let signature = hmac_sha256_hex(api_key.as_bytes(), payload.as_bytes());
    let mut out = headers.to_vec();
    // Drop any stale signature headers so retries re-sign cleanly.
    out.retain(|(k, _)| {
        let kl = k.to_ascii_lowercase();
        kl != "session-id" && kl != "x-iflow-timestamp" && kl != "x-iflow-signature"
    });
    out.push(("session-id".into(), session_id));
    out.push(("x-iflow-timestamp".into(), timestamp.to_string()));
    out.push(("x-iflow-signature".into(), signature));
    out
}

/// POST with retry on transient provider errors (5xx, 408, transport, etc.).
/// Rate limits (429) and balance/billing failures fail fast. Exponential
/// backoff: 0.5s, 1s, 2s, 4s (cap 8s), honoring Retry-After if present. Up to
/// 4 attempts. Cancellation-aware.
async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    headers: &[(String, String)],
    body: &Value,
    cancel: &CancellationToken,
    adapter: &dyn ProviderAdapter,
) -> Result<reqwest::Response, String> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // ponytail: no total .timeout() here. It's a *total* timeout covering
        // connect+headers+the entire body read, so a reasoning turn (GLM @ high)
        // that streams >5 min gets aborted mid-stream with "operation timed out".
        // Stalls are caught by connect_timeout (connect phase, on the client) +
        // the per-chunk idle timeout in stream_turn (body phase).
        // iFlow requires a fresh HMAC signature on every request (9router
        // IFlowExecutor). Re-sign on each attempt so retries stay valid.
        let signed: Vec<(String, String)>;
        let headers = if is_iflow_endpoint(url) {
            signed = iflow_signed_headers(api_key, headers);
            signed.as_slice()
        } else {
            headers
        };
        // Ask gateways and reverse proxies to pass SSE through as it arrives.
        // `identity` is especially important for compatibility endpoints:
        // compression middleware commonly buffers several small token events
        // before producing one compressed output chunk, which looks like fake
        // streaming to callers even though the upstream is emitting deltas.
        if logging::debug_verbose() {
            // Header names only — never log bearer tokens / api keys.
            let header_names: Vec<String> = headers.iter().map(|(k, _)| k.clone()).collect();
            logging::global_log(
                "http_request",
                json!({
                    "attempt": attempt,
                    "method": "POST",
                    "url": url,
                    "header_names": header_names,
                    "has_bearer": !api_key.is_empty(),
                    "body": logging::truncate_json_for_log(body, logging::VERBOSE_PAYLOAD_CAP),
                }),
            );
        }

        let mut req = client
            .post(url)
            .header("accept", "text/event-stream")
            .header("accept-encoding", "identity")
            .header("cache-control", "no-cache")
            .json(body);
        // Only attach Bearer when we have a real key. An empty `bearer_auth("")`
        // still emits `Authorization: Bearer `, which confuses gateways.
        let has_bearer = !api_key.is_empty();
        if has_bearer {
            req = req.bearer_auth(api_key);
        }
        for (k, v) in headers {
            // If we already set Authorization from api_key, drop a duplicate
            // Authorization from provider.headers (plugin/config mis-merge).
            if has_bearer && k.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(k, v);
        }

        let resp = tokio::select! {
            r = req.send() => r,
            _ = cancel.cancelled() => return Err("aborted".into()),
        };

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                // Transport error: retry with backoff.
                if logging::debug_verbose() {
                    logging::global_log(
                        "http_error",
                        json!({
                            "attempt": attempt,
                            "url": url,
                            "error": fmt_chain(&e),
                            "kind": "transport",
                        }),
                    );
                }
                if attempt >= 4 {
                    let normalized = adapter.normalize_error(None, &fmt_chain(&e));
                    return Err(format!(
                        "request failed after {attempt} attempts: {}",
                        normalized.message
                    ));
                }
                let backoff = backoff_ms(attempt, None);
                emit(
                    &Event::new("http_retry")
                        .with("attempt", json!(attempt))
                        .with("reason", json!("transport error"))
                        .with("backoff_ms", json!(backoff)),
                );
                sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            if logging::debug_verbose() {
                logging::global_log(
                    "http_response",
                    json!({
                        "attempt": attempt,
                        "url": url,
                        "status": status.as_u16(),
                        "ok": true,
                        // Streaming body is not buffered here; provider_request
                        // metrics cover the call. Log status + content-type only.
                        "content_type": resp
                            .headers()
                            .get(reqwest::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or(""),
                    }),
                );
            }
            return Ok(resp);
        }

        // Read the body first so balance/billing wording can fail fast even on
        // statuses that look transient. Policy: retry provider errors by default;
        // only rate limits and balance/billing (plus permanent client 4xx) stop us.
        let retry_after_ms = resp
            .headers()
            .get("retry-after-ms")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after_ms)
            .or_else(|| {
                resp.headers()
                    .get("x-ratelimit-reset-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after_seconds)
                    .map(|seconds| seconds.saturating_mul(1000))
            })
            .or_else(|| {
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after)
                    .map(|seconds| seconds.saturating_mul(1000))
            });
        let err_body = resp.text().await.unwrap_or_default();
        let retryable = is_retryable_http_error(status.as_u16(), &err_body);
        if !retryable || attempt >= 4 {
            if logging::debug_verbose() {
                let (body_text, body_len, truncated) =
                    logging::truncate_for_log(&err_body, logging::VERBOSE_PAYLOAD_CAP);
                logging::global_log(
                    "http_response",
                    json!({
                        "attempt": attempt,
                        "url": url,
                        "status": status.as_u16(),
                        "ok": false,
                        "body": body_text,
                        "body_len": body_len,
                        "truncated": truncated,
                    }),
                );
            }
            let normalized = adapter.normalize_error(Some(status.as_u16()), &err_body);
            return Err(format!("HTTP {status}: {}", normalized.message));
        }

        if logging::debug_verbose() {
            let (body_text, body_len, truncated) =
                logging::truncate_for_log(&err_body, logging::VERBOSE_PAYLOAD_CAP);
            logging::global_log(
                "http_response",
                json!({
                    "attempt": attempt,
                    "url": url,
                    "status": status.as_u16(),
                    "ok": false,
                    "retrying": true,
                    "body": body_text,
                    "body_len": body_len,
                    "truncated": truncated,
                }),
            );
        }
        let backoff = backoff_with_provider_delay(attempt, retry_after_ms);
        emit(
            &Event::new("http_retry")
                .with("attempt", json!(attempt))
                .with("status", json!(status.as_u16()))
                .with("backoff_ms", json!(backoff)),
        );
        sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
    }
}

fn parse_retry_after_ms(s: &str) -> Option<u64> {
    let parsed = s.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    Some(parsed.ceil() as u64)
}

fn parse_retry_after_seconds(s: &str) -> Option<u64> {
    let parsed = s.trim().parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    Some(parsed.ceil() as u64)
}

/// Parse a Retry-After header into seconds. Accepts an integer (seconds) or
/// an HTTP-date (RFC 7231 IMF-fixdate, e.g. "Wed, 21 Oct 2025 07:28:00 GMT");
/// the latter is converted to seconds-from-now (clamped >= 0). Returns None for
/// anything unparseable so the caller falls back to exponential backoff.
fn parse_retry_after(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let date = parse_http_date(s)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let diff = date.saturating_sub(now);
    if diff == 0 {
        None
    } else {
        Some(diff)
    }
}

/// Parse an HTTP IMF-fixdate ("Wed, 21 Oct 2025 07:28:00 GMT") into UNIX
/// seconds. The weekday is ignored (servers sometimes send the wrong one).
fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }
    let day: u32 = parts[1].trim_end_matches(',').parse().ok()?;
    let mon: u32 = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i32 = parts[3].parse().ok()?;
    let tparts: Vec<&str> = parts[4].split(':').collect();
    if tparts.len() != 3 {
        return None;
    }
    let h: u64 = tparts[0].parse().ok()?;
    let mi: u64 = tparts[1].parse().ok()?;
    let se: u64 = tparts[2].parse().ok()?;
    let days = days_from_civil(year, mon, day)?;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

/// Days since the UNIX epoch (1970-01-01) for a proleptic Gregorian date.
/// Howard Hinnant's days_from_civil algorithm; valid for any year.
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<u64> {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m_shift = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * (m_shift as i64) + 2) / 5 + (d as i64) - 1;
    let doe = (yoe as i64) * 365 + (yoe as i64) / 4 - (yoe as i64) / 100 + doy;
    let days = (era as i64) * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    Some(days as u64)
}

/// HTTP statuses worth retrying at the transport layer when no body is available.
/// Prefer [`is_retryable_http_error`] when the response body can be inspected so
/// rate-limit / balance wording can fail fast. 408 + any 5xx retry; permanent
/// client 4xx (incl. 429 rate-limit / 402 balance) do not.
fn is_retryable_http_status(status: reqwest::StatusCode) -> bool {
    is_retryable_http_error(status.as_u16(), "")
}

fn backoff_ms(attempt: u32, retry_after: Option<u64>) -> u64 {
    if let Some(ra) = retry_after {
        // Retry-After: 0 means "retry now"; still apply a tiny floor so we
        // don't spin the event loop on a misbehaving gateway that always
        // returns 0. Cap at 30s so a huge Retry-After cannot stall a turn.
        return ra.saturating_mul(1000).clamp(50, 30_000);
    }
    // 500, 1000, 2000, 4000 ... capped at 8000
    let base = 500u64;
    base.saturating_mul(1u64 << (attempt - 1)).min(8000)
}

fn backoff_with_provider_delay(attempt: u32, retry_after_ms: Option<u64>) -> u64 {
    retry_after_ms
        .map(|delay| delay.clamp(50, 60_000))
        .unwrap_or_else(|| backoff_ms(attempt, None))
}

async fn sleep_or_cancel(d: Duration, cancel: &CancellationToken) -> Result<(), String> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = cancel.cancelled() => Err("aborted".into()),
    }
}

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    args: String,
}

fn fmt_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s.push_str(" -> ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

/// Some gateways close a chunked SSE response without writing the terminating
/// zero-sized HTTP chunk. Hyper reports that as an "unexpected EOF during
/// chunk size line" even when the provider already sent a complete terminal
/// SSE event. Keep that transport quirk from discarding an otherwise complete
/// assistant turn, but never hide it when the SSE decoder still has a partial
/// event buffered or no terminal event was observed.
fn is_recoverable_truncated_chunked_body(
    error: &str,
    decoder: &SseDecoder,
    terminal_event: bool,
) -> bool {
    if !terminal_event || decoder.has_pending_data() {
        return false;
    }
    let lower = error.to_ascii_lowercase();
    lower.contains("unexpected eof") && lower.contains("chunk size line")
}

/// Transient mid-stream failures worth a fresh POST even after the TUI already
/// saw partial tokens. Covers reqwest/hyper body drops, idle timeouts, and
/// gateway/server blips. Rate limits and balance/billing are *not* retryable
/// (fail fast — see [`is_non_retryable_provider_message`]). Permanent request
/// failures (auth, validation, malformed SSE, context length) also stay fatal.
fn is_retryable_stream_failure(message: &str) -> bool {
    if is_non_retryable_provider_message(message) {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    // Permanent request/account failures — never loop on a broken shape.
    // Prefer specific phrases over bare tokens ("authentication"/"forbidden")
    // so retryable rate-limit copy containing those words stays retryable.
    if lower.contains("malformed provider stream")
        || lower.contains("invalid credentials")
        || lower.contains("invalid_api_key")
        || lower.contains("invalid api key")
        || lower.contains("authentication failed")
        || lower.contains("authentication error")
        || lower.contains("unauthorized")
        || lower.contains("access denied")
        || lower.contains("permission denied")
        || lower.contains("context_length")
        || lower.contains("context length")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("http 400")
        || lower.contains("http 401")
        || lower.contains("http 402")
        || lower.contains("http 403")
        || lower.contains("http 404")
        || lower.contains("http 422")
    {
        return false;
    }
    // Body/transport drops while reading an otherwise-accepted SSE response.
    // Idle timeout is the same class: the connection is up but the provider
    // stopped sending frames; a fresh POST often recovers (with discard_partial).
    if lower.contains("stream read:")
        || lower.contains("error decoding response body")
        || lower.contains("error reading a body from connection")
        || lower.contains("stream error received")
        || lower.contains("unexpected internal error")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
        || lower.contains("stream idle timeout")
        || (lower.contains("connection")
            && (lower.contains("closed") || lower.contains("aborted") || lower.contains("reset")))
    {
        return true;
    }
    // Gateway / provider overload frames (HTTP 5xx already retried on the
    // initial POST; this path covers mid-stream error events and body text).
    // Deliberately omits rate-limit / 429 — those fail fast.
    if lower.contains("gateway timeout")
        || lower.contains("temporarily unavailable")
        || lower.contains("overloaded")
        || lower.contains("server_error")
        || lower.contains("http 408")
        || lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
        // Bare status tokens in provider stream error frames / bodies.
        || lower.contains("\"code\":504")
        || lower.contains("\"code\":\"504\"")
        || lower.contains(" status 504")
        || lower.contains(" 504 ")
        || lower.ends_with(" 504")
        || lower.contains("status: 504")
        || lower.contains("error 504")
        || lower.contains(" 502 bad gateway")
        || lower.contains(" 503 service")
        || lower.contains("504 gateway")
    {
        return true;
    }
    // Default after partial output: retry unclassified provider stream errors
    // unless they look permanent above. Matches the user policy of retrying
    // any provider error except rate-limit / balance.
    true
}

/// Decide whether a failed stream attempt may be retried. Rate limits and
/// balance/billing always fail fast. Permanent request failures (auth, context,
/// malformed, validation) also stay fatal. Everything else — including before
/// the first token and after partial output — may retry (with discard_partial
/// after partial so a successful retry cannot duplicate text).
fn should_retry_stream_attempt(
    message: &str,
    _emitted: bool,
    attempt: u32,
    max_attempts: u32,
) -> bool {
    if attempt >= max_attempts {
        return false;
    }
    // Policy/DoS caps (H3) are permanent for this request shape — never retry.
    if message.contains("exceeds cap") || message.contains("exceed cap") {
        return false;
    }
    is_retryable_stream_failure(message)
}

/// Emit the shared http_retry event. `discard_partial` tells the TUI to drop
/// any in-progress assistant/thinking/tool blocks from the failed attempt.
fn emit_stream_retry(attempt: u32, reason: &str, backoff: u64, discard_partial: bool) {
    let mut ev = Event::new("http_retry")
        .with("attempt", json!(attempt))
        .with("reason", json!(reason))
        .with("backoff_ms", json!(backoff));
    if discard_partial {
        ev = ev.with("discard_partial", json!(true));
    }
    emit(&ev);
}

/// Streaming demuxer for MiniMax-style (and similar) content that embeds
/// reasoning inside `<think>…</think>` tags on the assistant `content` stream.
///
/// Gateways that do not honor `reasoning_split` return thinking this way. The
/// harness must still surface `thinking` events and persist `reasoning_content`
/// so the UI and multi-turn tool chains work. Partial open/close tags that
/// straddle chunk boundaries are held in `hold` until complete.
#[derive(Default, Debug)]
struct ThinkTagDemux {
    /// True while inside an open `<think>` region.
    in_think: bool,
    /// Incomplete tag prefix held across chunks (e.g. `"<thi"`).
    hold: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ThinkPiece {
    Text(String),
    Thinking(String),
}

impl ThinkTagDemux {
    fn push(&mut self, incoming: &str) -> Vec<ThinkPiece> {
        if incoming.is_empty() && self.hold.is_empty() {
            return Vec::new();
        }
        let mut src = std::mem::take(&mut self.hold);
        src.push_str(incoming);
        let mut out = Vec::new();
        let mut i = 0;
        while i < src.len() {
            if self.in_think {
                if let Some(rel) = src[i..].find("</think>") {
                    let end = i + rel;
                    if end > i {
                        out.push(ThinkPiece::Thinking(src[i..end].to_string()));
                    }
                    i = end + "</think>".len();
                    self.in_think = false;
                    continue;
                }
                // May end mid-close-tag — hold a short suffix that could grow into it.
                let hold_from = close_tag_hold_start(&src, i);
                if hold_from > i {
                    out.push(ThinkPiece::Thinking(src[i..hold_from].to_string()));
                }
                self.hold = src[hold_from..].to_string();
                return out;
            }
            // Outside think: drop bare closing tags (model noise after a
            // completed block) then look for the next open tag.
            if let Some(rel) = src[i..].find("</think>") {
                let open_rel = src[i..].find("<think>");
                let close_first = open_rel.map(|o| rel < o).unwrap_or(true);
                if close_first {
                    let start = i + rel;
                    if start > i {
                        out.push(ThinkPiece::Text(src[i..start].to_string()));
                    }
                    i = start + "</think>".len();
                    continue;
                }
            }
            if let Some(rel) = src[i..].find("<think>") {
                let start = i + rel;
                if start > i {
                    out.push(ThinkPiece::Text(src[i..start].to_string()));
                }
                i = start + "<think>".len();
                self.in_think = true;
                continue;
            }
            let hold_from = open_tag_hold_start(&src, i);
            if hold_from > i {
                out.push(ThinkPiece::Text(src[i..hold_from].to_string()));
            }
            self.hold = src[hold_from..].to_string();
            return out;
        }
        out
    }

    /// Flush any held bytes at stream end. Incomplete tags are emitted as the
    /// channel they were being accumulated for (text outside, thinking inside)
    /// so no tokens are silently dropped.
    fn finish(&mut self) -> Vec<ThinkPiece> {
        if self.hold.is_empty() {
            return Vec::new();
        }
        let held = std::mem::take(&mut self.hold);
        if self.in_think {
            vec![ThinkPiece::Thinking(held)]
        } else {
            vec![ThinkPiece::Text(held)]
        }
    }
}

fn open_tag_hold_start(src: &str, from: usize) -> usize {
    // Hold a trailing prefix of `<think>` that might complete in the next chunk.
    // Only probe hold lengths that land on a UTF-8 boundary — otherwise
    // `tail[len - n..]` panics on multi-byte trailing chars (e.g. U+2019 ’).
    const TAG: &str = "<think>";
    let tail = &src[from..];
    let max = (TAG.len() - 1).min(tail.len());
    for n in (1..=max).rev() {
        let start = tail.len() - n;
        if !tail.is_char_boundary(start) {
            continue;
        }
        if TAG.starts_with(&tail[start..]) {
            return src.len() - n;
        }
    }
    src.len()
}

fn close_tag_hold_start(src: &str, from: usize) -> usize {
    const TAG: &str = "</think>";
    let tail = &src[from..];
    let max = (TAG.len() - 1).min(tail.len());
    for n in (1..=max).rev() {
        let start = tail.len() - n;
        if !tail.is_char_boundary(start) {
            continue;
        }
        if TAG.starts_with(&tail[start..]) {
            return src.len() - n;
        }
    }
    src.len()
}

/// True when the model id is a MiniMax M-series id (host-agnostic). Used so
/// proxy gateways that front MiniMax still get think-tag demux + replay.
fn is_minimax_model(model: &str) -> bool {
    let l = model.to_ascii_lowercase();
    l.contains("minimax") || l.starts_with("m3-") || l == "m3"
}

/// Remove residual `<think>` / `</think>` markers from assistant visible content.
fn sanitize_assistant_content(content: &mut String) {
    while let Some(start) = content.find("<think>") {
        if let Some(rel) = content[start..].find("</think>") {
            let end = start + rel + "</think>".len();
            content.replace_range(start..end, "");
        } else {
            content.replace_range(start.., "");
            break;
        }
    }
    while let Some(pos) = content.find("</think>") {
        let end = pos + "</think>".len();
        content.replace_range(pos..end, "");
    }
    let trimmed = content.trim();
    if trimmed.len() != content.len() {
        *content = trimmed.to_string();
    }
}

/// Append a stream text fragment.
///
/// - `cumulative == false` (default OpenAI/Anthropic/Gemini/Codex): pure
///   incremental append. Never drop a token that is a prefix of `acc` — that
///   is a normal incremental re-emission (repeated digits, CJK, etc.).
/// - `cumulative == true` (MiniMax `reasoning_split` / cumulative content):
///   treat `incoming` as a full snapshot, emit only the new suffix, and ignore
///   identical/shorter rewinds.
///
/// Returns only the newly added suffix for emission.
fn append_stream_fragment(acc: &mut String, incoming: &str, cumulative: bool) -> Option<String> {
    if incoming.is_empty() {
        return None;
    }
    if cumulative {
        if !acc.is_empty() && incoming.starts_with(acc.as_str()) {
            let suffix = &incoming[acc.len()..];
            if suffix.is_empty() {
                return None;
            }
            acc.push_str(suffix);
            return Some(suffix.to_string());
        }
        // Cumulative vendor rewound or resent a shorter prefix — ignore.
        if !acc.is_empty() && acc.starts_with(incoming) {
            return None;
        }
    }
    acc.push_str(incoming);
    Some(incoming.to_string())
}

fn is_terminal_stream_event(event: &NormalizedStreamEvent) -> bool {
    matches!(event, NormalizedStreamEvent::FinishReason(_))
}

fn is_terminal_stream_object(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed")
            | Some("response.incomplete")
            | Some("response.failed")
            | Some("message_stop")
    )
}

/// True when the stream has delivered a terminal event AND the decoder has no
/// buffered bytes pending, so the read loop can break without waiting for the
/// transport EOF or the per-chunk idle timeout. Required for providers that
/// emit `finish_reason:stop` + `usage` but never send the `[DONE]` sentinel and
/// hold the connection open (e.g. mmx/MiniMax-M3 over 9router): without this
/// gate the TUI sits on an "infinite loading" footer until the 10s+ idle
/// timeout fires, which the user reads as a hang.
fn stream_logically_complete(terminal_event: bool, decoder: &SseDecoder) -> bool {
    terminal_event && !decoder.has_pending_data()
}

// =========================================================================
// Anthropic Messages API translation
// =========================================================================
//
// The harness keeps the conversation in OpenAI chat-completions shape. These
// functions translate OpenAI messages + tools -> an Anthropic `/v1/messages`
// request, and an Anthropic SSE stream -> the same delta/thinking/tool_call
// events the OpenAI path emits, then rebuild the assistant message in OpenAI
// shape. The rest of the harness never sees Anthropic wire format.

/// Map a reasoning effort to an Anthropic extended-thinking token budget.
/// Returns None when thinking can't be enabled (effort "none"/unknown, or
/// `max_tokens` too small to leave room for a >=1024 budget — Anthropic counts
/// thinking within `max_tokens`, so the budget must be < max_tokens).
#[allow(dead_code)]
fn anthropic_thinking_budget(effort: &str, max_tokens: u32) -> Option<u32> {
    let base: u32 = match effort.to_ascii_lowercase().as_str() {
        "low" | "minimal" => 4096,
        "medium" => 12288,
        "high" | "max" => 24576,
        _ => return None,
    };
    let budget = base.min(max_tokens.saturating_sub(1024));
    if budget < 1024 {
        return None;
    }
    Some(budget)
}

/// Push text from an OpenAI `content` (string or multimodal array) into a vec
/// of system-parts. Image parts are ignored (system is text-only).
#[allow(dead_code)]
fn push_content_str(content: &Value, parts: &mut Vec<String>) {
    if let Some(s) = content.as_str() {
        if !s.is_empty() {
            parts.push(s.to_string());
        }
    } else if let Some(arr) = content.as_array() {
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        parts.push(t.to_string());
                    }
                }
            }
        }
    }
}

/// Append a message with the given role + content blocks, merging into the
/// previous message when it has the same role (Anthropic requires alternating
/// roles; consecutive same-role messages 400). Merging concatenates the block
/// arrays — e.g. several OpenAI `role:tool` results fold into one user message
/// with multiple `tool_result` blocks.
#[allow(dead_code)]
fn push_or_merge(out: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(|c| c.as_array_mut()) {
                arr.extend(blocks);
                return;
            }
        }
    }
    out.push(json!({ "role": role, "content": blocks }));
}

/// Convert a single OpenAI message `content` (string or multimodal array) into
/// Anthropic content blocks. Images become Anthropic `image` blocks (base64 or
/// url source); text stays text. A plain string yields a single text block.
#[allow(dead_code)]
fn anthropic_content_blocks(content: &Value) -> Vec<Value> {
    if let Some(s) = content.as_str() {
        return vec![json!({ "type": "text", "text": s })];
    }
    let mut blocks = Vec::new();
    if let Some(arr) = content.as_array() {
        for part in arr {
            match part.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                Some("image_url") => {
                    if let Some(url) = part
                        .get("image_url")
                        .and_then(|iu| iu.get("url"))
                        .and_then(|u| u.as_str())
                    {
                        if let Some(img) = anthropic_image_block(url) {
                            blocks.push(img);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }
    blocks
}

/// Build an Anthropic `image` block from an OpenAI `image_url.url`. Supports
/// `data:<media>;base64,<data>` (-> base64 source) and plain URLs (-> url source).
#[allow(dead_code)]
fn anthropic_image_block(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media = meta.split(';').next()?;
        Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media, "data": data }
        }))
    } else {
        Some(json!({ "type": "image", "source": { "type": "url", "url": url } }))
    }
}

/// Convert OpenAI function tools to Anthropic tool definitions.
/// OpenAI: `{"type":"function","function":{"name","description","parameters"}}`
/// Anthropic: `{"name","description","input_schema"}`
#[allow(dead_code)]
fn anthropic_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let f = t.get("function")?;
            let name = f.get("name").and_then(|v| v.as_str())?;
            let description = f.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let schema = f.get("parameters").cloned().unwrap_or_else(|| json!({}));
            Some(json!({ "name": name, "description": description, "input_schema": schema }))
        })
        .collect()
}

/// Build an Anthropic `/v1/messages` request body from OpenAI-shaped messages +
/// tools. Extracts `role: system` messages into the top-level `system` field,
/// converts user/assistant/tool messages to Anthropic format, and converts
/// OpenAI function tools to `input_schema` tools. `thinking_levels` non-empty +
/// a supported effort enables extended thinking. Pure (no I/O) so it can be
/// unit-tested directly.
///
/// **DEPRECATED**: Use `message::build_anthropic_request(messages: &[Message], ...)`
/// instead — it works on typed `Message` values rather than opaque `Value` JSON.
/// This function is kept for backward-compat with existing tests and will be
/// removed once callers are migrated.
#[allow(dead_code)]
pub fn build_anthropic_request(
    messages: &[Value],
    tools: &[Value],
    model: &str,
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
) -> Value {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match role {
            "system" => {
                push_content_str(m.get("content").unwrap_or(&Value::Null), &mut system_parts)
            }
            "user" => {
                let blocks = anthropic_content_blocks(m.get("content").unwrap_or(&Value::Null));
                push_or_merge(&mut out, "user", blocks);
            }
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(content) = m.get("content") {
                    if let Some(s) = content.as_str() {
                        if !s.is_empty() {
                            blocks.push(json!({ "type": "text", "text": s }));
                        }
                    } else if let Some(arr) = content.as_array() {
                        for part in arr {
                            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                    if !t.is_empty() {
                                        blocks.push(json!({ "type": "text", "text": t }));
                                    }
                                }
                            }
                        }
                    }
                }
                // assistant tool_calls -> tool_use blocks. reasoning_content is
                // dropped: Anthropic can't replay raw thinking without matching
                // signatures (it would 400), so prior reasoning is never sent back.
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let func = tc.get("function").cloned().unwrap_or_else(|| json!({}));
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args = func
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                push_or_merge(&mut out, "assistant", blocks);
            }
            "tool" => {
                // OpenAI tool result -> Anthropic user message with a tool_result block.
                let tool_use_id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                push_or_merge(
                    &mut out,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content
                    })],
                );
            }
            _ => {}
        }
    }

    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(max_tokens));
    if !system_parts.is_empty() {
        body.insert("system".into(), json!(system_parts.join("\n\n")));
    }
    if !out.is_empty() {
        body.insert("messages".into(), Value::Array(out));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(anthropic_tools(tools)));
        body.insert("tool_choice".into(), json!({ "type": "auto" }));
    }
    if !thinking_levels.is_empty() {
        // Only enable extended thinking when the user actually asked for it.
        // `resolve_effort` would otherwise clamp "none" up to a supported level
        // and silently turn thinking on; gate on the raw requested effort first.
        let wants = !matches!(
            reasoning_effort.to_ascii_lowercase().as_str(),
            "" | "none" | "minimal" | "off"
        );
        if wants {
            let resolved = resolve_effort(reasoning_effort, thinking_levels);
            if let Some(budget) = anthropic_thinking_budget(&resolved, max_tokens) {
                body.insert(
                    "thinking".into(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
        }
    }
    Value::Object(body)
}

/// Accumulator for one Anthropic content block while streaming (text / thinking
/// / tool_use). Keyed by the block `index` from the SSE events.
#[derive(Default)]
struct AnthropicBlock {
    kind: String,
    tool_id: String,
    tool_name: String,
    tool_args: String,
}

/// POST an Anthropic request with retry on 429/5xx (same policy as the OpenAI
/// path). Auth is `x-api-key` (not Bearer); `anthropic-version` + any provider
/// headers are attached. Cancellation-aware.
async fn send_anthropic_request(
    client: &reqwest::Client,
    url: &str,
    provider: &ResolvedProvider,
    body: &Value,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, String> {
    let adapter = adapter_for(provider);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if logging::debug_verbose() {
            let header_names: Vec<String> =
                provider.headers.iter().map(|(k, _)| k.clone()).collect();
            logging::global_log(
                "http_request",
                json!({
                    "attempt": attempt,
                    "method": "POST",
                    "url": url,
                    "provider_kind": "anthropic",
                    "header_names": header_names,
                    "oauth": provider.oauth,
                    "has_key": provider.api_key.is_some(),
                    "body": logging::truncate_json_for_log(body, logging::VERBOSE_PAYLOAD_CAP),
                }),
            );
        }
        let mut req = client
            .post(url)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("accept", "text/event-stream")
            .header("accept-encoding", "identity")
            .header("cache-control", "no-cache")
            .header("content-type", "application/json")
            .json(body);
        // Track whether we attached a primary auth header so provider.headers
        // cannot inject a second Authorization / x-api-key (dual-auth 401s).
        let mut set_primary_auth = false;
        if provider.oauth {
            // Claude.ai subscription (OAuth): Bearer token + the claude-code
            // identity beta (Anthropic's gateway requires it for subscription
            // tokens). UA/x-app come from provider.headers (injected by
            // enrich_oauth). Reuses the same Messages endpoint as the API-key path.
            if let Some(k) = anthropic_nonempty_key(provider.api_key.as_deref()) {
                req = req.header("authorization", format!("Bearer {k}"));
                set_primary_auth = true;
            }
            req = req.header("anthropic-beta", CLAUDE_OAUTH_BETA);
        } else if let Some(k) = anthropic_nonempty_key(provider.api_key.as_deref()) {
            req = req.header("x-api-key", k);
            set_primary_auth = true;
        }
        for (k, v) in &provider.headers {
            if set_primary_auth && is_anthropic_primary_auth_header(k) {
                continue;
            }
            req = req.header(k, v);
        }
        let resp = tokio::select! {
            r = req.send() => r,
            _ = cancel.cancelled() => return Err("aborted".into()),
        };
        match resp {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    if logging::debug_verbose() {
                        logging::global_log(
                            "http_response",
                            json!({
                                "attempt": attempt,
                                "url": url,
                                "status": status.as_u16(),
                                "ok": true,
                                "provider_kind": "anthropic",
                            }),
                        );
                    }
                    return Ok(r);
                }
                let retry_after_ms = r
                    .headers()
                    .get("retry-after-ms")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_retry_after_ms)
                    .or_else(|| {
                        r.headers()
                            .get("x-ratelimit-reset-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(parse_retry_after_seconds)
                            .map(|seconds| seconds.saturating_mul(1000))
                    })
                    .or_else(|| {
                        r.headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(parse_retry_after)
                            .map(|seconds| seconds.saturating_mul(1000))
                    });
                let err_body = r.text().await.unwrap_or_default();
                // Same denylist as OpenAI: retry by default; rate-limit + balance
                // (and permanent client 4xx) fail fast.
                let retryable = is_retryable_http_error(status.as_u16(), &err_body);
                if !retryable || attempt >= 4 {
                    if logging::debug_verbose() {
                        let (body_text, body_len, truncated) =
                            logging::truncate_for_log(&err_body, logging::VERBOSE_PAYLOAD_CAP);
                        logging::global_log(
                            "http_response",
                            json!({
                                "attempt": attempt,
                                "url": url,
                                "status": status.as_u16(),
                                "ok": false,
                                "provider_kind": "anthropic",
                                "body": body_text,
                                "body_len": body_len,
                                "truncated": truncated,
                            }),
                        );
                    }
                    let normalized = adapter.normalize_error(Some(status.as_u16()), &err_body);
                    return Err(format!("HTTP {status}: {}", normalized.message));
                }
                if logging::debug_verbose() {
                    let (body_text, body_len, truncated) =
                        logging::truncate_for_log(&err_body, logging::VERBOSE_PAYLOAD_CAP);
                    logging::global_log(
                        "http_response",
                        json!({
                            "attempt": attempt,
                            "url": url,
                            "status": status.as_u16(),
                            "ok": false,
                            "retrying": true,
                            "provider_kind": "anthropic",
                            "body": body_text,
                            "body_len": body_len,
                            "truncated": truncated,
                        }),
                    );
                }
                let backoff = backoff_with_provider_delay(attempt, retry_after_ms);
                emit(
                    &Event::new("http_retry")
                        .with("attempt", json!(attempt))
                        .with("status", json!(status.as_u16()))
                        .with("backoff_ms", json!(backoff)),
                );
                sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
            }
            Err(e) => {
                if logging::debug_verbose() {
                    logging::global_log(
                        "http_error",
                        json!({
                            "attempt": attempt,
                            "url": url,
                            "error": fmt_chain(&e),
                            "kind": "transport",
                            "provider_kind": "anthropic",
                        }),
                    );
                }
                if attempt >= 4 {
                    let normalized = adapter.normalize_error(None, &fmt_chain(&e));
                    return Err(format!(
                        "request failed after {attempt} attempts: {}",
                        normalized.message
                    ));
                }
                let backoff = backoff_ms(attempt, None);
                emit(
                    &Event::new("http_retry")
                        .with("attempt", json!(attempt))
                        .with("reason", json!("transport error"))
                        .with("backoff_ms", json!(backoff)),
                );
                sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
            }
        }
    }
}

/// Anthropic streaming turn. Emits the same delta/thinking/tool_call events
/// and returns the same (assistant_msg, finish_reason, tokens) tuple as
/// `stream_turn_openai`, so the caller is protocol-agnostic.
async fn stream_turn_anthropic(
    client: &reqwest::Client,
    provider: &ResolvedProvider,
    idle_timeout_secs: u64,
    model: &str,
    messages: &[Message],
    tools: &[Value],
    reasoning_effort: &str,
    thinking_levels: &[String],
    max_tokens: u32,
    cancel: &CancellationToken,
    timer: &mut TurnTimer,
    prompt_est: u64,
    quiet: bool,
) -> Result<(Value, String, u64, u64, u64), String> {
    let minimax = is_minimax(&provider.base_url);
    let adapter = adapter_for(provider);
    let built = adapter.build_request(&ProviderRequest {
        provider,
        model,
        messages,
        tools,
        reasoning_effort,
        thinking_levels,
        max_tokens,
    })?;
    let body = built.body;
    let url = built.url;
    let idle = Duration::from_secs(idle_timeout_secs.max(10));
    // Live stats: same grounded prompt estimate as the OpenAI path; the real
    // `usage` at stream end overwrites the footer with exact values.
    let est_prompt = prompt_est;
    let mut last_stats: Option<Instant> = None;

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut blocks: Vec<AnthropicBlock> = Vec::new();
    let mut tool_args_bytes: usize = 0;
    let mut finish_reason = String::new();
    let mut tokens_in: u64 = 0;
    let mut tokens_out: u64 = 0;
    let mut cached_tokens: u64 = 0;

    let max_attempts = 3u32;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = send_anthropic_request(client, &url, provider, &body, cancel).await?;
        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut emitted = false;
        let mut terminal_event = false;
        let mut err: Option<String> = None;

        'read_stream: loop {
            let chunk = tokio::select! {
                c = tokio::time::timeout(idle, stream.next()) => match c {
                    Ok(x) => x,
                    Err(_) => {
                        err = Some(format!("stream idle timeout ({}s with no data)", idle_timeout_secs));
                        break;
                    }
                },
                _ = cancel.cancelled() => return Err("aborted".into()),
            };
            let Some(chunk) = chunk else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    let detail = fmt_chain(&e);
                    if !is_recoverable_truncated_chunked_body(&detail, &decoder, terminal_event) {
                        err = Some(format!("stream read: {detail}"));
                    }
                    break;
                }
            };
            for frame in decoder.push(&chunk) {
                let (event_name, mut obj) = match frame {
                    SseFrame::Json { event, value } => (event, value),
                    SseFrame::Done => {
                        terminal_event = true;
                        continue;
                    }
                    SseFrame::Malformed { preview, .. } => {
                        err = Some(malformed_response(&preview).message);
                        break 'read_stream;
                    }
                };

                // Some compatible gateways provide the discriminator only in
                // the SSE `event:` line. Normalize that transport quirk before
                // handing the object to the adapter's pure decoder.
                if obj.get("type").is_none() {
                    if let Some(event_name) = event_name {
                        obj["type"] = json!(event_name);
                    }
                }
                terminal_event |= is_terminal_stream_object(&obj);
                for event in adapter.decode_stream_event(&obj) {
                    terminal_event |= is_terminal_stream_event(&event);
                    match event {
                        NormalizedStreamEvent::TextDelta(text) => {
                            if let Some(delta) =
                                append_stream_fragment(&mut content, &text, minimax)
                            {
                                if content.len() == delta.len() {
                                    timer.mark_first_token();
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(&Event::new("delta").with("text", json!(delta)));
                                }
                            }
                        }
                        NormalizedStreamEvent::ReasoningDelta(text) => {
                            if let Some(delta) =
                                append_stream_fragment(&mut reasoning, &text, minimax)
                            {
                                if reasoning.len() == delta.len() {
                                    timer.mark_first_token();
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(&Event::new("thinking").with("text", json!(delta)));
                                }
                            }
                        }
                        NormalizedStreamEvent::ToolCallStart(delta) => {
                            timer.mark_first_token();
                            if let Err(e) = ensure_stream_tool_slot(&mut blocks, delta.index) {
                                err = Some(e);
                                break 'read_stream;
                            }
                            let block = &mut blocks[delta.index];
                            block.kind = "tool_use".into();
                            if let Some(id) = delta.id {
                                block.tool_id = id;
                            }
                            if let Some(name) = delta.name {
                                block.tool_name = name;
                            }
                            if !quiet {
                                emitted = true;
                                emit(
                                    &Event::new("tool_call_start")
                                        .with("id", json!(block.tool_id))
                                        .with("index", json!(delta.index)),
                                );
                                if !block.tool_name.is_empty() {
                                    emit(
                                        &Event::new("tool_call_name")
                                            .with("index", json!(delta.index))
                                            .with("name", json!(block.tool_name)),
                                    );
                                }
                            }
                        }
                        NormalizedStreamEvent::ToolCallDelta(delta) => {
                            timer.mark_first_token();
                            if let Err(e) = ensure_stream_tool_slot(&mut blocks, delta.index) {
                                err = Some(e);
                                break 'read_stream;
                            }
                            let block = &mut blocks[delta.index];
                            block.kind = "tool_use".into();
                            if let Some(arguments) = delta.arguments {
                                if let Err(e) = append_stream_tool_args(
                                    &mut tool_args_bytes,
                                    &mut block.tool_args,
                                    &arguments,
                                ) {
                                    err = Some(e);
                                    break 'read_stream;
                                }
                                if !quiet {
                                    emitted = true;
                                    emit(
                                        &Event::new("tool_call_args")
                                            .with("index", json!(delta.index))
                                            .with("args", json!(arguments)),
                                    );
                                }
                            }
                        }
                        NormalizedStreamEvent::ToolCallComplete { .. } => {}
                        NormalizedStreamEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cached_tokens: cached,
                        } => {
                            if let Some(value) = input_tokens {
                                tokens_in = value;
                            }
                            if let Some(value) = output_tokens {
                                tokens_out = value;
                            }
                            if let Some(value) = cached {
                                cached_tokens = value;
                            }
                        }
                        NormalizedStreamEvent::FinishReason(reason) => finish_reason = reason,
                        NormalizedStreamEvent::RetryableError(message)
                        | NormalizedStreamEvent::FatalError(message) => {
                            err = Some(message);
                            break;
                        }
                    }
                }

                // Live footer stats (same ~400ms throttle as the OpenAI path).
                if !quiet && (!content.is_empty() || !reasoning.is_empty()) {
                    let now = Instant::now();
                    let due = last_stats
                        .map(|t| now.duration_since(t) >= Duration::from_millis(400))
                        .unwrap_or(true);
                    if due {
                        last_stats = Some(now);
                        let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
                        let live_ctx = est_prompt.saturating_add(est_out);
                        let mut ev = Event::new("metrics")
                            .with("tokens_in", json!(live_ctx))
                            .with("tokens_out", json!(est_out));
                        if let Some(ttft) = timer
                            .first_token
                            .map(|t| t.duration_since(timer.start).as_millis() as u64)
                        {
                            ev = ev.with("ttft_ms", json!(ttft));
                        }
                        if let Some(tps) = timer.live_tps_estimate(est_out) {
                            ev = ev.with("tps_est", json!(tps));
                        }
                        emit(&ev);
                    }
                }
            }
        }

        if err.is_none() {
            break; // stream completed cleanly
        }
        let msg = err.unwrap();
        if !should_retry_stream_attempt(&msg, emitted, attempt, max_attempts) {
            return Err(msg);
        }
        let backoff = backoff_ms(attempt, None);
        let reason = if emitted {
            "retryable stream error after partial output"
        } else {
            "stream error before first token"
        };
        emit_stream_retry(attempt, reason, backoff, emitted);
        content.clear();
        reasoning.clear();
        blocks.clear();
        tool_args_bytes = 0;
        finish_reason.clear();
        tokens_in = 0;
        tokens_out = 0;
        cached_tokens = 0;
        timer.call_first_token = None;
        sleep_or_cancel(Duration::from_millis(backoff), cancel).await?;
    }

    let est_out = estimate_tokens(&content) + estimate_tokens(&reasoning);
    timer.end_call(tokens_out, est_out);

    // Rebuild the assistant message in OpenAI shape. reasoning is shown live but
    // NOT persisted: Anthropic thinking blocks aren't replayable (would 400 next
    // turn), so we drop them from history — same as the OpenAI path drops
    // reasoning_content on non-Umans endpoints.
    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert(
        "content".into(),
        if content.is_empty() {
            Value::Null
        } else {
            json!(content)
        },
    );
    let tool_calls: Vec<Value> = blocks
        .iter()
        .filter(|b| b.kind == "tool_use")
        .map(|b| {
            json!({
                "id": b.tool_id,
                "type": "function",
                "function": {
                    "name": b.tool_name,
                    "arguments": if b.tool_args.is_empty() {
                        "{}".to_string()
                    } else {
                        b.tool_args.clone()
                    }
                }
            })
        })
        .collect();
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), json!(tool_calls));
    }

    // Prefer an explicit stream finish reason. If the gateway omitted
    // message_delta.stop_reason (or only sent message_stop), fall back so the
    // turn loop still sees tool_calls vs stop — empty used to leak through.
    let reason = if !finish_reason.is_empty() {
        finish_reason
    } else if tool_calls.is_empty() {
        "stop".into()
    } else {
        "tool_calls".into()
    };

    Ok((
        Value::Object(msg),
        reason,
        tokens_in,
        tokens_out,
        cached_tokens,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_reasoning_only_dsml_tool_calls_narrowly() {
        assert!(reasoning_contains_dsml_tool_call(
            "<｜DSML｜invoke name=\"edit\">... </｜DSML｜invoke>\n</｜DSML｜tool_calls>"
        ));
        assert!(reasoning_contains_dsml_tool_call(
            "<|DSML|invoke name=\"bash\">"
        ));
        assert!(!reasoning_contains_dsml_tool_call(
            "I should use the structured tool_calls field next."
        ));
        assert!(!reasoning_contains_dsml_tool_call(
            "DSML must never be written instead of tool_calls."
        ));
        assert!(!reasoning_contains_dsml_tool_call("ordinary reasoning"));
    }

    #[test]
    fn stream_tool_slot_helpers_reject_unbounded_index() {
        // H3: high index must fail without growing the vector to multi-GB.
        let mut calls: Vec<ToolAccum> = Vec::new();
        assert!(ensure_stream_tool_slot(&mut calls, 0).is_ok());
        let err = ensure_stream_tool_slot(
            &mut calls,
            crate::providers::streaming::MAX_STREAM_TOOL_CALL_INDEX,
        )
        .unwrap_err();
        assert!(err.contains("exceeds cap"), "{err}");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn dsml_recovery_rejects_unknown_tools_and_invalid_json() {
        let registered = vec![json!({"function": {"name": "edit"}})];
        let unknown = r#"<｜DSML｜tool_calls>
<｜DSML｜invoke name="bash"></｜DSML｜invoke>
</｜DSML｜tool_calls>"#;
        assert!(parse_reasoning_dsml_tool_calls(unknown, &registered)
            .err()
            .unwrap()
            .contains("unavailable tool 'bash'"));

        let invalid_json = r#"<｜DSML｜tool_calls>
<｜DSML｜invoke name="edit">
<｜DSML｜parameter name="edits" string="false">not-json</｜DSML｜parameter>
</｜DSML｜invoke>
</｜DSML｜tool_calls>"#;
        assert!(parse_reasoning_dsml_tool_calls(invalid_json, &registered)
            .err()
            .unwrap()
            .contains("invalid JSON"));
    }

    #[test]
    fn recovery_instruction_is_request_local_and_prepend_only() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "original"},
                {"role": "user", "content": "fix it"}
            ]
        });
        add_structured_tool_call_recovery_instruction(&mut body);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("structured tool_calls"));
        assert_eq!(messages[1]["content"], "original");
        assert_eq!(messages[2]["content"], "fix it");
    }

    #[test]
    fn is_opencode_go_matches_zen_go_path() {
        assert!(is_opencode_go("https://opencode.ai/zen/go/v1"));
        assert!(is_opencode_go("https://opencode.ai/zen/go/v1/"));
        // host must be opencode.ai AND path must include /zen/go/
        assert!(!is_opencode_go("https://opencode.ai/zen/v1"));
        assert!(!is_opencode_go("https://evil.com/zen/go/v1"));
        // a look-alike host is not mistaken for opencode.ai
        assert!(!is_opencode_go("https://opencode.ai.evil.com/zen/go/v1"));
        // not umans (must not trigger Umans-only wire fields)
        assert!(!is_umans("https://opencode.ai/zen/go/v1"));
    }

    #[test]
    fn parse_umans_usage_fields() {
        // Documented /v1/usage shape from the Umans gateway (matches
        // pi-provider-umans): concurrent_sessions = current, limits.concurrency.limit
        // = guaranteed plan ceiling.
        let v = json!({
            "limits": { "concurrency": { "limit": 8 }, "requests": { "limit": 500 } },
            "usage": { "requests_in_window": 12, "concurrent_sessions": 3 }
        });
        let u = parse_umans_usage(&v);
        assert_eq!(u.used, Some(3));
        assert_eq!(u.limit, Some(8));
    }

    #[test]
    fn parse_umans_usage_unlimited_limit() {
        // A null concurrency limit = unlimited plan → None (UI renders ∞).
        let v = json!({
            "limits": { "concurrency": { "limit": null } },
            "usage": { "concurrent_sessions": 1 }
        });
        let u = parse_umans_usage(&v);
        assert_eq!(u.used, Some(1));
        assert_eq!(u.limit, None);
    }

    #[test]
    fn parse_umans_usage_missing_fields() {
        // An empty / differently-shaped payload yields None for both (UI hides).
        let u = parse_umans_usage(&json!({}));
        assert_eq!(u.used, None);
        assert_eq!(u.limit, None);
    }

    #[test]
    fn parse_umans_usage_full_windows() {
        let v = json!({
            "plan": { "display_name": "Pro", "slug": "pro" },
            "limits": { "concurrency": { "limit": 8 }, "requests": { "limit": 500 } },
            "usage": {
                "requests_in_window": 12,
                "concurrent_sessions": 3,
                "tokens_in": 1000,
                "tokens_out": 200
            },
            "window": { "remaining_minutes": 42, "resets_at": 1785542400 },
            "message": "Plan ceiling is not exposed by this provider."
        });
        let u = parse_umans_usage_full(&v);
        assert!(u.available);
        assert_eq!(u.plan.as_deref(), Some("Pro"));
        assert!(u.windows.iter().any(|w| w.id == "concurrency"));
        assert!(u.windows.iter().any(|w| w.id == "requests"));
        let req = u.windows.iter().find(|w| w.id == "requests").unwrap();
        assert_eq!(req.used, Some(12.0));
        assert_eq!(req.limit, Some(500.0));
        assert_eq!(req.resets_at, Some(1785542400));
        assert!(req.detail.as_deref().unwrap_or("").contains("42m"));
        assert_eq!(
            u.message.as_deref(),
            Some("Plan ceiling is not exposed by this provider.")
        );
    }

    #[test]
    fn parse_codex_usage_primary_secondary() {
        let v = json!({
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 18000,
                    "reset_at": 9999999999_i64
                },
                "secondary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 604800,
                    "reset_at": 9999999999_i64
                }
            }
        });
        let u = parse_codex_usage(&v);
        assert!(u.available);
        assert_eq!(u.plan.as_deref(), Some("pro"));
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].id, "five_hour");
        assert_eq!(u.windows[0].used, Some(42.0));
        assert_eq!(u.windows[0].unit, "percent");
        assert_eq!(u.windows[1].id, "weekly");
    }

    #[test]
    fn parse_anthropic_oauth_usage_windows() {
        let v = json!({
            "five_hour": { "utilization": 72.5, "resets_at": "2026-07-09T18:00:00Z" },
            "seven_day": { "utilization": 30, "resets_at": 9999999999_i64 },
            "seven_day_opus": { "utilization": 5, "resets_at": null }
        });
        let u = parse_anthropic_oauth_usage(&v);
        assert!(u.available);
        assert_eq!(u.windows.len(), 3);
        let five = u.windows.iter().find(|w| w.id == "five_hour").unwrap();
        assert_eq!(five.used, Some(72.5));
        assert_eq!(five.unit, "percent");
        assert!(five.resets_at.is_some());
        let week = u.windows.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(week.used, Some(30.0));
    }

    #[test]
    fn parse_iso8601_unix_basic() {
        // 2026-07-09T00:00:00Z
        let ts = parse_iso8601_unix("2026-07-09T00:00:00Z").unwrap();
        // Sanity: after 2020 and before 2030
        assert!(ts > 1_577_836_800);
        assert!(ts < 1_893_456_000);
        assert_eq!(parse_iso8601_unix("1710000000"), Some(1710000000));
    }

    #[test]
    fn parse_xai_billing_credits_format_matches_website() {
        // Live shape from /v1/billing?format=credits — matches Settings → Usage.
        let v = json!({
            "config": {
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-09T14:26:33.371434+00:00",
                    "end": "2026-07-16T14:26:33.371434+00:00"
                },
                "creditUsagePercent": 30.0,
                "onDemandCap": { "val": 0 },
                "onDemandUsed": { "val": 0 },
                "productUsage": [
                    { "product": "GrokBuild", "usagePercent": 29.0 },
                    { "product": "Api", "usagePercent": 1.0 },
                    { "product": "Chat", "usagePercent": 0.0 }
                ],
                "isUnifiedBillingUser": true,
                "prepaidBalance": { "val": 0 },
                "billingPeriodStart": "2026-07-09T14:26:33.371434+00:00",
                "billingPeriodEnd": "2026-07-16T14:26:33.371434+00:00"
            }
        });
        let u = parse_xai_billing(&v);
        assert!(u.available);
        let weekly = u.windows.iter().find(|w| w.id == "weekly").unwrap();
        assert_eq!(weekly.label, "Weekly usage");
        assert_eq!(weekly.used, Some(30.0));
        assert_eq!(weekly.limit, Some(100.0));
        assert_eq!(weekly.unit, "percent");
        assert!(weekly.resets_at.is_some());
        // Product rows (zero-share Chat skipped).
        let build = u
            .windows
            .iter()
            .find(|w| w.id == "product_grokbuild")
            .unwrap();
        assert_eq!(build.label, "Build");
        assert_eq!(build.used, Some(29.0));
        let api = u.windows.iter().find(|w| w.id == "product_api").unwrap();
        assert_eq!(api.used, Some(1.0));
        assert!(!u.windows.iter().any(|w| w.id == "product_chat"));
    }

    #[test]
    fn parse_xai_billing_legacy_raw_credits_fallback() {
        // Without ?format=credits the host returns used/monthlyLimit only.
        let v = json!({
            "config": {
                "monthlyLimit": { "val": 15000 },
                "used": { "val": 885 },
                "onDemandCap": { "val": 0 },
                "billingPeriodStart": "2026-07-01T00:00:00+00:00",
                "billingPeriodEnd": "2026-08-01T00:00:00+00:00"
            }
        });
        let u = parse_xai_billing(&v);
        assert!(u.available);
        let w = u.windows.iter().find(|w| w.id == "weekly").unwrap();
        assert_eq!(w.used, Some(885.0));
        assert_eq!(w.limit, Some(15000.0));
        assert_eq!(w.unit, "credits");
    }

    #[test]
    fn parse_xai_billing_with_on_demand() {
        let v = json!({
            "config": {
                "creditUsagePercent": 100.0,
                "currentPeriod": {
                    "type": "USAGE_PERIOD_TYPE_WEEKLY",
                    "start": "2026-07-09T00:00:00Z",
                    "end": "2026-07-16T00:00:00Z"
                },
                "onDemandCap": { "val": 5000 },
                "onDemandUsed": { "val": 1200 },
                "prepaidBalance": { "val": 50 }
            }
        });
        let u = parse_xai_billing(&v);
        assert!(u.available);
        assert!(u.windows.iter().any(|w| w.id == "weekly"));
        let od = u.windows.iter().find(|w| w.id == "on_demand").unwrap();
        assert_eq!(od.used, Some(1200.0));
        assert_eq!(od.limit, Some(5000.0));
        let pre = u.windows.iter().find(|w| w.id == "prepaid").unwrap();
        assert_eq!(pre.limit, Some(50.0));
    }

    #[test]
    fn parse_xai_subscription_plan_label() {
        let v = json!({
            "subscriptions": [{
                "tier": "SUBSCRIPTION_TIER_GROK_PRO",
                "status": "SUBSCRIPTION_STATUS_ACTIVE"
            }]
        });
        assert_eq!(
            parse_xai_subscription_plan(&v).as_deref(),
            Some("SuperGrok")
        );
    }

    #[test]
    fn opencode_go_curated_lists_partition_by_protocol() {
        let openai = opencode_go_openai_models();
        let anth = opencode_go_anthropic_models();
        // Curated OpenCode Go docs map (2026-08) — OpenAI chat/completions.
        assert_eq!(
            openai.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec![
                "glm-5.2",
                "glm-5.1",
                "glm-5-turbo",
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "mimo-v2.5",
                "mimo-v2.5-pro",
            ]
        );
        assert_eq!(
            anth.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec![
                "minimax-m3",
                "minimax-m2.7",
                "minimax-m2.7-highspeed",
                "minimax-m2.5",
                "minimax-m2.5-highspeed",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-plus",
            ]
        );
        // no model appears in both lists (each routes to exactly one protocol)
        let mut all: Vec<String> = openai
            .iter()
            .chain(anth.iter())
            .map(|m| m.id.clone())
            .collect();
        all.sort();
        let mut deduped = all.clone();
        deduped.dedup();
        assert_eq!(
            all.len(),
            deduped.len(),
            "model id duplicated across protocols"
        );
        // OpenAI-served on OpenCode Go: no advertised thinking levels (host is
        // not first-party Kimi/Zhipu, so vendor fields never hit the wire).
        for m in &openai {
            assert!(
                m.thinking_levels.is_empty(),
                "OpenAI {} has thinking levels",
                m.id
            );
            assert!(!m.reasoning, "OpenAI {} marked reasoning", m.id);
            assert!(m.context_window > 0 && m.max_tokens > 0);
            assert!(m.max_tokens < m.context_window, "{}", m.id);
        }
        // Anthropic-served models: extended thinking enabled
        for m in &anth {
            assert!(
                !m.thinking_levels.is_empty(),
                "Anthropic {} has no thinking levels",
                m.id
            );
            assert!(m.reasoning, "Anthropic {} not marked reasoning", m.id);
            assert!(m.context_window > 0 && m.max_tokens > 0);
        }
        // Spot-check official caps.
        let m3 = anth.iter().find(|m| m.id == "minimax-m3").unwrap();
        assert_eq!(m3.context_window, 1_000_000);
        let k3 = openai.iter().find(|m| m.id == "kimi-k3").unwrap();
        assert_eq!(k3.context_window, 1_048_576);
        assert_eq!(k3.max_tokens, 131_072);
    }

    #[test]
    fn opencode_go_model_protocol_partitions_by_family() {
        // OpenAI chat/completions families (incl. ids the docs table hasn't
        // caught up to).
        for id in [
            "glm-5.2",
            "glm-5",
            "kimi-k2.7-code",
            "kimi-k2.5",
            "deepseek-v4-pro",
            "mimo-v2.5",
            "mimo-v2-omni",
        ] {
            assert_eq!(
                opencode_go_model_protocol(id),
                Some(true),
                "{id} should be OpenAI"
            );
        }
        // Anthropic /v1/messages families.
        for id in [
            "minimax-m3",
            "minimax-m2.7",
            "qwen3.7-max",
            "qwen3.5-plus",
            "qwen3.6-plus",
        ] {
            assert_eq!(
                opencode_go_model_protocol(id),
                Some(false),
                "{id} should be Anthropic"
            );
        }
        // Unknown family → None (dropped, not misrouted).
        assert_eq!(opencode_go_model_protocol("hy3-preview"), None);
    }

    #[test]
    fn opencode_go_filter_models_partitions_live_endpoint_payload() {
        // Shape returned by https://opencode.ai/zen/go/v1/models (OpenAI-style
        // {data:[{id,...}]}; no display name, no protocol field). Includes ids
        // beyond the docs table (kimi-k2.5, glm-5, qwen3.5-plus, mimo-v2-pro,
        // mimo-v2-omni) and one unknown-family id (hy3-preview).
        let payload = json!({
            "object": "list",
            "data": [
                {"id":"minimax-m3","object":"model","owned_by":"opencode"},
                {"id":"minimax-m2.7","object":"model","owned_by":"opencode"},
                {"id":"minimax-m2.5","object":"model","owned_by":"opencode"},
                {"id":"kimi-k2.7-code","object":"model","owned_by":"opencode"},
                {"id":"kimi-k2.6","object":"model","owned_by":"opencode"},
                {"id":"kimi-k2.5","object":"model","owned_by":"opencode"},
                {"id":"glm-5.2","object":"model","owned_by":"opencode"},
                {"id":"glm-5.1","object":"model","owned_by":"opencode"},
                {"id":"glm-5","object":"model","owned_by":"opencode"},
                {"id":"deepseek-v4-pro","object":"model","owned_by":"opencode"},
                {"id":"deepseek-v4-flash","object":"model","owned_by":"opencode"},
                {"id":"qwen3.7-max","object":"model","owned_by":"opencode"},
                {"id":"qwen3.7-plus","object":"model","owned_by":"opencode"},
                {"id":"qwen3.6-plus","object":"model","owned_by":"opencode"},
                {"id":"qwen3.5-plus","object":"model","owned_by":"opencode"},
                {"id":"mimo-v2-pro","object":"model","owned_by":"opencode"},
                {"id":"mimo-v2-omni","object":"model","owned_by":"opencode"},
                {"id":"mimo-v2.5-pro","object":"model","owned_by":"opencode"},
                {"id":"mimo-v2.5","object":"model","owned_by":"opencode"},
                {"id":"hy3-preview","object":"model","owned_by":"opencode"}
            ]
        });
        let openai = opencode_go_filter_models(&payload, true);
        let anth = opencode_go_filter_models(&payload, false);
        // OpenAI partition: glm/kimi/deepseek/mimo families (order preserved).
        assert_eq!(
            openai.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec![
                "kimi-k2.7-code",
                "kimi-k2.6",
                "kimi-k2.5",
                "glm-5.2",
                "glm-5.1",
                "glm-5",
                "deepseek-v4-pro",
                "deepseek-v4-flash",
                "mimo-v2-pro",
                "mimo-v2-omni",
                "mimo-v2.5-pro",
                "mimo-v2.5",
            ]
        );
        // Anthropic partition: minimax/qwen families.
        assert_eq!(
            anth.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec![
                "minimax-m3",
                "minimax-m2.7",
                "minimax-m2.5",
                "qwen3.7-max",
                "qwen3.7-plus",
                "qwen3.6-plus",
                "qwen3.5-plus",
            ]
        );
        // No overlap between partitions.
        let mut all: Vec<String> = openai
            .iter()
            .chain(anth.iter())
            .map(|m| m.id.clone())
            .collect();
        all.sort();
        let mut deduped = all.clone();
        deduped.dedup();
        assert_eq!(all.len(), deduped.len(), "id in both partitions");
        // hy3-preview (unknown family) is dropped, not misrouted.
        assert!(!openai.iter().any(|m| m.id == "hy3-preview"));
        assert!(!anth.iter().any(|m| m.id == "hy3-preview"));
        // Known ids keep their curated display name; new ids get a synthesized one.
        assert_eq!(
            openai.iter().find(|m| m.id == "glm-5.2").unwrap().name,
            "GLM-5.2"
        );
        assert_eq!(
            openai.iter().find(|m| m.id == "kimi-k2.5").unwrap().name,
            "Kimi K2.5"
        );
        assert_eq!(
            anth.iter().find(|m| m.id == "qwen3.5-plus").unwrap().name,
            "Qwen 3.5 Plus"
        );
        // Capabilities: OpenAI-served no reasoning; Anthropic-served have thinking.
        for m in &openai {
            assert!(
                m.thinking_levels.is_empty(),
                "OpenAI {} has thinking levels",
                m.id
            );
            assert!(!m.reasoning, "OpenAI {} marked reasoning", m.id);
        }
        for m in &anth {
            assert!(
                !m.thinking_levels.is_empty(),
                "Anthropic {} has no thinking levels",
                m.id
            );
            assert!(m.reasoning, "Anthropic {} not marked reasoning", m.id);
        }
        // Malformed payload → empty (no panic).
        assert!(opencode_go_filter_models(&json!({}), true).is_empty());
        assert!(opencode_go_filter_models(&json!({"data":[]}), true).is_empty());
    }

    #[test]
    fn opencode_go_display_name_synthesizes_unknown_ids() {
        // Known → curated exact name.
        assert_eq!(opencode_go_display_name("glm-5.2"), "GLM-5.2");
        assert_eq!(opencode_go_display_name("kimi-k2.7-code"), "Kimi K2.7 Code");
        assert_eq!(opencode_go_display_name("qwen3.7-max"), "Qwen3.7 Max");
        // Unknown → synthesized "Brand <Rest>".
        assert_eq!(opencode_go_display_name("kimi-k2.5"), "Kimi K2.5");
        assert_eq!(opencode_go_display_name("glm-5"), "GLM 5");
        assert_eq!(opencode_go_display_name("qwen3.5-plus"), "Qwen 3.5 Plus");
        assert_eq!(opencode_go_display_name("mimo-v2-omni"), "MiMo V2 Omni");
        // Totally unknown family → raw id.
        assert_eq!(opencode_go_display_name("hy3-preview"), "hy3-preview");
    }

    #[test]
    fn token_count_handles_int_float_and_string() {
        // integer (standard OpenAI)
        assert_eq!(token_count(&json!(1234)), Some(1234));
        // float — some proxies serialize counts as `100.0`
        assert_eq!(token_count(&json!(100.0)), Some(100));
        // quoted number
        assert_eq!(token_count(&json!("567")), Some(567));
        // absent / null / garbage
        assert_eq!(token_count(&Value::Null), None);
        assert_eq!(token_count(&json!("n/a")), None);
    }

    #[test]
    fn parse_http_date_known_epochs() {
        // P2-6: HTTP-date Retry-After parsing.
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        // 2025-01-01 00:00:00 UTC = 1735689600
        assert_eq!(
            parse_http_date("Wed, 01 Jan 2025 00:00:00 GMT"),
            Some(1735689600)
        );
        // weekday is ignored (servers sometimes send the wrong one)
        assert_eq!(
            parse_http_date("Mon, 01 Jan 2025 00:00:00 GMT"),
            Some(1735689600)
        );
    }

    #[test]
    fn parse_retry_after_int_seconds() {
        assert_eq!(parse_retry_after("5"), Some(5));
        assert_eq!(parse_retry_after("  10 "), Some(10));
        assert!(parse_retry_after("garbage").is_none());
    }

    #[test]
    fn sanitize_inserts_synthetic_results() {
        let mut msgs: Vec<Message> = vec![
            Message::user("hi"),
            Message::assistant_tool_calls(vec![crate::message::ToolCall {
                id: "call_1".into(),
                typ: "function".into(),
                function: crate::message::FunctionCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            }]),
        ];
        let n = sanitize_orphaned_tool_calls(&mut msgs);
        // a tool result for call_1 should now follow the assistant message
        let has_result = msgs
            .iter()
            .any(|m| m.is_tool() && m.tool_call_id() == Some("call_1"));
        assert!(has_result);
        assert_eq!(msgs.len(), 3);
        assert_eq!(n, 1, "should report 1 synthetic result inserted");
        let body = msgs[2].content_text().unwrap_or("");
        assert!(
            body.contains("tool result was lost"),
            "non-finish orphans keep the re-issue hint: {body}"
        );
    }

    #[test]
    fn sanitize_finish_orphan_does_not_ask_to_reissue() {
        // A prior turn that called `finish` without appending a tool result
        // must not get the generic "Re-issue" synthetic — models then ignore
        // the next user prompt and call finish again.
        let mut msgs: Vec<Message> = vec![
            Message::user("summarize the repo"),
            Message::assistant_tool_calls(vec![crate::message::ToolCall {
                id: "fin_1".into(),
                typ: "function".into(),
                function: crate::message::FunctionCall {
                    name: "finish".into(),
                    arguments: "{}".into(),
                },
            }]),
            Message::user("What tools do you have available?"),
        ];
        let n = sanitize_orphaned_tool_calls(&mut msgs);
        assert_eq!(n, 1);
        let body = msgs
            .iter()
            .find(|m| m.is_tool() && m.tool_call_id() == Some("fin_1"))
            .and_then(|m| m.content_text())
            .unwrap_or("");
        assert_eq!(body, crate::tools::FINISH_MESSAGE);
        assert!(
            !body.to_lowercase().contains("re-issue"),
            "finish orphan must not encourage re-issue: {body}"
        );
    }

    #[test]
    fn sanitize_drops_orphaned_results() {
        // Compaction kept a `tool` result whose matching assistant `tool_calls`
        // was dropped. The orphaned `tool` message must be removed (not left to
        // 400 the request), and no synthetic call is inserted (there's no call
        // to synthesize a result for).
        let mut msgs: Vec<Message> = vec![
            Message::user("hi"),
            Message::tool("ghost_call", "stale result"),
            Message::assistant("ok"),
        ];
        let n = sanitize_orphaned_tool_calls(&mut msgs);
        assert!(
            !msgs.iter().any(|m| m.is_tool()),
            "orphaned tool result should be dropped: {msgs:?}"
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(n, 1, "should report 1 orphaned result dropped");
    }

    #[test]
    fn sanitize_noop_when_results_present() {
        let mut msgs: Vec<Message> = vec![
            Message::assistant_tool_calls(vec![crate::message::ToolCall {
                id: "c1".into(),
                typ: "function".into(),
                function: crate::message::FunctionCall {
                    name: "x".into(),
                    arguments: "{}".into(),
                },
            }]),
            Message::tool("c1", "ok"),
        ];
        let n = sanitize_orphaned_tool_calls(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(n, 0, "clean conversation: no fixes");
    }

    #[test]
    fn sanitize_args_fixes_malformed_arguments() {
        let mut msgs: Vec<Message> = vec![
            Message::assistant_tool_calls(vec![
                crate::message::ToolCall {
                    id: "c1".into(),
                    typ: "function".into(),
                    function: crate::message::FunctionCall {
                        name: "bulk".into(),
                        arguments: "{broken json".into(),
                    },
                },
                crate::message::ToolCall {
                    id: "c2".into(),
                    typ: "function".into(),
                    function: crate::message::FunctionCall {
                        name: "bash".into(),
                        arguments: "{\"command\":\"echo hi\"}".into(),
                    },
                },
                crate::message::ToolCall {
                    id: "c3".into(),
                    typ: "function".into(),
                    function: crate::message::FunctionCall {
                        name: "bulk".into(),
                        arguments: "{\"calls\":[{\"name\":\"bash\",\"args\":{\"command\":\"echo '"
                            .into(),
                    },
                },
            ]),
            Message::tool("c1", "err"),
            Message::tool("c2", "ok"),
            Message::tool("c3", "err"),
        ];
        let n = sanitize_tool_call_arguments(&mut msgs);
        assert_eq!(n, 2, "only the two malformed calls should be fixed");
        let calls = msgs[0].tool_calls().unwrap();
        assert_eq!(calls[0].function.arguments, "{}");
        assert_eq!(calls[1].function.arguments, "{\"command\":\"echo hi\"}");
        assert_eq!(calls[2].function.arguments, "{}");
        // every arguments field must now be valid JSON
        for tc in calls {
            serde_json::from_str::<Value>(&tc.function.arguments).unwrap();
        }
    }

    #[test]
    fn sanitize_args_coerces_non_json_arguments() {
        // A tool call with garbage arguments (not valid JSON at all)
        // gets fixed to "{}".
        let mut msgs: Vec<Message> = vec![Message::assistant_tool_calls(vec![
            crate::message::ToolCall {
                id: "c1".into(),
                typ: "function".into(),
                function: crate::message::FunctionCall {
                    name: "bash".into(),
                    arguments: "not valid json".into(),
                },
            },
        ])];
        let n = sanitize_tool_call_arguments(&mut msgs);
        assert_eq!(n, 1);
        let args = &msgs[0].tool_calls().unwrap()[0].function.arguments;
        assert_eq!(args, "{}");
    }

    #[test]
    fn sanitize_args_skips_non_assistant_messages() {
        let mut msgs: Vec<Message> = vec![
            Message::user("hi"),
            Message::tool("x", "{not real json but role is tool}"),
        ];
        assert_eq!(sanitize_tool_call_arguments(&mut msgs), 0);
    }

    #[test]
    fn backoff_progression() {
        assert_eq!(backoff_ms(1, None), 500);
        assert_eq!(backoff_ms(2, None), 1000);
        assert_eq!(backoff_ms(3, None), 2000);
        assert_eq!(backoff_ms(4, None), 4000);
        assert_eq!(backoff_ms(8, None), 8000); // capped
        assert_eq!(backoff_ms(2, Some(3)), 3000); // Retry-After honored
        assert_eq!(backoff_ms(2, Some(60)), 30000); // Retry-After capped at 30s
        assert_eq!(backoff_ms(1, Some(0)), 50); // zero Retry-After floored
    }

    #[test]
    fn retryable_http_status_policy() {
        use reqwest::StatusCode;
        // 408 + 5xx still retry. 429 rate-limit is now fail-fast (balance/rate denylist).
        assert!(!is_retryable_http_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_http_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_http_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_http_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_http_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_http_status(StatusCode::GATEWAY_TIMEOUT));
        // Permanent client errors must fail fast.
        assert!(!is_retryable_http_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_http_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_http_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_http_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_http_status(StatusCode::UNPROCESSABLE_ENTITY));
        assert!(!is_retryable_http_status(StatusCode::PAYMENT_REQUIRED));
        assert!(!is_retryable_http_status(StatusCode::OK));
    }
    #[test]
    fn provider_retry_delay_accepts_millisecond_headers() {
        assert_eq!(parse_retry_after_ms("125.1"), Some(126));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        assert!(parse_retry_after_ms("-1").is_none());
        assert_eq!(parse_retry_after_seconds("1.25"), Some(2));
        assert_eq!(
            parse_retry_after_seconds("1.25").map(|seconds| seconds * 1000),
            Some(2000)
        );
        assert!(parse_retry_after_ms("not-a-delay").is_none());
        assert_eq!(backoff_with_provider_delay(1, Some(125)), 125);
        assert_eq!(backoff_with_provider_delay(1, Some(0)), 50);
        assert_eq!(backoff_with_provider_delay(1, Some(100_000)), 60_000);
    }

    #[test]
    fn retryable_stream_failure_policy() {
        // The exact error chain from h2/hyper mid-SSE body drops (seen on
        // karutoil/ck-grok): must be retryable even after partial output.
        let h2_drop = "stream read: error decoding response body -> error reading a body from connection -> stream error received: unexpected internal error encountered";
        assert!(is_retryable_stream_failure(h2_drop));
        assert!(should_retry_stream_attempt(h2_drop, true, 1, 3));
        assert!(should_retry_stream_attempt(h2_drop, false, 1, 3));
        assert!(!should_retry_stream_attempt(h2_drop, true, 3, 3));

        assert!(is_retryable_stream_failure(
            "stream read: error decoding response body -> error reading a body from connection -> unexpected EOF during chunk size line"
        ));
        assert!(is_retryable_stream_failure("HTTP 504: gateway timeout"));
        assert!(is_retryable_stream_failure(
            "provider stream error: 504 Gateway Timeout"
        ));
        assert!(is_retryable_stream_failure(
            "provider stream error: overloaded"
        ));
        assert!(is_retryable_stream_failure("connection reset by peer"));

        // Idle timeout is retryable even after partial tokens (provider hung
        // mid-stream; discard_partial prevents duplicate visible output).
        assert!(is_retryable_stream_failure(
            "stream idle timeout (120s with no data)"
        ));
        assert!(should_retry_stream_attempt(
            "stream idle timeout (120s with no data)",
            true,
            1,
            3
        ));
        assert!(should_retry_stream_attempt(
            "stream idle timeout (90s with no data)",
            false,
            1,
            3
        ));
        assert!(!should_retry_stream_attempt(
            "stream idle timeout (120s with no data)",
            true,
            3,
            3
        ));

        // Non-transient: stay fatal once tokens were shown.
        assert!(!is_retryable_stream_failure(
            "malformed provider stream event: {not json"
        ));
        assert!(!is_retryable_stream_failure(
            "HTTP 401: invalid credentials"
        ));
        // Mid-stream auth frames (e.g. Cursor SDK) must fail fast — never burn
        // retries or leave a multi-accept mock hanging on server.await.
        assert!(!is_retryable_stream_failure(
            "provider stream error: Cursor SDK authentication failed"
        ));
        assert!(!should_retry_stream_attempt(
            "provider stream error: Cursor SDK authentication failed",
            false,
            1,
            3
        ));
        assert!(!should_retry_stream_attempt(
            "malformed provider stream event: nope",
            true,
            1,
            3
        ));

        // Rate limits and balance issues fail fast even before first token.
        assert!(!is_retryable_stream_failure(
            "HTTP 429: rate limit exceeded"
        ));
        assert!(!should_retry_stream_attempt(
            "rate limit exceeded — too many requests",
            false,
            1,
            3
        ));
        assert!(!should_retry_stream_attempt(
            "insufficient credits — top up your balance",
            false,
            1,
            3
        ));
        assert!(!should_retry_stream_attempt("payment required", true, 1, 3));

        // Unclassified provider stream errors now retry (denylist policy).
        assert!(is_retryable_stream_failure(
            "provider stream error: upstream hiccup, try again"
        ));
        assert!(should_retry_stream_attempt(
            "provider stream error: upstream hiccup, try again",
            true,
            1,
            3
        ));
        // Incidental bare tokens in retryable copy must not force FatalError.
        assert!(is_retryable_stream_failure(
            "provider stream error: upstream said forbidden briefly; retry"
        ));
        assert!(is_retryable_stream_failure(
            "provider stream error: transient authentication hiccup from gateway"
        ));
        // Specific auth/context phrases remain non-retryable.
        assert!(!is_retryable_stream_failure(
            "provider stream error: prompt is too long for this model"
        ));
    }

    #[test]
    fn is_umans_detection() {
        assert!(is_umans("https://api.code.umans.ai/v1"));
        assert!(is_umans("https://umans.ai/v1"));
        assert!(!is_umans("https://api.openai.com/v1"));
        assert!(!is_umans("https://localhost:11434/v1"));
        // Look-alike host must NOT be detected (substring `.contains` false-pos):
        // `api.umans.ai.evil.com` is not a subdomain of umans.ai.
        assert!(!is_umans("https://api.umans.ai.evil.com/v1"));
        assert!(!is_umans("https://umans.ai.evil.com/v1"));
        // port suffix is handled
        assert!(is_umans("https://api.umans.ai:443/v1"));
    }

    #[test]
    fn is_kimi_detection() {
        assert!(is_kimi("https://api.kimi.com/coding/v1"));
        assert!(is_kimi("https://api.kimi.com/v1"));
        assert!(is_kimi("https://api.kimi.com:443/coding/v1"));
        assert!(is_kimi("https://api.moonshot.ai/v1"));
        assert!(!is_kimi("https://api.openai.com/v1"));
        // look-alike host must NOT match (substring false-positive guard)
        assert!(!is_kimi("https://api.kimi.com.evil.com/v1"));
        assert!(!is_kimi("https://kimi.com/v1"));
        assert!(!is_kimi("https://evil.moonshot.ai/v1"));
    }

    #[test]
    fn is_deepseek_detection() {
        assert!(is_deepseek("https://api.deepseek.com"));
        assert!(is_deepseek("https://api.deepseek.com/v1"));
        assert!(is_deepseek("https://api.deepseek.com:443/beta"));
        assert!(!is_deepseek("https://deepseek.com/v1"));
        assert!(!is_deepseek("https://api.deepseek.com.evil.com/v1"));
        assert!(!is_deepseek("https://api.openai.com/v1"));
    }

    #[test]
    fn is_zhipu_and_minimax_detection() {
        assert!(is_zhipu("https://api.z.ai/api/paas/v4"));
        assert!(is_zhipu("https://open.bigmodel.cn/api/paas/v4"));
        assert!(!is_zhipu("https://api.z.ai.evil.com/v1"));
        assert!(!is_zhipu("https://evil.z.ai/v1"));
        assert!(!is_zhipu("https://api.openai.com/v1"));

        assert!(is_minimax("https://api.minimax.io/v1"));
        assert!(is_minimax("https://api.minimaxi.com/v1"));
        assert!(!is_minimax("https://api.minimax.io.evil.com/v1"));
        assert!(!is_minimax("https://evil.minimax.io/v1"));
        assert!(!is_minimax("https://api.openai.com/v1"));
    }

    #[test]
    fn think_tag_demux_splits_complete_and_partial_tags() {
        let mut d = ThinkTagDemux::default();
        // Opening tag split across chunks.
        assert!(d.push("<thi").is_empty());
        let p1 = d.push("nk>\nstep one");
        assert_eq!(p1, vec![ThinkPiece::Thinking("\nstep one".into())]);
        let p2 = d.push(" continues</th");
        assert_eq!(p2, vec![ThinkPiece::Thinking(" continues".into())]);
        let p3 = d.push("ink>\n\n## Answer\nHi");
        assert_eq!(p3, vec![ThinkPiece::Text("\n\n## Answer\nHi".into())]);
        assert!(d.finish().is_empty());
    }

    #[test]
    fn think_tag_demux_multibyte_tail_does_not_panic() {
        // Stream chunk ending on a multi-byte UTF-8 char (U+2019 ’) used to
        // panic in open_tag_hold_start when probing non-boundary hold lengths:
        // "start byte index N is not a char boundary; it is inside '’'".
        let mut d = ThinkTagDemux::default();
        let pieces = d.push("user’s request");
        assert_eq!(pieces, vec![ThinkPiece::Text("user’s request".into())]);
        // Inside thinking with a multi-byte trailing char + partial close tag.
        let mut d = ThinkTagDemux::default();
        let p1 = d.push("<think>cafés");
        assert_eq!(p1, vec![ThinkPiece::Thinking("cafés".into())]);
        // Hold a partial close across a multi-byte boundary in the next chunk.
        let p2 = d.push("…</th");
        assert_eq!(p2, vec![ThinkPiece::Thinking("…".into())]);
        let p3 = d.push("ink>done");
        assert_eq!(p3, vec![ThinkPiece::Text("done".into())]);
        assert!(d.finish().is_empty());
    }

    #[test]
    fn think_tag_demux_plain_text_passthrough() {
        let mut d = ThinkTagDemux::default();
        assert_eq!(d.push("hello "), vec![ThinkPiece::Text("hello ".into())]);
        assert_eq!(d.push("world"), vec![ThinkPiece::Text("world".into())]);
    }

    #[test]
    fn think_tag_demux_drops_stray_close_tags() {
        let mut d = ThinkTagDemux::default();
        let pieces = d.push("<think>step</think>\n\n</think>\nAnswer");
        let mut thinking = String::new();
        let mut text = String::new();
        for p in pieces.into_iter().chain(d.finish()) {
            match p {
                ThinkPiece::Thinking(t) => thinking.push_str(&t),
                ThinkPiece::Text(t) => text.push_str(&t),
            }
        }
        assert_eq!(thinking, "step");
        assert_eq!(text, "\n\n\nAnswer");
        assert!(!text.contains("</think>"));
    }

    #[test]
    fn is_minimax_model_detection() {
        assert!(is_minimax_model("minimax-m3"));
        assert!(is_minimax_model("MiniMax-M2.5"));
        assert!(is_minimax_model("umans-minimax-m3"));
        assert!(!is_minimax_model("glm-5.2"));
        assert!(!is_minimax_model("kimi-k3"));
    }

    #[test]
    fn cumulative_wire_text_then_demux_does_not_duplicate() {
        // Proxy MiniMax streams full content snapshots each chunk. De-cumulate
        // first, then demux — otherwise thinking is re-parsed and duplicated.
        let mut wire = String::new();
        let mut demux = ThinkTagDemux::default();
        let mut thinking = String::new();
        let mut content = String::new();
        let snapshots = [
            "<think>\nThe user",
            "<think>\nThe user is asking",
            "<think>\nThe user is asking what.\n</think>\n\n</think>\n\nHello",
        ];
        for snap in snapshots {
            let Some(suffix) = append_stream_fragment(&mut wire, snap, true) else {
                continue;
            };
            for piece in demux.push(&suffix) {
                match piece {
                    ThinkPiece::Thinking(t) => thinking.push_str(&t),
                    ThinkPiece::Text(t) => content.push_str(&t),
                }
            }
        }
        for piece in demux.finish() {
            match piece {
                ThinkPiece::Thinking(t) => thinking.push_str(&t),
                ThinkPiece::Text(t) => content.push_str(&t),
            }
        }
        sanitize_assistant_content(&mut content);
        assert_eq!(thinking, "\nThe user is asking what.\n");
        assert!(
            !thinking.contains("The userThe user"),
            "duplicated thinking: {thinking}"
        );
        assert_eq!(content, "Hello");
        assert!(!content.contains("</think>"));
    }

    #[test]
    fn sanitize_assistant_content_strips_residual_tags() {
        let mut s = "\n\n</think>".to_string();
        sanitize_assistant_content(&mut s);
        assert!(s.is_empty());
        let mut s2 = "Hi <think>x</think> there</think>".to_string();
        sanitize_assistant_content(&mut s2);
        assert_eq!(s2, "Hi  there");
    }

    #[test]
    fn append_stream_fragment_handles_cumulative_and_delta() {
        // Pure incremental: repeated prefix tokens must NOT be dropped.
        let mut inc = String::new();
        assert_eq!(
            append_stream_fragment(&mut inc, "1", false).as_deref(),
            Some("1")
        );
        assert_eq!(
            append_stream_fragment(&mut inc, "1", false).as_deref(),
            Some("1")
        );
        assert_eq!(inc, "11");
        assert_eq!(
            append_stream_fragment(&mut inc, " 22", false).as_deref(),
            Some(" 22")
        );
        assert_eq!(inc, "11 22");

        // MiniMax cumulative snapshot: full prefix repeated + new suffix.
        let mut acc = String::new();
        assert_eq!(
            append_stream_fragment(&mut acc, "think", true).as_deref(),
            Some("think")
        );
        assert_eq!(acc, "think");
        assert_eq!(
            append_stream_fragment(&mut acc, "thinking", true).as_deref(),
            Some("ing")
        );
        assert_eq!(acc, "thinking");
        // Identical cumulative frame → no emission.
        assert_eq!(append_stream_fragment(&mut acc, "thinking", true), None);
        // Non-prefix extension still appends under cumulative mode.
        assert_eq!(
            append_stream_fragment(&mut acc, " more", true).as_deref(),
            Some(" more")
        );
        assert_eq!(acc, "thinking more");
        // Shorter cumulative rewind is ignored.
        assert_eq!(append_stream_fragment(&mut acc, "think", true), None);
        assert_eq!(acc, "thinking more");
    }

    #[test]
    fn cursor_bridge_detection_is_loopback_and_path_scoped() {
        assert!(is_cursor_bridge("http://127.0.0.1:8788/cursor/v1"));
        assert!(is_cursor_bridge("http://localhost:8788/cursor/v1/"));
        assert!(is_cursor_bridge("http://[::1]:8788/cursor/v1"));
        assert!(!is_cursor_bridge("http://127.0.0.1:8788/v1"));
        assert!(!is_cursor_bridge("https://cursor-bridge.example/cursor/v1"));
        assert!(!is_cursor_bridge("not a URL"));
    }

    #[test]
    fn is_xai_endpoint_detection() {
        assert!(is_xai_endpoint("https://api.x.ai/v1"));
        assert!(is_xai_endpoint("https://api.x.ai/v1/"));
        assert!(is_xai_endpoint("https://x.ai/v1"));
        assert!(!is_xai_endpoint("https://api.openai.com/v1"));
        assert!(!is_xai_endpoint("https://api.x.ai.evil.com/v1"));
    }

    #[test]
    fn parse_xai_models_list_uses_live_context_and_filters_media() {
        let data = json!({
            "data": [
                {
                    "id": "grok-4.5",
                    "context_length": 500000,
                    "completion_text_token_price": 60000,
                    "prompt_image_token_price": 20000
                },
                {
                    "id": "grok-build-0.1",
                    "context_length": 256000,
                    "completion_text_token_price": 20000,
                    "prompt_image_token_price": 10000
                },
                {
                    "id": "grok-4.20-0309-non-reasoning",
                    "context_length": 1000000,
                    "completion_text_token_price": 25000,
                    "prompt_image_token_price": 12500
                },
                {
                    "id": "grok-imagine-image",
                    "context_length": 8000,
                    "image_price": 200000000
                },
                {
                    "id": "grok-imagine-video-1.5",
                    "owned_by": "xai"
                }
            ]
        });
        let models = parse_xai_models_list(&data);
        assert_eq!(models.len(), 3, "media models filtered: {:?}", models);
        // Coding default pinned first.
        assert_eq!(models[0].id, "grok-build-0.1");
        assert_eq!(models[0].context_window, 256_000);
        assert!(models[0].reasoning);
        assert!(models[0].vision);
        assert!(!models[0].thinking_levels.is_empty());

        let g45 = models.iter().find(|m| m.id == "grok-4.5").unwrap();
        assert_eq!(g45.context_window, 500_000);
        assert!(g45.vision);

        let non = models
            .iter()
            .find(|m| m.id == "grok-4.20-0309-non-reasoning")
            .unwrap();
        assert_eq!(non.context_window, 1_000_000);
        assert!(!non.reasoning);
        assert!(non.thinking_levels.is_empty());

        // No media models.
        assert!(models.iter().all(|m| !m.id.contains("imagine")));
    }

    #[test]
    fn apply_xai_language_models_enrichment_filters_and_sets_vision() {
        let mut models = vec![
            xai_model_caps("grok-build-0.1", "grok-build-0.1"),
            xai_model_caps("mystery-not-in-lang", "mystery"),
        ];
        let mut lang = std::collections::HashMap::new();
        lang.insert("grok-build-0.1".into(), true);
        apply_xai_language_models_enrichment(&mut models, &lang);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "grok-build-0.1");
        assert!(models[0].vision);
    }

    #[test]
    fn apply_live_model_fields_overlays_context_window() {
        let mut info = openai_model_caps("unknown-model", "unknown-model");
        assert_eq!(info.context_window, 200_000); // curated default
        apply_live_model_fields(
            &json!({"context_length": 750000, "prompt_image_token_price": 1}),
            &mut info,
        );
        assert_eq!(info.context_window, 750_000);
        assert!(info.vision);
    }

    #[test]
    fn apply_context_window_override_forces_local_context() {
        // Simulates a gemma model served by LM Studio: its `/v1/models` returns
        // bare ids, so openai_model_caps() has no gemma branch and falls to the
        // 200k default — which would oversend past a 32k loaded context.
        let provider = ResolvedProvider {
            name: "lmstudio".into(),
            kind: ProviderKind::OpenAI,
            base_url: "http://localhost:1234/v1".into(),
            api_key: None,
            headers: Vec::new(),
            oauth: false,
            context_window: Some(32_768),
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let mut models = vec![openai_model_caps("gemma-3-12b-it", "Gemma 3 12B")];
        assert_eq!(models[0].context_window, 200_000); // no gemma branch -> default
        apply_context_window_override(&provider, &mut models);
        assert_eq!(models[0].context_window, 32_768);
        // No override on the provider leaves discovered caps untouched.
        let none_provider = ResolvedProvider {
            context_window: None,
            ..provider
        };
        let mut m2 = vec![openai_model_caps("gemma-3-12b-it", "Gemma 3 12B")];
        apply_context_window_override(&none_provider, &mut m2);
        assert_eq!(m2[0].context_window, 200_000);
    }

    #[test]
    fn apply_models_override_refines_individual_models() {
        // A bare-id endpoint (e.g. LM Studio) leaves models on the 200k/8k
        // flat default. Per-model overrides refine ONLY the matched id + the
        // fields set; everything else keeps discovered/default caps.
        use crate::config::ModelOverride;
        let provider = ResolvedProvider {
            name: "lmstudio".into(),
            kind: ProviderKind::OpenAI,
            base_url: "http://localhost:1234/v1".into(),
            api_key: None,
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: vec![
                ModelOverride {
                    id: "gemma-3-12b-it".into(),
                    context_window: Some(32_768),
                    max_tokens: Some(4_096),
                    reasoning: Some(true),
                    thinking_levels: Some(vec!["low".into(), "high".into()]),
                    ..Default::default()
                },
                // An override for a model NOT in the discovered list is a no-op.
                ModelOverride {
                    id: "nonexistent".into(),
                    context_window: Some(999_999),
                    ..Default::default()
                },
            ],
            models_endpoint: None,
        };
        let mut models = vec![
            openai_model_caps("gemma-3-12b-it", "Gemma 3 12B"),
            openai_model_caps("qwen3-8b", "Qwen3 8B"),
        ];
        assert_eq!(models[0].context_window, 200_000); // no gemma branch -> default
        apply_models_override(&provider, &mut models);
        // Matched model refined on every field.
        assert_eq!(models[0].context_window, 32_768);
        assert_eq!(models[0].max_tokens, 4_096);
        assert!(models[0].reasoning);
        assert_eq!(models[0].thinking_levels, vec!["low", "high"]);
        // Unmatched model keeps its discovered/default caps (the 200k default).
        assert_eq!(models[1].context_window, 200_000);
        // An empty thinking_levels vec clears reasoning (model declares none).
        let p2 = ResolvedProvider {
            models_override: vec![ModelOverride {
                id: "qwen3-8b".into(),
                thinking_levels: Some(Vec::new()),
                ..Default::default()
            }],
            ..provider
        };
        apply_models_override(&p2, &mut models);
        assert!(!models[1].reasoning);
        assert!(models[1].thinking_levels.is_empty());
    }

    #[test]
    fn apply_live_model_fields_reads_cursor_reasoning_metadata() {
        let mut info = openai_model_caps("cursor-model", "Cursor Model");
        apply_live_model_fields(
            &json!({
                "reasoning": true,
                "reasoning_levels": ["low", "high", ""]
            }),
            &mut info,
        );
        assert!(info.reasoning);
        assert_eq!(info.thinking_levels, vec!["low", "high"]);
    }

    #[test]
    fn live_model_list_metadata_wins_after_registry_enrichment() {
        let mut models = vec![ModelInfo {
            id: "cursor-claude".into(),
            name: "Claude".into(),
            reasoning: false,
            context_window: 200_000,
            max_tokens: 8_192,
            ..Default::default()
        }];
        apply_live_model_list_fields(
            &json!({"data": [{
                "id": "cursor-claude",
                "reasoning": true,
                "reasoning_levels": ["low", "high"],
                "context_window": 1_000_000
            }]}),
            &mut models,
        );
        assert!(models[0].reasoning);
        assert_eq!(models[0].thinking_levels, vec!["low", "high"]);
        assert_eq!(models[0].context_window, 1_000_000);
    }

    #[test]
    fn resolve_effort_passthrough_when_no_levels() {
        assert_eq!(resolve_effort("medium", &[]), "medium");
        assert_eq!(resolve_effort("banana", &[]), "banana");
    }

    #[test]
    fn resolve_effort_keeps_supported_case_insensitive() {
        let levels = vec!["Low".into(), "Medium".into(), "High".into()];
        // supported → preserved, but returns the model's own casing
        assert_eq!(resolve_effort("medium", &levels), "Medium");
        assert_eq!(resolve_effort("HIGH", &levels), "High");
    }

    #[test]
    fn resolve_effort_clamps_unsupported_to_preferred() {
        let levels = vec!["low".into(), "medium".into(), "high".into()];
        // unknown effort → prefers high, then medium, then low
        assert_eq!(resolve_effort("max", &levels), "high");
        assert_eq!(resolve_effort("turbo", &levels), "high");
    }

    #[test]
    fn resolve_effort_glm_only_high() {
        // GLM advertises only "high": anything else clamps to it.
        let levels = vec!["high".into()];
        assert_eq!(resolve_effort("medium", &levels), "high");
        assert_eq!(resolve_effort("low", &levels), "high");
        assert_eq!(resolve_effort("high", &levels), "high");
    }

    #[test]
    fn resolve_effort_custom_levels_no_high() {
        // A model that only exposes low+medium: unknown → medium (preferred).
        let levels = vec!["low".into(), "medium".into()];
        assert_eq!(resolve_effort("high", &levels), "medium");
        assert_eq!(resolve_effort("zzz", &levels), "medium");
    }

    #[test]
    fn fallback_models_advertise_levels() {
        let models = fallback_models();
        // every fallback entry has at least one thinking level
        assert!(models.iter().all(|m| !m.thinking_levels.is_empty()));
        // Legacy GLM entries advertise only "high"; GLM-5.2 advertises high/max.
        for m in models.iter().filter(|m| m.id.contains("glm")) {
            if m.id.contains("glm-5.2") {
                assert_eq!(
                    m.thinking_levels,
                    vec!["high".to_string(), "max".to_string()]
                );
            } else {
                assert_eq!(m.thinking_levels, vec!["high".to_string()]);
            }
        }
        // a non-GLM model advertises the standard trio
        let coder = models.iter().find(|m| m.id == "umans-coder").unwrap();
        assert_eq!(
            coder.thinking_levels,
            vec!["low".to_string(), "medium".to_string(), "high".to_string()]
        );
        let m3 = models.iter().find(|m| m.id == "umans-minimax-m3").unwrap();
        assert_eq!(m3.context_window, 1_000_000);
        assert_eq!(m3.max_tokens, 128_000);
    }

    #[test]
    fn parse_models_response_reads_vision_flag() {
        // The endpoint exposes vision as capabilities.supports_vision, encoded
        // as true / false / "via-handoff". Only boolean true counts as native
        // client-side vision; "via-handoff" (vision only on /v1/messages, which
        // the harness doesn't use) falls through to false.
        let data = json!({
            "vision-model": { "display_name": "Vision", "capabilities": { "context_window": 128000, "recommended_max_tokens": 4096, "supports_vision": true } },
            "text-model": { "display_name": "Text", "capabilities": { "context_window": 128000, "recommended_max_tokens": 4096, "supports_vision": false } },
            "handoff-model": { "display_name": "Handoff", "capabilities": { "context_window": 128000, "recommended_max_tokens": 4096, "supports_vision": "via-handoff" } },
            "unspecified": { "display_name": "Unspec", "capabilities": { "context_window": 128000 } }
        });
        let models = parse_models_response(&data);
        let by_id: std::collections::HashMap<&str, &ModelInfo> =
            models.iter().map(|m| (m.id.as_str(), m)).collect();
        assert!(by_id["vision-model"].vision);
        assert!(!by_id["text-model"].vision);
        assert!(!by_id["handoff-model"].vision); // "via-handoff" is not native client-side vision
        assert!(!by_id["unspecified"].vision); // default false when absent
    }

    #[test]
    fn parse_codex_models_response_uses_subscription_catalog() {
        let data = json!({
            "models": [
                {
                    "slug": "chatgpt-remote-only",
                    "display_name": "ChatGPT Remote Only",
                    "supported_in_api": true,
                    "supported_reasoning_levels": [
                        {"effort": "max", "description": "Maximum"},
                        {"effort": "focused", "description": "Focused"}
                    ],
                    "context_window": 272000,
                    "supports_image_detail_original": true
                },
                {"slug": "hidden", "supported_in_api": false}
            ]
        });
        let models = parse_codex_models_response(&data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "chatgpt-remote-only");
        assert_eq!(models[0].name, "ChatGPT Remote Only");
        assert_eq!(models[0].context_window, 272000);
        assert_eq!(
            models[0].thinking_levels,
            vec!["max".to_string(), "focused".to_string()]
        );
        assert!(models[0].vision);
    }

    #[test]
    fn modelinfo_vision_defaults_false_when_absent() {
        let j = r#"{"id":"x","name":"X","context_window":1,"max_tokens":1}"#;
        let m: ModelInfo = serde_json::from_str(j).unwrap();
        assert!(!m.vision);
        let j2 = r#"{"id":"x","name":"X","context_window":1,"max_tokens":1,"vision":true}"#;
        let m2: ModelInfo = serde_json::from_str(j2).unwrap();
        assert!(m2.vision);
    }

    #[test]
    fn parse_models_response_reads_reasoning_levels_nested() {
        // The live /models/info endpoint nests reasoning levels under
        // capabilities.reasoning.levels (not a flat capabilities.thinking_levels).
        let data = json!({
            "umans-glm-5.2": { "display_name": "Umans GLM 5.2", "capabilities": {
                "context_window": 405504, "recommended_max_tokens": 131071,
                "reasoning": { "supported": true, "can_disable": true, "levels": ["none","high","max"], "default_level": "high" }
            }},
            "umans-flash": { "display_name": "Umans Flash", "capabilities": {
                "context_window": 262144, "recommended_max_tokens": 32768,
                "reasoning": { "supported": true, "can_disable": true, "levels": ["none","low","medium","high"], "default_level": "medium" }
            }},
            "umans-kimi-k2.7": { "display_name": "Umans Kimi K2.7", "capabilities": {
                "context_window": 262144, "recommended_max_tokens": 32768,
                "reasoning": { "supported": true, "can_disable": false, "levels": [], "default_level": null }
            }}
        });
        let models = parse_models_response(&data);
        let by_id: std::collections::HashMap<&str, &ModelInfo> =
            models.iter().map(|m| (m.id.as_str(), m)).collect();
        assert_eq!(
            by_id["umans-glm-5.2"].thinking_levels,
            vec!["none".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(
            by_id["umans-flash"].thinking_levels,
            vec![
                "none".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string()
            ]
        );
        assert!(by_id["umans-kimi-k2.7"].thinking_levels.is_empty());
        // reasoning flag follows reasoning.supported
        assert!(by_id["umans-glm-5.2"].reasoning);
        assert!(by_id["umans-kimi-k2.7"].reasoning);
    }

    #[test]
    fn parse_models_response_reasoning_supported_false() {
        let data = json!({
            "no-think": { "display_name": "No Think", "capabilities": {
                "context_window": 128000, "recommended_max_tokens": 4096,
                "reasoning": { "supported": false, "levels": [] }
            }}
        });
        let models = parse_models_response(&data);
        assert!(!models[0].reasoning);
        assert!(models[0].thinking_levels.is_empty());
    }

    #[test]
    fn parse_models_response_flat_levels_fallback() {
        // Endpoints that expose levels as a flat capability field still parse.
        let data = json!({
            "flat-model": { "display_name": "Flat", "capabilities": {
                "context_window": 128000, "recommended_max_tokens": 4096,
                "reasoning_levels": ["low","high"]
            }}
        });
        let models = parse_models_response(&data);
        assert_eq!(
            models[0].thinking_levels,
            vec!["low".to_string(), "high".to_string()]
        );
    }

    #[test]
    fn cache_version_gate() {
        // A cache with the current version is accepted.
        assert!(cache_version_ok(
            &json!({ "version": MODELS_CACHE_VERSION })
        ));
        // A pre-versioning cache (no version field) is rejected so a parser fix
        // isn't masked by stale data for the TTL window.
        assert!(!cache_version_ok(
            &json!({ "base_url": "x", "updated_at": 0 })
        ));
        // A future / mismatched version is rejected.
        assert!(!cache_version_ok(&json!({ "version": 99 })));
    }

    // ---- Anthropic translation ----

    #[test]
    fn anthropic_thinking_budget_maps_and_clamps() {
        // effort -> budget
        assert_eq!(anthropic_thinking_budget("low", 100_000), Some(4096));
        assert_eq!(anthropic_thinking_budget("medium", 100_000), Some(12288));
        assert_eq!(anthropic_thinking_budget("HIGH", 100_000), Some(24576));
        assert_eq!(anthropic_thinking_budget("max", 100_000), Some(24576));
        // unsupported effort -> no thinking
        assert_eq!(anthropic_thinking_budget("none", 100_000), None);
        assert_eq!(anthropic_thinking_budget("bogus", 100_000), None);
        // clamp to max_tokens-1024 when base exceeds it
        assert_eq!(anthropic_thinking_budget("high", 20000), Some(18976));
        // base below the cap passes through unchanged
        assert_eq!(anthropic_thinking_budget("high", 30000), Some(24576));
        // too small to leave room -> None
        assert_eq!(anthropic_thinking_budget("low", 2000), None);
        assert_eq!(anthropic_thinking_budget("high", 1500), None);
    }

    #[test]
    fn anthropic_auth_helpers_reject_empty_and_whitespace_keys() {
        assert_eq!(anthropic_nonempty_key(None), None);
        assert_eq!(anthropic_nonempty_key(Some("")), None);
        assert_eq!(anthropic_nonempty_key(Some("   ")), None);
        assert_eq!(anthropic_nonempty_key(Some(" sk-live ")), Some("sk-live"));
        assert!(is_anthropic_primary_auth_header("Authorization"));
        assert!(is_anthropic_primary_auth_header("X-Api-Key"));
        assert!(!is_anthropic_primary_auth_header("anthropic-beta"));
        assert!(!is_anthropic_primary_auth_header("user-agent"));
    }

    #[test]
    fn anthropic_image_block_data_url_and_plain_url() {
        let b = anthropic_image_block("data:image/png;base64,QUJD").unwrap();
        assert_eq!(b["type"], "image");
        assert_eq!(b["source"]["type"], "base64");
        assert_eq!(b["source"]["media_type"], "image/png");
        assert_eq!(b["source"]["data"], "QUJD");
        let b = anthropic_image_block("https://x.test/cat.png").unwrap();
        assert_eq!(b["source"]["type"], "url");
        assert_eq!(b["source"]["url"], "https://x.test/cat.png");
    }

    #[test]
    fn anthropic_content_blocks_string_and_multimodal() {
        // plain string -> single text block
        let b = anthropic_content_blocks(&json!("hi"));
        assert_eq!(b, vec![json!({ "type": "text", "text": "hi" })]);
        // multimodal: text + base64 image
        let content = json!([
            { "type": "text", "text": "look" },
            { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,ZGF0YQ==" } }
        ]);
        let b = anthropic_content_blocks(&content);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0]["type"], "text");
        assert_eq!(b[1]["type"], "image");
        assert_eq!(b[1]["source"]["media_type"], "image/jpeg");
        // empty -> placeholder text block
        let b = anthropic_content_blocks(&json!([]));
        assert_eq!(b, vec![json!({ "type": "text", "text": "" })]);
    }

    #[test]
    fn build_anthropic_extracts_system_to_toplevel() {
        let msgs = json!([
            { "role": "system", "content": "You are a coder." },
            { "role": "user", "content": "hi" }
        ]);
        let req =
            build_anthropic_request(msgs.as_array().unwrap(), &[], "claude-x", "none", &[], 4096);
        assert_eq!(req["system"], "You are a coder.");
        assert_eq!(req["model"], "claude-x");
        assert_eq!(req["max_tokens"], 4096);
        // system extracted -> messages starts with user
        assert_eq!(req["messages"][0]["role"], "user");
        assert!(req.get("tools").is_none());
        assert!(req.get("thinking").is_none());
    }

    #[test]
    fn build_anthropic_converts_tools_and_tool_choice() {
        let msgs = json!([{ "role": "user", "content": "do it" }]);
        let tools = json!([
            { "type": "function", "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": { "type": "object", "properties": {} }
            }}
        ]);
        let req = build_anthropic_request(
            msgs.as_array().unwrap(),
            tools.as_array().unwrap(),
            "claude-x",
            "none",
            &[],
            4096,
        );
        let t = req["tools"].as_array().unwrap();
        assert_eq!(t[0]["name"], "read_file");
        assert_eq!(t[0]["description"], "Read a file");
        assert_eq!(t[0]["input_schema"]["type"], "object");
        assert_eq!(req["tool_choice"]["type"], "auto");
    }

    #[test]
    fn build_anthropic_assistant_tool_calls_become_tool_use() {
        let msgs = json!([
            { "role": "user", "content": "read foo" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "call_1", "type": "function", "function": { "name": "read_file", "arguments": "{\"path\":\"foo.rs\"}" } }
            ]},
            { "role": "tool", "tool_call_id": "call_1", "content": "contents of foo" }
        ]);
        let req =
            build_anthropic_request(msgs.as_array().unwrap(), &[], "claude-x", "none", &[], 4096);
        let m = req["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(tool_result)
        assert_eq!(m.len(), 3);
        assert_eq!(m[1]["role"], "assistant");
        let blocks = m[1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_1");
        assert_eq!(blocks[0]["name"], "read_file");
        assert_eq!(blocks[0]["input"]["path"], "foo.rs");
        assert_eq!(m[2]["role"], "user");
        let rblocks = m[2]["content"].as_array().unwrap();
        assert_eq!(rblocks[0]["type"], "tool_result");
        assert_eq!(rblocks[0]["tool_use_id"], "call_1");
        assert_eq!(rblocks[0]["content"], "contents of foo");
    }

    #[test]
    fn build_anthropic_drops_reasoning_content() {
        // Prior Umans reasoning must NOT be replayed: Anthropic rejects raw
        // thinking blocks without signatures (400). Verify it's stripped.
        let msgs = json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": "hello", "reasoning_content": "secret thoughts" },
            { "role": "user", "content": "again" }
        ]);
        let req =
            build_anthropic_request(msgs.as_array().unwrap(), &[], "claude-x", "none", &[], 4096);
        let m = req["messages"].as_array().unwrap();
        let asst = &m[1];
        assert_eq!(asst["role"], "assistant");
        assert!(asst.get("reasoning_content").is_none());
        assert_eq!(asst["content"][0]["text"], "hello");
    }

    #[test]
    fn build_anthropic_merges_consecutive_same_role() {
        // Two tool results back-to-back fold into ONE user message with two
        // tool_result blocks (Anthropic requires alternating roles).
        let msgs = json!([
            { "role": "user", "content": "read two" },
            { "role": "assistant", "content": null, "tool_calls": [
                { "id": "a", "type": "function", "function": { "name": "f", "arguments": "{}" } },
                { "id": "b", "type": "function", "function": { "name": "f", "arguments": "{}" } }
            ]},
            { "role": "tool", "tool_call_id": "a", "content": "r1" },
            { "role": "tool", "tool_call_id": "b", "content": "r2" }
        ]);
        let req =
            build_anthropic_request(msgs.as_array().unwrap(), &[], "claude-x", "none", &[], 4096);
        let m = req["messages"].as_array().unwrap();
        // user, assistant, user(2 tool_results)
        assert_eq!(m.len(), 3);
        let rblocks = m[2]["content"].as_array().unwrap();
        assert_eq!(rblocks.len(), 2);
    }

    #[test]
    fn build_anthropic_enables_thinking_only_when_supported() {
        let msgs = json!([{ "role": "user", "content": "think" }]);
        // thinking-capable model advertises levels -> thinking present
        let levels: Vec<String> = vec!["low".into(), "medium".into(), "high".into()];
        let req = build_anthropic_request(
            msgs.as_array().unwrap(),
            &[],
            "claude-sonnet-4",
            "medium",
            &levels,
            100_000,
        );
        assert_eq!(req["thinking"]["type"], "enabled");
        assert_eq!(req["thinking"]["budget_tokens"], 12288);
        // non-thinking model (empty levels) -> no thinking even with effort set
        let req2 = build_anthropic_request(
            msgs.as_array().unwrap(),
            &[],
            "claude-3-5-sonnet",
            "high",
            &[],
            100_000,
        );
        assert!(req2.get("thinking").is_none());
        // effort "none" with thinking-capable -> no thinking
        let req3 = build_anthropic_request(
            msgs.as_array().unwrap(),
            &[],
            "claude-sonnet-4",
            "none",
            &levels,
            100_000,
        );
        assert!(req3.get("thinking").is_none());
    }

    #[test]
    fn anthropic_model_caps_known_families() {
        let opus = anthropic_model_caps("claude-opus-4-1-20250805", "Opus");
        assert!(opus.reasoning);
        assert!(opus.vision);
        assert_eq!(opus.max_tokens, 32_000);
        assert_eq!(opus.thinking_levels.len(), 3);
        let sonnet4 = anthropic_model_caps("claude-sonnet-4-5", "Sonnet 4.5");
        assert!(sonnet4.reasoning);
        assert_eq!(sonnet4.max_tokens, 16_000);
        let sonnet35 = anthropic_model_caps("claude-3-5-sonnet-20241022", "Sonnet 3.5");
        assert!(!sonnet35.reasoning);
        assert!(sonnet35.thinking_levels.is_empty());
        let haiku4 = anthropic_model_caps("claude-haiku-4-5", "Haiku 4.5");
        assert!(!haiku4.reasoning);
        let sonnet37 = anthropic_model_caps("claude-3-7-sonnet-20250219", "Sonnet 3.7");
        assert!(sonnet37.reasoning);
        // unknown id -> conservative defaults (no thinking, vision on)
        let unknown = anthropic_model_caps("claude-future-9", "Future");
        assert!(!unknown.reasoning);
        assert!(unknown.vision);
    }

    #[test]
    fn parse_anthropic_models_parses_and_dedups() {
        let data = json!({
            "data": [
                { "id": "claude-sonnet-4-5", "display_name": "Sonnet 4.5" },
                { "id": "claude-opus-4-1", "display_name": "Opus" },
                { "id": "claude-sonnet-4-5" }
            ],
            "has_more": false
        });
        let models = parse_anthropic_models(&data);
        assert_eq!(models.len(), 2); // dedup by id
        assert_eq!(models[0].id, "claude-sonnet-4-5");
        assert!(models[0].reasoning);
        assert_eq!(models[1].id, "claude-opus-4-1");
    }

    #[test]
    fn parse_anthropic_models_falls_back_when_empty() {
        // no data array -> static fallback list
        let models = parse_anthropic_models(&json!({}));
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m.id.contains("sonnet")));
        // empty data array -> fallback too
        let models = parse_anthropic_models(&json!({ "data": [] }));
        assert!(!models.is_empty());
    }

    #[test]
    fn truncated_chunked_eof_requires_a_complete_terminal_sse_event() {
        let error = "error decoding response body -> error reading a body from connection -> unexpected EOF during chunk size line";

        let mut complete = SseDecoder::default();
        complete.push(b"data: [DONE]\n\n");
        assert!(is_recoverable_truncated_chunked_body(
            error, &complete, true
        ));

        let mut no_terminal = SseDecoder::default();
        no_terminal.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n");
        assert!(!is_recoverable_truncated_chunked_body(
            error,
            &no_terminal,
            false
        ));

        let mut partial = SseDecoder::default();
        partial.push(b"data: [DONE]\n\ndata: {\"choices\":[");
        assert!(!is_recoverable_truncated_chunked_body(
            error, &partial, true
        ));
        assert!(!is_recoverable_truncated_chunked_body(
            "unexpected EOF while reading body",
            &complete,
            true
        ));
    }

    // ---- mocked-provider integration tests ----
    // A tiny one-shot HTTP server so summarize/extract_facts exercise the real
    // reqwest HTTP path (request build, POST /chat/completions, JSON parse)
    // end-to-end against a canned OpenAI response — not just the parsers.
    fn find_header_end(b: &[u8]) -> Option<usize> {
        b.windows(4).position(|w| w == b"\r\n\r\n")
    }

    async fn mock_openai_server(response_body: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let h = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 1024];
            while find_header_end(&buf).is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let header_end = find_header_end(&buf).unwrap_or(buf.len());
            let header_str = String::from_utf8_lossy(&buf[..header_end]);
            let clen = header_str
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = header_end + 4;
            let mut have = buf.len().saturating_sub(body_start);
            while have < clen {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                have += n;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (base, h)
    }

    fn mock_provider(base: String) -> ResolvedProvider {
        ResolvedProvider {
            name: "mock".into(),
            kind: ProviderKind::OpenAI,
            base_url: base,
            api_key: Some("test-key".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    async fn mock_truncated_chunked_openai_sse_server(
        response_body: String,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let h = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            while find_header_end(&buf).is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{}\r\n",
                response_body.len(), response_body
            );
            // Deliberately omit the terminating zero-sized chunk. Hyper reports
            // this as an unexpected EOF after yielding the complete SSE body.
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        (base, h)
    }

    async fn mock_openai_sse_server(
        responses: Vec<String>,
    ) -> (String, tokio::task::JoinHandle<Vec<Value>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let h = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut requests = Vec::new();
            for response_body in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 1024];
                while find_header_end(&buf).is_none() {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let header_end = find_header_end(&buf).unwrap_or(buf.len());
                let header_str = String::from_utf8_lossy(&buf[..header_end]);
                let content_len = header_str
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|s| s.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let body_start = header_end.saturating_add(4);
                while buf.len().saturating_sub(body_start) < content_len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let request_body = &buf[body_start..body_start.saturating_add(content_len)];
                requests.push(serde_json::from_slice(request_body).unwrap());

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                sock.write_all(response.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            requests
        });
        (base, h)
    }

    #[tokio::test]
    async fn truncated_chunked_sse_after_terminal_frame_is_accepted() {
        let response = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{"delta": {"content": "hello"}}]
            }),
            json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}]
            })
        );
        let (base, server) = mock_truncated_chunked_openai_sse_server(response).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("say hello")],
            &[],
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("a complete terminal SSE turn should survive truncated chunked EOF");

        assert_eq!(result.0["content"], "hello");
        assert_eq!(result.1, "stop");
        server.await.unwrap();
    }

    /// Regression test for the "infinite loading" symptom on `mmx/MiniMax-M3`
    /// over 9router: the provider emits a terminal `finish_reason:stop` +
    /// `usage` chunk but never sends `[DONE]` AND holds the connection open
    /// after. Without the early-break gate on `terminal_event && !has_pending`,
    /// catcode would wait for the per-chunk idle timeout (10s+) on every turn,
    /// which the user reads as a hang. With the fix the stream resolves in
    /// <500ms after the terminal chunk even though the socket is still open.
    #[tokio::test]
    async fn stalled_connection_after_terminal_frame_resolves_promptly() {
        // Build the terminal response body — same shape as the live mmx wire.
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"choices": [{"delta": {"role": "assistant"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": null}]}),
            json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 2, "total_tokens": 14}
            })
        );
        // Mock server: write the body, then idle indefinitely with no `[DONE]`
        // and no chunked terminator. This is the exact behavior the user hit
        // against mmx — the response is "complete" but the socket stays open.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let h = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            while find_header_end(&buf).is_none() {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{}\r\n",
                body.len(),
                body
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            // Hold the connection open (no terminating zero-chunk, no [DONE]).
            // Catcode's fix must detect the terminal frame and break the read
            // loop without waiting for this socket to close or for the idle
            // timeout to fire.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let started = std::time::Instant::now();
        let result = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10, // idle_timeout_secs
            "mock-model",
            &[Message::user("say ok")],
            &[],
            "none",
            &[],
            4096, // max_tokens
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("stream should resolve on terminal frame without waiting for socket EOF");
        let elapsed_ms = started.elapsed().as_millis();
        assert_eq!(result.0["content"], "ok");
        assert_eq!(result.1, "stop");
        // The whole point of the fix: this completes in well under the 10s
        // idle timeout (and well under 1s on a local mock). Pre-fix this would
        // hang at ~10s waiting for a chunk that never arrives.
        assert!(
            elapsed_ms < 5_000,
            "expected prompt resolution (<5s); took {elapsed_ms}ms"
        );
        h.abort();
    }

    #[tokio::test]
    async fn valid_reasoning_dsml_becomes_a_structured_call() {
        let dsml = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"reasoning_content": r#"I need to edit the file.
<｜DSML｜tool_calls>
<｜DSML｜invoke name="edit">
<｜DSML｜parameter name="edits" string="false">[{"search":"old","replace":"new"}]</｜DSML｜parameter>
<｜DSML｜parameter name="path" string="true">core/src/config.rs</｜DSML｜parameter>
</｜DSML｜invoke>
</｜DSML｜tool_calls>"#},
                    "finish_reason": "stop"
                }]
            })
        );
        let (base, server) = mock_openai_sse_server(vec![dsml]).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "edit", "parameters": {"type": "object"}}
        })];
        let result = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("fix it")],
            &tools,
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("valid DSML should recover into the normal tool-call path");

        let calls = result.0["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["function"]["name"], "edit");
        let args: Value =
            serde_json::from_str(calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "core/src/config.rs");
        assert_eq!(args["edits"][0]["replace"], "new");
        assert!(result.0.get("reasoning_content").is_none());
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 1, "valid DSML should not need a retry");
    }

    #[tokio::test]
    async fn reasoning_only_completion_retries_once_and_continues() {
        let reasoning_only = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "The last test failed; I need to fix the assertion.\n</work-state>"},
                    "finish_reason": "stop"
                }]
            })
        );
        let continued = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"content": "I found the failed assertion and will correct it."},
                    "finish_reason": "stop"
                }]
            })
        );
        let (base, server) = mock_openai_sse_server(vec![reasoning_only, continued]).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("fix it")],
            &[],
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("reasoning-only completion should recover on one retry");
        assert_eq!(
            result.0["content"],
            "I found the failed assertion and will correct it."
        );
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("do not end with reasoning alone"));
    }

    #[tokio::test]
    async fn repeated_reasoning_only_completion_is_a_visible_error() {
        let reasoning_only = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "status only\n</work-state>"},
                    "finish_reason": "stop"
                }]
            })
        );
        let (base, server) =
            mock_openai_sse_server(vec![reasoning_only.clone(), reasoning_only]).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let error = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("fix it")],
            &[],
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect_err("repeated reasoning-only output must not silently complete");
        assert!(error.contains("reasoning-only completion"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn openai_sse_error_frames_are_provider_failures() {
        // Auth/account failures fail fast (no stream retries). The mock must
        // only serve one response — extra accepts would hang forever on
        // server.await after the client stops.
        let failure = format!(
            "data: {}\n\n",
            json!({
                "error": {
                    "message": "Cursor SDK authentication failed",
                    "type": "cursor_sdk_error"
                }
            })
        );
        let (base, server) = mock_openai_sse_server(vec![failure]).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let error = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("continue")],
            &[],
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect_err("an SSE error object must not become a successful empty completion");

        assert!(error.contains("Cursor SDK authentication failed"));
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repeated_malformed_reasoning_tool_call_is_a_visible_error() {
        let malformed = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "delta": {"reasoning_content": "<｜DSML｜invoke name=\"bash\">\n</｜DSML｜tool_calls>"},
                    "finish_reason": "stop"
                }]
            })
        );
        let (base, server) = mock_openai_sse_server(vec![malformed.clone(), malformed]).await;
        let provider = mock_provider(base);
        let mut timer = TurnTimer::new();
        let error = stream_turn_openai(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("fix it")],
            &[],
            "none",
            &[],
            0, // max_tokens: omit on wire when 0
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect_err("a repeated protocol violation must not silently complete");
        assert!(error.contains("invalid reasoning DSML"));
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn anthropic_compat_stream_accepts_data_only_sse() {
        let response = [
            json!({
                "type": "message_start",
                "message": { "usage": { "input_tokens": 7 } }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "hello " }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "world" }
            }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 2 }
            }),
            json!({ "type": "message_stop" }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let (base, server) = mock_openai_sse_server(vec![response]).await;
        let provider = ResolvedProvider {
            name: "anthropic-compat-mock".into(),
            kind: ProviderKind::Anthropic,
            base_url: base,
            api_key: Some("test-key".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let mut timer = TurnTimer::new();
        let result = stream_turn_anthropic(
            &reqwest::Client::new(),
            &provider,
            10,
            "mock-model",
            &[Message::user("say hello")],
            &[],
            "none",
            &[],
            1024,
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("data-only Anthropic SSE should stream successfully");

        assert_eq!(result.0["content"], "hello world");
        assert_eq!(result.1, "stop");
        assert_eq!(result.2, 7);
        assert_eq!(result.3, 2);
        let requests = server.await.unwrap();
        assert_eq!(requests[0]["stream"], true);
    }

    // Helper: build an Anthropic-kind ResolvedProvider against a mock SSE base.
    fn mock_anthropic_provider(base: String) -> ResolvedProvider {
        ResolvedProvider {
            name: "anthropic-mock".into(),
            kind: ProviderKind::Anthropic,
            base_url: base,
            api_key: Some("test-key".into()),
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    // Helper: turn a Vec of JSON event values into an SSE response body.
    fn anthropic_sse_body(events: Vec<Value>) -> String {
        events
            .into_iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
    }

    #[tokio::test]
    async fn anthropic_tool_use_start_ignores_empty_input_placeholder() {
        // Regression for the bug where content_block_start's `input: {}`
        // placeholder was captured into tool_args and concatenated with the
        // input_json_delta fragments, producing malformed args like
        // `{}{"command":"ls -la"}`. The start event must set tool_id +
        // tool_name ONLY; args are assembled entirely from input_json_delta.
        let response = anthropic_sse_body(vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":10}}}),
            json!({
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"tool_use","id":"toolu_01","name":"bash","input":{}}
            }),
            json!({
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls -la\"}"}
            }),
            json!({"type":"content_block_stop","index":0}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":5}
            }),
            json!({"type":"message_stop"}),
        ]);
        let (base, _server) = mock_openai_sse_server(vec![response]).await;
        let provider = mock_anthropic_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_anthropic(
            &reqwest::Client::new(),
            &provider,
            10,
            "claude-3-5-sonnet",
            &[Message::user("list files")],
            &[],
            "none",
            &[],
            1024,
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("tool_use stream should succeed");

        let msg = &result.0;
        let tool_calls = msg["tool_calls"]
            .as_array()
            .expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "toolu_01");
        assert_eq!(tool_calls[0]["function"]["name"], "bash");
        let args = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        // The critical assertion: args must be the clean assembled JSON, NOT
        // prefixed with the empty `{}{` placeholder.
        assert_eq!(args, r#"{"command":"ls -la"}"#);
        assert!(
            !args.starts_with("{}"),
            "empty-input placeholder must not leak"
        );
        // stop_reason tool_use → finish_reason "tool_calls"
        assert_eq!(result.1, "tool_calls");
    }

    #[tokio::test]
    async fn anthropic_partial_json_deltas_assemble_tool_args() {
        // input_json_delta arrives in multiple partial_json fragments that must
        // concatenate into valid JSON.
        let response = anthropic_sse_body(vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":8}}}),
            json!({
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"tool_use","id":"toolu_02","name":"edit","input":{}}
            }),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"/main.rs\",\"edits\":[]}"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":4}
            }),
            json!({"type":"message_stop"}),
        ]);
        let (base, _server) = mock_openai_sse_server(vec![response]).await;
        let provider = mock_anthropic_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_anthropic(
            &reqwest::Client::new(),
            &provider,
            10,
            "claude-3-5-sonnet",
            &[Message::user("edit the file")],
            &[],
            "none",
            &[],
            1024,
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("partial-json stream should succeed");

        let tool_calls = result.0["tool_calls"].as_array().unwrap();
        let args = tool_calls[0]["function"]["arguments"].as_str().unwrap();
        // Fragments must concatenate cleanly into valid JSON.
        assert_eq!(args, r#"{"path":"src/main.rs","edits":[]}"#);
        let parsed: Value = serde_json::from_str(args).expect("assembled args must be valid JSON");
        assert_eq!(parsed["path"], "src/main.rs");
    }

    #[tokio::test]
    async fn anthropic_multi_block_two_tool_calls_in_one_response() {
        // A single response carrying TWO tool_use blocks at different indices —
        // both must assemble independently with correct ids/names/args.
        let response = anthropic_sse_body(vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":12}}}),
            // Block 0: text
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Running two commands."}}),
            json!({"type":"content_block_stop","index":0}),
            // Block 1: first tool_use (bash)
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_10","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            // Block 2: second tool_use (read_file)
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_11","name":"read_file","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x.rs\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"tool_use"},
                "usage":{"output_tokens":10}
            }),
            json!({"type":"message_stop"}),
        ]);
        let (base, _server) = mock_openai_sse_server(vec![response]).await;
        let provider = mock_anthropic_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_anthropic(
            &reqwest::Client::new(),
            &provider,
            10,
            "claude-3-5-sonnet",
            &[Message::user("do two things")],
            &[],
            "none",
            &[],
            1024,
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("multi-block stream should succeed");

        let msg = &result.0;
        assert_eq!(msg["content"], "Running two commands.");
        let tool_calls = msg["tool_calls"]
            .as_array()
            .expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 2, "two tool_use blocks → two tool_calls");
        // First call (index 1)
        assert_eq!(tool_calls[0]["id"], "toolu_10");
        assert_eq!(tool_calls[0]["function"]["name"], "bash");
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            r#"{"command":"ls"}"#
        );
        // Second call (index 2)
        assert_eq!(tool_calls[1]["id"], "toolu_11");
        assert_eq!(tool_calls[1]["function"]["name"], "read_file");
        assert_eq!(tool_calls[1]["function"]["arguments"], r#"{"path":"x.rs"}"#);
    }

    #[tokio::test]
    async fn anthropic_thinking_block_streams_then_text() {
        // Extended thinking (thinking_delta) followed by a text block. Thinking
        // is shown live but NOT persisted in the assistant message (Anthropic
        // thinking blocks aren't replayable). The final message must carry only
        // the text content, not the reasoning.
        let response = anthropic_sse_body(vec![
            json!({"type":"message_start","message":{"usage":{"input_tokens":6}}}),
            // Block 0: thinking
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me consider the approach."}}),
            json!({"type":"content_block_stop","index":0}),
            // Block 1: text
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Here is my answer."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({
                "type":"message_delta",
                "delta":{"stop_reason":"end_turn"},
                "usage":{"output_tokens":8}
            }),
            json!({"type":"message_stop"}),
        ]);
        let (base, _server) = mock_openai_sse_server(vec![response]).await;
        let provider = mock_anthropic_provider(base);
        let mut timer = TurnTimer::new();
        let result = stream_turn_anthropic(
            &reqwest::Client::new(),
            &provider,
            10,
            "claude-3-7-sonnet",
            &[Message::user("think and answer")],
            &[],
            "none",
            &[],
            1024,
            &CancellationToken::new(),
            &mut timer,
            0,
            true,
        )
        .await
        .expect("thinking+text stream should succeed");

        let msg = &result.0;
        // Text content is present.
        assert_eq!(msg["content"], "Here is my answer.");
        // Thinking is NOT persisted into the assistant message.
        assert!(msg.get("reasoning_content").is_none());
        assert!(!msg["content"]
            .as_str()
            .unwrap_or("")
            .contains("Let me consider"));
        // No tool calls in this turn.
        assert!(msg.get("tool_calls").is_none());
        assert_eq!(result.1, "stop");
    }

    #[tokio::test]
    async fn summarize_against_mock_provider() {
        let body = r#"{"choices":[{"message":{"content":"<summary>mocked</summary>"}}]}"#;
        let (base, _h) = mock_openai_server(body.into()).await;
        let client = reqwest::Client::new();
        let provider = mock_provider(base);
        let cancel = CancellationToken::new();
        let msgs: Vec<Message> = vec![
            Message::user("please refactor the auth module"),
            Message::assistant("on it"),
        ];
        let out = summarize(&client, &provider, "mock-model", &msgs, &cancel, None).await;
        assert_eq!(out.as_deref(), Some("mocked"));
    }

    #[tokio::test]
    async fn extract_facts_none_short_circuits() {
        let body = r#"{"choices":[{"message":{"content":"none"}}]}"#;
        let (base, _h) = mock_openai_server(body.into()).await;
        let client = reqwest::Client::new();
        let provider = mock_provider(base);
        let cancel = CancellationToken::new();
        let msgs: Vec<Message> = vec![Message::user("hello")];
        let out = extract_facts(&client, &provider, "mock-model", &msgs, &cancel).await;
        assert!(
            out.is_none(),
            "a 'none' reply must not be persisted as a fact"
        );
    }

    #[tokio::test]
    async fn summarize_returns_none_on_http_error() {
        let body = ""; // 200 with empty body -> JSON parse fails -> None
        let (base, _h) = mock_openai_server(body.into()).await;
        let client = reqwest::Client::new();
        let provider = mock_provider(base);
        let cancel = CancellationToken::new();
        let msgs: Vec<Message> = vec![Message::user("x")];
        let out = summarize(&client, &provider, "mock-model", &msgs, &cancel, None).await;
        assert!(out.is_none());
    }

    /// Live integration check, run on demand:
    ///   cargo test --bin core -- --ignored --nocapture refresh_live_umans_models_cache
    ///
    /// Hits the REAL public Umans `/models/info` endpoint (no key required)
    /// through the exact `discover_models_force_refresh` code path the startup
    /// background task uses, and writes the result into the real on-disk models
    /// cache so the latest models are cached locally. Ignored by default so it
    /// never runs in normal `cargo test` / CI (network + writes the user cache).
    #[tokio::test]
    #[ignore = "live: hits the real Umans endpoint and writes the models cache"]
    async fn refresh_live_umans_models_cache() {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("client");
        let provider = ResolvedProvider {
            name: "umans".to_string(),
            kind: ProviderKind::OpenAI,
            base_url: "https://api.code.umans.ai/v1".to_string(),
            api_key: None, // /models/info is public
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        let models = discover_models_force_refresh(&client, &provider).await;
        assert!(
            !models.is_empty(),
            "live Umans /models/info returned no models"
        );
        eprintln!("fetched {} Umans model(s):", models.len());
        for m in &models {
            eprintln!(
                "  {} ({}) ctx={} out={} reasoning={} vision={} levels={:?}",
                m.id,
                m.name,
                m.context_window,
                m.max_tokens,
                m.reasoning,
                m.vision,
                m.thinking_levels
            );
        }
    }
}
