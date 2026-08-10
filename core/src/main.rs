// catalyst-code-core: stdio JSON-RPC server. The TUI spawns this binary,
// writes commands to stdin, and reads newline-delimited events from stdout.
//
// Several core functions (stream_turn, run_turn, dispatch_*) intentionally
// carry many positional args (the seam between the request loop and the tool
// layer); refactoring each into a context struct is a larger change, so allow
// the lint here rather than obscure the call sites.
#![allow(clippy::too_many_arguments)]

mod advisor;
mod agent;
mod audit;
mod browser;
mod change_coupling;
mod checkpoint;
mod codebase_index;
mod collections;
mod commands;
mod config;
mod context_pack;
mod coverage_ledger;
mod dap;
mod embed;
mod episodes;
mod failure_atlas;
mod fetch_tool;
mod fsutil;
mod git_ctx;
mod goal;
mod goal_ceo;
mod intercom;
mod knowledge_tool;
mod learning_activations;
mod learning_proposals;
mod learning_retrieval;
mod learning_store;
mod logging;
mod mcp;
mod memory;
#[cfg(test)]
mod memory_eval;
mod memory_hygiene;
mod memory_recall;
mod memory_staleness;
mod message;
mod models_dev;
mod oauth;
mod pattern_log;
mod plugin_trust;
mod plugins;
mod preferences;
mod presence;
mod project_identity;
mod protocol;
mod provider;
mod providers;
mod rejected_approaches;
mod research_evidence;
mod runtime;
mod sandbox;
mod search_tool;
mod session;
mod skill_marketplace;
mod skill_metrics;
mod skills;
mod staging;
mod subagent;
mod task_fingerprint;
mod test_env;
mod tool_cache;
mod tooling;
mod tools;
mod vision;
mod workspace;
mod worktree;

use config::{Approval, Config, ResolvedProvider};
use git_ctx::{git_context_injection, read_git_context};
use intercom::IntercomBus;
use logging::{
    estimate_message_tokens, estimate_messages_tokens, grounded_estimate, Logger, TurnMetrics,
    TurnTimer,
};
use memory::{memory_injection, relevant_memories_tail};
#[allow(unused_imports)]
use message::{ContentPart, FunctionCall, ImageUrl, Message, ToolCall};
use plugins::{PluginManager, PLUGIN_DOCS};
use protocol::{emit, emit_aborted_done, emit_turn_rejected, Command, Event, ModelInfo};
use runtime::{CancellationReason, ResourceKind, RunContext, RuntimeCoordinator};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use vision::VisionConfig;

use futures_util::FutureExt;
use std::panic::AssertUnwindSafe;

#[derive(Clone)]
pub struct QueuedPrompt {
    prompt: String,
    model: String,
    /// Owning provider the user explicitly picked for `model` (from `/models`);
    /// wins routing over the active-provider tie-break. None for older clients.
    provider: Option<String>,
    effort: String,
}

/// A pending approval request the TUI must answer before the tool runs.
const SYSTEM_PROMPT_BASE: &str = r#"You are a coding agent operating inside a Rust/Go harness with native Umans model access.
You can read, edit, write, and list files, search with grep/glob, and run shell commands — all confined to the workspace.

Judgment (tool schemas own the mechanics):
- Read/search before changing; prefer the smallest correct edit; verify with a command.
- Prefer the smallest correct change: `ast_edit` for structural code rewrites when available, otherwise `edit` over `write_file` for targeted text; prefer grep/glob (scoped) before full reads; page with offset/limit.
- Call tools directly — use `bulk` only for genuinely independent parallel calls. Keep shell commands short; write a script for complex logic.
- Deferred tool schemas are opt-in — call `load_tools` with a group or name when needed.
- Paths are workspace-relative; absolute paths and ".." are rejected.

Complete and verify requests end-to-end. Stop only when done, blocked on the user,
or destructive work needs approval. Do in-scope next steps instead of offering them.
Be concise.

Self-learning:
- Persist durable facts with `memory` (workspace default; `scope:global` for cross-repo). Prefer `append` over duplicate saves; skip transient task state and trivia. Always pass a one-line `description`. Use durable types (convention/decision/gotcha/…) or importance=high; pass force=true only to override the write policy.
- The standing prompt carries a capped MEMORY CATALOG (name + one-line only). Use `memory` action=get for full text when a catalog entry matters; list to see everything.
- Call `memory` mid-task when you learn something reusable. Use action=consolidate to merge near-duplicates; action=stats for recall quality.
- Reusable workflows → `.catalyst-code/skills/<name>/SKILL.md` (frontmatter name/description; body when-to-use/steps/examples). Read a skill before applying; create one after the same shape recurs. Prefer `/skill:<name>` for global skills that read_file cannot reach.
- `/index` bootstraps an unfamiliar repo; `/reflect` is a deliberate learning pass.

Document collections (RAG): `collections` indexes docs/notes into named collections and searches them by similarity to ground a task; per-project."#;

/// Compact orchestrator stub — enough to use `subagent` without injecting the
/// full pi-subagents skill body on every turn. Parent-only (`with_skill`).
const SUBAGENT_ORCHESTRATOR_STUB: &str = r#"# Subagents

Delegate via the `subagent` tool. Builtins: scout, researcher, planner, worker, reviewer, context-builder, oracle, delegate.
Modes: single; fork (`context:"fork"`); parallel (`tasks` + `concurrency`); chain (`chain`, `{previous}` = prior output).
Children escalate with `contact_supervisor` — answer `need_decision` promptly. Manage runs with peek / steer / interrupt / resume / status.
Before non-trivial multi-agent work, apply `/skill:pi-subagents` for the full playbook."#;

/// How to add a model provider — config (API-key) vs plugin (OAuth). Always
/// injected (like PLUGIN_DOCS) so the agent recognizes "add provider X" as a
/// supported task in any workspace, even without the opt-in skills present.
/// Full schemas/edge cases live in the `add-key-provider` and `plugin-authoring`
/// skills; this is the actionable minimum.
/// Guidance for choosing structural edits over textual edits. Kept concise so
/// every turn gets the preference without embedding the full IDE manual.
const STRUCTURED_EDIT_GUIDE: &str = r#"## Editing preference

When `ast_edit` is available for the file's language, prefer it over `edit` for structural code changes: renames, call/signature changes, syntax-aware refactors, and repeated AST-pattern rewrites. Use `edit` for exact text replacements, prose/config edits, or languages without a bundled AST parser. `ast_edit` is deferred, so call `load_tools` with `ide` first when needed; use `apply:false` to inspect its diff before applying."#;

const PROVIDER_GUIDE: &str = r#"## Adding model providers

"Add/connect provider X" → two no-recompile paths, pick by auth type:
1. **API-key auth, OpenAI/Anthropic-compatible** → CONFIG. Built-in first-party presets include `umans`, `opencode-go`, `openrouter`, and `deepseek` (DeepSeek uses `https://api.deepseek.com` + `DEEPSEEK_API_KEY`). Add a `providers` entry to `~/.config/catalyst-code/config.json` when a preset is not suitable:
   `{"providers":[{"name":"x","kind":"openai","base_url":"https://api.x.com/v1","api_key_env":"X_API_KEY"}],"activeProvider":"x"}`
   `kind` sets wire+auth (`openai`→/chat/completions+Bearer; `anthropic`→/messages+`x-api-key`) + discovery. `api_key_env` (env var NAME, preferred) or `api_key` (literal). Models auto-discover via /models; non-standard discovery (custom fields/404) needs a code branch in `core/src/provider.rs` — skill `add-key-provider` has the config-vs-code decision tree.
2. **OAuth/subscription login** (browser/device-code, no plain key — e.g. Grok, ChatGPT/Codex) → PLUGIN. A plugin's `plugin.json` declares an `oauth` block (`provider_id`, `kind`, `base_url`, `token_path`, `script` handling login/complete/token/clear actions, JSON in/out). The harness resolves the bearer token at turn time and lists the provider in `/login`; device-code plugins may return `flow: "poll"` so the harness completes them without `/oauth-code`. The staged `codex` bundle also imports file-backed Codex CLI auth. Skill `plugin-authoring` has the full schema + script contract; `docs/examples/plugins/grok-oauth/` is a device-code example.
Rule: plain API key → config; login flow → plugin."#;

/// Deferred load_tools groups — always injected so the agent knows secondary
/// capabilities exist without an opt-in skill. Keep lean: groups + when-to-load;
/// full tool schemas arrive only after load_tools. When adding a new deferred
/// group (e.g. browser), list it here AND in handle_load_tools / load_tools schema.
const DEFERRED_TOOLS_GUIDE: &str = r#"## Deferred tools

Call `load_tools` when needed: `git`, `web`, `bulk`, `runtime` (eval/read), `ide` (lsp/ast_edit/snapshot_edit), `debug`, `mcp`, `browser`, `process`, or `all`. Individual tools such as spawn, workspace_activity, and test_env are also loadable. `goal_write_plan` is available only during /goal planning."#;

/// Cap standing skill-manifest size so a large skills/ tree does not bloat the
/// prefix cache. Remaining skills stay discoverable via list_dir / `/skill:`.
const SKILL_MANIFEST_MAX: usize = 12;
const SKILL_DESC_MAX_CHARS: usize = 80;

/// Shell guidance is derived at prompt-build time from
/// [`sandbox::policy::shell_guidance`] so it matches the live `bash` tool
/// (sandbox guest bash vs host PowerShell/cmd/posix, including
/// `CATALYST_CODE_SHELL`). The wire tool name stays `bash` for TUI/web/SDK
/// compatibility.

/// Build the full system prompt by appending git context, memory context,
/// the plugins pointer, the provider-onboarding guide, and the deferred-tools
/// group list (full manuals live in opt-in skills).
/// When `memory_provider` is set, standing-prompt memories come from that
/// plugin instead of the built-in markdown store.
pub fn build_system_prompt(
    workspace: &std::path::Path,
    with_skill: bool,
    memory_provider: Option<&plugins::PluginMemoryProviderConfig>,
) -> String {
    let mut prompt = SYSTEM_PROMPT_BASE.to_string();
    // Absolute workspace path — critical when models are proxied through an
    // external SDK that has its own decoy cwd (e.g. cursor-openai-api sandbox).
    prompt.push_str("\n\n");
    prompt.push_str(&format!(
        "Workspace root (absolute): {}. All relative tool paths resolve here. Ignore any other working-directory claims from the transport layer.",
        workspace.display()
    ));
    prompt.push_str("\n\n");
    prompt.push_str(sandbox::policy::shell_guidance());
    if let Some(git) = read_git_context(workspace) {
        prompt.push_str("\n\n");
        prompt.push_str(&git_context_injection(&git));
    }
    // Stable project identity + learning dir bootstrap (fail-open).
    {
        let ident = project_identity::resolve_project_identity(workspace);
        let _ = learning_store::ensure_project_learning(
            &ident.id,
            ident.remote.as_deref(),
            Some(&ident.workspace_hash),
        );
        prompt.push_str("\n\n");
        prompt.push_str(&format!(
            "Project identity: `{}` (workspace hash `{}`{}). Learning data is scoped to this project id so path moves keep memories and episodes.",
            ident.id,
            ident.workspace_hash,
            ident
                .remote
                .as_ref()
                .map(|r| format!(", remote `{r}`"))
                .unwrap_or_default()
        ));
    }
    let mem = match memory_provider {
        Some(cfg) => plugins::memory_provider_inject(cfg, &workspace.display().to_string(), ""),
        None => memory_injection(workspace, ""),
    };
    if !mem.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&mem);
    }
    prompt.push_str("\n\n");
    prompt.push_str(PLUGIN_DOCS);
    prompt.push_str("\n\n");
    prompt.push_str(PROVIDER_GUIDE);
    prompt.push_str("\n\n");
    prompt.push_str(DEFERRED_TOOLS_GUIDE);
    prompt.push_str("\n\n");
    prompt.push_str(STRUCTURED_EDIT_GUIDE);
    // Parent-only: stub + capped skill manifest. Subagents never receive these
    // (they'd wrongly think they are the orchestrator).
    if with_skill {
        prompt.push_str("\n\n");
        prompt.push_str(SUBAGENT_ORCHESTRATOR_STUB);
        let manifest = skill_manifest_injection(workspace);
        if !manifest.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&manifest);
        }
    }
    prompt
}

/// Build the MAIN agent's system prompt: the base prompt (git context +
/// memory + plugins pointer + orchestrator stub + skill manifest) PLUS any
/// text plugins inject via their `system_prompt` manifest field. Plugin
/// injection is empty (so the prompt + its prefix cache are untouched) when no
/// enabled plugin declares one — mirroring how `build_system_prompt` stays
/// cheap in the common case. Subagents do NOT get plugin injection (they use
/// the built-in tool set only), matching the plugin-tools-are-main-agent-scoped
/// design.
fn build_main_system_prompt(
    workspace: &std::path::Path,
    pm: &plugins::PluginManager,
    auto_reflect: bool,
) -> String {
    let mp = pm.memory_provider();
    let mut prompt = build_system_prompt(workspace, true, mp.as_ref());
    let inj = pm.system_prompt_injection();
    if !inj.is_empty() {
        prompt.push_str(&inj);
    }
    // Main-agent-only: the `ask` tool is dispatchable only in the orchestrator
    // loop (subagents escalate via contact_supervisor instead), so the ask-when
    // -under-specified guidance lives here, not in the shared base prompt.
    prompt.push_str(
        "\n\nAsk the user when it matters:\n\
         - Use `ask` whenever the request is under-specified and guessing could waste work or cause damage: ambiguous scope, multiple valid approaches with real trade-offs, missing required info you cannot find in the workspace, or an irreversible/destructive choice.\n\
         - Do NOT ask about things you can determine yourself by reading the code or running a command — check first, ask only what you can't resolve.\n\
         - One round of focused questions beats many; batch related questions in one `ask` call. If the user skips, proceed with best judgment and state your assumptions.",
    );
    // When auto-reflect is on, defer the completion summary until AFTER the
    // reflection step so the summary is the last message the user reads.
    // Supersedes the "summarize when done" line in SYSTEM_PROMPT_BASE (kept
    // for subagents + the auto_reflect-off case).
    if auto_reflect {
        prompt.push_str(
            "\n\nCompletion flow (auto-reflect on): call `finish` when work is verified — do not summarize first. \
             After the harness reflection step, write the summary as your final message, then `finish`. \
             A visible completion summary (≥ a short paragraph) is required before the turn ends; \
             bare `finish` / tool-only reflection is not enough. This supersedes \
             \"summarize when done\" above.",
        );
    }
    prompt
}

/// One-line manifest of opt-in skills (name + description) discovered under
/// `.catalyst-code/skills/` (project then user scope). Spliced into the
/// orchestrator's stable system prompt so available skills are visible without a
/// `list_dir` round-trip. Excludes `pi-subagents` (covered by the stub above),
/// caps at `SKILL_MANIFEST_MAX` entries with truncated descriptions, and
/// deduplicates by name (project wins). Returns empty when no opt-in skills
/// exist so a fresh install's prompt — and its provider prefix cache — is
/// left untouched.
fn skill_manifest_injection(workspace: &std::path::Path) -> String {
    let skills = subagent::discover_skills(workspace);
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut lines: Vec<String> = Vec::new();
    let mut omitted = 0usize;
    for (name, desc, loc) in &skills {
        if name.as_str() == "pi-subagents" {
            continue;
        }
        // Use the skill DIRECTORY name (parsed from the SKILL.md path) as the
        // identifier, so `/skill:<name>` / path hints resolve — frontmatter
        // `name` can drift from the dirname.
        let n = std::path::Path::new(loc)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| name.trim());
        if n.is_empty() || !seen.insert(n) {
            continue;
        }
        if lines.len() >= SKILL_MANIFEST_MAX {
            omitted += 1;
            continue;
        }
        let d = desc.trim();
        if d.is_empty() {
            lines.push(format!("- {n}"));
        } else if d.chars().count() > SKILL_DESC_MAX_CHARS {
            let truncated: String = d.chars().take(SKILL_DESC_MAX_CHARS).collect();
            lines.push(format!("- {n}: {truncated}…"));
        } else {
            lines.push(format!("- {n}: {d}"));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "Available opt-in skills — apply with `/skill:<name>` (or read the matching \
         .catalyst-code/skills/<name>/SKILL.md when it is in the workspace):\n{}",
        lines.join("\n")
    );
    if omitted > 0 {
        out.push_str(&format!(
            "\n- …and {omitted} more (list_dir .catalyst-code/skills/ or /skill:<name>)"
        ));
    }
    out
}

/// Build and emit a `skills` event listing every discoverable skill (project
/// then user scope) with its name, description, location, and parsed body
/// content. The TUI/web use name+description for the `/skill:<name>`
/// autocomplete; `apply_skill` re-reads the body from disk at invocation time,
/// so the body here lets a frontend optionally inline content without a second
/// round-trip. Called on `init` and `list_skills`.
fn emit_skills_event(workspace: &std::path::Path) {
    let skills = subagent::discover_skills_full(workspace);
    let arr: Vec<Value> = skills
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "location": s.location,
                "content": s.body,
            })
        })
        .collect();
    emit(&Event::new("skills").with("skills", json!(arr)));
}

/// Publish discoverable subagents (builtin + user + project) for the web/TUI
/// agent pickers. Called on `init` and `list_agents`.
fn emit_agents_event(workspace: &std::path::Path, cfg: &config::Config) {
    use subagent::AgentSource;
    let agents = subagent::discover_agents(workspace, &cfg.subagents);
    let arr: Vec<Value> = agents
        .iter()
        .map(|a| {
            let source = match a.source {
                AgentSource::Builtin => "builtin",
                AgentSource::User => "user",
                AgentSource::Project => "project",
            };
            json!({
                "name": a.name,
                "description": a.description,
                "source": source,
            })
        })
        .collect();
    emit(&Event::new("agents").with("agents", json!(arr)));
}

/// Build the JSON array of first-party provider presets for the `ready` and
/// `provider_presets` events. Each entry tells the client whether a key is
/// already stored from a prior explicit `/login` in this app — so a picker can
/// show "log in" vs "log out". Env vars are never treated as signed-in.
/// Subscription OAuth is plugin-only (appended below from `pm.oauth_configs()`).
fn provider_presets_json(cfg: &Config, pm: Option<&plugins::PluginManager>) -> Vec<Value> {
    let mut out: Vec<Value> = config::PROVIDER_PRESETS
        .iter()
        .map(|p| {
            let configured = cfg.find_provider(p.id).is_some();
            // Auth available = literal key already in config. Do not treat env
            // vars as signed-in — the user must paste a key via /login.
            let has_key = cfg
                .find_provider(p.id)
                .and_then(|pc| pc.api_key.clone().filter(|s| !s.is_empty()))
                .is_some();
            // Keyless local presets (empty api_key_env) count as logged-in once
            // configured — Ollama / LM Studio need no API key.
            let logged_in = configured && (has_key || p.api_key_env.is_empty());
            json!({
                "id": p.id,
                "label": p.label,
                "kind": p.kind.as_str(),
                "base_url": p.base_url,
                "envVar": p.api_key_env,
                "altEnvs": p.alt_envs,
                "description": p.description,
                "hasKey": has_key || (p.api_key_env.is_empty() && configured),
                "configured": configured,
                "loggedIn": logged_in,
                "supportsOauth": false,
            })
        })
        .collect();
    // Append plugin-declared OAuth providers so they appear in the /login picker
    // (built-in presets win on a colliding id).
    if let Some(pm) = pm {
        for c in pm.oauth_configs() {
            if config::PROVIDER_PRESETS
                .iter()
                .any(|p| p.id == c.provider_id)
            {
                continue;
            }
            let configured = cfg.find_provider(&c.provider_id).is_some();
            let has_key = pm.has_oauth_creds(&c.provider_id);
            out.push(json!({
                "id": c.provider_id,
                "label": c.label,
                "kind": c.kind.as_str(),
                "base_url": c.base_url,
                "envVar": null,
                "altEnvs": [],
                "description": c.description,
                "hasKey": has_key,
                "configured": configured,
                "loggedIn": configured && has_key,
                "supportsOauth": true,
            }));
        }
    }
    out
}

/// A pending approval request the TUI must answer before the tool runs.
#[allow(dead_code)]
pub struct PendingApproval {
    request_id: String,
    session_id: String,
    run_id: String,
    coordinator_bound: bool,
    cancellation: CancellationToken,
    tool_call_id: String,
    tool: String,
    risk: &'static str,
    created_at_ms: u64,
    args: Value,
    notify: Arc<Notify>,
    granted: Mutex<Option<bool>>, // Some(true)=approved, Some(false)=denied, None=awaiting
    escalated: Mutex<bool>,       // "always" was chosen → upgrade session mode
    /// "allow_session" — add a session-scoped allow rule for this tool+args pattern.
    allow_session: Mutex<bool>,
    /// "allow_pattern" — optional rule_content (e.g. path glob) to persist as allow.
    allow_pattern: Mutex<Option<String>>,
}

/// A pending `ask` tool call the user must answer before the model continues.
/// Mirrors PendingApproval but carries arbitrary structured answers back.
#[allow(dead_code)]
pub struct PendingAsk {
    request_id: String,
    /// The validated questions array sent to the TUI in the `ask_request`
    /// event (and used to format the model-facing result).
    questions: Value,
    notify: Arc<Notify>,
    /// None = awaiting. Some(obj) = answered (obj maps question id → answer).
    /// Some(Value::Null) = the user skipped the whole prompt.
    answers: Mutex<Option<Value>>,
}

/// A pending sudo approval: the agent wants to run a bash command that invokes
/// `sudo`. The user must approve (supplying a password) or decline (Esc). The
/// password is used once to feed `sudo -S` on stdin and is never stored.
#[allow(dead_code)]
pub struct PendingSudo {
    request_id: String,
    /// The full command string, shown to the user so they know what they're
    /// approving.
    command: String,
    notify: Arc<Notify>,
    /// None = awaiting. Some(Some(pw)) = approved with password.
    /// Some(None) = declined (Esc). The outer Option is the "resolved" flag.
    result: Mutex<Option<Option<String>>>,
}

pub struct State {
    pub cfg: RwLock<Config>,
    /// The shared HTTP client. Held on State so per-turn resolution can do
    /// async OAuth token refresh (Gemini gcloud ADC / Claude CLI creds) without
    /// threading the client through every call site.
    pub client: reqwest::Client,
    /// Per-provider runtime API keys (set via `set_key {provider,api_key}`).
    /// Keyed by provider name; the active provider's key (if present) wins over
    /// config literals/env vars during resolution. The "default" slot holds the
    /// legacy single key when no providers are configured.
    pub api_keys: RwLock<HashMap<String, String>>,
    /// Runtime override of the active provider name (set via `set_provider`).
    /// Wins over `cfg.active_provider`; None => use config's active provider.
    pub active_provider: RwLock<Option<String>>,
    pub conversation: Mutex<Vec<Message>>,
    pub models: RwLock<Vec<ModelInfo>>,
    /// Runtime identity/cancellation authority. `current` is retained as the
    /// async-facing active-turn slot while migration proceeds; both carry the
    /// same RunContext and stale finishers may clear it only by matching id.
    pub runtime: Arc<RuntimeCoordinator>,
    pub current: Mutex<Option<RunContext>>,
    pub handle: Mutex<Option<JoinHandle<()>>>,
    /// Pending approval requests keyed by their unique approval id (see
    /// APPROVAL_SEQ) so parallel subagents can't clobber each other's request.
    pub pending: Mutex<std::collections::HashMap<String, Arc<PendingApproval>>>,
    /// Pending `ask` tool calls keyed by their unique ask id (see ASK_SEQ).
    pub pending_asks: Mutex<std::collections::HashMap<String, Arc<PendingAsk>>>,
    /// Pending sudo approval requests keyed by their unique sudo id (see
    /// SUDO_SEQ). A bash command that invokes `sudo` blocks here until the user
    /// approves (with password) or declines (Esc).
    pub pending_sudos: Mutex<std::collections::HashMap<String, Arc<PendingSudo>>>,
    pub logger: Logger,
    /// Token counts accumulated across the session (for the status bar).
    pub tokens_in: Mutex<u64>,
    pub tokens_out: Mutex<u64>,
    /// Cumulative prefix-cache hits across the session (from
    /// usage.prompt_tokens_details.cached_tokens). Surfaces whether the
    /// stable-prefix strategy is actually landing cache hits.
    pub cached_tokens: Mutex<u64>,
    /// Tool kinds ("destructive"/"readonly") the user said "always" to,
    /// so subsequent calls of that kind skip the gate without escalating all.
    pub escalated_kinds: Mutex<std::collections::HashSet<&'static str>>,
    /// Prompt queued while a turn was running (one-deep buffer).
    pub queued: Mutex<Option<QueuedPrompt>>,
    /// User-bash (`!cmd`) context messages deferred while a turn is in flight.
    /// Flushed after the turn ends so we never insert a user message between
    /// an assistant `tool_calls` message and its `tool` results (providers
    /// reject that ordering). PI does the same with `_pendingBashMessages`.
    pub pending_bash: Mutex<Vec<Message>>,
    /// Plugin manager — scans, loads, and executes hooks.
    pub plugin_manager: PluginManager,
    /// Vision-handoff config (curated vision models + preferred target), persisted
    /// to .catalyst-code/vision.json; merged into the pre_turn hook context.
    pub vision: RwLock<VisionConfig>,
    /// Last time a turn completed (for idle compaction).
    pub last_turn_time: Mutex<std::time::Instant>,
    /// Incrementally maintained token estimate for the main conversation,
    /// updated on every push + recalculated after compaction.
    pub estimated_tokens: Mutex<u64>,
    /// Real `prompt_tokens` from the endpoint's most recent `usage` chunk — the
    /// authoritative count of the conversation exactly as the model tokenized it
    /// (system prompt + messages + tool-call framing the char/4 heuristic
    /// cannot see). Anchors `grounded_estimate` so the compaction trigger and the
    /// footer percentage reflect reality instead of a whole-history char/4 guess.
    /// `None` until the first request that reports usage, and reset whenever
    /// history is rewritten (compaction/digest/reset/undo/load/refresh) so the
    /// baseline never describes stale content.
    pub last_real_prompt_tokens: Mutex<Option<u64>>,
    /// Conversation length (message count) captured when
    /// `last_real_prompt_tokens` was recorded. `grounded_estimate` only
    /// char/4-estimates the messages added since this index, keeping the real
    /// baseline accurate across tool-use loop iterations.
    pub conv_len_at_last_real: Mutex<usize>,
    /// The model id the user last sent a turn with. Used by the manual `/compact`
    /// command to pick the right context window (instead of a hardcoded 200k)
    /// and to size the reclaim budget. `None` until the first turn.
    pub last_model: Mutex<Option<String>>,
    /// Metrics from the most recently completed turn (tokens, TTFT, TPS, cache
    /// hits). Surfaced to `session_stop` lifecycle hooks so a telemetry plugin
    /// can aggregate per-turn signal out-of-the-box (without the debug log).
    /// `None` until the first turn completes.
    pub last_turn_metrics: Mutex<Option<TurnMetrics>>,

    /// Rolling, KV-cache-aware work-state summary (goal / done / in-progress /
    /// next / recent files). Maintained incrementally from conversation signals
    /// and injected as a TRANSIENT tail system message before every request —
    /// never persisted — so it never invalidates the cached conversation prefix.
    /// See the `WorkState` block comment for the full cache strategy.
    pub work_state: Mutex<WorkState>,
    /// Concern/blocker recommendations persist as a compact transient tail on
    /// subsequent requests until a later mutation touches their target path.
    pub open_advisories: Mutex<Vec<crate::advisor::OpenAdvisory>>,
    /// First-class goal mode (plan → deploy subagents). See `goal.rs`.
    pub goal: Mutex<goal::GoalMode>,
    /// Cancel token for an in-flight goal deploy task (separate from the
    /// planning turn's token so cancel_goal can stop deploy without racing
    /// the turn join handle).
    pub goal_deploy_cancel: Mutex<Option<CancellationToken>>,
    /// True while the post-deploy synthesizing wrap-up turn is the live turn.
    /// Lets turn teardown finalize the goal even on abort/error paths without
    /// racing the planning turn's drain against a fast deploy.
    pub goal_wrapup_active: std::sync::atomic::AtomicBool,
    /// Intercom bus: in-process mailboxes for subagent ↔ orchestrator and
    /// subagent ↔ subagent coordination.
    pub intercom: IntercomBus,
    /// Tracked subagent runs for status/interrupt/resume (keyed by run id).
    pub subagent_runs: Mutex<std::collections::HashMap<String, subagent::SubagentRun>>,
    /// Pending no-browser OAuth login state (PKCE verifier + redirect_uri),
    /// set when `/login` picks the manual flow (SSH/headless) and consumed by
    /// the `oauth_code` command when the user pastes the code.
    pub pending_oauth: Mutex<Option<oauth::PendingOauth>>,
    /// Cached live peer sessions in this workspace, refreshed every heartbeat
    /// (~8s) by the presence task. Kept in-memory so the anomaly nudge in
    /// `run_turn` can check for concurrent activity WITHOUT a filesystem read
    /// on every tool result (the hot path). Empty when alone. See `presence`.
    pub peers: Mutex<Vec<presence::PresenceRecord>>,
    /// Last time the concurrency anomaly note was emitted, for per-session
    /// rate-limiting so a pathological tool-call loop can't nag every result.
    pub last_concurrency_note: Mutex<Option<std::time::Instant>>,
    /// Digested / ingress-capped tool outputs, keyed by tool+args hash, so an
    /// identical re-call of a read-only tool can restore full content without
    /// re-executing (bash is never restored). Cleared on workspace mutations.
    pub tool_output_cache: Mutex<tool_cache::ToolOutputCache>,
    /// Deferred tool names enabled for this session via `load_tools`. Core tools
    pub enabled_deferred_tools: Mutex<std::collections::HashSet<String>>,
    /// Successfully evaluated snippets retained per language for the deferred
    /// session-scoped eval tool. Each invocation replays this history.
    pub eval_history: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Session-scoped `/undo` count for telemetry (`human_corrections`).
    pub undo_count: std::sync::atomic::AtomicU64,
    /// True after an auto filesystem checkpoint has been taken for the current
    /// turn (so we don't snapshot before every destructive tool in a wave).
    pub auto_checkpoint_taken: std::sync::atomic::AtomicBool,
    /// Session-scoped count of `read_file` hits on `SKILL.md` (skill utilization).
    pub skill_read_count: std::sync::atomic::AtomicU64,
    /// Full conversation compactions completed in this logical session.
    pub compaction_count: std::sync::atomic::AtomicU64,
}

/// Cancel any in-flight turn and drop the one-deep follow-up queue. Shared by
/// `/abort`, `/new`, `/clear`, `/reset`, and `load_session` so conversation
/// boundaries never leave a prior turn streaming into the new context.
async fn cancel_in_flight_turn(state: &State, reason: CancellationReason, replace_session: bool) {
    let cancellation_started = std::time::Instant::now();
    *state.queued.lock().await = None;
    let cancelled = state.runtime.cancel_current(reason);
    if replace_session {
        state.runtime.replace_session(reason);
    }
    if let Some(run) = state.current.lock().await.take() {
        run.cancellation().cancel();
    }
    if let Some(goal) = state.goal_deploy_cancel.lock().await.take() {
        goal.cancel();
    }
    {
        fn cancel_run_tree(run: &subagent::SubagentRun) {
            if let Some(cancel) = &run.cancel {
                cancel.cancel();
            }
            for child in &run.children {
                cancel_run_tree(child);
            }
        }
        let runs = state.subagent_runs.lock().await;
        for run in runs.values() {
            cancel_run_tree(run);
        }
    }
    state.intercom.reset();

    // Wake every interactive waiter. Their run cancellation is authoritative,
    // but explicit notification bounds cleanup even if a waiter is between
    // registering itself and entering its cancellation select.
    for pending in state.pending.lock().await.values() {
        *pending.granted.lock().await = Some(false);
        pending.notify.notify_waiters();
    }
    for pending in state.pending_asks.lock().await.values() {
        *pending.answers.lock().await = Some(Value::Null);
        pending.notify.notify_waiters();
    }
    for pending in state.pending_sudos.lock().await.values() {
        *pending.result.lock().await = Some(None);
        pending.notify.notify_waiters();
    }

    // A session boundary must not race state replacement against a still-live
    // turn. Most turns stop immediately through their cancellation token; abort
    // the Rust task after a bounded grace period as a final containment step.
    let mut forced_abort = false;
    let mut cleanup_failures = 0_u64;
    if let Some(mut handle) = state.handle.lock().await.take() {
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => cleanup_failures = cleanup_failures.saturating_add(1),
            Err(_) => {
                forced_abort = true;
                handle.abort();
                if handle.await.is_err() {
                    // A forced task abort returns JoinError::cancelled. Record the
                    // forced containment separately, not as a cleanup failure.
                }
            }
        }
    }
    state.pending.lock().await.clear();
    state.pending_asks.lock().await.clear();
    state.pending_sudos.lock().await.clear();
    let duration_ms = cancellation_started.elapsed().as_millis() as u64;
    let remaining_uncancelled_resources = state
        .runtime
        .snapshot()
        .resources
        .into_iter()
        .filter(|resource| !resource.cancelled)
        .count() as u64;
    cleanup_failures = cleanup_failures.saturating_add(remaining_uncancelled_resources);
    if let Some(cancelled) = cancelled {
        state.logger.log(
            "cancellation",
            json!({
                "session_id": &cancelled.session_id,
                "run_id": &cancelled.run_id,
                "reason": cancelled.reason.as_str(),
                "duration_ms": duration_ms,
                "status": if cleanup_failures == 0 { "completed" } else { "cleanup_failed" },
                "forced_abort": forced_abort,
                "cleanup_failures": cleanup_failures,
            }),
        );
        emit(
            &Event::new("run_cancelled")
                .with("session_id", json!(cancelled.session_id))
                .with("run_id", json!(cancelled.run_id))
                .with("reason", json!(cancelled.reason.as_str()))
                .with("duration_ms", json!(duration_ms))
                .with("forced_abort", json!(forced_abort))
                .with("cleanup_failures", json!(cleanup_failures)),
        );
    }
}

/// Shared tail of `login_oauth` (web flow) and `oauth_code` (manual flow):
/// ensure the provider is configured (no api_key — the token is resolved +
/// refreshed at turn time by enrich_oauth), set it active, persist, emit the
/// success + provider_changed events, and refresh the model list.
/// Uses the free `protocol::emit` so this is safe to call from a `tokio::spawn`
/// task (no non-Send `&dyn Fn` borrow).
async fn finalize_oauth(state: &State, client: &reqwest::Client, preset: &str, label: &str) {
    {
        let mut cfg = state.cfg.write().await;
        if cfg.find_provider(preset).is_none() {
            if let Some(p) = config::find_preset(preset) {
                // OAuth-created configs need the same provider-specific
                // transport headers as API-key configs (Copilot and Kimi are
                // validated against their official client identities).
                cfg.providers
                    .extend(config::preset_provider_configs(p, None));
            } else if let Some(p) = state.plugin_manager.oauth_provider_config(preset) {
                // A plugin-declared OAuth provider (no built-in preset): build
                // the config from the plugin's declared base_url/kind/headers.
                cfg.providers.push(p);
            }
        }
    }
    *state.active_provider.write().await = Some(preset.to_string());
    {
        let cfg = state.cfg.read().await;
        let _ = config::save_providers_config(&cfg.providers, Some(preset));
    }
    state
        .logger
        .log("login_oauth", json!({ "provider": preset }));
    emit(&Event::new("info").with(
        "message",
        json!(format!(
            "logged into {label} via OAuth — you're signed in. Pick a model with /models if needed."
        )),
    ));
    // TUI gates prompt send on `authed`; API-key login emits this, OAuth must too.
    emit(&Event::new("authed").with("ok", json!(true)));
    let rp = state.resolved_provider_enriched().await;
    // Always report has_key=true after a successful OAuth exchange — even if
    // a transient enrich glitch can't re-read the token yet (it's on disk).
    emit(
        &Event::new("provider_changed")
            .with("provider", json!(rp.name))
            .with("kind", json!(rp.kind.as_str()))
            .with("base_url", json!(rp.base_url))
            .with("has_key", json!(true)),
    );
    state.refresh_models(client).await;
    // Confirm models landed so the user isn't left staring at an empty list.
    let n = state.models.read().await.len();
    let mine = state
        .models
        .read()
        .await
        .iter()
        .filter(|m| m.provider == preset)
        .count();
    emit(&Event::new("info").with(
        "message",
        json!(format!(
            "OAuth ready: {mine} {label} model(s) available ({n} total across providers)."
        )),
    ));
}

/// Pick which provider should serve a model id given its owner providers (in
/// aggregated-list order) and the effective active provider name. The ACTIVE
/// provider is authoritative: when it owns the model it always wins the tie,
/// and when it does NOT own the model we return `None` (no deterministic pick)
/// so the caller sends the id to the ACTIVE provider anyway — it will answer
/// with its own "unknown model" error rather than silently routing the turn to
/// a DIFFERENT provider's endpoint. There is deliberately NO cross-provider
/// failover: a down/unknown model must surface as an error on the provider the
/// user selected, never silently use another provider that happens to serve the
/// same model id. A single sole owner is served by that owner (unambiguous
/// routing; the active provider is a different provider that doesn't list the
/// id). Empty owners -> None (caller falls back to the active/legacy provider,
/// matching the pre-provider-tag behavior).
fn pick_provider_for_model<'a>(owners: &'a [String], active: Option<&str>) -> Option<&'a str> {
    match owners.len() {
        0 => None,
        1 => owners.first().map(|s| s.as_str()),
        _ => active
            .and_then(|a| owners.iter().find(|o| o.as_str() == a))
            .map(|s| s.as_str()),
    }
}

/// Like [`pick_provider_for_model`], but the caller may carry an EXPLICIT
/// provider pick (the provider the user selected alongside the model in the
/// `/models` picker). A pick that is a verified owner of the model id WINS
/// outright — this is the fix for "can't use the other provider's copy of the
/// same model id": selecting a model uses its owning provider, no `/login` +
/// provider switch needed. A pick that is NOT an owner (stale model list,
/// hand-written command) is ignored and routing falls back to the legacy
/// active-provider tie-break, preserving the no-silent-cross-provider-failover
/// rule.
fn pick_provider_for_model_with<'a>(
    owners: &'a [String],
    active: Option<&str>,
    pick: Option<&str>,
) -> Option<&'a str> {
    if let Some(p) = pick {
        if let Some(o) = owners.iter().find(|o| o.as_str() == p) {
            return Some(o.as_str());
        }
    }
    pick_provider_for_model(owners, active)
}

impl State {
    /// Resolve the active provider for an API call: kind, base URL, effective
    /// API key (runtime override -> config literal -> config env var -> global
    /// env), and extra headers. Combines the config snapshot with the runtime
    /// active-provider override and per-provider keys. This is the single
    /// source of truth every provider call site uses, so switching providers
    /// (or setting a key) takes effect on the next call with no other wiring.
    ///
    /// Note: does **not** inject OAuth subscription tokens — use
    /// [`Self::resolved_provider_enriched`] for that (turns, ready/authed).
    pub async fn resolved_provider(&self) -> ResolvedProvider {
        let cfg = self.cfg.read().await;
        let active = self.active_provider.read().await.clone();
        let keys = self.api_keys.read().await.clone();
        cfg.resolve_provider_with(&keys, active.as_deref())
    }

    /// Like [`Self::resolved_provider`], then fill in a SuperGrok / Claude /
    /// Gemini / plugin OAuth bearer when no API key is configured. Use this
    /// for status (`authed` / `has_key`) and any call that must talk to the
    /// vendor — OAuth-only providers (xAI) have no env key and would otherwise
    /// look permanently signed-out.
    pub async fn resolved_provider_enriched(&self) -> ResolvedProvider {
        let rp = self.resolved_provider().await;
        oauth::enrich_oauth(rp, &self.client, Some(&self.plugin_manager)).await
    }

    /// Resolve a named provider into a `ResolvedProvider` (key included when
    /// available). Returns None when no configured provider matches the name.
    /// Used by per-model routing: a model carries its owning provider name, and
    /// the turn is sent to THAT provider's endpoint regardless of which is
    /// "active", so multiple providers can be logged in and used simultaneously.
    pub async fn resolve_provider_by_name(&self, name: &str) -> Option<ResolvedProvider> {
        let cfg = self.cfg.read().await;
        let p = cfg.find_provider(name)?.clone();
        let keys = self.api_keys.read().await;
        Some(resolve_provider_from_config(&p, &keys))
    }

    /// Find a configured Umans provider that has a usable API key, searching
    /// ALL configured providers (not just the active one) so the concurrency
    /// `/v1/usage` poll stays live even when a non-Umans provider is active but
    /// a Umans model is selected. Prefers the active/legacy provider when it is
    /// Umans (the common case); otherwise scans the configured providers.
    pub async fn umans_provider_with_key(&self) -> Option<ResolvedProvider> {
        // Prefer the active/legacy provider when it is Umans with a key.
        let active = self.resolved_provider().await;
        if provider::is_umans(&active.base_url) && active.api_key.is_some() {
            return Some(active);
        }
        // Otherwise scan every configured provider for a Umans one with a key.
        let names: Vec<String> = {
            let cfg = self.cfg.read().await;
            cfg.providers.iter().map(|p| p.name.clone()).collect()
        };
        for name in &names {
            if let Some(rp) = self.resolve_provider_by_name(name).await {
                if provider::is_umans(&rp.base_url) && rp.api_key.is_some() {
                    return Some(rp);
                }
            }
        }
        None
    }

    /// Resolve a Umans provider to force-refresh the model cache at startup.
    /// Prefers a configured Umans provider that has a key (the logged-in case,
    /// same as [`Self::umans_provider_with_key`]); falls back to ANY Umans
    /// provider — including the legacy keyless default — because the Umans
    /// `/models/info` endpoint is public and needs no auth, so an
    /// unauthenticated default still benefits from a startup model refresh.
    /// Returns `None` only when no Umans provider is active or configured.
    pub async fn umans_provider_for_model_refresh(&self) -> Option<ResolvedProvider> {
        if let Some(rp) = self.umans_provider_with_key().await {
            return Some(rp);
        }
        // No key'd Umans provider — accept a keyless one (public endpoint).
        // Check the active/legacy provider first, then any configured provider.
        let active = self.resolved_provider().await;
        if provider::is_umans(&active.base_url) {
            return Some(active);
        }
        let names: Vec<String> = {
            let cfg = self.cfg.read().await;
            cfg.providers.iter().map(|p| p.name.clone()).collect()
        };
        for name in &names {
            if let Some(rp) = self.resolve_provider_by_name(name).await {
                if provider::is_umans(&rp.base_url) {
                    return Some(rp);
                }
            }
        }
        None
    }

    /// Resolve the provider that should serve a turn for `model`: look up the
    /// model in the aggregated list, route to its owning provider; fall back to
    /// the active/legacy provider when the model has no provider tag (legacy
    /// single-provider models) or its provider isn't configured. This is the
    /// per-model routing seam that lets `/models` mix models from several
    /// logged-in providers. When several providers serve the SAME model id
    /// (e.g. two gateways both listing `deepseek-v4-flash`), the user's
    /// ACTIVE provider wins the tie so the selected provider is actually used.
    /// There is NO cross-provider failover: when the active provider does not
    /// serve the id (and no single owner is unambiguous), the turn goes to the
    /// ACTIVE provider anyway so a down/unknown model surfaces as that
    /// provider's error instead of silently using another provider's copy of
    /// the same model name.
    /// Back-compat wrapper: resolve with no explicit provider pick (routing
    /// falls back to the active-provider tie-break). Newer call sites that know
    /// the user's selected provider use [`Self::resolve_provider_for_model_pick`].
    pub async fn resolve_provider_for_model(&self, model: &str) -> ResolvedProvider {
        self.resolve_provider_for_model_pick(model, None).await
    }

    /// Resolve the provider for a turn serving `model`, honoring an explicit
    /// `pick` (the provider the user selected in `/models`) when present. The
    /// pick WINS over the active-provider tie-break so selecting a model always
    /// uses its owning provider — no `/login` + provider switch needed. When
    /// `pick` is absent or not an owner of `model`, routing falls back to the
    /// legacy active-provider tie-break (see [`pick_provider_for_model`]).
    pub async fn resolve_provider_for_model_pick(
        &self,
        model: &str,
        pick: Option<&str>,
    ) -> ResolvedProvider {
        // Effective active provider: runtime override (set_provider) wins over
        // the config's `activeProvider` — same precedence as aggregate_models.
        let active = {
            let cfg = self.cfg.read().await;
            self.active_provider
                .read()
                .await
                .clone()
                .or_else(|| cfg.active_provider.clone())
        };
        // Owner providers for this model id, in aggregated-list order (deduped).
        let owners: Vec<String> = {
            let models = self.models.read().await;
            let mut seen = std::collections::HashSet::new();
            models
                .iter()
                .filter(|m| m.id == model && !m.provider.is_empty())
                .filter_map(|m| {
                    if seen.insert(m.provider.clone()) {
                        Some(m.provider.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        // An explicit pick that actually owns the model wins outright. This is
        // the fix for "can't use the other provider's copy of the same model":
        // the user's selection overrides the active-provider tie-break. A stale
        // or non-owner pick falls back to the legacy active-provider tie-break.
        let provider_name = pick_provider_for_model_with(&owners, active.as_deref(), pick);
        if let Some(name) = provider_name {
            if let Some(rp) = self.resolve_provider_by_name(name).await {
                return oauth::enrich_oauth(rp, &self.client, Some(&self.plugin_manager)).await;
            }
        }
        let rp = self.resolved_provider().await;
        oauth::enrich_oauth(rp, &self.client, Some(&self.plugin_manager)).await
    }

    /// Resolve the model-info entry (caps) for `model` the same way turns
    /// route: when several providers serve the SAME id, prefer the ACTIVE
    /// provider's entry — its caps reflect what that provider actually enforces
    /// (e.g. a gateway that caps ctx lower than the upstream model's
    /// models.dev figure). Falls back to the first listed owner, then any
    /// entry by id. `None` when the id is unknown.
    ///
    /// Prefer [`Self::model_info_for_pick`] when the client selected an
    /// explicit provider alongside the model — otherwise duplicate ids can
    /// resolve to the active provider's caps while the turn itself routes to
    /// the picked owner (wrong max_tokens / context_window).
    pub async fn model_info_for(&self, model: &str) -> Option<crate::protocol::ModelInfo> {
        self.model_info_for_pick(model, None).await
    }

    /// Like [`Self::model_info_for`], but honors an explicit provider `pick`
    /// the same way [`Self::resolve_provider_for_model_pick`] does. Caps and
    /// routing must agree on which owner wins for a duplicated model id.
    pub async fn model_info_for_pick(
        &self,
        model: &str,
        pick: Option<&str>,
    ) -> Option<crate::protocol::ModelInfo> {
        let models = self.models.read().await;
        let active = {
            let cfg = self.cfg.read().await;
            self.active_provider
                .read()
                .await
                .clone()
                .or_else(|| cfg.active_provider.clone())
        };
        select_model_info(&models, model, active.as_deref(), pick)
    }

    /// The set of provider names that are "logged in": configured providers
    /// with a usable key (runtime key -> config literal -> env var). The
    /// aggregation layer discovers models only for these, so `/models` shows
    /// exactly the providers the user has authenticated. The legacy default
    /// (Umans, when no providers are configured) is included when it has a key.
    pub async fn logged_in_providers(&self) -> Vec<String> {
        let cfg = self.cfg.read().await;
        let keys = self.api_keys.read().await;
        logged_in_providers_for(&cfg, &keys, Some(&self.plugin_manager))
    }

    /// Aggregate models across ALL logged-in providers, tagging each model with
    /// its owning provider name so per-model routing works. Deduplicates by
    /// (provider, id). When no provider is logged in, falls back to a single
    /// discovery of the active/legacy provider (so first-run still shows a model
    /// list before logging in, and the unauthenticated Umans default keeps working).
    /// `force_refresh` bypasses the disk-cache TTL for every provider (live
    /// fetch + cache rewrite) — used by the on-demand `refresh_models` command.
    pub async fn aggregate_models(
        &self,
        client: &reqwest::Client,
        force_refresh: bool,
    ) -> Vec<ModelInfo> {
        let cfg = self.cfg.read().await.clone();
        let keys = self.api_keys.read().await.clone();
        let active = self.active_provider.read().await.clone();
        aggregate_models_for(
            &cfg,
            &keys,
            active.as_deref(),
            client,
            Some(&self.plugin_manager),
            force_refresh,
        )
        .await
    }

    /// Re-aggregate models, store them, and emit a `models` event + a refreshed
    /// `provider_presets` event. Shared by login/logout/set_key/set_provider so
    /// every auth change keeps `/models` in sync across all logged-in providers.
    pub async fn refresh_models(&self, client: &reqwest::Client) {
        let models = self.aggregate_models(client, false).await;
        *self.models.write().await = models.clone();
        emit(&Event::new("models").with("models", json!(models)));
        let presets = {
            let cfg = self.cfg.read().await;
            provider_presets_json(&cfg, Some(&self.plugin_manager))
        };
        emit(&Event::new("provider_presets").with("presets", json!(presets)));
    }

    /// On-demand cache refresh: force a LIVE model discovery for every
    /// logged-in provider (bypassing the 8h disk-cache TTL, rewriting the
    /// cache), then re-aggregate + re-emit `models`/`provider_presets` like
    /// `refresh_models`. HTTP failure per provider falls back to the stale
    /// cache / curated snapshot, so a dead endpoint never shrinks the list.
    /// Emits `models_refreshed` with the count + per-provider ids so clients
    /// can clear their spinner and report what changed. Expensive (one live
    /// /models call per provider) — callers MUST run it off the command loop.
    pub async fn refresh_models_force(&self, client: &reqwest::Client) {
        let models = self.aggregate_models(client, true).await;
        *self.models.write().await = models.clone();
        emit(&Event::new("models").with("models", json!(models)));
        let presets = {
            let cfg = self.cfg.read().await;
            provider_presets_json(&cfg, Some(&self.plugin_manager))
        };
        emit(&Event::new("provider_presets").with("presets", json!(presets)));
        let mut per_provider: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for m in &models {
            per_provider
                .entry(m.provider.clone())
                .or_default()
                .push(m.id.clone());
        }
        emit(
            &Event::new("models_refreshed")
                .with("count", json!(models.len()))
                .with("providers", json!(per_provider)),
        );
    }

    /// Drop the real-usage baseline so estimates fall back to a full char/4 of
    /// the conversation until the next request re-establishes it. Call this
    /// whenever history is rewritten/replaced — compaction, soft-digest, reset,
    /// clear, new-session, undo, load-session, memory refresh — because the old
    /// `prompt_tokens` baseline no longer describes the (now-changed) messages.
    pub async fn invalidate_real_token_baseline(&self) {
        *self.last_real_prompt_tokens.lock().await = None;
        *self.conv_len_at_last_real.lock().await = 0;
    }

    /// Build the `args` payload handed to `session_stop` lifecycle hooks (when
    /// `pass_args: true`): the cumulative session totals plus the
    /// just-completed turn's metrics. Lets a telemetry plugin aggregate
    /// per-turn signal without the JSONL debug log (off by default). `turn`
    /// is null until the first turn completes. Includes memory recall stats
    /// when the turn offered relevant/synonym-miss memories.
    pub async fn session_stop_hook_args(&self) -> Value {
        let session = json!({
            "turns": self.logger.turn_count(),
            "tokens_in": *self.tokens_in.lock().await,
            "tokens_out": *self.tokens_out.lock().await,
            "cached_tokens": *self.cached_tokens.lock().await,
            "model": self.last_model.lock().await.clone().unwrap_or_default(),
        });
        let turn = self
            .last_turn_metrics
            .lock()
            .await
            .as_ref()
            .map(|m| {
                json!({
                    "tokens_in": m.tokens_in,
                    "tokens_out": m.tokens_out,
                    "cached_tokens": m.cached_tokens,
                    "ttft_ms": m.ttft_ms,
                    "elapsed_ms": m.elapsed_ms,
                    "tps": m.tps,
                    "model": m.model,
                })
            })
            .unwrap_or(Value::Null);
        let workspace = self.cfg.read().await.workspace.clone();
        let memory_recall = memory_recall::summary_json(&workspace);
        let undo_count = self.undo_count.load(std::sync::atomic::Ordering::Relaxed);
        let skill_reads = self
            .skill_read_count
            .load(std::sync::atomic::Ordering::Relaxed);
        json!({
            "session": session,
            "turn": turn,
            "memory_recall": memory_recall,
            "human_corrections": { "undo_count": undo_count },
            "skill_utilization": { "skill_md_reads": skill_reads },
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Multi-provider aggregation + per-model routing (free functions)
//
// These take a `&Config` + `&HashMap<String,String>` runtime keys (and an
// optional active-provider override) so they can run BEFORE the `State` exists
// (init discovery) as well as on the live state. The State wrappers above are
// thin facades over these. The harness keeps the conversation in OpenAI
// chat-completions shape internally; a provider's `kind` only decides the wire
// translation at the HTTP boundary, so routing a turn to a different provider
// needs no other change.
// ────────────────────────────────────────────────────────────────────────────

/// Resolve a `ProviderConfig` into a `ResolvedProvider` against the runtime key
/// map (runtime key -> config literal -> config env var). Empty keys are dropped.
pub fn resolve_provider_from_config(
    p: &config::ProviderConfig,
    keys: &HashMap<String, String>,
) -> ResolvedProvider {
    ResolvedProvider {
        name: p.name.clone(),
        kind: p.kind.clone(),
        base_url: p.base_url.clone(),
        api_key: provider_usable_api_key(p, keys),
        headers: p.headers.clone(),
        oauth: false,
        context_window: p.context_window,
        models_override: p.models_override.clone(),
        models_endpoint: p.models_endpoint.clone(),
    }
}

/// Effective non-empty API key for a configured provider (runtime override ->
/// config literal -> env var). Empty strings are treated as missing so a
/// cleared/blank key cannot look "logged in" or be sent as a Bearer token.
fn provider_usable_api_key(
    p: &config::ProviderConfig,
    keys: &HashMap<String, String>,
) -> Option<String> {
    keys.get(&p.name)
        .cloned()
        .or_else(|| p.api_key.clone())
        .or_else(|| {
            p.api_key_env.as_ref().and_then(|v| {
                // Empty env-var *name* means keyless (Ollama/LM Studio style),
                // not "look up the empty string in the environment".
                if v.is_empty() {
                    None
                } else {
                    std::env::var(v).ok()
                }
            })
        })
        .filter(|s| !s.is_empty())
}

/// Lowercased host from a URL (`https://api.x.ai/v1` → `api.x.ai`). Best-effort
/// without a URL crate — mirrors the plugin OAuth host matcher so aggregation
/// and turn-time enrich agree on alias detection.
fn provider_url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, h)| h).unwrap_or(url);
    let host = rest.split(['/', ':']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// True for loopback endpoints (Ollama / LM Studio / local gateways) that can
/// run without an API key.
fn is_loopback_base_url(base_url: &str) -> bool {
    match provider_url_host(base_url).as_deref() {
        Some("localhost") | Some("127.0.0.1") | Some("::1") => true,
        _ => false,
    }
}

/// Keyless local provider: no non-empty `api_key_env` is required AND the
/// endpoint is loopback. Cloud OAuth providers also omit `api_key_env` but are
/// gated separately via [`oauth_creds_for_provider`] — never treat a remote
/// host as keyless just because a key is missing.
fn is_keyless_local_provider(p: &config::ProviderConfig) -> bool {
    let requires_key_env = p.api_key_env.as_ref().is_some_and(|e| !e.is_empty());
    !requires_key_env && is_loopback_base_url(&p.base_url)
}

/// Names of providers that are "logged in": configured providers with a usable
/// non-empty key, a keyless local endpoint, or reusable OAuth credentials. The
/// aggregation layer discovers models only for these, so `/models` shows
/// exactly the providers the user has authenticated. When no providers are
/// configured (legacy single-endpoint Umans setup) this returns empty so that
/// `aggregate_models_for`'s `names.is_empty()` branch handles the legacy
/// default discovery — returning a synthetic "default" name here would break,
/// because `find_provider("default")` finds no explicit entry to resolve.
pub fn logged_in_providers_for(
    cfg: &Config,
    keys: &HashMap<String, String>,
    pm: Option<&plugins::PluginManager>,
) -> Vec<String> {
    if cfg.providers.is_empty() {
        return Vec::new();
    }
    cfg.providers
        .iter()
        .filter(|p| {
            // Logged in =
            //   * non-empty API key (runtime/config/env), OR
            //   * keyless local (loopback + no required key env), OR
            //   * OAuth-capable provider with reusable credentials.
            // Empty strings must NOT count — resolve_provider drops them, and
            // a blank key previously made the provider look authed while every
            // turn still failed with "no API key".
            provider_usable_api_key(p, keys).is_some()
                || is_keyless_local_provider(p)
                || oauth_creds_for_provider(p, pm)
        })
        .map(|p| p.name.clone())
        .collect()
}

/// True when a provider's reusable OAuth credentials exist (cheap sync file
/// check, no refresh). Used by `logged_in_providers_for` to gate plugin OAuth
/// providers into aggregation. The actual token refresh happens at turn /
/// discovery time via `oauth::enrich_oauth`.
///
/// Matching mirrors `PluginManager::oauth_config_for_provider`: provider-config
/// name == plugin `provider_id` first, then base_url host. Without the host
/// fallback, a manually-named alias pointing at the plugin endpoint (e.g.
/// `chatgpt` → codex host) would be excluded from aggregation even though
/// turn-time `enrich_oauth` would still inject the token.
fn oauth_creds_for_provider(
    p: &config::ProviderConfig,
    pm: Option<&plugins::PluginManager>,
) -> bool {
    // Subscription OAuth is plugin-only.
    let Some(pm) = pm else {
        return false;
    };
    if pm.oauth_config(&p.name).is_some() {
        return pm.has_oauth_creds(&p.name);
    }
    let Some(host) = provider_url_host(&p.base_url) else {
        return false;
    };
    for cfg in pm.oauth_configs() {
        if provider_url_host(&cfg.base_url).as_deref() == Some(host.as_str()) {
            // Creds are stored under the plugin's provider_id, not the alias name.
            return pm.has_oauth_creds(&cfg.provider_id);
        }
    }
    false
}

/// Pick the `ModelInfo` row for `model` using the same owner/active/pick rules
/// as turn routing ([`pick_provider_for_model_with`]). Pure + sync so unit
/// tests cover caps selection without spinning up `State`.
fn select_model_info(
    models: &[ModelInfo],
    model: &str,
    active: Option<&str>,
    pick: Option<&str>,
) -> Option<ModelInfo> {
    let mut seen = std::collections::HashSet::new();
    let owners: Vec<String> = models
        .iter()
        .filter(|m| m.id == model && !m.provider.is_empty())
        .filter_map(|m| {
            if seen.insert(m.provider.clone()) {
                Some(m.provider.clone())
            } else {
                None
            }
        })
        .collect();
    let picked = pick_provider_for_model_with(&owners, active, pick);
    match picked {
        Some(p) => models
            .iter()
            .find(|m| m.id == model && m.provider == p)
            .cloned()
            .or_else(|| models.iter().find(|m| m.id == model).cloned()),
        // No deterministic owner (multi-owner + non-owner active, or empty
        // owners): fall back to any row by id. Callers that also resolve the
        // provider will pin the turn to the active/legacy endpoint; caps from
        // the first listed row are the least-bad estimate when ownership is
        // ambiguous.
        None => models.iter().find(|m| m.id == model).cloned(),
    }
}

/// Aggregate models across ALL logged-in providers, tagging each model with its
/// owning provider name so per-model routing works. Deduplicates by (provider,
/// id). When no provider is logged in, falls back to a single discovery of the
/// active/legacy provider (first-run before login, unauthenticated Umans default).
/// `force_refresh` bypasses the fresh disk-cache TTL so a live fetch is ALWAYS
/// performed (and the cache rewritten) for every provider — used by the
/// on-demand `refresh_models` command. HTTP failure still falls back to the
/// stale cache / curated snapshot, so a forced refresh never degrades the list.
pub async fn aggregate_models_for(
    cfg: &Config,
    keys: &HashMap<String, String>,
    active: Option<&str>,
    client: &reqwest::Client,
    pm: Option<&plugins::PluginManager>,
    force_refresh: bool,
) -> Vec<ModelInfo> {
    let names = logged_in_providers_for(cfg, keys, pm);
    if names.is_empty() {
        let rp = cfg.resolve_provider_with(keys, active);
        let mut models = if force_refresh {
            providers::discovery::discover_models_force_refresh(client, &rp).await
        } else {
            providers::registry::adapter_for(&rp)
                .discover_models(providers::adapter::ProviderContext {
                    client,
                    provider: &rp,
                })
                .await
        };
        // Legacy/default models get the resolved provider tag so they route
        // correctly; models already tagged (e.g. by an earlier aggregation run
        // round-tripped through the session) keep their tag.
        for m in &mut models {
            if m.provider.is_empty() {
                m.provider = rp.name.clone();
            }
        }
        return models;
    }
    let mut merged: Vec<ModelInfo> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for name in &names {
        let Some(pc) = cfg.find_provider(name) else {
            continue;
        };
        let rp = resolve_provider_from_config(pc, keys);
        // Enrich with an OAuth subscription token (gcloud/`claude` CLI) when the
        // provider has no API key, so OAuth-only providers can discover models.
        let rp = oauth::enrich_oauth(rp, client, pm).await;
        let mut discovered = if force_refresh {
            providers::discovery::discover_models_force_refresh(client, &rp).await
        } else {
            providers::registry::adapter_for(&rp)
                .discover_models(providers::adapter::ProviderContext {
                    client,
                    provider: &rp,
                })
                .await
        };
        for m in &mut discovered {
            m.provider = rp.name.clone();
            if seen.insert((m.provider.clone(), m.id.clone())) {
                merged.push(m.clone());
            }
        }
    }
    merged
}

// ============================================================================
// Rolling, KV-cache-aware work-state summary
// ============================================================================
// A live summary of what the session is working on (goal / done / in-progress
// / next / recently-touched files), maintained incrementally from conversation
// signals — `todo_write` (the structured backbone), the user's first substantive
// message (the goal), and file-edit tool calls — with NO model call, so it is
// free and deterministic.
//
// It is injected as a TRANSIENT tail `system` message right before every model
// request, then stripped: never stored in `conversation`, never persisted to
// the session file. This is the KV-cache strategy:
//
//   * The persisted conversation is strictly append-only from the provider's
//     point of view, so its prefix `[system][u1][a1]…]` is byte-identical turn
//     to turn → the provider's prefix cache hits on everything already sent.
//   * The work-state is the LAST message each turn, so updating it invalidates
//     nothing earlier in the prefix; only the small work-state (~200-400
//     tokens) plus the new turn are prefilled. Contrast with injecting it into
//     the system prompt (position 0), which would invalidate the ENTIRE cache
//     on every change.
//   * Because it is transient, a resumed session never accumulates a trail of
//     stale summaries; the live state is rebuilt from the next signals.
//
// The compaction summary (model-generated, in the prefix) covers dropped
// history; this rolling state covers the CURRENT state. They complement each
// other: the deep summary is cached and changes only on compaction; the
// rolling state changes often but lives at the cheap tail.

/// Rolling work-state summary. See the block comment above for the cache
/// strategy. Updated by the signal helpers below and rendered into the
/// transient tail message by `work_state_message`.
pub(crate) use crate::agent::goal_runtime::*;

/// Mirror a `todo_write` payload into the work-state's done/in-progress/next
/// lists. The todo list IS the structured work state; this keeps the rolling
/// summary in sync so the model sees current progress every turn without a
/// `todo_read` round-trip.
async fn sync_work_state_from_todos(st: &State, args: &Value) {
    let Some(todos) = args.get("todos").and_then(|v| v.as_array()) else {
        return;
    };
    let mut ws = st.work_state.lock().await;
    ws.sync_from_todos(todos);
    drop(ws);
    emit_work_state(st).await;
}

/// Record a file touch from a write/edit/patch/bulk_* call into the work-state
/// recent-files list (most-recent-first, deduped, capped). Keeps the rolling
/// summary aware of what the session has actually changed.
async fn maybe_auto_checkpoint(st: &State) {
    if st
        .auto_checkpoint_taken
        .swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let cfg = st.cfg.read().await;
    let _ = checkpoint::create(
        &cfg.workspace,
        cfg.session_file.as_deref(),
        "auto-before-destructive",
        &[],
        true,
    );
}

/// Warm the tool-output cache with readonly greps/globs suggested by recent
/// pattern-log categories and tokens from the user prompt. Cap concurrency at 2.
async fn speculative_prefetch(st: &Arc<State>, prompt: &str) {
    let cfg = st.cfg.read().await.clone();
    let patterns = pattern_log::recurring_patterns(&cfg.workspace);
    let mut globs: Vec<String> = patterns
        .into_iter()
        .take(4)
        .filter_map(|(_, label)| {
            // Labels look like "edit|core/src/*.rs" — pull the file category.
            label.split('|').nth(1).map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty() && s != "<root>")
        .collect();
    // Also pull a couple of significant tokens from the prompt as grep patterns.
    let mut greps: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| t.len() >= 4 && t.len() <= 40)
        .take(3)
        .map(|s| s.to_string())
        .collect();
    greps.dedup();
    globs.dedup();
    if greps.is_empty() && globs.is_empty() {
        return;
    }
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    let mut handles = Vec::new();
    for g in greps.into_iter().take(2) {
        let stc = st.clone();
        let cfg = cfg.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _p = sem.acquire().await.ok();
            let args = json!({ "pattern": g, "head_limit": 20 });
            let args_str = args.to_string();
            let outcome = tokio::task::spawn_blocking(move || tools::execute("grep", &args, &cfg))
                .await
                .unwrap_or_else(|_| tools::Outcome::err("prefetch panicked"));
            if outcome.ok {
                stc.tool_output_cache
                    .lock()
                    .await
                    .store("grep", &args_str, &outcome.output);
            }
        }));
    }
    for g in globs.into_iter().take(2) {
        let stc = st.clone();
        let cfg = cfg.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _p = sem.acquire().await.ok();
            let args = json!({ "pattern": g });
            let args_str = args.to_string();
            let outcome = tokio::task::spawn_blocking(move || tools::execute("glob", &args, &cfg))
                .await
                .unwrap_or_else(|_| tools::Outcome::err("prefetch panicked"));
            if outcome.ok {
                stc.tool_output_cache
                    .lock()
                    .await
                    .store("glob", &args_str, &outcome.output);
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

/// Record a file touch from a write/edit/patch/bulk_* call into the work-state
/// recent-files list (most-recent-first, deduped, capped). Keeps the rolling
/// summary aware of what the session has actually changed.
async fn record_file_touch(st: &State, tool: &str, args: &Value) {
    let paths: Vec<String> = match tool {
        "bulk_write" => args
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        "bulk_edit" => args
            .get("edits")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(|v| v.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        "rename" => {
            let mut v = Vec::new();
            if let Some(p) = args.get("from").and_then(|v| v.as_str()) {
                v.push(p.to_string());
            }
            if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                v.push(p.to_string());
            }
            v
        }
        _ => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
    };
    if paths.is_empty() {
        return;
    }
    let mut ws = st.work_state.lock().await;
    ws.record_files(tool, &paths);
    drop(ws);
    emit_work_state(st).await;
}

/// Cooldown between concurrency-anomaly notes (seconds). Prevents nagging in a
/// tight tool-call loop; one nudge per minute is enough to change behavior from
/// "fix the phantom error" to "check the neighbors".
const CONCURRENCY_NOTE_COOLDOWN: u64 = 60;

/// If another session is active in this workspace AND something looks off (a
/// tool failed, or we're touching a file a peer recently touched), surface a
/// short note appended to the tool result — so the agent doesn't assume every
/// error is its own fault and "fix" a neighbor's in-flight work. Uses the cached
/// peer snapshot (refreshed by the heartbeat) so the hot path does NO filesystem
/// read. Rate-limited per session to avoid nagging.
async fn maybe_concurrency_note(
    st: &State,
    tool_name: &str,
    args: &Value,
    outcome_ok: bool,
) -> Option<String> {
    // Cooldown: at most one note per window. Checked first (before cloning the
    // peer snapshot) so a tight loop short-circuits cheaply after the first nudge.
    {
        let last = st.last_concurrency_note.lock().await;
        if let Some(t) = *last {
            if t.elapsed() < std::time::Duration::from_secs(CONCURRENCY_NOTE_COOLDOWN) {
                return None;
            }
        }
    }
    let peers = st.peers.lock().await.clone();
    if peers.is_empty() {
        return None; // alone — nothing to surface; don't arm the cooldown
    }
    let touching = peers_touching(&peers, tool_name, args);
    // (a) a tool failed — could be a neighbor leaving the tree inconsistent.
    if !outcome_ok {
        *st.last_concurrency_note.lock().await = Some(std::time::Instant::now());
        let mut s = format!(
            "⚠ {} other agent session(s) are active in this workspace. This error may \
             not be from your changes — another session may have left the tree in an \
             inconsistent state. Consider `workspace_activity` to inspect before \
             'fixing' it.",
            peers.len()
        );
        if !touching.is_empty() {
            s.push_str(&format!(" Active sessions recently touched: {touching}."));
        }
        return Some(s);
    }
    // (b) a file tool touching a path a peer recently touched (a real conflict).
    if !touching.is_empty() {
        *st.last_concurrency_note.lock().await = Some(std::time::Instant::now());
        return Some(format!(
            "ℹ Another agent session in this workspace recently touched: {touching}. \
             You may be reading/editing in-flight work — consider `workspace_activity` \
             to coordinate."
        ));
    }
    None
}

/// Return a comma-list of "pid N" for peers whose recent_files contain the
/// current tool's target path. Exact separator-normalized match — precise to
/// avoid false-positive nagging; a miss just means no warning (safe).
fn peers_touching(peers: &[presence::PresenceRecord], tool_name: &str, args: &Value) -> String {
    let path = match tool_name {
        "read_file" | "edit" | "write_file" | "patch" | "bulk_read" | "bulk_write"
        | "bulk_edit" => args.get("path").and_then(|v| v.as_str()).unwrap_or(""),
        _ => "",
    };
    if path.is_empty() {
        return String::new();
    }
    let target = path.replace('\\', "/");
    let hitting: Vec<_> = peers
        .iter()
        .filter(|p| {
            p.recent_files
                .iter()
                .any(|f| f.replace('\\', "/") == target)
        })
        .map(|p| format!("pid {}", p.pid))
        .collect();
    hitting.join(", ")
}

/// Build the transient work-state system message, or `None` when disabled or
/// when there is no state to show yet. The caller pushes it as the LAST message
/// before the model request and pops it right after, so it never reaches the
/// persisted conversation or the session file — the conversation prefix stays
/// byte-identical turn to turn and the provider's prefix cache is never
/// invalidated by it.
async fn work_state_message(st: &State) -> Option<Message> {
    if !st.cfg.read().await.rolling_state {
        return None;
    }
    let ws = st.work_state.lock().await;
    if ws.is_empty() {
        return None;
    }
    Some(Message::system(ws.render()))
}

/// Reset the rolling work-state (new session / reset / clear / undo / load).
/// Emits an empty `work_state` so frontends clear their panel. Also clears
/// goal mode so a new session doesn't inherit a stale plan/deploy.
async fn clear_work_state(st: &State) {
    cancel_goal_deploy(st).await;
    {
        let mut g = st.goal.lock().await;
        if g.phase != goal::GoalPhase::Idle {
            goal::clear_goal(&mut g);
        }
    }
    *st.work_state.lock().await = WorkState::default();
    emit_work_state(st).await;
}

/// Persist cumulative session stats to the `<session>.stats` sidecar so `/stats`
/// survives a restart. Called at turn completion (after `record_turn`).
/// Also fsyncs the session JSONL so the turn's appends are durable.
async fn persist_stats(st: &State) {
    let Some(p) = st.cfg.read().await.session_file.clone() else {
        return;
    };
    session::sync(&p);
    let stats = session::SessionStats {
        tokens_in: *st.tokens_in.lock().await,
        tokens_out: *st.tokens_out.lock().await,
        cached_tokens: *st.cached_tokens.lock().await,
        turns: st.logger.turn_count(),
        compactions: st
            .compaction_count
            .load(std::sync::atomic::Ordering::Relaxed),
    };
    session::save_stats(&p, &stats);
}

/// Best-effort fsync of the session file (no-op when no session is configured).
/// Used on abort paths that may have appended messages without going through
/// [`persist_stats`].
async fn sync_session_file(st: &State) {
    if let Some(p) = st.cfg.read().await.session_file.as_ref() {
        session::sync(p);
    }
}

/// Zero the cumulative stats in memory and on the sidecar (reset / clear /
/// new session) so a fresh conversation doesn't carry a prior session's totals.
async fn reset_stats(st: &State) {
    *st.tokens_in.lock().await = 0;
    *st.tokens_out.lock().await = 0;
    *st.cached_tokens.lock().await = 0;
    st.logger.set_turns(0);
    st.compaction_count
        .store(0, std::sync::atomic::Ordering::Relaxed);
    if let Some(p) = st.cfg.read().await.session_file.clone() {
        session::save_stats(&p, &session::SessionStats::default());
    }
}

/// Restore cumulative stats from a session's sidecar into memory (load_session /
/// init), so switching/resuming a session shows its real totals.
async fn restore_stats(st: &State, session_path: &std::path::Path) {
    let s = session::load_stats(session_path);
    *st.tokens_in.lock().await = s.tokens_in;
    *st.tokens_out.lock().await = s.tokens_out;
    *st.cached_tokens.lock().await = s.cached_tokens;
    st.logger.set_turns(s.turns);
    st.compaction_count
        .store(s.compactions, std::sync::atomic::Ordering::Relaxed);
}

/// Generate a unique filename for a new session file. std has no date
/// formatting; the picker shows the derived title, not the filename, so a
/// monotonic nanos id is fine. Used only as a fallback when the TUI does not
/// supply a name.
fn new_session_filename() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.jsonl", now.as_nanos())
}

fn main() {
    // The agent turn is intentionally decomposed into helpers, but its joined
    // async state machine still carries provider, tool-wave, plugin, and goal
    // state. Tokio's default worker stack is too small for debug/instrumented
    // builds and can abort the whole core before the panic guard is entered.
    // Keep the stack explicit and bounded rather than depending on the caller's
    // RUST_MIN_STACK environment.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .expect("failed to build core async runtime")
        .block_on(commands::dispatcher::run());
}
/// Check if a tool call matches a permission rule. Used by the approval gate
/// to skip prompting for allow-listed tools, or block deny-listed ones outright.
pub(crate) use crate::tooling::approval::{restricted_path_for_tool, tool_matches_rule};
fn build_skill_prompt(skill: &subagent::SkillEntry, task: Option<&str>) -> String {
    let mut p = format!(
        "Apply the \"{}\" skill. Read and follow the procedure in the skill file below.\n\n<skill name=\"{}\">\n{}\n</skill>\n",
        skill.name, skill.name, skill.body
    );
    if let Some(t) = task.map(str::trim).filter(|t| !t.is_empty()) {
        p.push_str(&format!("\nTask: {}\n", t));
    }
    p
}

/// Expand `@<path>` file mentions in a prompt by inlining the referenced
/// file's contents directly, so the model sees them without a `read_file`
/// round-trip — mirroring how `apply_skill` inlines a skill body. The
/// transcript still shows the concise `@path` (the TUI/web logged the raw
/// text before the core received it); only the message the model reads is
/// expanded.
///
/// A mention is `@` followed by a non-whitespace path, where the `@` is at
/// start-of-string or preceded by whitespace (so emails / `foo@bar` and
/// inline `@param` tags without a leading space don't trigger). Paths resolve
/// relative to the workspace and pass through canonical confinement; absolute,
/// parent-traversing, and symlink-escaping paths are left as plain text.
/// Directories, unreadable files, and files larger than the per-file or
/// aggregate inline budget are likewise left for an explicit `read_file` call.
/// Returns the expanded prompt and the list of paths successfully inlined.
fn expand_file_mentions(
    prompt: &str,
    workspace: &std::path::Path,
    max_bytes: u64,
) -> (String, Vec<String>) {
    const MENTION_INLINE_TOTAL_MAX: usize = 64 * 1024;
    const MENTION_INLINE_FILE_MAX: u64 = 24 * 1024;
    let max_file_bytes = max_bytes.min(MENTION_INLINE_FILE_MAX);
    let chars: Vec<(usize, char)> = prompt.char_indices().collect();
    let mut out = String::with_capacity(prompt.len() + 256);
    let mut attached: Vec<String> = Vec::new();
    let mut inlined_bytes = 0usize;
    let mut k = 0;
    let mut prev_ws_or_start = true;
    while k < chars.len() {
        let (idx, ch) = chars[k];
        if ch == '@' && prev_ws_or_start {
            // Span from after '@' to the next whitespace char (or end).
            let tok_byte_start = idx + '@'.len_utf8();
            let mut m = k + 1;
            while m < chars.len() && !is_mention_ws(chars[m].1) {
                m += 1;
            }
            let tok_byte_end = if m < chars.len() {
                chars[m].0
            } else {
                prompt.len()
            };
            let raw = &prompt[tok_byte_start..tok_byte_end];
            if !raw.is_empty() {
                if let Some((path, content)) = read_mentioned_file(raw, workspace, max_file_bytes) {
                    if inlined_bytes.saturating_add(content.len()) > MENTION_INLINE_TOTAL_MAX {
                        out.push('@');
                        k += 1;
                        prev_ws_or_start = false;
                        continue;
                    }
                    inlined_bytes += content.len();
                    out.push('@');
                    out.push_str(&path);
                    out.push_str("\n<file path=\"");
                    out.push_str(&path);
                    out.push_str("\">\n");
                    out.push_str(&content);
                    if !content.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("</file>\n");
                    attached.push(path);
                    k = m;
                    prev_ws_or_start = true; // the block ends in '\n'
                    continue;
                }
            }
            // Not an attachable mention: emit the '@' and keep scanning.
            out.push('@');
            k += 1;
            prev_ws_or_start = false;
        } else {
            out.push(ch);
            prev_ws_or_start = is_mention_ws(ch);
            k += 1;
        }
    }
    (out, attached)
}

fn is_mention_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// Try to read a mentioned file. The raw token may carry trailing prose
/// punctuation ("see @file.rs." → "file.rs"); try the token verbatim first,
/// then with trailing punctuation stripped, so legitimate paths keep their
/// characters while common prose edge cases still resolve.
fn read_mentioned_file(
    token: &str,
    workspace: &std::path::Path,
    max_bytes: u64,
) -> Option<(String, String)> {
    let trimmed = token.trim_end_matches(|c: char| {
        matches!(
            c,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
        )
    });
    for cand in [token, trimmed] {
        if cand.is_empty() {
            continue;
        }
        if let Some(res) = try_read_mentioned_file(cand, workspace, max_bytes) {
            return Some(res);
        }
    }
    None
}

fn try_read_mentioned_file(
    token: &str,
    workspace: &std::path::Path,
    max_bytes: u64,
) -> Option<(String, String)> {
    let resolved = crate::workspace::resolve(workspace, token).ok()?;
    let meta = std::fs::metadata(&resolved).ok()?;
    if meta.is_dir() {
        return None;
    }
    if meta.len() > max_bytes {
        return None;
    }
    let content = std::fs::read_to_string(&resolved).ok()?;
    Some((token.to_string(), content))
}

/// Start (or queue) an assistant turn for `prompt`. Shared by `send` and
/// `apply_skill`: if a turn is already running, buffer this prompt one-deep
/// (the running turn's drain picks it up); otherwise spawn run_turn_and_drain.
async fn start_turn(
    state: &Arc<State>,
    client: &reqwest::Client,
    model: String,
    provider: Option<String>,
    prompt: String,
    effort: String,
    images: Option<Vec<String>>,
) {
    // Living codebase index: refresh once per turn-start window (throttled
    // inside codebase_index). Fail-open — never block the turn on index I/O.
    // Skip for conversational/strategic prompts: the index, coupling, and
    // coverage only aid code-change tasks, and the context pack still reads
    // the existing (possibly stale) index for file hints when needed.
    if crate::task_fingerprint::looks_like_code_task(&prompt) {
        let ws = state.cfg.read().await.workspace.clone();
        let (project_id, _f, _s) = codebase_index::ensure_index(&ws);
        // Best-effort git coupling refresh (capped internally).
        let _ = change_coupling::refresh_coupling(&ws, &project_id);
        // Coverage ledger rebuild (fail-open, cheap relative to indexing).
        let _ = coverage_ledger::rebuild_coverage(&ws, &project_id);
    }
    // Explicit preference capture from the user prompt (spec §12.1).
    {
        let ws = state.cfg.read().await.workspace.clone();
        let _ = learning_proposals::maybe_capture_explicit_preference(&ws, &prompt);
    }
    let already = state.current.lock().await.is_some();
    if already {
        let mut q = state.queued.lock().await;
        if q.is_some() {
            emit(&Event::new("error").with(
                "message",
                json!("a prompt is already queued; send abort first or wait"),
            ));
        } else {
            *q = Some(QueuedPrompt {
                prompt,
                model,
                provider,
                effort,
            });
            emit(&Event::new("info").with(
                "message",
                json!("prompt queued; will run after the current turn"),
            ));
        }
        return;
    }
    let run = state.runtime.start_run();
    *state.current.lock().await = Some(run.clone());
    let handle = tokio::spawn(run_turn_and_drain(
        state.clone(),
        client.clone(),
        model,
        provider,
        prompt,
        effort,
        images,
        run,
    ));
    *state.handle.lock().await = Some(handle);
}

fn panic_payload_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .map(str::to_string)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-string panic payload)".into())
}

/// Run one assistant turn, then drain a queued prompt (one-deep) into another
/// turn. Shared by `send` (idle start) and `steer` (idle fallback) so the
/// queue-drain logic and the `current` token hand-off live in one place. A
/// follow-up or steer queued while this turn ran is run next; the recursion is
/// via `tokio::spawn` so it never grows the stack. Boxed because recursive
/// async fns can't prove `Send` for `tokio::spawn` on their own.
fn run_turn_and_drain(
    st: Arc<State>,
    client: reqwest::Client,
    model: String,
    provider: Option<String>,
    prompt: String,
    effort: String,
    images: Option<Vec<String>>,
    run: RunContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(runtime::scope_run(run.clone(), async move {
        let Some(resource) =
            st.runtime
                .register_run_resource(&run, ResourceKind::Task, "foreground_agent_turn")
        else {
            return;
        };
        let tok = resource.cancellation().clone();
        append_runtime_run_state(&st, &run, session::RunState::Started, None).await;
        // Run the turn inside a panic guard: if run_turn panics (a bug or a
        // malformed model payload hitting an unwrap/index), we still clear
        // `current` and emit error+done so the TUI never wedges on a stuck
        // "working" footer with no turn actually running.
        let result = AssertUnwindSafe(run_turn(
            &st,
            &client,
            run.clone(),
            model,
            provider,
            prompt,
            effort,
            images,
            tok.clone(),
        ))
        .catch_unwind()
        .await;
        // The turn ended for any reason — notify lifecycle plugins and release
        // the current-token slot unconditionally so new turns can start.
        dispatch_lifecycle(&st, "turn_end").await;
        dispatch_lifecycle(&st, "session_stop").await;
        {
            let mut current = st.current.lock().await;
            if current
                .as_ref()
                .is_some_and(|active| active.run_id() == run.run_id())
            {
                current.take();
            }
        }
        // Flush any `!cmd` context messages that were deferred while this turn
        // ran (must land after tool_use/tool_result pairs are complete).
        flush_pending_bash(&st).await;
        // A turn freed several conversation clones + tool-result buffers
        // (compaction alone drops the old history). glibc malloc keeps those
        // freed bytes in its arenas, so RSS creeps up and never falls — trim the
        // heap back to the OS once per turn to bound long-session growth.
        trim_heap();
        // Finalize a goal wrap-up turn on every exit path (finish, natural
        // stop, abort, panic). Flag is set only when spawn_goal_deploy starts
        // that turn, so a fast deploy cannot race the planning turn's drain.
        if st
            .goal_wrapup_active
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            // Review / verify / wrap-up parent turns share this flag.
            // Do NOT run planning here — that would false-fail a replan mid-flight.
            maybe_finish_goal_followup_turn(&st, &client, tok.is_cancelled()).await;
        }
        if let Err(panic) = result {
            let detail = panic_payload_message(&panic);
            st.logger
                .log("turn_error", json!({ "error": format!("panic: {detail}") }));
            emit(&Event::new("error").with(
                "message",
                json!(format!(
                    "turn terminated unexpectedly (panic): {detail}; please retry"
                )),
            ));
            sync_session_file(&st).await;
            emit(&Event::new("done"));
            append_runtime_run_state(
                &st,
                &run,
                session::RunState::Failed,
                Some("agent turn panicked"),
            )
            .await;
            st.runtime.complete_run(&run);
            return;
        }
        let terminal_state = if tok.is_cancelled() {
            session::RunState::Cancelled
        } else {
            session::RunState::Completed
        };
        append_runtime_run_state(&st, &run, terminal_state, None).await;
        st.runtime.complete_run(&run);
        // Drain a queued prompt if one was buffered while we ran (follow-up/steer).
        let same_session = st.runtime.session_id() == *run.session_id();
        if same_session {
            if let Some(q) = st.queued.lock().await.take() {
                let next_run = st.runtime.start_run();
                *st.current.lock().await = Some(next_run.clone());
                // Store the handle so stdin EOF (which awaits state.handle) waits for
                // this drained turn too — otherwise it may tear the runtime down
                // while a queued follow-up/steer is still running.
                *st.handle.lock().await = Some(tokio::spawn(run_turn_and_drain(
                    st.clone(),
                    client.clone(),
                    q.model,
                    q.provider,
                    q.prompt,
                    q.effort,
                    None,
                    next_run,
                )));
            }
        }
    }))
}

async fn append_runtime_run_state(
    state: &State,
    run: &RunContext,
    status: session::RunState,
    detail: Option<&str>,
) {
    let path = state.cfg.read().await.session_file.clone();
    if let Some(path) = path {
        session::append_run_state(
            &path,
            run.session_id().as_str(),
            run.run_id().as_str(),
            status,
            detail,
        );
    }
}

pub(crate) async fn execute_plugin_hook_logged(
    st: &Arc<State>,
    hook: &str,
    plugin_name: &str,
    config: &plugins::HookConfig,
    context: &Value,
) -> plugins::HookResult {
    let started = std::time::Instant::now();
    let result = plugins::execute_hook(hook, plugin_name, config, context).await;
    st.logger.log(
        "plugin_hook",
        json!({
            "plugin": plugin_name,
            "hook": hook,
            "duration_ms": started.elapsed().as_millis() as u64,
            "status": if result.allow { "allowed" } else { "denied" },
        }),
    );
    result
}

/// Dispatch a lifecycle/session hook (session_start / session_stop /
/// pre_compact) to every enabled plugin that registered for it. Best-effort:
/// lifecycle hooks run for their side effects and never block the turn (their
/// `allow`/`deny` is ignored; a failing/timed-out/missing hook is skipped, not
/// fatal). This wires the hook points that were previously advertised in
/// HOOK_POINTS but never dispatched.
pub(crate) async fn dispatch_lifecycle(st: &Arc<State>, hook: &str) {
    let (workspace, session_id) = {
        let cfg = st.cfg.read().await;
        (
            cfg.workspace.display().to_string(),
            cfg.session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
    };
    // For session_stop, attach cumulative + last-turn metrics so a telemetry
    // plugin can aggregate per-turn signal out-of-the-box, without the
    // (off-by-default) JSONL debug log. Built once; each plugin's pass_args
    // decides whether it is included in its context.
    let metrics_args: Option<Value> = if hook == "session_stop" {
        Some(st.session_stop_hook_args().await)
    } else {
        None
    };
    for (plugin_name, config) in &st.plugin_manager.get_hook_configs(hook) {
        let ctx = plugins::build_context(
            hook,
            "",
            &workspace,
            metrics_args.as_ref(),
            &session_id,
            config.pass_args,
        );
        let _ = execute_plugin_hook_logged(st, hook, plugin_name, config, &ctx).await;
    }
}

/// Caps for agent-loop hook `modify` payloads (P0-H1). Invalid / oversized
/// modifies are ignored (no-op + log) so a bad plugin cannot corrupt the turn.
const PRE_INPUT_MAX_BYTES: usize = 1_048_576;
const PRE_CONTEXT_MAX_MESSAGES: usize = 500;
const PRE_CONTEXT_MAX_BYTES: usize = 8 * 1_048_576;
const SYSTEM_PROMPT_MODIFY_MAX_BYTES: usize = 100 * 1024;

/// Run `pre_input` hooks over the user's text before it becomes a Message.
/// Returns `Err(reason)` when a hook denies (honor_allow); otherwise the
/// (possibly modified) text. Failures that Deny are surfaced to the caller.
pub(crate) async fn run_pre_input(st: &Arc<State>, text: &str) -> Result<String, String> {
    let configs = st.plugin_manager.get_hook_configs("pre_input");
    if configs.is_empty() {
        return Ok(text.to_string());
    }
    let (workspace, session_id) = {
        let cfg = st.cfg.read().await;
        (
            cfg.workspace.display().to_string(),
            cfg.session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
    };
    let mut current = text.to_string();
    for (plugin_name, config) in &configs {
        let args = json!({ "text": current });
        let ctx = plugins::build_context(
            "pre_input",
            "",
            &workspace,
            Some(&args),
            &session_id,
            config.pass_args,
        );
        let result = execute_plugin_hook_logged(st, "pre_input", plugin_name, config, &ctx).await;
        if !result.allow {
            return Err(format!(
                "input denied by plugin '{}' pre_input: {}",
                plugin_name, result.reason
            ));
        }
        if let Some(obj) = result.modify.as_ref().and_then(|m| m.as_object()) {
            if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
                if t.len() > PRE_INPUT_MAX_BYTES {
                    eprintln!(
                        "[plugins] pre_input modify.text from '{plugin_name}' exceeds {PRE_INPUT_MAX_BYTES} bytes; ignored"
                    );
                } else {
                    current = t.to_string();
                }
            }
        }
    }
    Ok(current)
}

/// Run `pre_agent_start` hooks. Collects `append_system_prompt` / `system_prompt`
/// modifies into a transient system-prompt fragment (caller pushes as a
/// non-persisted message before the LLM call). Advisory — fail-open.
pub(crate) async fn run_pre_agent_start(st: &Arc<State>) -> Option<String> {
    let configs = st.plugin_manager.get_hook_configs("pre_agent_start");
    if configs.is_empty() {
        return None;
    }
    let (workspace, session_id) = {
        let cfg = st.cfg.read().await;
        (
            cfg.workspace.display().to_string(),
            cfg.session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
    };
    let mut replaced: Option<String> = None;
    let mut appends: Vec<String> = Vec::new();
    for (plugin_name, config) in &configs {
        let ctx = plugins::build_context(
            "pre_agent_start",
            "",
            &workspace,
            None,
            &session_id,
            config.pass_args,
        );
        let result =
            execute_plugin_hook_logged(st, "pre_agent_start", plugin_name, config, &ctx).await;
        if let Some(obj) = result.modify.as_ref().and_then(|m| m.as_object()) {
            if let Some(s) = obj.get("system_prompt").and_then(|v| v.as_str()) {
                if s.len() > SYSTEM_PROMPT_MODIFY_MAX_BYTES {
                    eprintln!(
                        "[plugins] pre_agent_start modify.system_prompt from '{plugin_name}' exceeds cap; ignored"
                    );
                } else {
                    replaced = Some(s.to_string());
                    appends.clear();
                }
            }
            if let Some(s) = obj.get("append_system_prompt").and_then(|v| v.as_str()) {
                if s.len() > SYSTEM_PROMPT_MODIFY_MAX_BYTES {
                    eprintln!(
                        "[plugins] pre_agent_start modify.append_system_prompt from '{plugin_name}' exceeds cap; ignored"
                    );
                } else if !s.is_empty() {
                    appends.push(format!("# Plugin: {plugin_name}\n{s}"));
                }
            }
        }
    }
    let mut out = String::new();
    if let Some(r) = replaced {
        out.push_str(&r);
    }
    if !appends.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&appends.join("\n\n"));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Run `pre_context` hooks over the in-flight message list before an LLM call.
/// Fail-open: timeout / invalid modify keeps the prior messages and logs.
pub(crate) async fn run_pre_context(st: &Arc<State>, messages: &mut Vec<Message>) {
    let configs = st.plugin_manager.get_hook_configs("pre_context");
    if configs.is_empty() {
        return;
    }
    let (workspace, session_id) = {
        let cfg = st.cfg.read().await;
        (
            cfg.workspace.display().to_string(),
            cfg.session_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
    };
    for (plugin_name, config) in &configs {
        let Ok(serialized) = serde_json::to_value(&*messages) else {
            eprintln!("[plugins] pre_context: failed to serialize messages; skipping");
            return;
        };
        let ser_bytes = serialized.to_string().len();
        if ser_bytes > PRE_CONTEXT_MAX_BYTES {
            eprintln!(
                "[plugins] pre_context: messages payload ({ser_bytes} bytes) exceeds cap; skipping hooks"
            );
            return;
        }
        let args = json!({ "messages": serialized });
        let ctx = plugins::build_context(
            "pre_context",
            "",
            &workspace,
            Some(&args),
            &session_id,
            config.pass_args,
        );
        let result = execute_plugin_hook_logged(st, "pre_context", plugin_name, config, &ctx).await;
        if let Some(obj) = result.modify.as_ref().and_then(|m| m.as_object()) {
            if let Some(msgs_v) = obj.get("messages") {
                let Ok(new_msgs) = serde_json::from_value::<Vec<Message>>(msgs_v.clone()) else {
                    eprintln!(
                        "[plugins] pre_context modify.messages from '{plugin_name}' failed schema validation; ignored"
                    );
                    continue;
                };
                if new_msgs.len() > PRE_CONTEXT_MAX_MESSAGES {
                    eprintln!(
                        "[plugins] pre_context modify.messages from '{plugin_name}' has {} msgs (max {PRE_CONTEXT_MAX_MESSAGES}); ignored",
                        new_msgs.len()
                    );
                    continue;
                }
                let new_bytes = serde_json::to_string(&new_msgs)
                    .map(|s| s.len())
                    .unwrap_or(usize::MAX);
                if new_bytes > PRE_CONTEXT_MAX_BYTES {
                    eprintln!(
                        "[plugins] pre_context modify.messages from '{plugin_name}' exceeds size cap; ignored"
                    );
                    continue;
                }
                *messages = new_msgs;
            }
        }
    }
}

/// Run every enabled plugin's pre-execution hook for `hook_name` against a tool
/// call, composing each hook's `modify` into `exec_args` and recording reasons
/// into `hook_notes`. Returns `Some(deny_message)` when a hook denies the call
/// (the caller emits the tool_result and skips the tool), or `None` to proceed.
/// Used for BOTH the tool-specific pre_* hook (pre_bash/pre_write/pre_read) and
/// the catch-all `pre_tool` that fires for every tool call — giving a plugin the
/// same per-call reach over `memory`/`todo_write`/`git_*`/`subagent`/… that a
/// core edit of the dispatch loop has.
///
/// `pub(crate)` so subagent tool dispatch can reuse the same pipeline (P0-F3).
pub(crate) async fn run_pre_hooks(
    st: &Arc<State>,
    cfg: &crate::config::Config,
    hook_name: &str,
    tool_name: &str,
    exec_args: &mut Value,
    hook_notes: &mut Vec<String>,
) -> Option<String> {
    let configs = st.plugin_manager.get_hook_configs(hook_name);
    if configs.is_empty() {
        return None;
    }
    let session_id = cfg
        .session_file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ws = cfg.workspace.display().to_string();
    for (plugin_name, config) in &configs {
        let ctx = plugins::build_context(
            hook_name,
            tool_name,
            &ws,
            Some(exec_args),
            &session_id,
            config.pass_args,
        );
        let result = execute_plugin_hook_logged(st, hook_name, plugin_name, config, &ctx).await;
        if !result.allow {
            return Some(format!(
                "tool call '{}' denied by plugin '{}' hook '{}': {}",
                tool_name, plugin_name, hook_name, result.reason
            ));
        }
        if let Some(ref modify) = result.modify {
            plugins::apply_modify(exec_args, modify);
        }
        if !result.reason.is_empty() {
            hook_notes.push(format!("{}/{}: {}", plugin_name, hook_name, result.reason));
        }
    }
    None
}

/// Run every enabled plugin's post-execution hook for `hook_name`, handing each
/// the tool's CURRENT result (so it can read it) and letting it MODIFY that
/// result. A post hook returns `modify: { "output": "…", "ok": false }` to
/// replace the result text / flip success — e.g. redact a secret, append
/// context, or reformat. Post hooks never block (the op already ran), so
/// `allow:false` is ignored (only its `reason` is surfaced). Used for BOTH the
/// tool-specific post_* hook and the catch-all `post_tool`.
///
/// `pub(crate)` so subagent tool dispatch can reuse the same pipeline (P0-F3).
pub(crate) async fn run_post_hooks(
    st: &Arc<State>,
    cfg: &crate::config::Config,
    hook_name: &str,
    tool_name: &str,
    exec_args: &Value,
    outcome: &mut tools::Outcome,
    hook_notes: &mut Vec<String>,
) {
    let configs = st.plugin_manager.get_hook_configs(hook_name);
    if configs.is_empty() {
        return;
    }
    let session_id = cfg
        .session_file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ws = cfg.workspace.display().to_string();
    for (plugin_name, config) in &configs {
        // Give the hook the current result so it can redact/append/transform it.
        let result_json = json!({
            "ok": outcome.ok,
            "output": outcome.output,
            "diff": outcome.diff,
        });
        let mut ctx = plugins::build_context(
            hook_name,
            tool_name,
            &ws,
            Some(exec_args),
            &session_id,
            config.pass_args,
        );
        if let Some(obj) = ctx.as_object_mut() {
            obj.insert("result".to_string(), result_json);
        }
        let result = execute_plugin_hook_logged(st, hook_name, plugin_name, config, &ctx).await;
        // Post hooks can't block; a deny is treated as an observed note only.
        if !result.reason.is_empty() {
            hook_notes.push(format!("{}/{}: {}", plugin_name, hook_name, result.reason));
        }
        // Apply an optional result mutation: `output` replaces the text, `ok`
        // flips success, `diff` (string) replaces / (null) clears the diff.
        if let Some(obj) = result.modify.as_ref().and_then(|m| m.as_object()) {
            if let Some(out) = obj.get("output").and_then(|v| v.as_str()) {
                outcome.output = out.to_string();
            }
            if let Some(ok) = obj.get("ok").and_then(|v| v.as_bool()) {
                outcome.ok = ok;
            }
            if let Some(diff) = obj.get("diff") {
                outcome.diff = diff.as_str().map(String::from);
            }
        }
    }
}

/// Token counts reported in the `metrics` event, which drive the footer's
/// context budget. The provider returns the *last request's* usage
/// (prompt/completion tokens ≈ the live context size) when the endpoint
/// includes it; when usage is absent these come back as zero, which would
/// pin the footer at "0%". Fall back to a char-based estimate of the current
/// conversation so the budget always reflects reality.
async fn reported_tokens(st: &Arc<State>, usage_in: u64, usage_out: u64) -> (u64, u64) {
    if usage_in > 0 || usage_out > 0 {
        return (usage_in, usage_out);
    }
    ({ *st.estimated_tokens.lock().await }, 0)
}

/// Whether a turn's originating prompt is itself a learning delegation
/// (`/reflect` or `/index`), which is exempt from auto-reflect — it IS the
/// reflection. NB: these prefixes must stay in sync with the TUI's
/// `sendDelegation` prompts for `/reflect` and `/index` (handlers.go).
fn is_learning_turn(prompt: &str) -> bool {
    let p = prompt.trim();
    p.starts_with("Reflect on the work done in this session")
        || p.starts_with("Run a full knowledge index of this repository")
        || p.starts_with("Run an incremental knowledge index of this repository")
}

/// Build the reflection text injected before completion. Includes recurring
/// patterns (if any) so the model can decide whether to write a skill — the
/// recurrence signal that makes the "solve the same shape 2+ times" rule
/// evaluable instead of a vibes check the model can't track across sessions.
fn build_reflect_text(recurring: &[(usize, String)]) -> String {
    let mut s = String::from(
        "[auto-reflect] Before you write your completion summary, reflect on this turn. \n\
         (1) If you learned a durable convention, architecture fact, decision, or \n\
         gotcha, persist it with the `memory` tool (action: append if a topic memory \n\
         exists, else save; use scope: \"global\" for cross-codebase facts like the \n\
         user's identity, tech-stack preferences, or harness conventions) — skip \n\
         transient task state. Use ONLY tool calls for reflection itself; do NOT write \n\
         reflection prose for the user. \n\
         (2) If you just performed a reusable workflow, consider writing a skill under \n\
         `.catalyst-code/skills/<name>/SKILL.md` (run `list_dir .catalyst-code/skills/` \n\
         first to extend rather than duplicate). \n\
         After reflecting (or if there is nothing durable to persist): you MUST write a \n\
         concise user-facing completion summary as assistant text — what was done, what \n\
         failed if anything, and notable files/changes — then call `finish`. A bare \n\
         `finish` with no summary text is rejected. Only skip the summary when you already \n\
         delivered a full answer earlier in this same turn; in that case save remaining \n\
         memories (tool calls only) and call `finish` with no new prose.",
    );
    if !recurring.is_empty() {
        s.push_str("\n\nRecurring patterns detected (performed 2+ times across sessions):");
        for (count, label) in recurring.iter().take(5) {
            s.push_str(&format!("\n- {label} ({count} times)"));
        }
        s.push_str("\nIf no existing skill covers a recurring pattern, write one.");
    }
    s
}

/// Extract file categories from a tool call's arguments, for shape analysis.
/// Only file-writing tools contribute a stable path signal (bash etc. do not).
fn extract_file_categories(tool: &str, args_json: &str) -> Vec<String> {
    let args: Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let paths: Vec<String> = match tool {
        "write_file" | "edit" | "patch" | "delete" | "mkdir" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        "rename" => {
            let mut v = Vec::new();
            if let Some(p) = args.get("from").and_then(|v| v.as_str()) {
                v.push(p.to_string());
            }
            if let Some(p) = args.get("to").and_then(|v| v.as_str()) {
                v.push(p.to_string());
            }
            v
        }
        "bulk_write" => args
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        "bulk_edit" => args
            .get("edits")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("path").and_then(|p| p.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    paths
        .into_iter()
        .map(|p| pattern_log::file_category(&p))
        .collect()
}

/// Auto-reflect gate + prompt builder. Returns `(reflect_text, recurrence_count)`
/// if auto-reflect should fire for this turn, else `None`. As a side effect,
/// records the turn's shape to the pattern log so recurrence is tracked.
/// Callers guard with `!reflected` (one reflection per turn).
///
/// Goal mode: skip while a goal is mid-flight (planning / plan_ready /
/// deploying / running / synthesizing / blocked). Planning only writes a plan —
/// there is nothing durable to reflect on until the goal finishes. Deploy is
/// core-driven after the planning turn ends, so reflecting here would also
/// delay `maybe_finish_goal_planning`. The synthesizing wrap-up is itself the
/// completion summary turn.
async fn maybe_reflect_prompt(
    st: &Arc<State>,
    prompt: &str,
    turn_tool_calls: u32,
    shape_tools: &[String],
    shape_files: &[String],
    cancelled: bool,
) -> Option<(String, usize)> {
    if cancelled {
        return None;
    }
    // Skip while goal mode is still working — planning is not "task complete".
    // Check before taking cfg so we never nest cfg + goal locks.
    {
        let g = st.goal.lock().await;
        if g.is_active() {
            return None;
        }
    }
    let cfg = st.cfg.read().await;
    let min_tools = cfg.auto_reflect_min_tool_calls;
    let auto_reflect = cfg.auto_reflect;
    let workspace = cfg.workspace.clone();
    drop(cfg);
    if turn_tool_calls < min_tools {
        return None;
    }
    if is_learning_turn(prompt) {
        return None;
    }

    // Coding-episode capture (fail-open). Independent of the auto_reflect
    // toggle so learning storage still accumulates when reflection is off.
    {
        let outcome = if shape_tools.iter().any(|t| t == "bash") {
            // Tests may have run; without parsing output treat as unverified.
            episodes::EpisodeOutcome::SuccessUnverified
        } else if shape_tools.iter().any(|t| {
            matches!(
                t.as_str(),
                "edit" | "write_file" | "patch" | "bulk_edit" | "bulk_write"
            )
        }) {
            episodes::EpisodeOutcome::SuccessUnverified
        } else {
            episodes::EpisodeOutcome::Unknown
        };
        let model = st.last_model.lock().await.clone();
        let tin = *st.tokens_in.lock().await;
        let tout = *st.tokens_out.lock().await;
        let _ = episodes::record_turn_episode(
            &workspace,
            prompt,
            shape_tools,
            shape_files, // categories; fingerprint treats them as path-like
            &[],
            outcome,
            0,
            model.as_deref(),
            Some(tin),
            Some(tout),
            None,
        );
        // Staleness: mark memories whose ref_files overlap changed categories.
        let _ = memory_staleness::invalidate_for_paths(&workspace, shape_files);
        // Learning proposals from episode digest (validated; no secrets).
        for prop in learning_proposals::proposals_from_episode_digest(
            prompt.lines().next().unwrap_or(prompt),
            shape_files,
            &[],
            &[],
        ) {
            let _ = learning_proposals::validate_and_apply(&workspace, &prop);
        }
    }

    // Pattern log + reflect nudge still gated on auto_reflect.
    if !auto_reflect {
        return None;
    }
    let sig = pattern_log::shape_signature(shape_tools, shape_files);
    let label = prompt.lines().next().unwrap_or(prompt);
    pattern_log::append_pattern(&workspace, &sig, label);
    let recurring = pattern_log::recurring_patterns(&workspace);
    let text = build_reflect_text(&recurring);
    Some((text, recurring.len()))
}

enum ParallelWaveResult {
    Done,
    Aborted,
}

/// Gate + concurrently execute a contiguous batch of readonly recon tools.
/// Falls back to emitting per-call errors when a gate denies a member; aborts
/// the turn if the user hits /abort during an approval prompt.
async fn run_parallel_readonly_wave(
    st: &Arc<State>,
    run: &RunContext,
    calls: &[message::ToolCall],
    tool_defs: &[Value],
    cancel: &CancellationToken,
    turn_tool_calls: &mut u32,
    shape_tools: &mut Vec<String>,
    shape_files: &mut Vec<String>,
) -> ParallelWaveResult {
    struct Prepared {
        id: String,
        name: String,
        args_str: String,
        exec_args: Value,
        hook_notes: Vec<String>,
        context: crate::tooling::ToolExecutionContext,
        _resource: runtime::ResourceLease,
        /// Pre-resolved outcome (deny / restore / duplicate) skips execution.
        early: Option<tools::Outcome>,
    }

    let mut prepared: Vec<Prepared> = Vec::with_capacity(calls.len());

    for tc in calls {
        if cancel.is_cancelled() {
            sync_session_file(st).await;
            emit_aborted_done();
            return ParallelWaveResult::Aborted;
        }
        let id = tc.id.clone();
        let name = tc.function.name.clone();
        let args_str = tc.function.arguments.clone();
        emit(
            &Event::new("tool_call")
                .with("id", json!(id))
                .with("name", json!(name))
                .with("args", json!(args_str)),
        );
        *turn_tool_calls = turn_tool_calls.saturating_add(1);
        shape_tools.push(name.clone());
        for cat in extract_file_categories(&name, &args_str) {
            shape_files.push(cat);
        }

        let args: Value = match serde_json::from_str(&args_str) {
            Ok(v) => v,
            Err(_) => {
                let msg = format!(
                    "tool call '{}' produced malformed JSON arguments (the argument string was not valid JSON).",
                    name
                );
                emit(
                    &Event::new("tool_result")
                        .with("id", json!(id))
                        .with("ok", json!(false))
                        .with("output", json!(msg)),
                );
                let tool_result = Message::tool(id.clone(), msg);
                let est = estimate_message_tokens(&tool_result);
                let mut conv = st.conversation.lock().await;
                conv.push(tool_result);
                if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                    session::append(p, conv.last().unwrap());
                }
                *st.estimated_tokens.lock().await += est;
                continue;
            }
        };

        let offered = tool_defs.iter().any(|d| {
            d.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                == Some(name.as_str())
        });
        if !offered {
            let msg = if tools::is_deferred_tool(&name) {
                format!(
                    "tool '{name}' is deferred and not enabled this session. \
                     Call load_tools with tools:[\"{name}\"] (or a group: git, web, bulk, browser, all), \
                     then retry the call."
                )
            } else {
                format!(
                    "tool '{name}' is not available on this agent (not in the current tool list)."
                )
            };
            emit(
                &Event::new("tool_result")
                    .with("id", json!(id))
                    .with("ok", json!(false))
                    .with("output", json!(msg)),
            );
            let tool_result = Message::tool(id.clone(), msg);
            let est = estimate_message_tokens(&tool_result);
            let mut conv = st.conversation.lock().await;
            conv.push(tool_result);
            if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                session::append(p, conv.last().unwrap());
            }
            *st.estimated_tokens.lock().await += est;
            continue;
        }

        let cfg = st.cfg.read().await.clone();
        let context = crate::tooling::ToolExecutionContext::new(
            run.clone(),
            id.clone(),
            cfg.clone(),
            st.runtime.clone(),
            None,
        );
        let Some(resource) = context.register_resource(ResourceKind::Task, format!("tool:{name}"))
        else {
            context.note_stale_result();
            return ParallelWaveResult::Aborted;
        };
        let kind = tools::classify(&name);
        let kind_str: &'static str = match kind {
            tools::ToolKind::ReadOnly => "readonly",
            tools::ToolKind::Destructive => "destructive",
        };
        let escalated = st.escalated_kinds.lock().await.contains(kind_str);

        let mut force_allow = false;
        let mut force_deny = false;
        for rule in &cfg.allow_rules {
            if tool_matches_rule(&name, &args, rule) {
                force_allow = true;
                break;
            }
        }
        if !force_allow {
            for rule in &cfg.deny_rules {
                if tool_matches_rule(&name, &args, rule) {
                    force_deny = true;
                    break;
                }
            }
        }
        if force_deny {
            let msg = format!("tool call '{}' denied by permission rule", name);
            emit(
                &Event::new("tool_result")
                    .with("id", json!(id))
                    .with("ok", json!(false))
                    .with("output", json!(msg)),
            );
            let tool_result = Message::tool(id.clone(), msg);
            let est = estimate_message_tokens(&tool_result);
            let mut conv = st.conversation.lock().await;
            conv.push(tool_result);
            if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                session::append(p, conv.last().unwrap());
            }
            *st.estimated_tokens.lock().await += est;
            continue;
        }

        let restricted = if matches!(cfg.approval, Approval::Never) {
            None
        } else {
            restricted_path_for_tool(&name, &args, &cfg.workspace)
        };
        let needs_approval = crate::tooling::approval::approval_required(
            &cfg.approval,
            kind,
            restricted.is_some(),
            force_allow,
            escalated,
            false,
        );
        if needs_approval {
            match request_approval(st, &id, &name, &args_str, kind_str, None, cancel).await {
                ApprovalResult::Granted => {}
                ApprovalResult::Denied => {
                    let msg = format!("tool call '{}' was denied by the user", name);
                    emit(
                        &Event::new("tool_result")
                            .with("id", json!(id))
                            .with("ok", json!(false))
                            .with("output", json!(msg)),
                    );
                    let tool_result = Message::tool(id.clone(), msg);
                    let est = estimate_message_tokens(&tool_result);
                    let mut conv = st.conversation.lock().await;
                    conv.push(tool_result);
                    if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                        session::append(p, conv.last().unwrap());
                    }
                    *st.estimated_tokens.lock().await += est;
                    continue;
                }
                ApprovalResult::Aborted => {
                    sync_session_file(st).await;
                    emit(&Event::new("aborted"));
                    emit(&Event::new("done"));
                    return ParallelWaveResult::Aborted;
                }
            }
        }

        let hook_name = match name.as_str() {
            "bash" => "pre_bash",
            "write_file" | "edit" => "pre_write",
            "read_file" | "grep" | "glob" => "pre_read",
            _ => "",
        };
        let any_pre = (!hook_name.is_empty() && st.plugin_manager.has_hook(hook_name))
            || st.plugin_manager.has_hook("pre_tool");
        let mut exec_args = if any_pre { args.clone() } else { args };
        let mut hook_notes: Vec<String> = Vec::new();
        let mut denied: Option<String> = None;
        if !hook_name.is_empty() {
            denied =
                run_pre_hooks(st, &cfg, hook_name, &name, &mut exec_args, &mut hook_notes).await;
        }
        if denied.is_none() {
            denied =
                run_pre_hooks(st, &cfg, "pre_tool", &name, &mut exec_args, &mut hook_notes).await;
        }
        if let Some(msg) = denied {
            emit(
                &Event::new("tool_result")
                    .with("id", json!(id))
                    .with("ok", json!(false))
                    .with("output", json!(msg)),
            );
            let tool_result = Message::tool(id.clone(), msg);
            let est = estimate_message_tokens(&tool_result);
            let mut conv = st.conversation.lock().await;
            conv.push(tool_result);
            if let Some(p) = st.cfg.read().await.session_file.as_ref() {
                session::append(p, conv.last().unwrap());
            }
            *st.estimated_tokens.lock().await += est;
            continue;
        }

        let early = if let Some(restored) = {
            let cache = st.tool_output_cache.lock().await;
            cache.get(&name, &args_str).map(|s| s.to_string())
        } {
            Some(tools::Outcome::ok(apply_restore_cap(&restored)))
        } else if let Some((prior_id, preview)) = {
            let conv = st.conversation.lock().await;
            find_duplicate_tool_result(&conv, &name, &args_str)
        } {
            Some(tools::Outcome::ok(format!(
                "[duplicate of tool_call_id {prior_id}; content unchanged]\n{preview}"
            )))
        } else {
            None
        };

        prepared.push(Prepared {
            id,
            name,
            args_str,
            exec_args,
            hook_notes,
            context,
            _resource: resource,
            early,
        });
    }

    if prepared.is_empty() {
        return ParallelWaveResult::Done;
    }

    let cfg = st.cfg.read().await.clone();
    let to_run: Vec<(usize, String, Value)> = prepared
        .iter()
        .enumerate()
        .filter(|(_, p)| p.early.is_none())
        .map(|(i, p)| (i, p.name.clone(), p.exec_args.clone()))
        .collect();

    for item in prepared.iter().filter(|item| item.early.is_none()) {
        item.context
            .persist_state(session::RunState::Started, Some(item.name.as_str()));
    }

    let mut outcomes: Vec<tools::Outcome> = prepared
        .iter()
        .map(|p| {
            p.early
                .clone()
                .unwrap_or_else(|| tools::Outcome::err("pending"))
        })
        .collect();

    if !to_run.is_empty() {
        let batch: Vec<(String, Value)> = to_run
            .iter()
            .map(|(_, n, a)| (n.clone(), a.clone()))
            .collect();
        let ran = tokio::select! {
            r = tools::execute_parallel_wave(&batch, &cfg) => r,
            _ = cancel.cancelled() => {
                sync_session_file(st).await;
                emit_aborted_done();
                return ParallelWaveResult::Aborted;
            }
        };
        for ((idx, _, _), outcome) in to_run.into_iter().zip(ran) {
            outcomes[idx] = outcome;
        }
    }

    if prepared.iter().any(|item| !item.context.is_active()) {
        for item in &prepared {
            if !item.context.is_active() {
                item.context.note_stale_result();
            }
        }
        return ParallelWaveResult::Aborted;
    }

    for (p, mut outcome) in prepared.into_iter().zip(outcomes) {
        let post_hook = match p.name.as_str() {
            "read_file" | "grep" | "glob" => "post_read",
            _ => "",
        };
        let mut hook_notes = p.hook_notes;
        if !post_hook.is_empty() {
            run_post_hooks(
                st,
                &cfg,
                post_hook,
                &p.name,
                &p.exec_args,
                &mut outcome,
                &mut hook_notes,
            )
            .await;
        }
        run_post_hooks(
            st,
            &cfg,
            "post_tool",
            &p.name,
            &p.exec_args,
            &mut outcome,
            &mut hook_notes,
        )
        .await;

        if p.early.is_none() {
            let persisted_tool_state = if cancel.is_cancelled() {
                session::RunState::Cancelled
            } else if outcome.ok {
                session::RunState::Completed
            } else {
                session::RunState::Failed
            };
            p.context
                .persist_state(persisted_tool_state, Some(p.name.as_str()));
        }

        if outcome.ok && p.name == "read_file" {
            if let Some(path) = p.exec_args.get("path").and_then(|v| v.as_str()) {
                let lower = path.to_ascii_lowercase();
                if lower.ends_with("skill.md")
                    || lower.contains("/.catalyst-code/skills/")
                    || lower.contains("\\.catalyst-code\\skills\\")
                {
                    st.skill_read_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        if !hook_notes.is_empty() {
            outcome.output.push_str("\n\nPlugin hooks:\n- ");
            outcome.output.push_str(&hook_notes.join("\n- "));
        }
        if let Some(note) = maybe_concurrency_note(st, &p.name, &p.exec_args, outcome.ok).await {
            outcome.output.push_str("\n\n");
            outcome.output.push_str(&note);
        }
        if outcome.ok {
            if tool_cache::invalidates_cache(&p.name) {
                st.tool_output_cache.lock().await.invalidate_all();
            } else if tool_cache::ToolOutputCache::is_restorable(&p.name)
                && !outcome.output.starts_with("[restored from digest cache]")
                && !outcome.output.starts_with("[duplicate of tool_call_id")
            {
                st.tool_output_cache
                    .lock()
                    .await
                    .store(&p.name, &p.args_str, &outcome.output);
            }
        }
        if outcome.ok
            && !outcome.output.starts_with("[restored from digest cache]")
            && !outcome.output.starts_with("[duplicate of tool_call_id")
        {
            outcome.output = apply_ingress_cap(&p.name, &p.args_str, outcome.output);
        }
        let status = crate::tooling::ToolResultStatus::from_legacy(outcome.ok, &outcome.output);
        let mut payload = json!({
            "tool_call_id": &p.id,
            "name": &p.name,
            "args_hash": audit::args_hash(&p.args_str),
            "status": status.as_str(),
            "output_len": outcome.output.len(),
            "duration_ms": p.context.elapsed_ms(),
            "parallel_wave": true,
        });
        if st.logger.is_verbose() {
            if let Some(obj) = payload.as_object_mut() {
                let (args_text, args_len, args_trunc) = crate::logging::truncate_for_log(
                    &p.args_str,
                    crate::logging::VERBOSE_PAYLOAD_CAP,
                );
                let (out_text, _out_len, out_trunc) = crate::logging::truncate_for_log(
                    &outcome.output,
                    crate::logging::VERBOSE_PAYLOAD_CAP,
                );
                obj.insert("args".into(), json!(args_text));
                obj.insert("args_len".into(), json!(args_len));
                obj.insert("args_truncated".into(), json!(args_trunc));
                obj.insert("output".into(), json!(out_text));
                obj.insert("output_truncated".into(), json!(out_trunc));
            }
        }
        st.logger.log("tool", payload);
        let mut ev = Event::new("tool_result")
            .with("id", json!(p.id))
            .with("ok", json!(outcome.ok))
            .with("output", json!(outcome.output));
        if let Some(d) = &outcome.diff {
            ev = ev.with("diff", json!(d));
        }
        emit(&ev);
        let tool_result = Message::tool(p.id.clone(), &outcome.output);
        let est = estimate_message_tokens(&tool_result);
        let mut conv = st.conversation.lock().await;
        conv.push(tool_result);
        if let Some(sess) = st.cfg.read().await.session_file.as_ref() {
            session::append(sess, conv.last().unwrap());
        }
        *st.estimated_tokens.lock().await += est;
    }

    ParallelWaveResult::Done
}

pub(crate) use crate::agent::turn_loop::run_turn;
pub(crate) enum ApprovalResult {
    Granted,
    Denied,
    Aborted,
}

/// Monotonic generator for globally-unique approval ids so parallel subagents
/// (which may each emit a tool call `call_1`) never collide on the shared
/// pending-approval map. The id embeds the originating tool-call id for tracing.
static APPROVAL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

use crate::tooling::approval::{
    approval_pattern_within_requested_scope, sanitized_approval_preview,
};
pub(crate) async fn request_approval(
    st: &Arc<State>,
    id: &str,
    name: &str,
    args: &str,
    kind_str: &'static str,
    owner_run_id: Option<&str>,
    cancel: &CancellationToken,
) -> ApprovalResult {
    let request_id = format!(
        "apv-{}-{}",
        APPROVAL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        id
    );
    let notify = Arc::new(Notify::new());
    let active = st.runtime.snapshot();
    let coordinator_bound = owner_run_id.is_none();
    let run_id = match owner_run_id
        .map(str::to_string)
        .or_else(|| active.run_id.as_ref().map(ToString::to_string))
    {
        Some(run_id) => run_id,
        None => return ApprovalResult::Aborted,
    };
    let resource = if coordinator_bound {
        st.runtime
            .register_active_run_resource(ResourceKind::Approval, format!("approval:{request_id}"))
    } else {
        let session = st.runtime.session_context();
        st.runtime.register_session_resource(
            &session,
            ResourceKind::Approval,
            format!("child_approval:{request_id}"),
        )
    };
    let Some(resource) = resource else {
        return ApprovalResult::Aborted;
    };
    let pending = Arc::new(PendingApproval {
        request_id: request_id.clone(),
        session_id: active.session_id.to_string(),
        run_id,
        coordinator_bound,
        cancellation: cancel.clone(),
        tool_call_id: id.to_string(),
        tool: name.to_string(),
        risk: kind_str,
        created_at_ms: now_ms(),
        args: serde_json::from_str(args).unwrap_or(json!({})),
        notify: notify.clone(),
        granted: Mutex::new(None),
        escalated: Mutex::new(false),
        allow_session: Mutex::new(false),
        allow_pattern: Mutex::new(None),
    });

    st.pending
        .lock()
        .await
        .insert(request_id.clone(), pending.clone());
    // Surface the resulting change to the human, not just the raw search/replace
    // blobs: compute the unified diff the call *would* produce (without writing)
    // and attach it to the approval_request event so the TUI can render it. Only
    // write/edit/patch produce a file diff; other destructive tools (bash, git)
    // carry no preview.
    let cfg = st.cfg.read().await.clone();
    let args_v: Value = serde_json::from_str(args).unwrap_or(json!({}));
    let diff: Option<String> = match name {
        "write_file" => {
            let path = args_v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args_v.get("content").and_then(|v| v.as_str()).unwrap_or("");
            tools::preview_diff_write(path, content, &cfg).ok()
        }
        "edit" => {
            let path = args_v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match args_v.get("edits").and_then(|v| v.as_array()) {
                Some(e) if !e.is_empty() => tools::preview_diff_edit(path, e, &cfg).ok(),
                _ => None,
            }
        }
        "patch" => {
            let path = args_v.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let patch = args_v.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() && !patch.is_empty() {
                tools::preview_diff_patch(path, patch, &cfg).ok()
            } else {
                None
            }
        }
        _ => None,
    };
    let args_preview = sanitized_approval_preview(&args_v);
    let evt = Event::new("approval_request")
        .with("request_id", json!(request_id))
        .with("session_id", json!(pending.session_id))
        .with("run_id", json!(pending.run_id))
        .with("tool_call_id", json!(pending.tool_call_id))
        .with("tool", json!(name))
        .with("risk", json!(pending.risk))
        .with("requested_permission", json!(kind_str))
        .with("created_at", json!(pending.created_at_ms))
        .with("args", json!(args_preview));
    let evt = if let Some(d) = &diff {
        evt.with("diff", json!(d))
    } else {
        evt
    };
    emit(&evt);

    // Wait for the approve command or abort.
    let granted = tokio::select! {
        _ = notify.notified() => pending.granted.lock().await.unwrap_or(false),
        _ = cancel.cancelled() => {
            st.pending.lock().await.remove(&request_id);
            st.logger.log("approval_wait", json!({
                "request_id": &request_id,
                "tool_call_id": id,
                "tool": name,
                "status": "cancelled",
                "duration_ms": now_ms().saturating_sub(pending.created_at_ms),
            }));
            return ApprovalResult::Aborted;
        }
        _ = resource.cancellation().cancelled() => {
            st.pending.lock().await.remove(&request_id);
            st.logger.log("approval_wait", json!({
                "request_id": &request_id,
                "tool_call_id": id,
                "tool": name,
                "status": "cancelled",
                "duration_ms": now_ms().saturating_sub(pending.created_at_ms),
            }));
            return ApprovalResult::Aborted;
        }
        _ = tokio::time::sleep(APPROVAL_TIMEOUT) => {
            st.pending.lock().await.remove(&request_id);
            emit(
                &Event::new("approval_expired")
                    .with("request_id", json!(request_id))
                    .with("tool_call_id", json!(id)),
            );
            st.logger.log("approval_wait", json!({
                "request_id": &request_id,
                "tool_call_id": id,
                "tool": name,
                "status": "timed_out",
                "duration_ms": now_ms().saturating_sub(pending.created_at_ms),
            }));
            return ApprovalResult::Denied;
        }
    };

    // "always" escalates: record this tool KIND so subsequent calls of the same
    // kind skip the gate, without un-gating other kinds or the whole session.
    if *pending.escalated.lock().await {
        st.escalated_kinds.lock().await.insert(kind_str);
        emit(&Event::new("approval_changed").with("mode", json!(format!("{}:always", kind_str))));
        // Persist the escalation so a restart doesn't un-gate this kind.
        if let Some(p) = st.cfg.read().await.session_file.as_ref() {
            let set: std::collections::HashSet<String> = st
                .escalated_kinds
                .lock()
                .await
                .iter()
                .map(|s| s.to_string())
                .collect();
            session::save_escalations(p, &set);
        }
    }
    // allow_session / allow_pattern → push a PermissionRule allow for this tool.
    let allow_session = *pending.allow_session.lock().await;
    let allow_pattern = pending.allow_pattern.lock().await.clone();
    if allow_session || allow_pattern.is_some() {
        let content = allow_pattern.clone().unwrap_or_else(|| "*".into());
        let rule = config::PermissionRule {
            tool_name: name.to_string(),
            rule_content: content.clone(),
            behavior: config::PermissionBehavior::Allow,
        };
        st.cfg.write().await.allow_rules.push(rule);
        emit(
            &Event::new("approval_changed")
                .with("mode", json!("allow_pattern"))
                .with("tool", json!(name))
                .with("pattern", json!(content)),
        );
    }
    // Audit sidecar (opt-in).
    {
        let cfg = st.cfg.read().await;
        let decision = if !granted {
            "no"
        } else if *pending.escalated.lock().await {
            "always"
        } else if allow_session {
            "allow_session"
        } else if allow_pattern.is_some() {
            "allow_pattern"
        } else {
            "yes"
        };
        audit::record(
            cfg.audit_log,
            cfg.session_file.as_deref(),
            &cfg.workspace,
            name,
            args,
            decision,
            "user",
            None,
            diff.as_deref(),
        );
    }
    st.pending.lock().await.remove(&request_id);
    st.logger.log(
        "approval_wait",
        json!({
            "request_id": &request_id,
            "tool_call_id": id,
            "tool": name,
            "status": if granted { "granted" } else { "denied" },
            "duration_ms": now_ms().saturating_sub(pending.created_at_ms),
        }),
    );
    if granted {
        ApprovalResult::Granted
    } else {
        ApprovalResult::Denied
    }
}

/// Outcome of a pending `ask` tool call.
pub(crate) enum AskResult {
    /// The user answered. Carries the validated questions array (for
    /// formatting) and the answers object (question id → answer).
    Answered { questions: Value, answers: Value },
    /// The user skipped the whole prompt (closed the flyout without answering).
    Skipped,
    /// The turn was aborted (/abort) while the ask was pending.
    Aborted,
}

/// Monotonic generator for globally-unique ask ids so concurrent asks (e.g.
/// from a parallel subagent that somehow gained the tool) never collide.
static ASK_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Validate the `ask` tool args and return the normalized questions array.
/// Returns Err(message) on invalid input (sent back to the model as a tool
/// error WITHOUT blocking — the model can retry with a well-formed call).
pub(crate) fn validate_ask_questions(args: &Value) -> Result<Value, String> {
    let questions = args
        .get("questions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "ask requires a non-empty 'questions' array".to_string())?;
    if questions.is_empty() {
        return Err("ask 'questions' must not be empty".to_string());
    }
    let mut seen_ids = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        let id = q
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("question {i}: missing 'id'"))?
            .trim()
            .to_string();
        if id.is_empty() {
            return Err(format!("question {i}: 'id' must not be empty"));
        }
        if !seen_ids.insert(id.clone()) {
            return Err(format!("question {i}: duplicate id '{id}'"));
        }
        let prompt = q
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("question {i}: missing 'prompt'"))?
            .to_string();
        let typ = q
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("question {i}: missing 'type'"))?;
        let typ = match typ {
            "select" | "text" => typ,
            other => {
                return Err(format!(
                    "question {i}: invalid type '{other}' (select|text)"
                ))
            }
        };
        let options = if typ == "select" {
            let opts = q
                .get("options")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("question {i}: type 'select' requires 'options'"))?;
            if opts.is_empty() {
                return Err(format!("question {i}: 'options' must not be empty"));
            }
            let strs: Vec<String> = opts
                .iter()
                .map(|o| o.as_str().unwrap_or("").to_string())
                .collect();
            Value::from(strs)
        } else {
            Value::Null
        };
        let required = q.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
        let allow_custom = q
            .get("allowCustom")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let placeholder = q
            .get("placeholder")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(json!({
            "id": id,
            "prompt": prompt,
            "type": typ,
            "options": options,
            "allowCustom": allow_custom,
            "required": required,
            "placeholder": placeholder,
        }));
    }
    Ok(Value::from(out))
}

/// Format the user's answers into the model-facing tool-result string.
/// Skipped (unanswered optional) questions are listed as "(skipped)".
pub(crate) fn format_ask_answers(questions: &Value, answers: &Value) -> String {
    let qs = questions.as_array();
    let ans = answers.as_object();
    let mut lines = vec!["User answered:".to_string()];
    if let Some(qs) = qs {
        for q in qs {
            let id = q.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let prompt = q.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            let val = ans.and_then(|m| m.get(id)).and_then(|v| v.as_str());
            let display = match val {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => "(skipped)".to_string(),
            };
            lines.push(format!("- {id} ({prompt}): {display}"));
        }
    }
    lines.join("\n")
}

/// Ask the user one or more questions via the TUI/web flyout; block until
/// answered, skipped, or aborted. Mirrors `request_approval` but carries
/// structured answers back instead of a granted/denied bool.
pub(crate) async fn request_ask(
    st: &Arc<State>,
    args: &Value,
    cancel: &CancellationToken,
) -> AskResult {
    let questions = match validate_ask_questions(args) {
        Ok(q) => q,
        Err(e) => {
            // Validation failed BEFORE we block — surface as an info event and
            // return Skipped so the model gets a tool result it can act on.
            // (The dispatch wraps this: it formats the result.)
            emit(
                &Event::new("error").with("message", json!(format!("ask validation failed: {e}"))),
            );
            return AskResult::Skipped;
        }
    };
    let request_id = format!(
        "ask-{}",
        ASK_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let notify = Arc::new(Notify::new());
    let Some(resource) = st
        .runtime
        .register_active_run_resource(ResourceKind::Ask, format!("ask:{request_id}"))
    else {
        return AskResult::Aborted;
    };
    let pending = Arc::new(PendingAsk {
        request_id: request_id.clone(),
        questions: questions.clone(),
        notify: notify.clone(),
        answers: Mutex::new(None),
    });
    st.pending_asks
        .lock()
        .await
        .insert(request_id.clone(), pending.clone());
    emit(
        &Event::new("ask_request")
            .with("request_id", json!(request_id))
            .with("questions", json!(questions)),
    );

    // Wait for the ask_reply command or abort.
    let answers = tokio::select! {
        _ = notify.notified() => pending.answers.lock().await.take(),
        _ = cancel.cancelled() => {
            st.pending_asks.lock().await.remove(&request_id);
            return AskResult::Aborted;
        }
        _ = resource.cancellation().cancelled() => {
            st.pending_asks.lock().await.remove(&request_id);
            return AskResult::Aborted;
        }
    };
    st.pending_asks.lock().await.remove(&request_id);
    match answers {
        Some(v) if v.is_object() => AskResult::Answered {
            questions: pending.questions.clone(),
            answers: v,
        },
        // Some(Null) or Some(non-object) = user skipped the prompt.
        _ => AskResult::Skipped,
    }
}

/// Outcome of a pending sudo approval.
pub(crate) enum SudoResult {
    /// The user approved and supplied a password to feed `sudo -S` on stdin.
    Approved { password: String },
    /// The user declined (Esc) — the command was NOT run.
    Declined,
    /// The turn was aborted (/abort) while the sudo prompt was pending.
    Aborted,
}

/// Monotonic generator for globally-unique sudo-request ids.
/// Format a user-initiated bash result the way PI's `bashExecutionToText` does,
/// so the next model turn sees a clear "Ran `cmd`" + fenced output block.
fn format_user_bash_context(command: &str, output: &str, ok: bool) -> String {
    let mut text = format!("Ran `{command}`\n```\n{output}");
    if !output.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("```");
    if !ok {
        text.push_str("\n(exit non-zero)");
    }
    text
}

/// Append deferred `!cmd` context messages now that no turn is in flight.
async fn flush_pending_bash(st: &Arc<State>) {
    let pending = {
        let mut q = st.pending_bash.lock().await;
        std::mem::take(&mut *q)
    };
    if pending.is_empty() {
        return;
    }
    let cfg = st.cfg.read().await;
    let session_file = cfg.session_file.clone();
    drop(cfg);
    let mut conv = st.conversation.lock().await;
    for msg in pending {
        let est = estimate_message_tokens(&msg);
        conv.push(msg);
        if let Some(p) = session_file.as_ref() {
            session::append(p, conv.last().unwrap());
        }
        *st.estimated_tokens.lock().await += est;
    }
}

/// Run a user-initiated bang command (`!cmd` / `!!cmd`).
/// Emits `bash_execution` for the UI; optionally injects into conversation
/// context (deferred while a turn is running).
async fn handle_user_bash(st: &Arc<State>, command: String, exclude_from_context: bool) {
    let command = command.trim().to_string();
    if command.is_empty() {
        emit(&Event::new("error").with("message", json!("empty bash command")));
        return;
    }

    let cfg = st.cfg.read().await.clone();
    // Independent of any in-flight turn — bang commands are user-owned.
    let cancel = CancellationToken::new();

    let outcome = if tools::command_uses_sudo(&command) {
        let needs_prompt = if matches!(cfg.approval, Approval::Never) {
            let sudo_preflight = tools::sudo_preflight(&cfg).await;
            tools::sudo_should_prompt(&cfg.approval, sudo_preflight)
        } else {
            true
        };
        if needs_prompt {
            match request_sudo(st, &command, &cancel).await {
                SudoResult::Approved { password } => {
                    tools::execute_bash(&command, &cfg, None, tools::SudoAuth::Password(password))
                        .await
                }
                SudoResult::Declined => {
                    emit(
                        &Event::new("bash_execution")
                            .with("command", json!(command))
                            .with("output", json!("(sudo declined — command was not run)"))
                            .with("ok", json!(false))
                            .with("exclude_from_context", json!(true)),
                    );
                    return;
                }
                SudoResult::Aborted => {
                    emit(
                        &Event::new("bash_execution")
                            .with("command", json!(command))
                            .with("output", json!("(aborted)"))
                            .with("ok", json!(false))
                            .with("exclude_from_context", json!(true)),
                    );
                    return;
                }
            }
        } else {
            tools::execute_bash(&command, &cfg, None, tools::SudoAuth::NonInteractive).await
        }
    } else {
        tools::execute_bash(&command, &cfg, None, tools::SudoAuth::None).await
    };

    emit(
        &Event::new("bash_execution")
            .with("command", json!(command))
            .with("output", json!(outcome.output))
            .with("ok", json!(outcome.ok))
            .with("exclude_from_context", json!(exclude_from_context)),
    );

    if exclude_from_context {
        return;
    }

    let msg = Message::user(format_user_bash_context(
        &command,
        &outcome.output,
        outcome.ok,
    ));

    // Defer while a turn is running so we don't break tool_use/tool_result order.
    let busy = st.current.lock().await.is_some();
    if busy {
        st.pending_bash.lock().await.push(msg);
        return;
    }

    let est = estimate_message_tokens(&msg);
    {
        let mut conv = st.conversation.lock().await;
        conv.push(msg);
        if let Some(p) = cfg.session_file.as_ref() {
            session::append(p, conv.last().unwrap());
        }
    }
    *st.estimated_tokens.lock().await += est;
}

static SUDO_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ask the user to approve a bash command that invokes `sudo`. Blocks until the
/// user approves (with a password) or declines (Esc). Mirrors `request_ask`
/// but carries a single password string back instead of structured answers.
/// The password is used once to feed `sudo -S` on stdin and is never persisted.
pub(crate) async fn request_sudo(
    st: &Arc<State>,
    command: &str,
    cancel: &CancellationToken,
) -> SudoResult {
    let request_id = format!(
        "sudo-{}",
        SUDO_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let notify = Arc::new(Notify::new());
    let session = st.runtime.session_context();
    let Some(resource) = st.runtime.register_session_resource(
        &session,
        ResourceKind::Sudo,
        format!("sudo:{request_id}"),
    ) else {
        return SudoResult::Aborted;
    };
    let pending = Arc::new(PendingSudo {
        request_id: request_id.clone(),
        command: command.to_string(),
        notify: notify.clone(),
        result: Mutex::new(None),
    });
    st.pending_sudos
        .lock()
        .await
        .insert(request_id.clone(), pending.clone());
    emit(
        &Event::new("sudo_request")
            .with("request_id", json!(request_id))
            .with("command", json!(command)),
    );

    // Wait for the sudo_reply command or abort.
    let result = tokio::select! {
        _ = notify.notified() => pending.result.lock().await.take(),
        _ = cancel.cancelled() => {
            st.pending_sudos.lock().await.remove(&request_id);
            return SudoResult::Aborted;
        }
        _ = resource.cancellation().cancelled() => {
            st.pending_sudos.lock().await.remove(&request_id);
            return SudoResult::Aborted;
        }
    };
    st.pending_sudos.lock().await.remove(&request_id);
    match result {
        Some(Some(pw)) => SudoResult::Approved { password: pw },
        // Some(None) = declined (Esc). None should not happen (notify implies resolved).
        _ => SudoResult::Declined,
    }
}

/// If the conversation ends with an assistant message carrying an unanswered
/// `ask` tool call (a prior core restart happened while blocked mid-`ask`),
/// return that call's id and its arguments as a `Value` ready for `request_ask`.
/// Only the LAST unanswered `ask` is returned (the one the prior core was blocked
/// on). A call whose arguments fail validation is skipped — the
/// orphan-sanitizer will later resolve it with a synthetic result. Returns None
/// on the common case of no trailing unanswered ask.
fn find_trailing_unanswered_ask(conv: &[Message]) -> Option<(String, Value)> {
    let last = conv.last()?;
    let calls = last.tool_calls()?;
    if calls.is_empty() {
        return None;
    }
    // Tool-call ids that already have a matching `role:"tool"` result.
    let answered: HashSet<&str> = conv
        .iter()
        .filter_map(|m| if m.is_tool() { m.tool_call_id() } else { None })
        .collect();
    // Prefer the last unanswered `ask` (most likely the one that was blocking).
    for tc in calls.iter().rev() {
        if tc.function.name != "ask" || answered.contains(tc.id.as_str()) {
            continue;
        }
        let args: Value =
            serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| json!({}));
        if validate_ask_questions(&args).is_ok() {
            return Some((tc.id.clone(), args));
        }
    }
    None
}

/// Re-present an `ask` question that a prior core restart left unanswered, so
/// the question is not lost and the session is not wedged. Called at the top of
/// each turn; idempotent (a no-op once the ask has a result). The assistant
/// `ask` tool_call is already persisted; this appends the matching tool result
/// (the user's answers, or a "skipped" note) so the orphan-sanitizer never has
/// to insert a synthetic EMPTY result that would silently drop the question.
async fn resume_pending_ask(st: &Arc<State>, cancel: &CancellationToken) {
    let (call_id, args) = {
        let conv = st.conversation.lock().await;
        match find_trailing_unanswered_ask(&conv[..]) {
            Some(x) => x,
            None => return,
        }
    };
    // Re-present (fresh ask request_id) and block for the reply. An abort (the
    // turn was cancelled while re-presenting) ends the turn like the in-turn
    // ask abort; a skip resolves the orphan with a best-judgment note.
    let content = match request_ask(st, &args, cancel).await {
        AskResult::Answered { questions, answers } => {
            format_ask_answers(&questions, &answers)
        }
        AskResult::Skipped => {
            "The user skipped the questions. Proceed with your best judgment and note any assumptions."
                .to_string()
        }
        AskResult::Aborted => {
            sync_session_file(st).await;
            emit(&Event::new("aborted"));
            emit(&Event::new("done"));
            return;
        }
    };
    let tool_result = Message::tool(&call_id, &content);
    let est = estimate_message_tokens(&tool_result);
    {
        let mut conv = st.conversation.lock().await;
        conv.push(tool_result);
        if let Some(p) = st.cfg.read().await.session_file.as_ref() {
            session::append(p, conv.last().unwrap());
        }
    }
    *st.estimated_tokens.lock().await += est;
    emit(
        &Event::new("tool_result")
            .with("id", json!(call_id))
            .with("ok", json!(true))
            .with("output", json!(content)),
    );
}

/// Soft-digest keep-window floor (messages). Mirrors compaction's MIN_TAIL so
/// soft digest never touches anything a subsequent compact would keep by count.
const SOFT_DIGEST_MIN_KEEP: usize = 6;
/// Soft-digest keep-window as a fraction of the context window (token budget).
const SOFT_DIGEST_KEEP_FRACTION: f32 = 0.20;
/// Minimum tool-result size (bytes) worth digesting. Small results (ok/err
/// one-liners, denial messages) stay full — they're cheap and the model may
/// need them verbatim.
pub(crate) use crate::agent::compaction::*;
pub(crate) use crate::agent::stuck_detector::*;
async fn handle_load_tools(st: &State, args: &Value, tool_defs: &mut Vec<Value>) -> tools::Outcome {
    let mut names: Vec<String> = Vec::new();
    if let Some(arr) = args.get("tools").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() {
                    names.push(t.to_string());
                }
            }
        }
    }
    if let Some(s) = args.get("tool").and_then(|v| v.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            names.push(t.to_string());
        }
    }
    // Expand group aliases.
    let mut expanded: Vec<String> = Vec::new();
    for n in &names {
        match n.as_str() {
            "all" => {
                expanded.extend(
                    tools::deferred_tool_names()
                        .iter()
                        .filter(|n| **n != "goal_write_plan")
                        .map(|s| (*s).to_string()),
                );
            }
            "git" => {
                for g in ["git_status", "git_diff", "git_log", "git_add", "git_commit"] {
                    expanded.push(g.into());
                }
            }
            "web" => {
                expanded.push("fetch".into());
                expanded.push("web_search".into());
            }
            "bulk" => {
                for g in ["bulk", "bulk_read", "bulk_write", "bulk_edit"] {
                    expanded.push(g.into());
                }
            }
            "runtime" => {
                expanded.push("eval".into());
                expanded.push("read".into());
            }
            "ide" => {
                for g in ["lsp", "snapshot_edit", "ast_edit"] {
                    expanded.push(g.into());
                }
            }
            "process" => expanded.push("process".into()),
            "debug" => expanded.push("debug".into()),
            "mcp" => expanded.push("mcp".into()),
            "browser" => {
                for g in crate::browser::MVP_TOOL_NAMES {
                    expanded.push((*g).to_string());
                }
            }
            other => expanded.push(other.to_string()),
        }
    }
    expanded.sort();
    expanded.dedup();
    if expanded.is_empty() {
        return tools::Outcome::ok(format!(
            "No tools requested. Deferred tools: {}. Groups: all, git, web, bulk, runtime, ide, debug, browser, process, mcp. Core tools are already available.",
            tools::deferred_tool_names().join(", ")
        ));
    }
    let all_defs = tools::definitions();
    let mut enabled = st.enabled_deferred_tools.lock().await;
    let mut added: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let existing: std::collections::HashSet<String> = tool_defs
        .iter()
        .filter_map(|d| {
            d.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();
    for name in expanded {
        if tools::is_core_tool(&name) {
            continue; // already available
        }
        // goal_write_plan is planning-phase only — never session-enable via load_tools.
        if name == "goal_write_plan" {
            unknown.push(format!(
                "{name} (only available during /goal planning — not loadable)"
            ));
            continue;
        }
        if !tools::is_deferred_tool(&name) {
            unknown.push(name);
            continue;
        }
        // P0-F4: plugins' `disable_tools` wins — mid-turn load_tools must not
        // resurrect a disabled name into the live toolset or the session set.
        if st.plugin_manager.disabled_tools().contains(&name) {
            unknown.push(format!("{name} (disabled by plugin — cannot load)"));
            continue;
        }
        enabled.insert(name.clone());
        if existing.contains(&name) {
            added.push(format!("{name} (already enabled)"));
            continue;
        }
        // P0-F4: when a plugin overrides this deferred builtin, inject the
        // plugin's declared schema — never the built-in one — so mid-turn
        // load matches the turn-start assembly.
        if st.plugin_manager.overridden_tool_names().contains(&name) {
            if let Some(def) = st.plugin_manager.tool_definitions().into_iter().find(|d| {
                d.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    == Some(name.as_str())
            }) {
                tool_defs.push(def);
                added.push(format!("{name} (plugin override)"));
                continue;
            }
            unknown.push(format!(
                "{name} (overridden by plugin but no plugin schema found)"
            ));
            continue;
        }
        if let Some(def) = all_defs.iter().find(|d| {
            d.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                == Some(name.as_str())
        }) {
            tool_defs.push(def.clone());
            added.push(name);
        } else {
            unknown.push(name);
        }
    }
    let mut out = String::new();
    if !added.is_empty() {
        out.push_str(&format!("Enabled: {}\n", added.join(", ")));
    }
    if !unknown.is_empty() {
        out.push_str(&format!("Unknown (skipped): {}\n", unknown.join(", ")));
    }
    if out.is_empty() {
        out.push_str("Nothing to enable.");
    }
    out.push_str("These tools are available on subsequent model rounds this session.");
    tools::Outcome::ok(out)
}

/// Emit the per-turn `metrics` event, finalizing memory-recall telemetry for
/// the turn so hit/miss + synonym-miss rates accumulate for Milestone 4.
async fn emit_turn_metrics(st: &State, metrics: &TurnMetrics) {
    let ws = st.cfg.read().await.workspace.clone();
    let recall = memory_recall::finalize_turn(&ws);
    let mut ev = Event::new("metrics")
        .with("ttft_ms", json!(metrics.ttft_ms))
        .with("elapsed_ms", json!(metrics.elapsed_ms))
        .with(
            "tokens_in",
            json!(metrics.tokens_in.saturating_add(metrics.tokens_out)),
        )
        .with("prompt_tokens", json!(metrics.tokens_in))
        .with("tokens_out", json!(metrics.tokens_out))
        .with("cached_tokens", json!(metrics.cached_tokens))
        .with("tps", json!(metrics.tps))
        .with("model", json!(metrics.model));
    if let Some(r) = recall {
        ev = ev.with(
            "memory_recall",
            json!({
                "relevant": r.relevant,
                "got": r.got,
                "missed_relevant": r.missed_relevant,
                "synonym_misses": r.synonym_misses,
                "synonym_hits": r.synonym_hits,
            }),
        );
    }
    emit(&ev);
    // Cost/cache update for clients (estimated USD left null unless a price
    // overlay is configured later — tokens + cache hits are always useful).
    let cache_hit_pct = if metrics.tokens_in > 0 {
        Some((metrics.cached_tokens as f64) * 100.0 / (metrics.tokens_in as f64))
    } else {
        None
    };
    emit(
        &Event::new("cost_update")
            .with("tokens_in", json!(metrics.tokens_in))
            .with("tokens_out", json!(metrics.tokens_out))
            .with("cached_tokens", json!(metrics.cached_tokens))
            .with("cache_hit_pct", json!(cache_hit_pct))
            .with("estimated_usd", json!(null))
            .with("model", json!(metrics.model)),
    );
}

/// Compact the conversation when it nears the context window.
/// ponytail: simple strategy — drop the oldest tool results (the bulk of tokens)
/// and keep system + recent turns. A summarization call would be better but adds
/// cost+complexity; this keeps the agent unblocked. Orphaned-tool-call sanitizer
/// in provider.rs backstops any tool_call/result mismatch this creates.
/// Rebuild the system prompt with fresh memory and persist it. Returns a
/// human-readable status. No-op (preserving the provider prefix cache) when
/// the prompt is unchanged. Shared by RefreshMemory and the save/forget
/// commands so a saved/removed memory is visible to the very next turn.
async fn refresh_memory_injection(state: &State) -> String {
    let (ws, auto_reflect) = {
        let c = state.cfg.read().await;
        (c.workspace.clone(), c.auto_reflect)
    };
    let mp = state.plugin_manager.memory_provider();
    let mem = match mp.as_ref() {
        Some(cfg) => plugins::memory_provider_inject(cfg, &ws.display().to_string(), ""),
        None => memory_injection(&ws, ""),
    };
    let new_system = build_main_system_prompt(&ws, &state.plugin_manager, auto_reflect);
    let mut conv = state.conversation.lock().await;
    if let Some(first) = conv.first() {
        let old_content = first.content_text().unwrap_or("");
        if old_content == new_system {
            return "memory unchanged; system prompt kept intact (preserving prefix cache)"
                .to_string();
        }
    }
    if let Some(first) = conv.first_mut() {
        if first.is_system() {
            *first = Message::system(new_system);
            *state.estimated_tokens.lock().await = estimate_messages_tokens(&conv);
            // System prompt changed; the real baseline's system portion is stale.
            state.invalidate_real_token_baseline().await;
            if let Some(p) = state.cfg.read().await.session_file.as_ref() {
                session::rewrite(p, &conv);
            }
        }
    }
    drop(conv);
    if mem.is_empty() {
        "memory refreshed: no memories found".to_string()
    } else {
        "memory refreshed: memories injected".to_string()
    }
}

/// Index where the kept verbatim tail begins, chosen by token budget rather
/// than a fixed message count. Walks backward from the end accumulating
/// `estimate_message_tokens` until the budget (25% of the context window,
/// floored at 6k tokens) is exceeded, always keeping at least `MIN_TAIL`
/// messages. A fixed count over-keeps a quiet stretch and under-keeps when a
/// huge tool result eats the whole window; a budget keeps the live context
/// that actually fits and lets the summary reclaim the rest.
/// Turn an image reference into a data URL. Accepts:
/// - an existing data URL (data:image/...;base64,...) → passthrough
/// - an absolute or workspace-relative file path → read + base64-encode
///
/// Returns a placeholder data URL on failure so the model gets a clear signal.
fn image_to_data_url(img: &str) -> String {
    if img.starts_with("data:") {
        return img.to_string();
    }
    // Resolve relative to cwd (the TUI's workspace). Refuse absolute paths that
    // escape — but vision input is a trust-boundary feature, so we allow any
    // readable path the host process can see.
    let p = std::path::Path::new(img);
    match std::fs::read(p) {
        Ok(bytes) => {
            // Sniff type from magic bytes; default to png.
            let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
                "image/png"
            } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
                "image/jpeg"
            } else if bytes.starts_with(b"GIF8") {
                "image/gif"
            } else if bytes.starts_with(b"<svg") {
                "image/svg+xml"
            } else if bytes.starts_with(b"RIFF") && bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
                "image/webp"
            } else if bytes.starts_with(&[0x42, 0x4d]) && bytes.len() > 1 {
                // BMP (BM) — lenient; a non-image file almost never starts with BM
                // followed by plausible header fields, so this is low false-positive risk.
                "image/bmp"
            } else {
                "application/octet-stream"
            };
            if mime == "application/octet-stream" {
                // Refuse to attach a non-image: /attach must only send actual
                // images to the provider. This is the vision-trust boundary —
                // a user typing `/attach ~/.ssh/id_rsa` (or any non-image) is
                // rejected here rather than base64-encoded and leaked to the API.
                return format!(
                    "data:text/plain;base64,{}",
                    base64_encode(
                        b"refused: not a recognized image format (png/jpeg/gif/webp/bmp/svg)"
                    )
                );
            }
            let b64 = base64_encode(&bytes);
            format!("data:{mime};base64,{b64}")
        }
        Err(e) => format!(
            "data:text/plain;base64,{}",
            base64_encode(format!("image read failed: {e}").as_bytes())
        ),
    }
}

/// Minimal base64 encoder (no extra crate).
fn base64_encode(input: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | (input[i + 2] as u32);
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(T[((n >> 6) & 0x3f) as usize] as char);
        out.push(T[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(T[((n >> 18) & 0x3f) as usize] as char);
        out.push(T[((n >> 12) & 0x3f) as usize] as char);
        out.push(T[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod skill_manifest_tests {
    use super::*;

    fn write_skill(dir: &std::path::Path, name: &str, desc: &str) {
        write_skill_raw(dir, name, &format!("name: {name}\ndescription: {desc}\n"))
    }

    /// Write a SKILL.md with arbitrary extra frontmatter lines (for deprecated, etc.).
    fn write_skill_raw(dir: &std::path::Path, name: &str, frontmatter_body: &str) {
        let p = dir
            .join(".catalyst-code/skills")
            .join(name)
            .join("SKILL.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("---\n{frontmatter_body}---\nbody\n")).unwrap();
    }

    fn fresh_workspace() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "catalyst-code-skill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_lists_opt_in_skills_excludes_pi_subagents() {
        let ws = fresh_workspace();
        write_skill(&ws, "foo", "Foo skill");
        write_skill(&ws, "nodesc", "");
        write_skill(&ws, "pi-subagents", "should be excluded (covered by stub)");
        let m = skill_manifest_injection(&ws);
        assert!(m.contains("foo"), "manifest should list foo: {m}");
        assert!(
            m.contains("Foo skill"),
            "manifest should include description: {m}"
        );
        // A skill with no description renders without a colon-suffix.
        assert!(m.contains("- nodesc"), "manifest should list nodesc: {m}");
        assert!(
            !m.lines().any(|l| l.starts_with("- pi-subagents")),
            "pi-subagents must be excluded from the manifest: {m}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn manifest_excludes_deprecated_skills_and_uses_dirname() {
        let ws = fresh_workspace();
        write_skill(&ws, "active", "An active skill");
        write_skill_raw(
            &ws,
            "old",
            "name: old\ndescription: A deprecated skill\ndeprecated: true\n",
        );
        // frontmatter `name` deliberately != dir name; the manifest must use the
        // DIR name (the resolvable path component), not the frontmatter name.
        write_skill_raw(
            &ws,
            "realdir",
            "name: pretty-display-name\ndescription: uses a fancy name\n",
        );
        let m = skill_manifest_injection(&ws);
        assert!(m.contains("active"), "active skill should appear: {m}");
        assert!(
            !m.lines().any(|l| l.starts_with("- old")),
            "deprecated skill must be excluded: {m}"
        );
        assert!(
            m.contains("- realdir:"),
            "manifest must use the dir name, not the frontmatter name: {m}"
        );
        assert!(
            !m.contains("pretty-display-name"),
            "frontmatter name should not leak into the manifest path: {m}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn manifest_caps_entries_and_truncates_long_descriptions() {
        let ws = fresh_workspace();
        for i in 0..(SKILL_MANIFEST_MAX + 3) {
            write_skill(&ws, &format!("skill-{i:02}"), &format!("desc {i}"));
        }
        let long = "x".repeat(SKILL_DESC_MAX_CHARS + 40);
        write_skill(&ws, "zzzz-long", &long);
        let m = skill_manifest_injection(&ws);
        let listed = m
            .lines()
            .filter(|l| l.starts_with("- ") && !l.starts_with("- …"))
            .count();
        assert_eq!(
            listed, SKILL_MANIFEST_MAX,
            "manifest should list at most {SKILL_MANIFEST_MAX} skills: {m}"
        );
        // Omitted count may include user-scope skills from ~/.catalyst-code in
        // addition to the extras we wrote — just require the overflow marker.
        assert!(
            m.contains("- …and ") && m.contains(" more"),
            "overflow should mention omitted count: {m}"
        );
        // Dedicated workspace: a single long description must truncate.
        let ws2 = fresh_workspace();
        write_skill(&ws2, "only", &long);
        let m2 = skill_manifest_injection(&ws2);
        assert!(
            m2.contains(&format!("- only: {}…", "x".repeat(SKILL_DESC_MAX_CHARS))),
            "long descriptions must be truncated: {m2}"
        );
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&ws2);
    }
}

#[cfg(test)]
mod system_prompt_slim_tests {
    use super::*;

    #[test]
    fn standing_prompt_stays_lean_and_defers_plugin_manual() {
        let ws = std::env::temp_dir().join(format!(
            "catalyst-code-prompt-slim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let prompt = build_system_prompt(&ws, true, None);
        // Must keep the short plugins pointer + subagent stub.
        assert!(prompt.contains("## Plugins"));
        assert!(prompt.contains("plugin-authoring"));
        assert!(prompt.contains(SUBAGENT_ORCHESTRATOR_STUB));
        // Must NOT embed the old always-on authoring manual.
        assert!(
            !prompt.contains("Declaring an OAuth provider"),
            "full plugin OAuth manual must not be in the standing prompt"
        );
        assert!(
            !prompt.contains("### Hook contract"),
            "full hook contract must not be in the standing prompt"
        );
        assert!(
            !prompt.contains("# Skill: pi-subagents"),
            "full pi-subagents skill body must not be injected"
        );
        // Fixed prefix pieces (base + plugin pointer + stub) stay small even
        // when the developer's real global memories inflate the full prompt.
        let fixed = SYSTEM_PROMPT_BASE.len()
            + PLUGIN_DOCS.len()
            + SUBAGENT_ORCHESTRATOR_STUB.len()
            + PROVIDER_GUIDE.len()
            + DEFERRED_TOOLS_GUIDE.len()
            + STRUCTURED_EDIT_GUIDE.len();
        assert!(
            prompt.contains("## Deferred tools"),
            "deferred tools guide must be in the standing prompt"
        );
        assert!(
            prompt.contains("`git`"),
            "deferred git group must be named in the standing prompt"
        );
        assert!(
            prompt.contains("`ide`"),
            "deferred ide group must be named in the standing prompt"
        );
        assert!(
            prompt.contains("prefer it over `edit` for structural code changes"),
            "structured edit preference must be in the standing prompt"
        );
        // 6000: provider guide + deferred tools + structured edit preference
        // stay lean while remaining self-sufficient for common tasks.
        assert!(
            fixed < 6_000,
            "fixed standing-prompt pieces unexpectedly large ({fixed} chars)"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    fn asst_tool_call(id: &str, name: &str, args: &str) -> Message {
        Message::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            typ: "function".into(),
            function: message::FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }])
    }
    fn tool_result(id: &str, content: &str) -> Message {
        Message::tool(id, content)
    }
    fn asst_text(t: &str) -> Message {
        Message::assistant(t)
    }

    fn big_content(n: usize) -> String {
        "x\n".repeat(n)
    }

    /// system + a stale large read result + padding + a recent large read result.
    fn fixture() -> Vec<Message> {
        let mut m = vec![Message::system("sys")];
        m.push(asst_tool_call(
            "call_1",
            "read_file",
            "{\"path\":\"src/big.rs\"}",
        ));
        m.push(tool_result("call_1", &big_content(150))); // 300 bytes, 150 lines
                                                          // padding (assistant texts) to push call_1's result out of the keep window
        for i in 1..=9 {
            m.push(asst_text(&format!("pad{i}")));
        }
        // a recent large read result that must stay verbatim (inside keep window)
        m.push(asst_tool_call(
            "call_2",
            "read_file",
            "{\"path\":\"src/recent.rs\"}",
        ));
        m.push(tool_result("call_2", &big_content(140))); // 280 bytes
        m.push(asst_text("final"));
        m
    }

    #[test]
    fn digests_old_large_tool_result_keeps_recent() {
        let mut m = fixture();
        let n = digest_stale_tool_results(&mut m, 10);
        assert_eq!(n, 1, "only the stale large result should be digested");
        // stale result (index 2) is now a digest
        let d = m[2].content_text().unwrap();
        assert!(d.starts_with("[digested:"), "{}", d);
        assert!(d.contains("read_file"), "{}", d);
        assert!(d.contains("src/big.rs"), "{}", d);
        assert!(d.contains("lines"), "should report line count: {}", d);
        assert!(
            d.contains("re-run identical call to restore (cached"),
            "{}",
            d
        );
        // tool_call_id preserved so the assistant/tool pairing stays valid
        assert_eq!(m[2].tool_call_id(), Some("call_1"));
        // recent large result (inside the keep tail) is untouched
        let r = m[m.len() - 2].content_text().unwrap();
        assert_eq!(r.len(), 280, "recent result kept full: {}", r);
        assert!(!r.starts_with("[digested:"));
        assert_eq!(m[m.len() - 2].tool_call_id(), Some("call_2"));
    }

    #[test]
    fn digest_is_idempotent() {
        let mut m = fixture();
        let n1 = digest_stale_tool_results(&mut m, 10);
        assert_eq!(n1, 1);
        let after = m[2].content_text().unwrap().to_string();
        let n2 = digest_stale_tool_results(&mut m, 10);
        assert_eq!(n2, 0, "second pass must find nothing to digest");
        assert_eq!(m[2].content_text(), Some(after.as_str()));
    }

    #[test]
    fn digest_skips_small_results() {
        let mut m = vec![
            Message::system("sys"),
            asst_tool_call("c1", "edit", "{\"path\":\"a.rs\"}"),
            tool_result("c1", "applied 1 edit(s)"), // 17 bytes — under MIN_BYTES
        ];
        // pad to push it out of the keep window
        for i in 0..12 {
            m.push(asst_text(&format!("p{i}")));
        }
        let n = digest_stale_tool_results(&mut m, 10);
        assert_eq!(n, 0, "small result must not be digested");
        assert_eq!(m[2].content_text(), Some("applied 1 edit(s)"));
    }

    #[test]
    fn digest_noop_when_under_keep() {
        let mut m = vec![
            Message::system("sys"),
            asst_tool_call("c1", "read_file", "{\"path\":\"a.rs\"}"),
            tool_result("c1", &big_content(200)),
        ];
        // only 3 messages, keep=10 → nothing eligible
        assert_eq!(digest_stale_tool_results(&mut m, 10), 0);
        assert_eq!(m[2].content_text().unwrap().len(), 400);
    }

    #[test]
    fn digest_bash_label_says_rerun_if_needed() {
        let mut m = vec![
            Message::system("sys"),
            asst_tool_call("c1", "bash", "{\"command\":\"cargo build\"}"),
            tool_result("c1", &big_content(150)),
        ];
        for i in 0..12 {
            m.push(asst_text(&format!("p{i}")));
        }
        digest_stale_tool_results(&mut m, 10);
        let d = m[2].content_text().unwrap();
        assert!(d.contains("bash"), "{}", d);
        assert!(d.contains("cargo build"), "{}", d);
        assert!(
            d.contains("re-run if needed"),
            "bash digest should not promise side-effect-free recovery: {}",
            d
        );
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn sys() -> Message {
        Message::system("sys")
    }
    fn user(t: &str) -> Message {
        Message::user(t)
    }

    #[test]
    fn tail_start_keeps_min_tail_on_tiny_budget() {
        // 20 messages each big enough that the floor budget (6k tokens) is
        // exceeded, so the tail trims the middle but always keeps MIN_TAIL (6).
        let mut m = vec![sys()];
        for i in 0..20 {
            m.push(user(&format!("msg {i}: {}", "x".repeat(2000))));
        }
        // small context window -> budget hits the 6k floor
        let s = token_budget_tail_start(&m, 1000);
        assert!(s >= 1, "must not fold system into the tail: {s}");
        assert!(s <= m.len() - 6, "must keep at least the min tail: {s}");
    }

    #[test]
    fn tail_start_keeps_everything_when_small() {
        let m = vec![sys(), user("a"), user("b"), user("c")];
        assert_eq!(token_budget_tail_start(&m, 200_000), 0);
    }

    #[test]
    fn tail_start_shrinks_for_huge_recent_result() {
        // normal turns then one giant tool result: with a small window the
        // budget keeps only the giant result + the min tail, trimming the rest.
        let mut m = vec![sys()];
        for i in 0..40 {
            m.push(user(&format!("turn {i}")));
        }
        m.push(Message::tool("x", "x".repeat(50_000)));
        let s = token_budget_tail_start(&m, 1000); // floor budget 6k
        assert!(
            s >= m.len() - 7 && s <= m.len() - 6,
            "kept the giant result + min tail: {s}"
        );
    }

    #[test]
    fn digest_to_budget_reclaims_huge_tool_call_arguments() {
        // H3: an assistant tool_call whose `arguments` JSON embeds a huge
        // payload (a write_file of a large file) is a NON-tool message, so the
        // tool-result digest loop never touches it. digest_to_budget must also
        // replace that payload field with a one-line digest so a single such
        // message in the kept tail can't keep the request oversized → 400.
        let huge = "x".repeat(40_000);
        let args = format!(
            "{{\"path\":\"big.rs\",\"content\":{}}}",
            serde_json::to_string(&huge).unwrap()
        );
        let mut m = vec![
            sys(),
            asst_tool_call("c1", "write_file", &args),
            tool_result("c1", "ok"),
        ];
        // budget well under the args size → must digest the call args
        let n = digest_to_budget(&mut m, 1000);
        assert!(n >= 1, "should digest the oversized call args: {n}");
        let call_args = match &m[1] {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.as_ref().unwrap()[0].function.arguments.clone()
            }
            _ => panic!("expected assistant tool_call"),
        };
        assert!(
            !call_args.contains(&huge),
            "huge content should be replaced"
        );
        assert!(
            call_args.contains("digested"),
            "should carry the digest marker: {call_args}"
        );
        assert!(
            call_args.contains("\"path\":\"big.rs\""),
            "path field should be preserved: {call_args}"
        );
    }

    fn asst_tool_call(id: &str, name: &str, args: &str) -> Message {
        Message::assistant_tool_calls(vec![ToolCall {
            id: id.into(),
            typ: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }])
    }
    fn tool_result(id: &str, content: &str) -> Message {
        Message::tool(id, content)
    }

    #[test]
    fn digest_to_budget_collapses_oversized_results() {
        // Few messages but huge tokens — the case that used to no-op compaction.
        // ~250k chars ≈ 62k tokens each; two of them exceed a 100k-token budget.
        let huge = "x\n".repeat(125_000);
        let mut m = vec![
            sys(),
            asst_tool_call("c1", "read_file", "{\"path\":\"old.rs\"}"),
            tool_result("c1", &huge),
            asst_tool_call("c2", "read_file", "{\"path\":\"recent.rs\"}"),
            tool_result("c2", &huge),
            user("go"),
        ];
        let before = estimate_messages_tokens(&m);
        assert!(before > 100_000, "fixture should be large: {before}");
        // Digest oldest-first until under budget; the recent result is processed
        // last and stays verbatim once the older one already fit the budget.
        let n = digest_to_budget(&mut m, 100_000);
        assert_eq!(n, 1, "only the older result needs digesting: {n}");
        let d = m[2].content_text().unwrap();
        assert!(d.starts_with("[digested:"), "{d}");
        assert!(d.contains("old.rs"), "{d}");
        assert_eq!(m[2].tool_call_id(), Some("c1"));
        assert!(
            !m[4].content_text().unwrap().starts_with("[digested:"),
            "recent result kept verbatim"
        );
        let after = estimate_messages_tokens(&m);
        // Digesting one of two equal huge results reclaims ~half (~62k tokens for
        // this fixture), which is enough to land under the 100k budget.
        assert!(
            after < 100_000 && after < before - 50_000,
            "must reclaim a huge result under budget: {after} vs {before}"
        );
    }

    #[test]
    fn digest_to_budget_noop_under_budget() {
        let mut m = vec![
            sys(),
            asst_tool_call("c1", "read_file", "{\"path\":\"a.rs\"}"),
            tool_result("c1", &"x\n".repeat(125_000)),
        ];
        // ~62k tokens, budget 100k → already fits, nothing digested.
        assert_eq!(digest_to_budget(&mut m, 100_000), 0);
        assert!(!m[2].content_text().unwrap().starts_with("[digested:"));
    }

    #[test]
    fn compact_conversation_reclaims_few_huge_messages() {
        // Regression: a conversation with only 6 messages but ~125k tokens used
        // to bail out of compaction entirely (the old `len <= 12` guard left
        // before == after), so the next request hit a context-window 400.
        let huge = "x\n".repeat(125_000);
        let mut m = vec![
            sys(),
            asst_tool_call("c1", "read_file", "{\"path\":\"a.rs\"}"),
            tool_result("c1", &huge),
            asst_tool_call("c2", "read_file", "{\"path\":\"b.rs\"}"),
            tool_result("c2", &huge),
            user("continue"),
        ];
        let before = estimate_messages_tokens(&m);
        compact_conversation(&mut m, 200_000);
        let after = estimate_messages_tokens(&m);
        assert!(
            after < before,
            "compaction must reduce tokens (was a no-op): {before} -> {after}"
        );
        assert!(
            after < 70_000,
            "should be well under 35% of the window: {after}"
        );
        // Both tool messages survive (pairing intact); the older one is digested.
        assert_eq!(m.iter().filter(|x| x.is_tool()).count(), 2);
    }

    #[test]
    fn apply_ingress_cap_truncates_oversized() {
        let huge = "x".repeat(60_000);
        let out = apply_ingress_cap("read_file", r#"{"path":"a.txt"}"#, huge);
        assert!(out.len() <= INGRESS_MAX_BYTES + 256, "len={}", out.len());
        assert!(
            out.contains("truncated") || out.len() <= INGRESS_MAX_BYTES,
            "{out}"
        );
        assert!(
            !out.starts_with("[digested:"),
            "prefer truncate over digest on ingress"
        );
    }

    #[test]
    fn apply_ingress_cap_passthrough_small() {
        let out = apply_ingress_cap("grep", r#"{"pattern":"x"}"#, "a:1:x".into());
        assert_eq!(out, "a:1:x");
    }

    #[test]
    fn soft_digest_reclaims_stale_call_args() {
        let huge = "x".repeat(40_000);
        let args = format!(
            "{{\"path\":\"big.rs\",\"content\":{}}}",
            serde_json::to_string(&huge).unwrap()
        );
        let mut m = vec![
            Message::system("sys"),
            asst_tool_call("c1", "write_file", &args),
            tool_result("c1", "ok"),
        ];
        for i in 0..12 {
            m.push(Message::assistant(format!("pad{i}")));
        }
        // Small window so the soft target is under the write_file args size
        // and digest_to_budget must reclaim even if keep_start is 0.
        let n = soft_digest_conversation(&mut m, 8_000, None);
        assert!(n >= 1, "should digest oversized write_file args: {n}");
        let call_args = match &m[1] {
            Message::Assistant { tool_calls, .. } => {
                tool_calls.as_ref().unwrap()[0].function.arguments.clone()
            }
            _ => panic!("expected assistant"),
        };
        assert!(call_args.contains("digested"), "{call_args}");
        assert!(!call_args.contains(&huge));
    }

    #[test]
    fn soft_digest_few_huge_messages_reclaims_tool_results() {
        // ≤ MIN_KEEP messages but huge tool results — must digest results, not
        // only call args (the case keep_start==0 used to under-reclaim).
        let huge = "x\n".repeat(20_000);
        let mut m = vec![
            Message::system("sys"),
            asst_tool_call("c1", "read_file", r#"{"path":"a.rs"}"#),
            tool_result("c1", &huge),
            asst_tool_call("c2", "read_file", r#"{"path":"b.rs"}"#),
            tool_result("c2", &huge),
            Message::user("continue"),
        ];
        assert!(m.len() <= SOFT_DIGEST_MIN_KEEP);
        let before = estimate_messages_tokens(&m);
        let n = soft_digest_conversation(&mut m, 200_000, None);
        assert!(n >= 1, "few-but-huge must digest: {n}");
        let after = estimate_messages_tokens(&m);
        assert!(after < before, "{before} -> {after}");
        assert!(
            m[2].content_text().unwrap().starts_with("[digested:"),
            "{}",
            m[2].content_text().unwrap()
        );
    }

    #[test]
    fn context_policy_defaults_digest_at_70_and_compact_at_90() {
        let policy = context_policy(&[], 100_000, 20_000, 0.90, 0.70);
        assert_eq!(policy.digest_threshold, 70_000);
        assert_eq!(policy.compact_threshold, 90_000);
        assert_eq!(policy.response_reserve, 5_000);
        assert_eq!(policy.safety_margin, 2_000);
        assert_eq!(policy.hard_limit, 93_000);
    }

    #[test]
    fn context_policy_reserves_more_after_large_responses() {
        let messages = vec![Message::assistant("x".repeat(80_000))];
        let policy = context_policy(&messages, 100_000, 50_000, 0.90, 0.70);
        assert_eq!(policy.response_reserve, 25_000, "reserve is bounded at 25%");
        assert!(policy.hard_limit < 75_000);
        assert_eq!(policy.compact_threshold, policy.hard_limit);
    }

    #[test]
    fn automatic_rewrites_honor_switch_and_boundaries() {
        let policy = context_policy(&[], 100_000, 20_000, 0.90, 0.70);
        assert!(!should_auto_digest(false, 99_000, policy));
        assert!(!should_auto_compact(false, 99_000, 20, policy));
        assert!(!should_auto_digest(true, 70_000, policy));
        assert!(should_auto_digest(true, 70_001, policy));
        assert!(!should_auto_compact(true, 90_000, 20, policy));
        assert!(should_auto_compact(true, 90_001, 20, policy));
        // Idleness now uses this same predicate, so message count alone cannot
        // compact a low-pressure conversation.
        assert!(!should_auto_compact(true, 10_000, 100, policy));
        // A few-message conversation still compacts when it exceeds the hard
        // safe-input limit.
        assert!(should_auto_compact(true, 93_001, 2, policy));
    }

    #[test]
    fn oversized_non_tool_tail_is_detected_after_compaction() {
        let mut messages = vec![Message::system("sys")];
        for i in 0..8 {
            messages.push(Message::user(format!("small {i}")));
        }
        messages.push(Message::user("x".repeat(500_000)));
        let policy = context_policy(&messages, 100_000, 20_000, 0.90, 0.70);
        compact_conversation(&mut messages, 100_000);
        assert!(
            estimate_messages_tokens(&messages) > policy.hard_limit,
            "caller must block a request when a singular non-tool payload cannot be reclaimed"
        );
    }
}

#[cfg(test)]
mod work_state_tests {
    use super::*;

    fn todo(subject: &str, status: &str) -> Value {
        json!({ "subject": subject, "status": status })
    }

    #[test]
    fn render_empty_shows_placeholder_and_omits_sections() {
        let ws = WorkState::default();
        assert!(ws.is_empty());
        let r = ws.render();
        assert!(r.contains("Goal: (not yet stated)"));
        // Empty lists must not emit their headers.
        assert!(!r.contains("Done:"));
        assert!(!r.contains("In progress:"));
        assert!(!r.contains("Next:"));
        assert!(!r.contains("Recently touched:"));
    }

    #[test]
    fn render_includes_all_populated_sections() {
        let ws = WorkState {
            goal: "ship context management".into(),
            done: vec!["design".into()],
            in_progress: vec!["implement".into()],
            next: vec!["test".into(), "doc".into()],
            recent_files: vec!["core/src/main.rs".into()],
            last_activity: "edit core/src/main.rs".into(),
            ..Default::default()
        };
        let r = ws.render();
        assert!(r.contains("Goal: ship context management"));
        assert!(r.contains("Done:"));
        assert!(r.contains("- design"));
        assert!(r.contains("In progress:"));
        assert!(r.contains("- implement"));
        assert!(r.contains("Next:"));
        assert!(r.contains("- test"));
        assert!(r.contains("- doc"));
        assert!(r.contains("Recently touched: core/src/main.rs"));
        assert!(r.contains("Last: edit core/src/main.rs"));
        // Framing so the model treats it as ambient, not a prompt to answer.
        assert!(r.contains("respond to the user's latest message"));
    }

    #[test]
    fn render_caps_long_lists() {
        let ws = WorkState {
            goal: "g".into(),
            done: (0..10).map(|i| format!("item {i}")).collect(),
            ..Default::default()
        };
        let r = ws.render();
        // Only the first MAX_LIST (6) entries appear verbatim ...
        assert!(r.contains("- item 0"));
        assert!(r.contains("- item 5"));
        assert!(!r.contains("- item 6"));
        // ... and the overflow is summarized.
        assert!(r.contains("… +4 more"));
    }

    #[test]
    fn sync_from_todos_partitions_by_status() {
        let mut ws = WorkState {
            goal: "g".into(),
            ..Default::default()
        };
        let todos = vec![
            todo("design", "completed"),
            todo("implement", "in_progress"),
            todo("test", "pending"),
            todo("doc", "pending"),
        ];
        ws.sync_from_todos(&todos);
        assert_eq!(ws.done, vec!["design"]);
        assert_eq!(ws.in_progress, vec!["implement"]);
        assert_eq!(ws.next, vec!["test", "doc"]);
        assert!(ws.version > 0);
    }

    #[test]
    fn sync_from_todos_skips_empty_subjects() {
        let mut ws = WorkState::default();
        let todos = vec![todo("", "completed"), todo("real", "in_progress")];
        ws.sync_from_todos(&todos);
        assert!(ws.done.is_empty());
        assert_eq!(ws.in_progress, vec!["real"]);
    }

    #[test]
    fn record_files_dedup_and_mru_order() {
        let mut ws = WorkState::default();
        ws.record_files("edit", &["a.rs".into(), "b.rs".into()]);
        assert_eq!(ws.recent_files, vec!["a.rs", "b.rs"]);
        // Touching an existing file moves it to the front (most-recent-first).
        ws.record_files("edit", &["a.rs".into()]);
        assert_eq!(ws.recent_files, vec!["a.rs", "b.rs"]);
        assert_eq!(ws.last_activity, "edit a.rs");
    }

    #[test]
    fn record_files_caps_at_eight() {
        let mut ws = WorkState::default();
        for i in 0..12 {
            ws.record_files("edit", &[format!("f{i}.rs")]);
        }
        assert_eq!(ws.recent_files.len(), 8);
        // Most-recent (f11) is at the front.
        assert_eq!(ws.recent_files[0], "f11.rs");
    }

    #[test]
    fn peers_touching_matches_exact_normalized_path() {
        let mk = |pid: u32, files: &[&str]| presence::PresenceRecord {
            pid,
            session_id: None,
            started_at: 0,
            last_heartbeat: 0,
            goal: String::new(),
            in_progress: vec![],
            next: vec![],
            recent_files: files.iter().map(|s| s.to_string()).collect(),
            last_activity: String::new(),
            model: None,
        };
        let peers = vec![mk(111, &["core/src/main.rs"]), mk(222, &["other.go"])];
        // exact match → the touching peer's pid
        assert_eq!(
            peers_touching(&peers, "edit", &json!({"path":"core/src/main.rs"})),
            "pid 111"
        );
        // separator-normalized (backslash) still matches
        assert_eq!(
            peers_touching(&peers, "write_file", &json!({"path":"core\\src\\main.rs"})),
            "pid 111"
        );
        // a path nobody is touching → empty (no false positive)
        assert_eq!(
            peers_touching(&peers, "read_file", &json!({"path":"foo.rs"})),
            ""
        );
        // a non-file tool (bash) → empty
        assert_eq!(peers_touching(&peers, "bash", &json!({"command":"ls"})), "");
        // multiple touching peers → comma-list
        let peers2 = vec![mk(111, &["shared.rs"]), mk(333, &["shared.rs"])];
        let s = peers_touching(&peers2, "edit", &json!({"path":"shared.rs"}));
        assert!(s.contains("pid 111") && s.contains("pid 333"));
    }
}

#[cfg(test)]
mod auto_reflect_tests {
    use super::*;

    #[test]
    fn learning_turns_are_exempt() {
        // The prefixes the TUI's /reflect and /index delegations produce.
        assert!(is_learning_turn(
            "Reflect on the work done in this session so far…"
        ));
        assert!(is_learning_turn(
            "Run a full knowledge index of this repository now."
        ));
        assert!(is_learning_turn(
            "  Run an incremental knowledge index of this repository"
        ));
        // A normal user task is not exempt.
        assert!(!is_learning_turn("Add a release packaging flow"));
        assert!(!is_learning_turn("fix the typo in README"));
    }

    #[test]
    fn reflect_text_mentions_memory_and_skills() {
        let txt = build_reflect_text(&[]);
        assert!(txt.contains("[auto-reflect]"));
        assert!(txt.contains("memory"));
        assert!(txt.contains("skills/<name>/SKILL.md"));
        assert!(txt.contains("finish"));
        // Must demand a user-facing completion summary after reflection — bare
        // finish is the failure mode this prompt exists to prevent.
        assert!(txt.contains("completion summary"));
        assert!(txt.contains("bare"));
        // No recurrence → no recurring-patterns section.
        assert!(!txt.contains("Recurring patterns detected"));
    }

    #[test]
    fn reflect_text_lists_recurring_patterns() {
        let rec = vec![
            (3, "add a core tool".into()),
            (2, "add a tui renderer".into()),
        ];
        let txt = build_reflect_text(&rec);
        assert!(txt.contains("Recurring patterns detected"));
        assert!(txt.contains("add a core tool (3 times)"));
        assert!(txt.contains("add a tui renderer (2 times)"));
    }

    #[test]
    fn extract_file_categories_for_file_tools() {
        let cats = extract_file_categories("edit", r#"{"path":"core/src/main.rs"}"#);
        assert_eq!(cats, vec!["core/src/*.rs".to_string()]);
        // bulk_write: one category per file, deduped by shape_signature later.
        let cats = extract_file_categories(
            "bulk_write",
            r#"{"files":[{"path":"tui/render.go"},{"path":"tui/handlers.go"}]}"#,
        );
        assert_eq!(cats.len(), 2);
        assert!(cats.iter().all(|c| c == "tui/*.go"));
    }

    #[test]
    fn extract_file_categories_empty_for_non_file_tools() {
        assert!(extract_file_categories("bash", r#"{"command":"ls"}"#).is_empty());
        assert!(extract_file_categories("grep", r#"{"pattern":"x"}"#).is_empty());
        // Malformed JSON is tolerated (no panic, no categories).
        assert!(extract_file_categories("edit", "not json").is_empty());
    }
}

#[cfg(test)]
mod restricted_path_tests {
    use super::*;

    // A throwaway workspace root for the canonical-path re-check. Unique per
    // call so parallel `cargo test` never collides (mirrors workspace::tmp_root).
    fn root() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("umans_rpt_test_{}", n));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn read_file_restricted_path_detected() {
        let r = root();
        // A direct read of a restricted file is flagged so the gate can prompt.
        let args = json!({"path": ".env"});
        assert!(restricted_path_for_tool("read_file", &args, &r).is_some());
        // Safe path: no flag.
        let args = json!({"path": "src/main.rs"});
        assert!(restricted_path_for_tool("read_file", &args, &r).is_none());
    }

    #[test]
    fn write_edit_patch_restricted_paths_detected() {
        let r = root();
        for tool in ["write_file", "edit", "patch"] {
            let args = json!({"path": ".git/config"});
            assert!(
                restricted_path_for_tool(tool, &args, &r).is_some(),
                "{tool} should flag .git/config"
            );
        }
        let args = json!({"path": "README.md"});
        assert!(restricted_path_for_tool("write_file", &args, &r).is_none());
    }

    #[test]
    fn case_insensitive_match() {
        let r = root();
        // .ENV / .GIT/config must still match on case-insensitive filesystems.
        assert!(restricted_path_for_tool("read_file", &json!({"path": ".ENV"}), &r).is_some());
        assert!(
            restricted_path_for_tool("write_file", &json!({"path": ".GIT/config"}), &r).is_some()
        );
    }

    #[test]
    fn bulk_read_flags_any_restricted() {
        let r = root();
        let args = json!({"paths": ["ok.txt", ".env", "other.rs"]});
        assert!(restricted_path_for_tool("bulk_read", &args, &r).is_some());
        let args = json!({"paths": ["a.rs", "b.go"]});
        assert!(restricted_path_for_tool("bulk_read", &args, &r).is_none());
    }

    #[test]
    fn bulk_write_and_bulk_edit_flag_restricted() {
        let r = root();
        let args = json!({"files": [{"path": ".env", "content": "x"}]});
        assert!(restricted_path_for_tool("bulk_write", &args, &r).is_some());
        let args = json!({"edits": [{"path": ".ssh/config", "edits": []}]});
        assert!(restricted_path_for_tool("bulk_edit", &args, &r).is_some());
        let args = json!({"files": [{"path": "ok.txt", "content": "x"}]});
        assert!(restricted_path_for_tool("bulk_write", &args, &r).is_none());
    }

    #[test]
    fn bulk_recurses_into_inner_calls() {
        let r = root();
        // A bulk containing an inner write to a restricted path is flagged so
        // the whole bulk prompts (then approved inner calls proceed).
        let args = json!({"calls": [
            {"name": "read_file", "args": {"path": "ok.txt"}},
            {"name": "write_file", "args": {"path": ".env", "content": "LEAK=1"}}
        ]});
        assert!(restricted_path_for_tool("bulk", &args, &r).is_some());
        // All-safe bulk: no flag.
        let args = json!({"calls": [
            {"name": "read_file", "args": {"path": "a.rs"}},
            {"name": "write_file", "args": {"path": "b.go", "content": "x"}}
        ]});
        assert!(restricted_path_for_tool("bulk", &args, &r).is_none());
    }

    #[test]
    fn excluded_tools_never_flag() {
        let r = root();
        // bash/grep/glob/list_dir are intentionally excluded — they don't read a
        // single restricted file's content by path.
        assert!(restricted_path_for_tool("bash", &json!({"command": "cat .env"}), &r).is_none());
        assert!(
            restricted_path_for_tool("grep", &json!({"pattern": "x", "path": "src"}), &r).is_none()
        );
        assert!(restricted_path_for_tool("glob", &json!({"pattern": "**/*"}), &r).is_none());
        assert!(restricted_path_for_tool("list_dir", &json!({"path": "."}), &r).is_none());
    }

    // Regression (M1): a symlink alias to a restricted dir (linkdir -> .git)
    // must be flagged. The raw path "linkdir/config" contains no `.git`
    // component, so only the canonical (symlink-resolved) re-check catches it.
    #[cfg(unix)]
    #[test]
    fn symlink_alias_to_restricted_is_flagged() {
        use std::os::unix::fs::symlink;
        let r = root();
        // `linkdir` is a relative symlink to the in-workspace `.git` dir.
        std::fs::create_dir_all(r.join(".git")).unwrap();
        symlink(".git", r.join("linkdir")).unwrap();
        // Reading through the alias must prompt (canonical path = <root>/.git/config).
        let args = json!({"path": "linkdir/config"});
        assert!(
            restricted_path_for_tool("read_file", &args, &r).is_some(),
            "symlink alias to .git must be flagged by the canonical re-check"
        );
        // The literal restricted path is also flagged.
        let args = json!({"path": ".git/config"});
        assert!(restricted_path_for_tool("read_file", &args, &r).is_some());
        // A genuinely safe path is not flagged.
        let args = json!({"path": "src/main.rs"});
        assert!(restricted_path_for_tool("read_file", &args, &r).is_none());
    }
}

#[cfg(test)]
mod ask_tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_and_missing() {
        assert!(validate_ask_questions(&json!({})).is_err());
        assert!(validate_ask_questions(&json!({"questions": []})).is_err());
        // missing id
        assert!(
            validate_ask_questions(&json!({"questions": [{"prompt":"p","type":"text"}]})).is_err()
        );
        // missing prompt
        assert!(validate_ask_questions(&json!({"questions": [{"id":"a","type":"text"}]})).is_err());
        // invalid type
        assert!(validate_ask_questions(
            &json!({"questions": [{"id":"a","prompt":"p","type":"radio"}]})
        )
        .is_err());
    }

    #[test]
    fn validate_select_requires_options() {
        assert!(validate_ask_questions(
            &json!({"questions": [{"id":"a","prompt":"p","type":"select"}]})
        )
        .is_err());
        assert!(validate_ask_questions(
            &json!({"questions": [{"id":"a","prompt":"p","type":"select","options":[]}]})
        )
        .is_err());
        // valid select
        let q = validate_ask_questions(
            &json!({"questions": [{"id":"a","prompt":"p","type":"select","options":["x","y"]}]}),
        )
        .unwrap();
        assert_eq!(q.as_array().unwrap().len(), 1);
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let r = validate_ask_questions(&json!({"questions": [
            {"id":"a","prompt":"p","type":"text"},
            {"id":"a","prompt":"q","type":"text"}
        ]}));
        assert!(r.is_err());
    }

    #[test]
    fn format_answers_marks_skipped() {
        let qs = validate_ask_questions(&json!({"questions": [
            {"id":"fw","prompt":"Which framework?","type":"select","options":["React","Vue"]},
            {"id":"notes","prompt":"Any notes?","type":"text","required":false}
        ]}))
        .unwrap();
        // answered fw, skipped notes
        let out = format_ask_answers(&qs, &json!({"fw": "React"}));
        assert!(out.contains("fw (Which framework?): React"));
        assert!(out.contains("notes (Any notes?): (skipped)"));
    }

    #[test]
    fn format_answers_all_answered() {
        let qs = validate_ask_questions(&json!({"questions": [
            {"id":"a","prompt":"Q1","type":"text"}
        ]}))
        .unwrap();
        let out = format_ask_answers(&qs, &json!({"a": "hello"}));
        assert!(out.contains("a (Q1): hello"));
        assert!(!out.contains("(skipped)"));
    }
}

#[cfg(test)]
mod expand_mentions_tests {
    use super::*;

    fn fresh_workspace() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "catalyst-code-mentions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn inlines_existing_file() {
        let ws = fresh_workspace();
        std::fs::write(ws.join("main.rs"), "fn main() {}\n").unwrap();
        let (out, attached) = expand_file_mentions("fix @main.rs please", &ws, u64::MAX);
        assert_eq!(attached, vec!["main.rs".to_string()]);
        assert!(out.contains("<file path=\"main.rs\">"));
        assert!(out.contains("fn main() {}"));
        assert!(out.contains("</file>"));
        // The surrounding prose is preserved (no trailing newline in input).
        assert!(out.starts_with("fix "));
        assert!(out.ends_with(" please"));
    }

    #[test]
    fn leaves_missing_path_as_is() {
        let ws = fresh_workspace();
        let (out, attached) = expand_file_mentions("look at @nope.rs", &ws, u64::MAX);
        assert!(attached.is_empty());
        assert_eq!(out, "look at @nope.rs");
    }

    #[test]
    fn email_not_triggered() {
        // `foo@bar` has no whitespace before `@`, so it must NOT be a mention.
        let ws = fresh_workspace();
        std::fs::write(ws.join("bar"), "x").unwrap();
        let (out, attached) = expand_file_mentions("email foo@bar.com here", &ws, u64::MAX);
        assert!(attached.is_empty());
        assert_eq!(out, "email foo@bar.com here");
    }

    #[test]
    fn inline_param_tag_not_triggered_without_space() {
        // `@param` embedded mid-word (no leading space) is left alone even if a
        // file named `param` exists.
        let ws = fresh_workspace();
        std::fs::write(ws.join("param"), "x").unwrap();
        let (out, attached) = expand_file_mentions("see the@param tag", &ws, u64::MAX);
        assert!(attached.is_empty());
        assert_eq!(out, "see the@param tag");
    }

    #[test]
    fn strips_trailing_punctuation() {
        let ws = fresh_workspace();
        std::fs::write(ws.join("file.rs"), "pub fn f() {}\n").unwrap();
        let (out, attached) = expand_file_mentions("see @file.rs.", &ws, u64::MAX);
        assert_eq!(attached, vec!["file.rs".to_string()]);
        assert!(out.contains("<file path=\"file.rs\">"));
        // A file with a trailing dot literally does not exist, so the literal
        // candidate is skipped and the trimmed one wins.
        assert!(!out.contains("<file path=\"file.rs.\">"));
    }

    #[test]
    fn skips_directory() {
        let ws = fresh_workspace();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let (out, attached) = expand_file_mentions("look at @sub", &ws, u64::MAX);
        assert!(attached.is_empty());
        // Directory left as-is so the model can fall back to read_file/list_dir.
        assert_eq!(out, "look at @sub");
    }

    #[test]
    fn skips_oversized_file() {
        let ws = fresh_workspace();
        // max_bytes = 3, file is 10 bytes → skipped, left as-is.
        std::fs::write(ws.join("big.txt"), "0123456789").unwrap();
        let (out, attached) = expand_file_mentions("@big.txt", &ws, 3);
        assert!(attached.is_empty());
        assert_eq!(out, "@big.txt");
    }

    #[test]
    fn multiple_mentions_inlined() {
        let ws = fresh_workspace();
        std::fs::write(ws.join("a.rs"), "a\n").unwrap();
        std::fs::write(ws.join("b.go"), "b\n").unwrap();
        let (out, attached) = expand_file_mentions("@a.rs and @b.go", &ws, u64::MAX);
        assert_eq!(attached, vec!["a.rs".to_string(), "b.go".to_string()]);
        assert!(out.contains("<file path=\"a.rs\">"));
        assert!(out.contains("<file path=\"b.go\">"));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_path_is_not_inlined() {
        let dir = std::env::temp_dir().join(format!(
            "catalyst-code-abs-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abs.txt");
        std::fs::write(&f, "abs content\n").unwrap();
        let ws = fresh_workspace();
        let mention = format!("@{}", f.display());
        let (out, attached) = expand_file_mentions(&mention, &ws, u64::MAX);
        assert!(attached.is_empty());
        assert_eq!(out, mention);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_mention_enforces_inline_budget() {
        let ws = fresh_workspace();
        std::fs::write(ws.join("large.txt"), "x".repeat(25 * 1024)).unwrap();
        let (out, attached) = expand_file_mentions("@large.txt", &ws, u64::MAX);
        assert!(attached.is_empty());
        assert_eq!(out, "@large.txt");
    }

    #[test]
    fn user_bash_context_format_matches_pi() {
        let text = format_user_bash_context("ls -la", "total 0\n", true);
        assert!(text.starts_with("Ran `ls -la`\n```\n"));
        assert!(text.contains("total 0\n"));
        assert!(text.ends_with("```"));
        let fail = format_user_bash_context("false", "(no output)", false);
        assert!(fail.contains("(exit non-zero)"));
    }
}

#[cfg(test)]
mod provider_routing_tests {
    use super::{
        is_keyless_local_provider, is_loopback_base_url, logged_in_providers_for,
        pick_provider_for_model, pick_provider_for_model_with, provider_url_host,
        provider_usable_api_key, resolve_provider_from_config, select_model_info,
    };
    use crate::config::{Config, ProviderConfig, ProviderKind};
    use crate::protocol::ModelInfo;
    use std::collections::HashMap;

    fn owners(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn pc(
        name: &str,
        base_url: &str,
        api_key: Option<&str>,
        api_key_env: Option<&str>,
    ) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            kind: ProviderKind::OpenAI,
            base_url: base_url.into(),
            api_key: api_key.map(|s| s.to_string()),
            api_key_env: api_key_env.map(|s| s.to_string()),
            headers: Vec::new(),
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }

    fn mi(id: &str, provider: &str, ctx: u32, max_tokens: u32) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            reasoning: false,
            context_window: ctx,
            max_tokens,
            thinking_levels: Vec::new(),
            vision: false,
            provider: provider.into(),
            ..Default::default()
        }
    }

    #[test]
    fn active_provider_wins_tie_when_it_owns_the_model() {
        let o = owners(&["opencode-go", "karutoil"]);
        assert_eq!(
            pick_provider_for_model(&o, Some("karutoil")),
            Some("karutoil")
        );
        assert_eq!(
            pick_provider_for_model(&o, Some("opencode-go")),
            Some("opencode-go")
        );
    }

    #[test]
    fn non_owner_active_pins_to_active_provider() {
        // The active provider does NOT serve this model id, but several other
        // providers do. We deliberately return None so the caller sends the id
        // to the ACTIVE provider (surfacing its error) instead of silently
        // routing the turn to a different provider's copy of the same model.
        let o = owners(&["opencode-go", "karutoil"]);
        assert_eq!(pick_provider_for_model(&o, Some("umans")), None);
        // No active provider and no single owner: also None (no deterministic
        // pick) -> caller falls back to the resolved active/legacy provider.
        assert_eq!(pick_provider_for_model(&o, None), None);
    }

    #[test]
    fn single_owner_unaffected_by_active() {
        let o = owners(&["karutoil"]);
        assert_eq!(
            pick_provider_for_model(&o, Some("opencode-go")),
            Some("karutoil")
        );
        assert_eq!(pick_provider_for_model(&o, None), Some("karutoil"));
    }

    #[test]
    fn no_owners_means_fallback() {
        let o: Vec<String> = Vec::new();
        assert_eq!(pick_provider_for_model(&o, Some("karutoil")), None);
    }

    #[test]
    fn explicit_pick_wins_over_active_tie_break() {
        // Two providers serve the same id; the ACTIVE provider is karutoil but
        // the user explicitly picked local's copy in /models — the pick wins.
        let o = owners(&["karutoil", "local"]);
        assert_eq!(
            pick_provider_for_model_with(&o, Some("karutoil"), Some("local")),
            Some("local")
        );
        // Pick matching the active provider still resolves to it.
        assert_eq!(
            pick_provider_for_model_with(&o, Some("karutoil"), Some("karutoil")),
            Some("karutoil")
        );
        // No active provider at all: the pick still wins.
        assert_eq!(
            pick_provider_for_model_with(&o, None, Some("local")),
            Some("local")
        );
    }

    #[test]
    fn non_owner_pick_falls_back_to_legacy_tie_break() {
        // A stale/wrong pick (provider doesn't serve this id) must NOT pin
        // routing — fall back to the legacy active-provider behavior.
        let o = owners(&["karutoil", "local"]);
        assert_eq!(
            pick_provider_for_model_with(&o, Some("karutoil"), Some("umans")),
            Some("karutoil")
        );
        assert_eq!(
            pick_provider_for_model_with(&o, Some("umans"), Some("umans")),
            None
        );
    }

    #[test]
    fn none_pick_matches_legacy_behavior() {
        let o = owners(&["opencode-go", "karutoil"]);
        assert_eq!(
            pick_provider_for_model_with(&o, Some("karutoil"), None),
            pick_provider_for_model(&o, Some("karutoil"))
        );
        assert_eq!(
            pick_provider_for_model_with(&o, None, None),
            pick_provider_for_model(&o, None)
        );
    }

    #[test]
    fn empty_api_key_is_not_usable_or_logged_in() {
        // Blank config / runtime keys must not count as authentication — they
        // previously made logged_in_providers_for report the provider while
        // resolve_provider_from_config dropped the empty string (authed UX lie).
        let p = pc(
            "karutoil",
            "https://ai.example/v1",
            Some(""),
            Some("KARUTOIL_KEY"),
        );
        let mut keys = HashMap::new();
        keys.insert("karutoil".into(), "".into());
        assert!(provider_usable_api_key(&p, &keys).is_none());
        assert!(resolve_provider_from_config(&p, &keys).api_key.is_none());

        let mut cfg = Config::default();
        cfg.providers = vec![p];
        assert!(logged_in_providers_for(&cfg, &keys, None).is_empty());
    }

    #[test]
    fn non_empty_runtime_key_logs_provider_in() {
        let p = pc(
            "karutoil",
            "https://ai.example/v1",
            None,
            Some("KARUTOIL_KEY"),
        );
        let mut keys = HashMap::new();
        keys.insert("karutoil".into(), "sk-live".into());
        let mut cfg = Config::default();
        cfg.providers = vec![p.clone()];
        assert_eq!(
            logged_in_providers_for(&cfg, &keys, None),
            vec!["karutoil".to_string()]
        );
        assert_eq!(
            resolve_provider_from_config(&p, &keys).api_key.as_deref(),
            Some("sk-live")
        );
    }

    #[test]
    fn keyless_loopback_is_logged_in_without_key() {
        // Ollama / LM Studio style: no required key env + loopback host.
        let local = pc("ollama", "http://localhost:11434/v1", None, None);
        assert!(is_loopback_base_url(&local.base_url));
        assert!(is_keyless_local_provider(&local));
        let mut cfg = Config::default();
        cfg.providers = vec![
            local,
            // Cloud provider with no key must NOT piggy-back on the keyless path.
            pc("remote", "https://api.example/v1", None, None),
        ];
        let names = logged_in_providers_for(&cfg, &HashMap::new(), None);
        assert_eq!(names, vec!["ollama".to_string()]);
    }

    #[test]
    fn empty_api_key_env_name_does_not_lookup_empty_env() {
        // api_key_env: Some("") is the keyless marker, not an env lookup of "".
        let p = pc("lmstudio", "http://127.0.0.1:1234/v1", None, Some(""));
        assert!(is_keyless_local_provider(&p));
        assert!(provider_usable_api_key(&p, &HashMap::new()).is_none());
        let mut cfg = Config::default();
        cfg.providers = vec![p];
        assert_eq!(
            logged_in_providers_for(&cfg, &HashMap::new(), None),
            vec!["lmstudio".to_string()]
        );
    }

    #[test]
    fn provider_url_host_strips_scheme_port_path() {
        assert_eq!(
            provider_url_host("https://api.x.ai/v1").as_deref(),
            Some("api.x.ai")
        );
        assert_eq!(
            provider_url_host("http://127.0.0.1:1234/v1").as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn select_model_info_honors_explicit_pick_over_active() {
        // Caps must follow the same owner rules as turn routing: an explicit
        // /models pick of `local` must not keep using karutoil's smaller budget.
        let models = vec![
            mi("deepseek-v4-flash", "karutoil", 512_000, 8_192),
            mi("deepseek-v4-flash", "local", 128_000, 32_768),
        ];
        let info = select_model_info(
            &models,
            "deepseek-v4-flash",
            Some("karutoil"),
            Some("local"),
        )
        .expect("row");
        assert_eq!(info.provider, "local");
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.max_tokens, 32_768);
    }

    #[test]
    fn select_model_info_active_wins_without_pick() {
        let models = vec![
            mi("deepseek-v4-flash", "opencode-go", 1_000_000, 64_000),
            mi("deepseek-v4-flash", "karutoil", 512_000, 8_192),
        ];
        let info =
            select_model_info(&models, "deepseek-v4-flash", Some("karutoil"), None).expect("row");
        assert_eq!(info.provider, "karutoil");
        assert_eq!(info.context_window, 512_000);
    }

    #[test]
    fn select_model_info_non_owner_active_falls_back_to_first_row() {
        // Mirrors pick_provider_for_model returning None: no deterministic
        // owner, so caps fall back to the first listed row by id.
        let models = vec![
            mi("deepseek-v4-flash", "opencode-go", 1_000_000, 64_000),
            mi("deepseek-v4-flash", "karutoil", 512_000, 8_192),
        ];
        let info =
            select_model_info(&models, "deepseek-v4-flash", Some("umans"), None).expect("row");
        assert_eq!(info.provider, "opencode-go");
    }
}
