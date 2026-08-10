// Shared wire + UI types for the catalyst-code web frontend.
//
// The core speaks newline-delimited JSON over stdio. The server bridge forwards
// raw core events to the browser over SSE, and accepts raw core commands via
// POST. Wire-level event payloads are sourced from the SDK (@catalyst-code/coding-agent)'s
// typed event catalog (core-events.ts / core-process.ts); the UI message model
// is assembled by the reducer from the event stream.
//
// This file is imported by BOTH server and browser code — use ONLY `import type`
// (no runtime SDK imports).

import type {
  ApprovalRequestEvent,
  CostUpdateEvent,
  FileChangeEvent,
  MetricsEvent,
  ProtocolHelloEvent,
  SudoRequestEvent,
  WorktreeReadyEvent,
} from "@catalyst-code/coding-agent";
import type { ReadyPayload as SDKReadyPayload } from "@catalyst-code/coding-agent";

// ─── Core wire types ────────────────────────────────────────────────────────

export interface ModelInfo {
  id: string;
  name: string;
  reasoning: boolean;
  context_window: number;
  max_tokens: number;
  /** Thinking levels the model advertises (e.g. ["low","medium","high"]). */
  thinking_levels: string[];
  vision: boolean;
  /** Accepted and emitted content modalities. */
  input?: string[];
  output?: string[];
  tool_call?: boolean;
  structured_output?: boolean;
  /** Owning provider name (e.g. "openai", "gemini", "anthropic"), populated by
   * the core's multi-provider aggregation so a turn routes to the right endpoint
   * when multiple providers are logged in. Empty for legacy single-provider models. */
  provider?: string;
}

/** A built-in first-party provider template advertised by the core. */
export interface ProviderPreset {
  id: string;
  label: string;
  kind: string;
  base_url: string;
  envVar: string;
  altEnvs?: string[];
  description: string;
  hasKey: boolean;
  configured: boolean;
  loggedIn: boolean;
  supportsOauth?: boolean;
}

/** A per-model capability override (mirrors the core's ModelOverride). */
export interface ModelOverride {
  id: string;
  context_window?: number;
  max_tokens?: number;
  reasoning?: boolean;
  thinking_levels?: string[];
  input?: string[];
  output?: string[];
  tool_call?: boolean;
  structured_output?: boolean;
}

/** Fields for the add_custom_provider form — full config.json parity. */
export interface CustomProviderDraft {
  name: string;
  kind: "openai" | "anthropic";
  base_url: string;
  /** Literal API key stored in the 0600 config (wins over apiKeyEnv). */
  apiKey: string;
  /** Env var NAME holding the key (read at request time). */
  apiKeyEnv: string;
  /** Extra HTTP headers, one `Key: value` per line. */
  headersText: string;
  /** Optional per-provider context-window override (tokens). */
  contextWindow: string;
  /** Per-model caps overrides discovered via the discover step. */
  modelsOverride: ModelOverride[];
}

export interface ReadyPayload extends SDKReadyPayload {
  type: "ready";
  models: ModelInfo[];
  /** True when ANY configured provider is logged in (multi-provider sends). */
  anyLoggedIn?: boolean;
  providerPresets?: ProviderPreset[];
  /** When true, the core auto-compacts context on thresholds / idle. */
  auto_compact?: boolean;
  /** Configured fractions; runtime thresholds may be lower when response
   * headroom for the selected model requires it. */
  context_compact_at?: number;
  context_digest_at?: number;
  /** Bash hard sandbox mode: `"none"` | `"microsandbox"`. Legacy values
   *  ("firejail", "seatbelt") are migrated to "microsandbox" by the core with a
   *  deprecation notice — only "none" | "microsandbox" ever reaches the UI. */
  sandbox?: string;
  /** Effective guest shell: `"bash"` (Linux bash, including inside the
   *  microsandbox guest) or `"powershell"` (host PowerShell on Windows when
   *  sandboxing is off). Drives command-generation hints so Windows users are
   *  not told to generate PowerShell while the agent runs in a Linux microVM. */
  shell?: string;
  /** Active OCI image reference for the microsandbox guest. */
  sandboxImage?: string;
  /** CPU limit for the microsandbox guest. */
  sandboxCpus?: number;
  /** Memory limit (MiB) for the microsandbox guest. */
  sandboxMemoryMb?: number;
  /** Network policy mode for the microsandbox guest
   *  ("none" | "restricted" | "allowlist"). */
  sandboxNetworkMode?: string;
  /** True when the microsandbox guest is ready to run agent workloads. This is
   *  the source of truth for sandbox readiness, independent of the configured
   *  mode — a requested-but-not-ready sandbox must NOT run commands on host. */
  sandboxReady?: boolean;
  plugins_skipped?: string[];
}

/** A discoverable subagent (builtin / user / project) from `agents` events. */
export interface AgentInfo {
  name: string;
  description: string;
  source: "builtin" | "user" | "project" | string;
}

/** Latest `/context` payload kept for the Diagnostics panel. */
export interface ContextBreakdown {
  total_tokens: number;
  context_window: number;
  pct: number;
  messages: number;
  system_tokens: number;
  digest_threshold_tokens?: number;
  compact_threshold_tokens?: number;
  hard_limit_tokens?: number;
  response_reserve_tokens?: number;
  safety_margin_tokens?: number;
  by_role: Record<string, number>;
  top_consumers: {
    index: number;
    role: string;
    tokens: number;
    preview: string;
  }[];
}

/** Latest `/usage` payload kept for the Diagnostics panel. */
export interface UsageSnapshot {
  provider: string;
  provider_kind?: string;
  model?: string;
  base_url?: string;
  available: boolean;
  plan?: string;
  message?: string;
  windows: Array<{
    id: string;
    label: string;
    used?: number;
    limit?: number;
    unit: string;
    resets_at?: number;
    detail?: string;
  }>;
}

export type ApprovalRequest = Omit<ApprovalRequestEvent, "type">;

/** Stream + final-turn metrics. Mid-stream: `tokens_in` = live context;
 *  final = `elapsed_ms`/`prompt_tokens` present (true input).
 *  `tps_est` (mid-stream estimate) is mapped to `tps` in the reducer. */
export type Metrics = Omit<MetricsEvent, "type">;

/** Live, account-wide Umans concurrency usage from the gateway's `/v1/usage`
 *  endpoint, polled every few seconds by the core (independent of turns) so the
 *  footer can show a "Conc used/limit" field ahead of tps. `used == null` means
 *  not Umans / fetch failed (hide); `limit == null` means the plan is unlimited
 *  (render ∞). */
export interface UmansConc {
  used: number | null;
  limit: number | null;
  /** The Umans provider name the poll is tracking; the UI only renders the
   *  field when the selected model routes to this provider. */
  provider: string;
}

export interface SessionEntry {
  name: string;
  mtime: number;
  size?: number;
  /** Auto-derived title (first user message) from the core. May be overridden
   *  by the bridge's session-titles overlay (user-defined rename). */
  title?: string;
  /** Absolute path to the .jsonl session file. */
  path?: string;
  /** Message count in the session file. */
  messages?: number;
  /** True when this is the currently-active session. */
  current?: boolean;
  /** Pinned in the session picker. */
  pinned?: boolean;
}

/** What kind of human-gate a session is blocked on (mirrors AgentState.pending*). */
export type AttentionKind = "approval" | "ask" | "sudo" | "intercom" | "oauth";

/** Live status of a session, synthesized by the bridge from each LiveSession's
 *  runtime state (NOT from disk). Broadcast cross-session so a client viewing one
 *  session can see every other live session's streaming/attention state and be
 *  notified when a background session finishes or needs the user. */
export interface LiveSessionStatus {
  /** Absolute session-file path (matches SessionEntry.path / currentSessionFile). */
  sessionFile: string;
  workspace: string;
  /** Display title (best-effort; client may re-resolve from its sessions list). */
  title?: string;
  /** A turn is currently streaming output. */
  streaming: boolean;
  /** The backing core process is running. */
  running: boolean;
  /** True when the agent is blocked on a human gate (approval/ask/sudo/intercom/oauth). */
  needsAttention: boolean;
  attentionKind?: AttentionKind;
  /** Number of SSE subscribers currently viewing this session. */
  viewers: number;
  /** Epoch ms of the last status-relevant activity. */
  lastEventAt: number;
}

/** A cross-session notification item in the in-app feed. Derived client-side
 *  from LiveSessionStatus transitions (a background session entered an attention
 *  state, or finished a turn) — never emitted for the currently-viewed session. */
export interface NotificationItem {
  id: string;
  sessionFile: string;
  workspace: string;
  title: string;
  /** "attention" = agent blocked on a human gate; "finished" = a turn completed. */
  kind: "attention" | "finished";
  attentionKind?: AttentionKind;
  ts: number;
  read: boolean;
}

/** Cumulative / turn cost from core `cost_update` events. */
export type CostUpdate = Omit<CostUpdateEvent, "type">;

export type ProtocolHello = Omit<ProtocolHelloEvent, "type"> & {
  /** Numeric wire version; absent when connected to a legacy v1 core. */
  protocol_version?: number;
};

export interface CheckpointInfo {
  id: string;
  label?: string;
  kind?: string;
  auto?: boolean;
  paths?: string[];
  created_at?: number;
  [key: string]: unknown;
}

export type WorktreeInfo = Omit<WorktreeReadyEvent, "type">;

// ─── Sandbox (Microsandbox) ─────────────────────────────────────────────────
// The Rust core is the single source of truth for platform detection,
// readiness, and setup guidance — the UI never duplicates OS-detection logic.
// These types mirror the core's preflight report + status events.

/** Effective sandbox mode. The core migrates legacy "firejail"/"seatbelt"
 *  values to "microsandbox" (with a deprecation notice), so only these two
 *  values ever reach the UI. */
export type SandboxMode = "none" | "microsandbox";

/** Network policy for the microsandbox guest. */
export type SandboxNetworkMode = "none" | "restricted" | "allowlist";

/** Status of a single preflight check (stable, machine-readable). */
export type SandboxCheckStatus = "pass" | "fail" | "warn" | "info";

/** One preflight check in a sandbox status report. */
export interface SandboxPreflightCheck {
  /** Stable machine-readable code (e.g. "kvm_device_missing"). */
  code: string;
  title: string;
  status: SandboxCheckStatus;
  detail: string;
}

/** A user-facing setup/remediation action from a preflight report. */
export interface SandboxSetupAction {
  title: string;
  explanation: string;
  /** Copyable setup command, or null when no command applies (e.g. a BIOS step). */
  command: string | null;
  requires_admin: boolean;
  requires_reboot: boolean;
}

/** Structured preflight report from the core. `platform`/`architecture` are
 *  the source of truth for OS detection in the UI — do not re-derive them. */
export interface SandboxPreflightReport {
  requested: boolean;
  supported: boolean;
  ready: boolean;
  platform: string;
  architecture: string;
  checks: SandboxPreflightCheck[];
  actions: SandboxSetupAction[];
}

/** Live sandbox runtime status kept in `AgentState.sandbox`. Synthesized from
 *  the `ready` payload (source of truth for readiness) and the four
 *  sandbox_* events. */
export interface SandboxRuntimeStatus {
  /** Effective sandbox mode from the core. */
  mode: SandboxMode;
  /** True when the guest is ready to run agent workloads (from `ready`/
   *  `sandbox_ready`). When the mode is "microsandbox" but this is false,
   *  agent commands must NOT execute on the host. */
  ready: boolean;
  /** Active OCI image reference, when known. */
  image: string | null;
  /** CPU limit. */
  cpus: number | null;
  /** Memory limit (MiB). */
  memoryMb: number | null;
  /** Network policy mode. */
  networkMode: SandboxNetworkMode | null;
  /** Effective guest shell ("bash" | "powershell"), when known. */
  shell: "bash" | "powershell" | null;
  /** Latest preflight report (from `sandbox_status`). Null until the first
   *  `get_sandbox_status` / `sandbox_status` arrives. */
  report: SandboxPreflightReport | null;
  /** Latest prepare phase (e.g. "downloading_runtime"), null when idle. */
  preparePhase: string | null;
  /** Last sandbox error message, null when none. */
  error: string | null;
}

export type FileChangeRecord = Omit<FileChangeEvent, "type"> & { ts: number };

export type ApproveDecision =
  | "yes"
  | "no"
  | "always"
  | "allow_session"
  | "allow_pattern";

export interface Stats {
  type: "stats";
  /** Current real context size (matches the footer) — NOT cumulative. */
  tokens_in: number;
  /** Cumulative output tokens (total produced this session). */
  tokens_out: number;
  /** Cumulative prompt tokens (billing total input; drives cache_hit_ratio). */
  total_in?: number;
  /** Cumulative in + out (billing total). */
  tokens_total: number;
  cached_tokens: number;
  /** cached_tokens / total_in — fraction of cumulative prompt that was a cache hit. */
  cache_hit_ratio?: number;
  turns: number;
  messages: number;
  session_file: string;
}

/** A saved memory note (persisted per-workspace, injected into the system prompt).
 *  The core emits the full memory text under `content`, a one-line `description`,
 *  a `name` (slug/title), and `scope` ("workspace" | "global"). `text` is an
 *  alias for `name` kept for TUI parity; `tags` surfaces the memory type. */
export interface MemoryEntry {
  id: string;
  /** Memory slug/title (the core generates one on save). */
  name?: string;
  /** One-line description shown as a subtitle. */
  description?: string;
  /** Full memory text — the actual content the agent remembers. */
  content?: string;
  /** "workspace" or "global". */
  scope?: string;
  /** Memory type label (e.g. "note", "convention", "decision"). */
  type?: string;
  /** Alias for `name` (TUI parity). Kept for backward compat. */
  text: string;
  tags?: string[];
}

/** A loaded plugin. The core emits `name`, `version`, `enabled`, `description`,
 *  and `hooks` (the list of hook-point names the plugin registers). */
export interface PluginEntry {
  name: string;
  enabled: boolean;
  version?: string;
  path?: string;
  description?: string;
  /** Hook-point names this plugin registers (e.g. ["pre_write","post_bash"]). */
  hooks?: string[];
  error?: string;
}

/** One project-scoped plugin listed in a `plugin_trust_prompt` event.
 *  `decision` is the recorded user decision: "" (undecided) | "trust" | "deny". */
export interface PluginTrustEntry {
  name: string;
  version?: string;
  description?: string;
  path?: string;
  decision: "" | "trust" | "deny";
}

/** A discoverable skill (project then user scope). `content` is the parsed
 *  SKILL.md body — sent by the core so `/skill:<name>` can apply a skill even
 *  when it lives under ~/.catalyst-code/skills (outside the workspace, which
 *  the read_file tool cannot reach). */
export interface SkillInfo {
  name: string;
  description: string;
  location: string;
  content: string;
}

export interface MarketplaceSkill {
  id: string;
  skillId?: string;
  name: string;
  installs: number;
  source: string;
}

export interface InstalledMarketplaceSkill {
  name: string;
  source: string;
  scope: "project" | "global";
  hash?: string;
  location?: string;
}

/** An intercom message from a subagent to the orchestrator. `need_decision`
 *  asks are surfaced as a blocking prompt; other traffic is logged. */
export interface IntercomPrompt {
  request_id: string;
  from: string;
  message: string;
  reason?: string;
}

/** One question in an `ask` tool call: a multiple-choice selection or a
 *  free-text box. `options` is required for type "select". */
export interface AskQuestion {
  id: string;
  prompt: string;
  type: "select" | "text";
  options?: string[];
  allowCustom?: boolean;
  required?: boolean;
  placeholder?: string;
}

/** A pending `ask` tool call — the model asked the user one or more questions
 *  and is blocking until they answer or skip. */
export interface AskPrompt {
  request_id: string;
  questions: AskQuestion[];
}

/** A pending sudo_request: the agent wants to run a bash command that invokes
 *  `sudo`. The user must approve (with a password) or decline. */
export type SudoPrompt = Omit<SudoRequestEvent, "type">;

/** A log entry for intercom/subagent activity (kept recent, capped). */
export interface IntercomEntry {
  id: string;
  kind: "ask" | "reply" | "status";
  from?: string;
  to?: string;
  message: string;
  ts: number;
}

/** A single item in a subagent run's live chat transcript: either a
 *  user/assistant message or a tool call with its (later-arriving) result. */
export interface SubagentChatItem {
  /** Generated unique id (NOT the tool call_id, which can be empty/duplicate). */
  id: string;
  kind: "message" | "tool";
  ts: number;
  // message
  role?: "user" | "assistant";
  content?: string;
  // tool
  callId?: string;
  name?: string;
  args?: Record<string, unknown>;
  result?: string;
  ok?: boolean;
}

/** A live subagent run: lifecycle metadata + a per-run chat transcript the
 *  SubagentsPanel drills into. Keyed by run_id in `AgentState.subagentRuns`. */
export interface SubagentRunView {
  id: string;
  mode: string; // single | parallel | chain
  agent?: string;
  agents: string[];
  task: string;
  state: string; // running | completed | failed | paused
  depth: number;
  startedAt: number;
  endedAt?: number;
  summary?: string;
  phase?: string; // last progress phase
  tool?: string; // current tool name
  toolCount: number;
  tokensIn: number;
  tokensOut: number;
  elapsedMs: number;
  items: SubagentChatItem[];
}

/** Vision-handoff configuration (curated vision-capable models + target). */
export interface VisionConfig {
  /** Auto handoff on image turns (default true / recommended ON). */
  enabled: boolean;
  vision_models: string[];
  vision_model: string | null;
}

/** An OAuth authorization prompt from the core: the user must visit `url`
 *  (and, for the device flow, enter `code`) to complete a provider login. For
 *  the no-browser flow the user pastes the resulting code/callback URL back via
 *  the `oauth_code` command. */
export interface OauthPrompt {
  url: string;
  code?: string;
  message?: string;
}

/** Rolling work-state summary the core maintains from conversation signals
 *  (goal / done / in-progress / next / recent files / last activity). Emitted
 *  via `work_state` so the UI can render a live status panel. */
export interface WorkState {
  version: number;
  goal: string;
  done: string[];
  in_progress: string[];
  next: string[];
  recent_files: string[];
  last_activity: string;
}

/** Structured review / verify verdict from CEO goal events / `goal_state`. */
export interface GoalVerdict {
  ok: boolean;
  summary: string;
  evidence_paths?: string[];
  /** Unix epoch ms (`at` on the wire). */
  at?: number;
}

/** One verify→replan cycle (or pre-deploy plan-revision wave) for Control Center. */
export interface GoalIterationRecord {
  iteration: number;
  plan_revision?: number;
  review_verdict?: GoalVerdict | null;
  verify_verdict?: GoalVerdict | null;
  remaining_gaps?: string[];
  /** Prompt snapshot when the iteration was last updated. */
  prompts?: GoalPrompt[];
  certified?: boolean;
}

/** First-class goal mode snapshot from `goal_state` events. */
export interface GoalModeState {
  id: string;
  goal: string;
  phase: string;
  concurrency: number;
  max_tasks: number;
  allowed_models: string[];
  allowed_providers: string[];
  auto_deploy: boolean;
  /** Autonomous Control Center CEO loop (self-review + verify/replan). */
  ceo_mode?: boolean;
  /** Wire: `"ceo"` | `"single_pass"`. */
  mode?: string;
  iteration?: number;
  max_iterations?: number;
  plan_revision?: number;
  max_plan_revisions?: number;
  review_verdict?: GoalVerdict | null;
  verify_verdict?: GoalVerdict | null;
  remaining_gaps?: string[];
  self_review_feedback?: string | null;
  certified?: boolean;
  role_models?: {
    planner?: string | null;
    worker?: string | null;
    reviewer?: string | null;
  };
  model_concurrency?: Record<string, number>;
  prompts: GoalPrompt[];
  active_run_ids: string[];
  version: number;
  error: string | null;
  parent_model: string;
}

export interface GoalPrompt {
  step_id: string;
  agent: string;
  title: string;
  task: string;
  model?: string | null;
  status: string;
  run_id?: string | null;
  summary?: string | null;
}

export interface GoalPlan {
  id: string;
  summary: string;
  steps: Array<{
    id: string;
    agent: string;
    title: string;
    task: string;
    model?: string;
    depends_on?: string[];
    parallel_group?: string;
  }>;
  risks: string[];
  validation: string[];
  version: number;
}

/** Core events (server → client). A typed subset of what the core emits. */
export type CoreEvent =
  | ReadyPayload
  | { type: "models"; models: ModelInfo[] }
  /** Terminal event for `refresh_models` (after the `models` re-emit). */
  | {
      type: "models_refreshed";
      count?: number;
      providers?: Record<string, string[]>;
    }
  | { type: "provider_presets"; presets: ProviderPreset[] }
  | { type: "provider_models_preview"; models: ModelInfo[]; base_url: string; error?: string }
  | { type: "authed"; ok: boolean; provider: string }
  | { type: "provider_changed"; provider: string; kind: string; base_url: string; has_key: boolean }
  | { type: "approval_changed"; mode: string } // "destructive" | "always" | "<kind>:always"
  | { type: "delta"; text: string }
  | { type: "thinking"; text: string }
  | { type: "tool_call_start"; id: string; index: number }
  | { type: "tool_call_name"; index: number; name: string }
  | { type: "tool_call_args"; index: number; args: string }
  | { type: "tool_call"; id: string; name: string; args: string }
  | { type: "tool_result"; id: string; ok: boolean; status?: "success" | "denied" | "cancelled" | "timed_out" | "failed" | "stale" | "partially_completed"; output: string; diff?: string; tool?: string }
  /** User-initiated `!cmd` / `!!cmd` (PI-compatible bang bash). */
  | {
      type: "bash_execution";
      command: string;
      output: string;
      ok: boolean;
      exclude_from_context?: boolean;
    }
  | { type: "approval_request"; request_id: string; tool: string; args: string; diff?: string }
  | { type: "approval_expired"; request_id: string; tool_call_id?: string }
  | { type: "run_cancelled"; run_id?: string; reason?: string }
  | { type: "stuck_nudge"; message: string }
  | {
      type: "runtime_status";
      discarded_stale_results?: number;
      resources?: unknown[];
      pending_approvals?: number;
      pending_asks?: number;
      pending_sudos?: number;
    }
  | { type: "session_recovered"; warnings?: string[]; interrupted_runs?: string[] }
  | { type: "summary_required"; attempt?: number; max_attempts?: number }
  | {
      type: "advisor_note";
      scope?: string;
      advisor?: string;
      model?: string;
      severity?: string;
      message?: string;
      finding?: string;
      where?: string;
      action?: string;
      check?: string;
    }
  | {
      type: "advisor_status";
      scope?: string;
      advisor?: string;
      state?: string;
      model?: string;
      reason?: string;
      elapsed_ms?: number;
    }
  | {
      type: "protocol_hello";
      version: string;
      min_client: string;
      capabilities: string[];
    }
  | {
      type: "file_change";
      path: string;
      unified_diff?: string;
      tool: string;
      agent_id?: string;
      run_id?: string;
    }
  | {
      type: "checkpoint_created";
      id: string;
      label: string;
      kind: string;
      auto?: boolean;
      paths?: string[];
    }
  | { type: "checkpoint_restored"; id: string; kind: string }
  | { type: "checkpoints"; checkpoints: CheckpointInfo[] }
  | { type: "worktree_ready"; run_id: string; path: string; branch?: string }
  | { type: "worktree_seeded"; run_id: string; paths: string[] }
  | { type: "worktree_cleaned"; path: string }
  | { type: "worktree_promoted"; run_id: string; paths: string[] }
  | { type: "audit"; tool: string; decision: string; actor: string }
  | ({ type: "cost_update" } & CostUpdate)
  | { type: "goal_step_verdict"; ok: boolean; output: string }
  | {
      type: "goal_step_complete";
      step_id: string;
      title?: string;
      agent?: string;
      ok?: boolean;
      status?: string;
      summary?: string;
      run_id?: string;
    }
  | { type: "goal_completion_summary"; text?: string; summary?: string }
  | {
      type: "goal_iteration";
      id?: string;
      iteration: number;
      max_iterations: number;
      plan_revision?: number;
      max_plan_revisions?: number;
    }
  | {
      type: "goal_review_verdict";
      id?: string;
      ok: boolean;
      summary: string;
      iteration?: number;
      plan_revision?: number;
      evidence_paths?: string[];
    }
  | {
      type: "goal_verify_verdict";
      id?: string;
      ok: boolean;
      summary: string;
      iteration?: number;
      remaining_gaps?: string[];
      evidence_paths?: string[];
    }
  | {
      type: "goal_certified";
      id?: string;
      summary: string;
      iteration?: number;
      certified?: boolean;
    }
  | { type: "search_key_set"; provider: string; has_key: boolean }
  | { type: "plugin_commands"; commands: unknown[] }
  | { type: "plugin_status"; plugin: string; text: string }
  | { type: "plugin_trust_prompt"; plugins: PluginTrustEntry[] }
  | { type: "plugin_trust_applied"; trusted: string[]; denied: string[]; loaded: number }
  | { type: "session_changed"; path: string; new?: boolean }
  | { type: "session_change_failed"; path: string; message: string }
  | { type: "session_deleted"; path: string }
  | { type: "session_pinned"; path: string; pinned: boolean }
  | { type: "ask_request"; request_id: string; questions: AskQuestion[] }
  | { type: "sudo_request"; request_id: string; command: string }
  | { type: "metrics" } & Metrics
  | { type: "umans_conc"; used: number | null; limit: number | null; provider: string }
  | {
      type: "compacted";
      before_tokens: number;
      after_tokens: number;
      summary_chars?: number;
      context_window?: number;
      threshold_tokens?: number;
      hard_limit_tokens?: number;
      within_limit?: boolean;
      scope?: string;
    }
  | {
      type: "compacting";
      before_tokens: number;
      trigger: string;
      context_window?: number;
      threshold_tokens?: number;
      hard_limit_tokens?: number;
      response_reserve_tokens?: number;
      safety_margin_tokens?: number;
      utilization_pct?: number;
    }
  | ({ type: "context_breakdown" } & ContextBreakdown)
  | ({ type: "usage" } & UsageSnapshot)
  | { type: "agents"; agents: AgentInfo[] }
  | { type: "http_retry"; attempt?: number; status?: number; backoff_ms?: number; reason?: string }
  | { type: "sessions"; sessions: SessionEntry[]; files: string[] }
  | { type: "session_status"; sessions: LiveSessionStatus[] }
  | Stats
  | { type: "history"; messages: unknown[]; tokens_in?: number }
  | { type: "done" }
  | { type: "aborted" }
  | { type: "reset" }
  | { type: "error"; message: string }
  | { type: "info"; message: string }
  | { type: "steer"; prompt: string }
  // ── Subagent / intercom ──
  | { type: "intercom_message"; id: string; from: string; message: string; reason?: string; to?: string }
  | { type: "subagent_progress"; run_id: string; agent: string; phase: string; tool: string; tool_count: number; tokens_in: number; tokens_out: number; elapsed_ms: number; ok: boolean }
  | { type: "subagent_start"; run_id: string; mode: string; agent?: string; agents: string[]; task: string; depth: number; started_at: number }
  | { type: "subagent_message"; run_id: string; role: string; content: string }
  | { type: "subagent_tool_call"; run_id: string; call_id: string; name: string; args: Record<string, unknown>; tool_count: number }
  | { type: "subagent_tool_result"; run_id: string; call_id: string; name: string; result: string; ok: boolean }
  | { type: "subagent_done"; run_id: string; state: string; summary?: string; ended_at: number }
  // ── Memory ──
  | { type: "memory_saved"; id?: string; text?: string; deleted?: boolean; message?: string }
  | { type: "memory_list"; entries: MemoryEntry[] }
  // ── Plugins ──
  | { type: "plugins_list"; plugins: PluginEntry[] }
  | { type: "plugin_installed"; name: string; ok?: boolean; message?: string }
  | { type: "plugin_removed"; name: string; ok?: boolean; message?: string }
  | { type: "plugin_enabled"; name: string; ok?: boolean }
  | { type: "plugin_disabled"; name: string; ok?: boolean }
  | { type: "plugin_error"; name?: string; message: string }
  // ── Vision ──
  | { type: "vision_config"; enabled?: boolean; vision_models: string[]; vision_model: string | null }
  // ── Projects / workspace ──
  | { type: "projects"; projects: ProjectEntry[] }
  | { type: "workspace_changed"; workspace: string; projects: ProjectEntry[] }
  | { type: "session_renamed"; name: string; title: string }
  // ── Compaction / config ──
  | {
      type: "digested";
      results: number;
      before_tokens?: number;
      after_tokens?: number;
      trigger?: string;
      context_window?: number;
      threshold_tokens?: number;
      hard_limit_tokens?: number;
      utilization_pct?: number;
      scope?: string;
    }
  | { type: "config_changed"; key: string; value: string | number | boolean }
  // ── OAuth / lifecycle status ──
  | { type: "oauth_prompt"; url: string; code?: string; message?: string }
  | { type: "reflecting"; recurrence: number | string }
  | { type: "work_state"; version: number; goal: string; done: string[]; in_progress: string[]; next: string[]; recent_files: string[]; last_activity: string }
  // ── Goal mode ──
  | ({ type: "goal_state" } & GoalModeState)
  | ({ type: "goal_plan" } & GoalPlan)
  | { type: "goal_phase"; from: string; to: string; message?: string; wave?: number; step_count?: number; done_count?: number }
  // ── Skills ──
  | { type: "skills"; skills: SkillInfo[] }
  | { type: "skill_marketplace_state"; disclaimer_accepted: boolean; installed: InstalledMarketplaceSkill[] }
  | { type: "skill_marketplace_results"; query: string; skills: MarketplaceSkill[] }
  | { type: "skill_marketplace_changed"; action: string; name: string; scope: string }
  | { type: "skill_marketplace_error"; message: string }
  // ── Sandbox (Microsandbox) ──
  // Core wire events emitted by the sandbox subsystem. Not yet in the SDK's
  // CORE_EVENT_TYPES catalog (the SDK lags the core); the reducer.test.ts
  // "gap" set documents them so the coverage test stays green.
  | { type: "sandbox_status"; mode: SandboxMode; report: SandboxPreflightReport }
  | { type: "sandbox_prepare_progress"; phase: string }
  | { type: "sandbox_ready"; ready: boolean; report?: SandboxPreflightReport }
  | { type: "job_cancel_result"; run_id: string; artifact: unknown }
  | { type: "subagent_delivery"; run_id: string; parent_run_id?: string | null; state: string }
  | { type: "sandbox_error"; error: string }
  | { type: "job_list"; runs: Array<{ run_id: string; parent_run_id?: string | null; state: string; summary?: string | null }> }
  | { type: "job_status" | "job_wait_result" | "job_cancel_requested" | "job_wait_timeout"; run_id: string; parent_run_id?: string | null; state?: string; summary?: string | null }
  | { type: "session_tree"; tree: SessionTreeSnapshot }
  | { type: "session_branch"; entry_id: string; parent_id?: string };

export interface SessionTreeEntry {
  id: string;
  parent_id?: string | null;
  title?: string;
  summary?: string;
  branch?: string;
  created_at?: number;
}

export interface SessionTreeSnapshot {
  leaf?: string | null;
  ancestry?: string[];
  siblings?: string[];
  entries: SessionTreeEntry[];
}

export interface ProcessStatusView {
  name: string;
  pid: number;
  argv: string[];
  cwd: string;
  started_at_ms: number;
  state: "starting" | "ready" | "exited" | string;
}

export interface ProcessLogsView { name: string; text: string; truncated?: boolean }

/** Core commands (client → server → core stdin). A typed subset. */
export type CoreCommand =
  | { type: "send"; prompt: string; model: string; reasoning_effort?: string; images?: string[] }
  | { type: "steer"; prompt: string; model: string; reasoning_effort?: string }
  | { type: "abort" }
  | { type: "clear_queue" }
  | { type: "user_bash"; command: string; exclude_from_context?: boolean }
  | { type: "reset" }
  | { type: "clear" }
  | { type: "compact"; instructions?: string }
  | { type: "context" }
  | { type: "usage"; model?: string }
  | {
      type: "approve";
      request_id: string;
      decision: ApproveDecision;
      pattern?: string;
    }
  | { type: "set_approval"; mode: "never" | "destructive" | "always" }
  | { type: "create_checkpoint"; label?: string; paths?: string[] }
  | { type: "list_checkpoints" }
  | { type: "restore_checkpoint"; id: string }
  | { type: "pin_session"; path: string; pinned: boolean }
  | { type: "set_key"; api_key: string; provider?: string }
  | { type: "set_search_key"; provider: string; api_key: string }
  | { type: "set_provider"; name: string }
  | { type: "list_provider_presets" }
  | { type: "refresh_models" }
  | { type: "login"; preset: string; api_key?: string }
  | {
      type: "add_custom_provider";
      name: string;
      base_url: string;
      kind?: string;
      api_key?: string;
      api_key_env?: string;
      headers?: Record<string, string>;
      context_window?: number;
      models_override?: ModelOverride[];
    }
  | {
      type: "discover_provider_models";
      base_url: string;
      kind?: string;
      api_key?: string;
      headers?: Record<string, string>;
    }
  | { type: "login_oauth"; preset: string }
  | { type: "logout"; provider: string }
  | { type: "oauth_code"; code: string }
  | { type: "list_sessions" }
  | { type: "load_session"; path: string }
  | { type: "new_session"; path?: string }
  | { type: "stats" }
  | { type: "runtime_status" }
  | { type: "set_config"; key: string; value: string | number | boolean }
  // ── Turn / history ──
  | { type: "undo" }
  // ── Subagent / intercom ──
  | { type: "intercom_reply"; request_id: string; reply: string }
  | { type: "job_list" }
  | { type: "job_status" | "job_cancel" | "job_wait"; run_id: string; timeout_ms?: number }
  | { type: "session_tree" }
  | { type: "session_branch"; entry_id: string }
  // ── Ask tool ──
  | { type: "ask_reply"; request_id: string; answers: Record<string, string> | null }
  // ── Sudo passthrough (bash command invokes sudo) ──
  | { type: "sudo_reply"; request_id: string; approved: boolean; password?: string }
  // ── Memory ──
  | { type: "save_memory"; text: string; tags?: string[]; scope?: "workspace" | "global" }
  | { type: "list_memory" }
  | { type: "forget_memory"; id: string }
  | { type: "refresh_memory" }
  // ── Plugins ──
  | { type: "install_plugin"; path: string; scope?: "workspace" | "global" }
  | { type: "remove_plugin"; name: string }
  | { type: "enable_plugin"; name: string }
  | { type: "disable_plugin"; name: string }
  | { type: "list_plugins" }
  | { type: "plugin_trust_prompt" }
  | { type: "plugin_trust_decisions"; decisions: Record<string, "trust" | "deny"> }
  | { type: "list_agents" }
  // ── Vision ──
  | { type: "get_vision_config" }
  | {
      type: "set_vision_config";
      enabled?: boolean;
      vision_models?: string[];
      vision_model?: string | null;
    }
  // ── Projects / workspace ──
  | { type: "switch_workspace"; path: string }
  | { type: "rename_session"; name: string; title: string }
  | { type: "list_projects" }
  | { type: "add_project"; path: string }
  | { type: "remove_project"; path: string }
  // ── Session lifecycle ──
  | { type: "delete_session"; path: string }
  // ── Skills ──
  | { type: "list_skills" }
  | { type: "list_marketplace_skills" }
  | { type: "accept_skill_disclaimer" }
  | { type: "search_marketplace_skills"; query: string }
  | { type: "install_marketplace_skill"; source: string; name: string; scope: "project" | "global" }
  | { type: "update_marketplace_skill"; name: string; scope: "project" | "global" }
  | { type: "remove_marketplace_skill"; name: string; scope: "project" | "global" }
  | { type: "apply_skill"; name: string; task?: string; model: string; reasoning_effort?: string }
  // ── Goal mode ──
  | {
      type: "start_goal";
      goal: string;
      concurrency?: number;
      max_tasks?: number;
      allowed_models?: string[];
      allowed_providers?: string[];
      auto_deploy?: boolean;
      ceo_mode?: boolean;
      max_iterations?: number;
      max_plan_revisions?: number;
      planner_model?: string;
      worker_model?: string;
      reviewer_model?: string;
      model_concurrency?: Record<string, number>;
      model: string;
      reasoning_effort?: string;
    }
  | { type: "cancel_goal" }
  | { type: "goal_status" }
  | { type: "approve_goal_plan" }
  | { type: "revise_goal"; feedback: string; model: string; reasoning_effort?: string }
  // ── Sandbox (Microsandbox) ──
  | { type: "get_sandbox_status" }
  | { type: "prepare_sandbox" }
  | { type: "reset_sandbox" };

/** Synthetic events produced by the bridge/client (not from the core). */
export type SyntheticEvent =
  // A user message was sent (added optimistically by the client; tracked by the
  // bridge for snapshot hydration). `model` records the model used for the turn.
  | { type: "_user"; text: string; model?: string; steer?: boolean; images?: string[] }
  // The selected model / thinking level changed in the UI.
  | { type: "_select_model"; id: string }
  | { type: "_set_thinking"; level: string }
  // A toast was dismissed by the UI.
  | { type: "_dismiss_toast"; id: string }
  // Optimistic: the bridge is (re)spawning the core after a workspace switch.
  | { type: "_set_switching"; switching: boolean }
  // A custom session title was set/removed (web-layer rename overlay).
  | { type: "_session_title"; name: string; title: string }
  /** Client-side undo: drop the last turn locally; next `reset` keeps messages. */
  | { type: "_undo_local" }
  | { type: "_goal_approve_optimistic" }
  | { type: "_clear_provider_models_preview" }
  | { type: "_set_provider_models_preview_error"; error: string | null }
  /** Optimistic: an on-demand `refresh_models` is in flight. */
  | { type: "_set_models_refreshing"; refreshing: boolean }
  // Cross-session notification feed (client-only; derived in useAgent from
  // LiveSessionStatus transitions, never dispatched server-side).
  | { type: "_add_notifications"; items: NotificationItem[] }
  | { type: "_dismiss_notification"; id: string }
  | { type: "_mark_notifications_read" }
  | { type: "_clear_notifications" };

export type AgentEvent = CoreEvent | SyntheticEvent;

// ─── UI message model ───────────────────────────────────────────────────────

export interface ToolResult {
  ok: boolean;
  output: string;
  diff?: string;
  /** True when the result was reconstructed from session history (no live
   *  ok/error known). Renders a neutral badge instead of green "ok". */
  unknown?: boolean;
}

export interface UIToolCall {
  id: string;
  name: string;
  args: Record<string, unknown>;
  argString: string;
  /** Present once the tool_result event arrives. */
  result?: ToolResult;
}

export interface UserMsg {
  id: string;
  role: "user";
  text: string;
  ts: number;
  /** True when this message was a steer (redirect of an in-flight turn),
   *  not a fresh prompt. Drives a visual "steering" badge. */
  steer?: boolean;
  /** Images attached to this message (data URLs). */
  images?: string[];
}

export interface AssistantMsg {
  id: string;
  role: "assistant";
  text: string;
  thinking: string;
  toolCalls: UIToolCall[];
  model?: string;
  /** True while this assistant message is still receiving deltas. */
  streaming: boolean;
  usage?: Metrics;
  ts: number;
}

export interface ToolMsg {
  id: string;
  role: "tool";
  toolCallId: string;
  toolName: string;
  output: string;
  ok: boolean;
  diff?: string;
  ts: number;
}

/** User-initiated bang bash (`!` / `!!`) shown in the transcript. */
export interface BashMsg {
  id: string;
  role: "bash";
  command: string;
  output: string;
  ok: boolean;
  /** True for `!!cmd` — output was not added to model context. */
  excludeFromContext?: boolean;
  ts: number;
}

/** Lasting goal-progress card in the transcript (not toast-only). */
export interface GoalMsg {
  id: string;
  role: "goal";
  kind: "step_complete" | "phase" | "completion_summary" | "verdict";
  title: string;
  text: string;
  ok?: boolean;
  stepId?: string;
  status?: string;
  agent?: string;
  runId?: string;
  ts: number;
}

/** Durable watchdog lifecycle/finding card in the transcript. */
export interface AdvisorMsg {
  id: string;
  role: "advisor";
  scope: string;
  advisor: string;
  model: string;
  state: string;
  severity?: string;
  text?: string;
  elapsedMs?: number;
  ts: number;
}

export type UIMessage = UserMsg | AssistantMsg | ToolMsg | BashMsg | GoalMsg | AdvisorMsg;

export interface Toast {
  id: string;
  kind: "info" | "error" | "success" | "warning";
  message: string;
}

/** A workspace file entry for the @-mention flyout. */
export interface FileEntry {
  /** Path relative to the workspace root. */
  path: string;
  /** Just the filename. */
  name: string;
  /** True if this is a directory. */
  dir: boolean;
}

// ─── Agent state (the reducer output) ───────────────────────────────────────

export interface ProjectEntry {
  /** Absolute workspace path. */
  path: string;
  /** Display name (basename). */
  name: string;
  /** Last-accessed timestamp (ms). */
  lastUsed: number;
}

export interface AgentState {
  ready: ReadyPayload | null;
  /** Live sandbox (Microsandbox) runtime status. Synthesized from the `ready`
   *  payload + sandbox_* events; `ready.sandboxReady` is the source of truth
   *  for whether agent commands may run. */
  sandbox: SandboxRuntimeStatus;
  models: ModelInfo[];
  /** True when the ACTIVE provider has a usable key (footer/status). */
  authed: boolean | null;
  /** True when ANY configured provider is logged in (multi-provider sends). */
  anyLoggedIn: boolean | null;
  provider: string;
  providerKind: string;
  approvalMode: string;
  escalatedKinds: string[];
  workspace: string;
  /** Known workspace projects (for the project picker). */
  projects: ProjectEntry[];
  /** First-party provider presets (Umans, OpenCode Go, OpenRouter) from the
   * core, used by the /login + /logout pickers. */
  providerPresets: ProviderPreset[];
  /** Models discovered by `discover_provider_models` (a preview for the
   * add-custom-provider flow); null when no preview is active. */
  providerModelsPreview: ModelInfo[] | null;
  /** Error from the latest discover_provider_models preview (empty-list or
   * hard failure). Cleared on a successful non-empty preview or on modal close. */
  providerModelsPreviewError: string | null;
  selectedModel: string | null;
  /** True while an on-demand `refresh_models` is in flight (optimistic UI). */
  modelsRefreshing: boolean;
  thinkingLevel: string;
  messages: UIMessage[];
  currentAssistantId: string | null;
  streaming: boolean;
  retrying: boolean;
  pendingApproval: ApprovalRequest | null;
  pendingAsk: AskPrompt | null;
  pendingSudo: SudoPrompt | null;
  metrics: Metrics | null;
  /** Live Umans concurrency (used/limit) from the `/v1/usage` poll; null when
   *  not Umans / fetch failed. Drives a footer field ahead of tps. */
  umansConc: UmansConc | null;
  sessions: SessionEntry[];
  currentSessionFile: string | null;
  /** Live status of every currently-live session (bridge-synthesized,
   *  cross-session). Keyed by absolute session-file path. Drives sidebar live
   *  badges + the notification feed (via transitions detected in useAgent). */
  liveSessions: Record<string, LiveSessionStatus>;
  /** Cross-session notification feed (background session finished / needs
   *  attention). Client-only; derived from liveSessions transitions. */
  notifications: NotificationItem[];
  stats: Stats | null;
  toasts: Toast[];
  memories: MemoryEntry[];
  plugins: PluginEntry[];
  skills: SkillInfo[];
  marketplaceDisclaimerAccepted: boolean;
  marketplaceInstalled: InstalledMarketplaceSkill[];
  marketplaceResults: MarketplaceSkill[];
  /** Discoverable subagents from core `agents` events. */
  availableAgents: AgentInfo[];
  /** Intercom transcript retained for the subagent panel. */
  intercomLog: IntercomEntry[];
  /** Project plugin trust decisions awaiting the user. */
  pendingPluginTrust: PluginTrustEntry[] | null;
  pendingIntercom: IntercomPrompt | null;
  pendingOauth: OauthPrompt | null;
  /** Live subagent runs keyed by run_id — the SubagentsPanel list + drill-in chat. */
  subagentRuns: Record<string, SubagentRunView>;
  /** Durable job/process tree snapshots from job_list/status events. */
  jobTree: Record<string, { runId: string; parentRunId?: string | null; state: string; summary?: string | null }>;
  sessionTree: unknown | null;
  /** Named-process snapshots decoded from ordinary `process` tool results. */
  processes: Record<string, ProcessStatusView>;
  processLogs: Record<string, ProcessLogsView>;
  visionConfig: VisionConfig | null;
  /** Last `/context` breakdown for the Diagnostics panel. */
  contextBreakdown: ContextBreakdown | null;
  /** Last `/usage` snapshot for the Diagnostics panel. */
  usageSnapshot: UsageSnapshot | null;
  /** Rolling work-state summary (goal/done/doing/next/recent files) from
   *  `work_state` events — drives the ambient status panel. */
  workState: WorkState | null;
  /** Active goal mode (plan → deploy). Null when idle. */
  goalMode: GoalModeState | null;
  /** Last structured plan from `goal_plan` (for plan-ready review). */
  goalPlan: GoalPlan | null;
  /** CEO / Control Center iteration history (verify→replan cycles). */
  goalIterations: GoalIterationRecord[];
  /** Protocol capabilities handshake from `protocol_hello`. */
  protocolHello: ProtocolHello | null;
  /** Latest `cost_update` totals (session-scoped estimate). */
  cost: CostUpdate | null;
  /** Known hybrid checkpoints from `checkpoints` / create events. */
  checkpoints: CheckpointInfo[];
  /** Monotonic counter bumped on every `file_change` (IDE refresh signal). */
  fileChangeSeq: number;
  /** Recent agent file mutations (newest first, capped). */
  recentFileChanges: FileChangeRecord[];
  /** Active parallel-subagent worktrees. */
  worktrees: WorktreeInfo[];
  /** Slash commands contributed by plugins. */
  pluginCommands: unknown[];
  /** Search-provider API key presence (`provider` → has_key). */
  searchKeys: Record<string, boolean>;
  /** Last goal wave verifier result. */
  lastGoalVerdict: { ok: boolean; output: string } | null;
  /** Per-step finals keyed by step_id — durable for GoalProgressPanel. */
  goalStepFinals: Record<
    string,
    {
      stepId: string;
      title: string;
      agent: string;
      ok: boolean;
      status: string;
      summary: string;
      runId?: string;
      ts: number;
    }
  >;
  /** True while the bridge is (re)spawning the core after a workspace switch. */
  switching: boolean;
  /** True when the core has a one-deep follow-up/steer queued behind the live turn. */
  followUpQueued: boolean;
  /** After `_undo_local`, the next core `reset` must not wipe the trimmed transcript. */
  pendingUndo: boolean;
}

/** Sent to a freshly-connected client to hydrate the full current state. */
export interface SnapshotEvent {
  type: "_snapshot";
  state: AgentState;
}

export type ServerToClient = CoreEvent | SnapshotEvent;

// ─── IDE panel types (client-only + API DTOs) ──────────────────────────────
// Per docs/IDE_PANELS_CONTRACT.md §2. Client-only layout state + API DTOs;
// NEVER reduced into AgentState / never sent over SSE.

/** A panel the IDE shell can show. "copilot" is handled separately (the dock). */
export type IdePanelId = "explorer" | "git" | "terminal" | "preview" | "screen";

/** Panels that can be moved between IDE dock zones. */
export type MovablePanelId = "chat" | "git" | "terminal" | "preview" | "screen";

/** A drop target around (or in place of) the fixed editor work area. */
export type DockPosition = "left" | "right" | "bottom" | "main";

/** One entry in the file-explorer tree (one level of a directory). */
export interface FileNode {
  /** Workspace-relative path with forward slashes (e.g. "src/lib/foo.ts"). */
  path: string;
  /** Just the basename. */
  name: string;
  /** True if this is a directory. */
  dir: boolean;
  /** File size in bytes (0 for dirs). */
  size?: number;
  /** mtime in ms (for change detection / refresh). */
  mtime?: number;
  /** True if the entry is a symlink (rendered with an arrow). */
  symlink?: boolean;
}

/** One row of `git status --porcelain=v2`. */
export interface GitStatusEntry {
  /** Workspace-relative path. For renames: "old -> new". */
  path: string;
  /** Original path for renames, else null. */
  oldPath?: string | null;
  /** XY status codes from porcelain v2 (e.g. "M ", " M", "A ", "??", "R "). */
  xy: string;
  /** Human label. */
  status: "modified" | "added" | "deleted" | "renamed" | "untracked" | "conflicted";
  /** Staged (index) vs unstaged (worktree). */
  staged: boolean;
}

/** In-progress Git operations the panel can continue or abort. */
export type GitOperation = "merge" | "rebase" | "cherry-pick" | "revert";

/** Aggregate git state for the git panel + status bar. */
export interface GitStatus {
  /** Current branch name, or "HEAD (detached)". */
  branch: string;
  /** Commits ahead of upstream (0 if no upstream). */
  ahead: number;
  /** Commits behind upstream. */
  behind: number;
  /** All changed entries (staged + unstaged + untracked). */
  entries: GitStatusEntry[];
  /** HEAD commit short oid, or null if no commits. */
  head: { oid: string; message: string; author: string; ts: number } | null;
  /** True if the workspace is not a git repo (panel shows "initialize" CTA). */
  bare: boolean;
  /** Configured upstream for the current branch, if any. */
  upstream?: string | null;
  /** Local and remote branches. */
  branches?: GitBranch[];
  /** Recent commits across all refs. */
  commits?: GitCommit[];
  /** Saved worktree snapshots. */
  stashes?: GitStash[];
  /** Repository tags. */
  tags?: GitTag[];
  /** Configured remotes and their fetch/push URLs. */
  remotes?: GitRemote[];
  /** Active merge/rebase/cherry-pick/revert operations (if any). */
  operations?: GitOperation[];
}

export interface GitBranch {
  name: string;
  oid: string;
  current: boolean;
  remote: boolean;
  upstream: string | null;
  ahead: number;
  behind: number;
}

export interface GitCommit {
  oid: string;
  shortOid: string;
  parents: string[];
  subject: string;
  author: string;
  email: string;
  ts: number;
  refs: string[];
}

export interface GitStash {
  ref: string;
  oid: string;
  subject: string;
  ts: number;
}

export interface GitTag {
  name: string;
  oid: string;
  subject: string;
}

export interface GitRemote {
  name: string;
  fetchUrl: string;
  pushUrl: string;
}

/** A live, persistent PTY session rendered by Ghostty in the browser.
 *  Session metadata is persisted per project; the server PTY survives refresh
 *  until the user closes the tab or the web process restarts. */
export interface TerminalSession {
  /** Client-generated id (e.g. "term_<ts>_<n>"). */
  id: string;
  /** Display title (defaults to shell name; user-renamable). */
  title: string;
  /** Workspace-relative or absolute cwd the shell started in. */
  cwd: string;
  /** True while the shell process is alive. */
  alive: boolean;
  /** Last exit code (null while alive / not yet exited). */
  exitCode: number | null;
}

/** Preview panel state. */
export interface PreviewState {
  /** What is being previewed. */
  kind: "file" | "url" | "none";
  /** Workspace-relative file path (kind="file") or absolute URL (kind="url"). */
  target: string;
  /** Optional query/anchor to append. */
  query?: string;
}

/** A tab in the main work area (open file / preview / terminal-host / diff). */
export interface IdeTab {
  /** Unique id (path for files, "preview:<target>", "diff:staged|working:<path>", "patch:…"). */
  id: string;
  kind: "file" | "preview" | "terminal" | "diff" | "patch";
  /** Workspace-relative path (file/diff) or target (preview) or terminal id / commit oid. */
  target: string;
  /** Display label (basename for files). */
  label: string;
  /** Dirty flag (unsaved editor changes). */
  dirty: boolean;
  /** Detected language id for the editor (e.g. "typescript", "markdown"). */
  language?: string;
  /** Diff/patch metadata. */
  diffMeta?: {
    /** Staged (index) vs worktree for file diffs. */
    staged?: boolean;
    /** Commit oid or stash ref when kind is "patch". */
    source?: "commit" | "stash" | "file";
  };
}

/** Client-only IDE layout state. NEVER sent over SSE / never in AgentState. */
export interface IdeLayoutState {
  /** Which panel's sidebar is shown in PrimarySidebar. */
  activePanel: IdePanelId;
  /** Open tabs in the main work area (ordered). */
  openTabs: IdeTab[];
  /** id of the active tab (null = none). */
  activeTabId: string | null;
  /** PrimarySidebar width in px. */
  sidebarWidth: number;
  /** True when PrimarySidebar is collapsed (hidden). */
  sidebarCollapsed: boolean;
  /** Bottom panel height in px (0 = collapsed). */
  bottomPanelHeight: number;
  /** True when the bottom panel is visible. */
  bottomPanelVisible: boolean;
  /** True when the copilot (Chat) dock is visible. */
  copilotVisible: boolean;
  /** Copilot dock width in px. */
  copilotWidth: number;
  /** Current dock position for every movable panel. */
  panelLocations: Record<MovablePanelId, DockPosition>;
  /** Panels remain mounted only while visible. */
  panelVisibility: Record<MovablePanelId, boolean>;
  /** Selected panel when several panels share a dock zone. */
  activeDockPanels: Record<DockPosition, MovablePanelId | null>;
  /** Shared width of the optional dock on the editor's left edge. */
  leftDockWidth: number;
  /** Live terminal sessions. */
  terminals: TerminalSession[];
  /** Active terminal session id (null = none). */
  activeTerminalId: string | null;
  /** Last-known git status (null until first refresh). */
  gitStatus: GitStatus | null;
  /** Current preview target. */
  preview: PreviewState;
  /** File-tree expanded directory paths (set, persisted across reloads). */
  expandedDirs: string[];
  /** Shell chrome mode. Distinct from ephemeral focusMode (editor zen). */
  uiMode: "ide" | "chat";
}
