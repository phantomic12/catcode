// The agent state reducer.
//
// Shared by the server bridge (reduces the full event stream to maintain a
// snapshot for reconnecting clients) and the browser (reduces live SSE events).
// One reducer, one source of truth for the message-assembly rules — mirrors the
// SDK's AgentSession logic: a new assistant message is begun lazily on the first
// delta/thinking; a tool_call finalizes the current assistant (clearing it) so
// the next delta starts a fresh assistant message (multi-step agent turns);
// tool_result matches its tool call by id.

import type { CoreEventType } from "@catalyst-code/coding-agent";
import type {
  AgentEvent,
  AgentState,
  AssistantMsg,
  BashMsg,
  GoalMsg,
  GoalPrompt,
  IntercomEntry,
  LiveSessionStatus,
  NotificationItem,
  ReadyPayload,
  SandboxMode,
  SandboxNetworkMode,
  SandboxPreflightReport,
  SandboxRuntimeStatus,
  SubagentChatItem,
  SubagentRunView,
  Toast,
  UIMessage,
  UIToolCall,
} from "./types";

export const initialState: AgentState = {
  ready: null,
  sandbox: {
    mode: "none",
    ready: false,
    image: null,
    cpus: null,
    memoryMb: null,
    networkMode: null,
    shell: null,
    report: null,
    preparePhase: null,
    error: null,
  },
  models: [],
  authed: null,
  anyLoggedIn: null,
  provider: "",
  providerKind: "",
  approvalMode: "destructive",
  escalatedKinds: [],
  workspace: "",
  projects: [],
  providerPresets: [],
  providerModelsPreview: null,
  providerModelsPreviewError: null,
  selectedModel: null,
  modelsRefreshing: false,
  thinkingLevel: "medium",
  messages: [],
  currentAssistantId: null,
  streaming: false,
  retrying: false,
  pendingApproval: null,
  pendingAsk: null,
  pendingSudo: null,
  metrics: null,
  umansConc: null,
  sessions: [],
  currentSessionFile: null,
  liveSessions: {},
  notifications: [],
  stats: null,
  toasts: [],
  memories: [],
  plugins: [],
  skills: [],
  marketplaceDisclaimerAccepted: false,
  marketplaceInstalled: [],
  marketplaceResults: [],
  availableAgents: [],
  pendingIntercom: null,
  pendingOauth: null,
  intercomLog: [],
  subagentRuns: {},
  visionConfig: null,
  contextBreakdown: null,
  usageSnapshot: null,
  workState: null,
  goalMode: null,
  goalPlan: null,
  goalIterations: [],
  protocolHello: null,
  cost: null,
  checkpoints: [],
  fileChangeSeq: 0,
  recentFileChanges: [],
  worktrees: [],
  pluginCommands: [],
  searchKeys: {},
  lastGoalVerdict: null,
  goalStepFinals: {},
  switching: false,
  followUpQueued: false,
  pendingUndo: false,
};

let counter = 0;
function newId(prefix: string): string {
  counter += 1;
  return `${prefix}_${Date.now().toString(36)}_${counter.toString(36)}`;
}

function pushToast(toasts: Toast[], kind: Toast["kind"], message: string): Toast[] {
  const next = [...toasts, { id: newId("t"), kind, message }];
  // ponytail: keep the last 6 toasts so the stack can't grow unbounded.
  return next.length > 6 ? next.slice(next.length - 6) : next;
}

/** Get-or-create a subagent run view for a run_id, then apply a mutation. Used
 *  by every subagent_* event so ordering gaps (a progress arriving before its
 *  start) never crash — a missing run is created as a running stub. */
function upsertRun(
  state: AgentState,
  runId: string,
  fn: (r: SubagentRunView) => SubagentRunView,
): AgentState {
  const prev: SubagentRunView =
    state.subagentRuns[runId] ?? {
      id: runId,
      mode: "",
      agents: [],
      task: "",
      state: "running",
      depth: 0,
      startedAt: 0,
      toolCount: 0,
      tokensIn: 0,
      tokensOut: 0,
      elapsedMs: 0,
      items: [],
    };
  return {
    ...state,
    subagentRuns: { ...state.subagentRuns, [runId]: fn(prev) },
  };
}

/** Cap on retained COMPLETED/FAILED subagent runs. The core prunes its own
 *  terminal runs (core/src/subagent.rs::prune_terminal_runs, MAX=64); the web
 *  reducer mirrors this so a long session with many delegations doesn't grow
 *  `subagentRuns` (and each run's full transcript) without bound. Running runs
 *  are always kept; only the oldest terminal runs beyond this cap are dropped. */
export const MAX_TERMINAL_RUNS = 64;

/** Drop the oldest terminal runs so `subagentRuns` can't grow unbounded over a
 *  long session. Running runs are always retained. No-op when under the cap. */
function pruneTerminalRuns(state: AgentState): AgentState {
  const runs = Object.values(state.subagentRuns);
  const terminal = runs.filter((r) => r.state !== "running");
  if (terminal.length <= MAX_TERMINAL_RUNS) return state;
  // Keep every running run + the most-recently-ended MAX_TERMINAL_RUNS.
  const keep = new Set<string>(runs.filter((r) => r.state === "running").map((r) => r.id));
  terminal
    .sort((a, b) => (b.endedAt ?? b.startedAt ?? 0) - (a.endedAt ?? a.startedAt ?? 0))
    .slice(0, MAX_TERMINAL_RUNS)
    .forEach((r) => keep.add(r.id));
  const next: Record<string, SubagentRunView> = {};
  for (const id of keep) next[id] = state.subagentRuns[id];
  return { ...state, subagentRuns: next };
}

/** Begin a new assistant message if none is in flight. */
function beginAssistant(state: AgentState, model?: string): AgentState {
  if (state.currentAssistantId) return state;
  const id = newId("a");
  const msg: AssistantMsg = {
    id,
    role: "assistant",
    text: "",
    thinking: "",
    toolCalls: [],
    model: model ?? state.selectedModel ?? undefined,
    streaming: true,
    ts: Date.now(),
  };
  return {
    ...state,
    messages: [...state.messages, msg],
    currentAssistantId: id,
    streaming: true,
  };
}

function updateCurrentAssistant(
  state: AgentState,
  fn: (m: AssistantMsg) => AssistantMsg,
): AgentState {
  const id = state.currentAssistantId;
  if (!id) return state;
  return {
    ...state,
    messages: state.messages.map((m) => (m.id === id && m.role === "assistant" ? fn(m) : m)),
  };
}

function finalizeCurrentAssistant(state: AgentState): AgentState {
  const id = state.currentAssistantId;
  if (!id) return state;
  return {
    ...state,
    messages: state.messages.map((m) =>
      m.id === id && m.role === "assistant" ? { ...m, streaming: false } : m,
    ),
    currentAssistantId: null,
  };
}

function finishTurn(state: AgentState): AgentState {
  return {
    ...finalizeCurrentAssistant(state),
    streaming: false,
    retrying: false,
    followUpQueued: false,
    // Abort/done must drop blocking gates — otherwise a cancelled turn leaves a
    // dead approval/ask/sudo/intercom banner that can no longer be answered.
    pendingApproval: null,
    pendingAsk: null,
    pendingSudo: null,
    pendingIntercom: null,
  };
}

/** Goal deployment outlives the planning model turn. During that hand-off the
 * core emits the planning turn's `done`, but the session is still doing live
 * work. Keep the global working/streaming state armed until the goal reaches a
 * terminal phase. `plan_ready` only counts when auto-deploy is enabled; manual
 * review must return the composer to idle while it waits for the user. */
function goalKeepsStreaming(goal: AgentState["goalMode"]): boolean {
  if (!goal) return false;
  return (
    goal.phase === "planning" ||
    goal.phase === "reviewing" ||
    goal.phase === "deploying" ||
    goal.phase === "running" ||
    goal.phase === "synthesizing" ||
    goal.phase === "verifying" ||
    goal.phase === "replanning" ||
    (goal.phase === "plan_ready" && goal.auto_deploy)
  );
}

const GOAL_TERMINAL_STATUSES = new Set(["done", "failed", "skipped"]);
const GOAL_LASTING_PHASES = new Set([
  "planning",
  "reviewing",
  "deploying",
  "running",
  "synthesizing",
  "verifying",
  "replanning",
  "done",
  "failed",
  "cancelled",
]);

function isGoalTerminalStatus(status: string): boolean {
  return GOAL_TERMINAL_STATUSES.has(String(status ?? "").toLowerCase());
}

function truncateGoalSummary(text: string, max = 800): string {
  const t = String(text ?? "").trim();
  if (!t) return "(step finished with no written summary)";
  return t.length > max ? `${t.slice(0, max - 1)}…` : t;
}

function parseGoalVerdict(raw: unknown): import("./types").GoalVerdict | null {
  if (!raw || typeof raw !== "object") return null;
  const v = raw as Record<string, unknown>;
  return {
    ok: !!v.ok,
    summary: String(v.summary ?? ""),
    evidence_paths: Array.isArray(v.evidence_paths)
      ? v.evidence_paths.map((p) => String(p))
      : undefined,
    at: typeof v.at === "number" ? v.at : undefined,
  };
}

function upsertGoalIteration(
  list: AgentState["goalIterations"],
  iteration: number,
  patch: Partial<AgentState["goalIterations"][number]>,
): AgentState["goalIterations"] {
  const next = [...(list ?? [])];
  const idx = next.findIndex((r) => r.iteration === iteration);
  if (idx < 0) {
    next.push({ iteration, ...patch });
    next.sort((a, b) => a.iteration - b.iteration);
    return next;
  }
  next[idx] = { ...next[idx], ...patch };
  return next;
}

function storeGoalStepFinal(
  state: AgentState,
  final: AgentState["goalStepFinals"][string],
): AgentState {
  return {
    ...state,
    goalStepFinals: { ...(state.goalStepFinals ?? {}), [final.stepId]: final },
  };
}

function appendGoalMsg(
  state: AgentState,
  partial: Omit<GoalMsg, "id" | "role" | "ts">,
): AgentState {
  const msg: GoalMsg = {
    id: newId("g"),
    role: "goal",
    ts: Date.now(),
    ...partial,
  };
  return { ...state, messages: [...state.messages, msg] };
}

/** Find existing lasting step card (if any). */
function findGoalStepCard(
  messages: UIMessage[],
  stepId: string,
): GoalMsg | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role === "goal" && m.kind === "step_complete" && m.stepId === stepId) {
      return m;
    }
  }
  return undefined;
}

/** Record a lasting step-complete card + durable final store (deduped by step_id).
 *  On verifier remap (done→failed), append an update card when status/ok changed (H1). */
function ingestGoalStepFinal(
  state: AgentState,
  opts: {
    stepId: string;
    title?: string;
    agent?: string;
    ok?: boolean;
    status?: string;
    summary?: string;
    runId?: string;
  },
): AgentState {
  const stepId = String(opts.stepId ?? "").trim();
  if (!stepId) return state;
  const status = String(opts.status ?? (opts.ok === false ? "failed" : "done")).toLowerCase();
  const summary = truncateGoalSummary(opts.summary ?? "");
  const title = String(opts.title ?? stepId).trim() || stepId;
  const agent = String(opts.agent ?? "").trim();
  const ok =
    opts.ok !== undefined
      ? !!opts.ok
      : !(status === "failed" || status === "skipped");
  const runId = opts.runId ? String(opts.runId) : undefined;
  let next = storeGoalStepFinal(state, {
    stepId,
    title,
    agent,
    ok,
    status,
    summary,
    runId,
    ts: Date.now(),
  });
  const existing = findGoalStepCard(next.messages, stepId);
  const badge = status === "failed" ? "failed" : status === "skipped" ? "skipped" : "done";
  if (existing) {
    const prevStatus = String(existing.status ?? "").toLowerCase();
    const prevOk = existing.ok;
    // Verifier remap / status change: append an update card (TUI fingerprint parity).
    if (prevStatus === badge && prevOk === ok) return next;
    return appendGoalMsg(next, {
      kind: "step_complete",
      title: agent ? `[${agent}] ${title}` : title,
      text: summary,
      ok,
      stepId,
      status: badge,
      agent: agent || undefined,
      runId: runId ?? existing.runId,
    });
  }
  return appendGoalMsg(next, {
    kind: "step_complete",
    title: agent ? `[${agent}] ${title}` : title,
    text: summary,
    ok,
    stepId,
    status: badge,
    agent: agent || undefined,
    runId,
  });
}

/** Backfill step finals from goal_state.prompts when discrete events are absent. */
function ingestPromptFinalsFromGoalState(
  state: AgentState,
  prompts: GoalPrompt[],
): AgentState {
  let next = state;
  for (const p of prompts) {
    if (!isGoalTerminalStatus(p.status ?? "")) continue;
    next = ingestGoalStepFinal(next, {
      stepId: p.step_id,
      title: p.title,
      agent: p.agent,
      ok: !["failed", "skipped"].includes(String(p.status).toLowerCase()),
      status: p.status,
      summary: p.summary ?? undefined,
      runId: p.run_id ?? undefined,
    });
  }
  return next;
}

/** Drop the last user message and everything after it (assistant/tool/bash). */
function dropLastTurn(messages: UIMessage[]): UIMessage[] {
  let lastUser = -1;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].role === "user") {
      lastUser = i;
      break;
    }
  }
  if (lastUser < 0) return messages;
  return messages.slice(0, lastUser);
}

/** True when the current turn has produced assistant/tool/bash activity (or a
 *  blocking gate). Used so a pre-turn `error` (bad skill/model) can clear the
 *  optimistic `streaming` flag without killing a live mid-turn stream. */
function turnHasStarted(state: AgentState): boolean {
  if (state.currentAssistantId) return true;
  if (state.pendingApproval || state.pendingAsk || state.pendingSudo) return true;
  if (state.followUpQueued) return true;
  let lastUser = -1;
  for (let i = state.messages.length - 1; i >= 0; i--) {
    if (state.messages[i].role === "user") {
      lastUser = i;
      break;
    }
  }
  for (let i = lastUser + 1; i < state.messages.length; i++) {
    const role = state.messages[i].role;
    if (role === "assistant" || role === "bash") return true;
  }
  // Queued follow-up user line while a prior turn is still streaming: the last
  // message is the new user, but the turn is live (assistant exists earlier).
  if (state.streaming && lastUser > 0) {
    for (let i = lastUser - 1; i >= 0; i--) {
      const role = state.messages[i].role;
      if (role === "assistant" || role === "bash") return true;
      if (role === "user") break;
    }
  }
  return false;
}

function parseArgs(raw: string): { args: Record<string, unknown>; argString: string } {
  const argString = raw ?? "";
  if (!argString) return { args: {}, argString: "" };
  try {
    return { args: JSON.parse(argString) as Record<string, unknown>, argString };
  } catch {
    return { args: { raw: argString }, argString };
  }
}

function onToolCall(
  state: AgentState,
  ev: { id: string; name: string; args: string },
): AgentState {
  const s = beginAssistant(state);
  const id = ev.id || newId("call");
  const { args, argString } = parseArgs(ev.args);
  const tc: UIToolCall = { id, name: ev.name || "", args, argString };
  const withCall = updateCurrentAssistant(s, (m) => ({ ...m, toolCalls: [...m.toolCalls, tc] }));
  // The assistant turn is complete for this message; clear it so the next delta
  // (after the tool result) begins a fresh assistant message.
  return finalizeCurrentAssistant(withCall);
}

function onToolResult(
  state: AgentState,
  ev: { id: string; ok: boolean; output: string; diff?: string; tool?: string },
): AgentState {
  const result = { ok: ev.ok !== false, output: ev.output ?? "", diff: ev.diff };
  let matched = false;
  const messages = state.messages.map((m) => {
    if (m.role !== "assistant") return m;
    if (!m.toolCalls.some((t) => t.id === ev.id)) return m;
    matched = true;
    return {
      ...m,
      toolCalls: m.toolCalls.map((t) => (t.id === ev.id ? { ...t, result } : t)),
    };
  });
  if (matched) return { ...state, messages };
  // Fallback: no matching tool call (shouldn't happen) — render a standalone card.
  const fallback: UIMessage = {
    id: newId("tool"),
    role: "tool",
    toolCallId: ev.id,
    toolName: ev.tool ?? "",
    output: ev.output ?? "",
    ok: ev.ok !== false,
    diff: ev.diff,
    ts: Date.now(),
  };
  return { ...state, messages: [...state.messages, fallback] };
}

function peelThinkTags(raw: string): { thinking: string; text: string } | null {
  let trimmed = raw.trimStart();
  if (!trimmed.startsWith("<think>")) return null;
  const close = trimmed.indexOf("</think>");
  if (close < 0) return null;
  const thinking = trimmed.slice("<think>".length, close).trim();
  let text = trimmed.slice(close + "</think>".length);
  // MiniMax proxy streams sometimes append bare extra closing tags.
  while (true) {
    const next = text.trimStart();
    if (!next.startsWith("</think>")) {
      text = next;
      break;
    }
    text = next.slice("</think>".length);
  }
  return { thinking, text: text.trimStart() };
}

function asText(content: unknown): string {
  if (typeof content === "string") {
    const peeled = peelThinkTags(content);
    return peeled ? peeled.text : content;
  }
  if (Array.isArray(content)) {
    return content
      .map((c: any) =>
        typeof c === "string"
          ? c
          : c?.type === "text"
            ? c.text ?? ""
            : c?.type === "toolcall"
              ? ""
              : c?.text ?? "",
      )
      .join("");
  }
  return "";
}

function asThinking(m: any): string {
  if (typeof m.reasoning_content === "string" && m.reasoning_content.length > 0) {
    return m.reasoning_content;
  }
  if (typeof m.content === "string") {
    const peeled = peelThinkTags(m.content);
    if (peeled?.thinking) return peeled.thinking;
  }
  if (Array.isArray(m.content)) {
    return m.content
      .filter((c: any) => c?.type === "thinking" || c?.type === "reasoning")
      .map((c: any) => c.thinking ?? c.text ?? "")
      .join("");
  }
  return "";
}

/** Extract image data URLs from a multimodal user message content array. */
function asImages(content: unknown): string[] {
  if (!Array.isArray(content)) return [];
  const out: string[] = [];
  for (const c of content as any[]) {
    if (c?.type === "image_url" && c.image_url?.url) {
      out.push(String(c.image_url.url));
    }
  }
  return out;
}

/** Convert an OpenAI-style history array (from the core's `history` event) into
 *  the UI message model. Tool results attach to their tool call when possible. */
function historyToMessages(raw: unknown[]): UIMessage[] {
  const list = Array.isArray(raw) ? raw : [];
  const out: UIMessage[] = [];
  for (const item of list) {
    const m: any = item;
    if (!m || typeof m !== "object") continue;
    const role = m.role;
    const ts = typeof m.timestamp === "number" ? m.timestamp : Date.now();
    if (role === "user") {
      const imgs = asImages(m.content);
      out.push({ id: newId("u"), role: "user", text: asText(m.content), ts, ...(imgs.length ? { images: imgs } : {}) });
    } else if (role === "assistant") {
      const toolCalls: UIToolCall[] = (Array.isArray(m.tool_calls) ? m.tool_calls : []).map(
        (tc: any) => {
          const fn = tc?.function ?? {};
          const { args, argString } = parseArgs(typeof fn.arguments === "string" ? fn.arguments : "");
          return { id: tc?.id ?? newId("call"), name: fn.name ?? "", args, argString };
        },
      );
      out.push({
        id: newId("a"),
        role: "assistant",
        text: asText(m.content),
        thinking: asThinking(m),
        toolCalls,
        model: m.model,
        streaming: false,
        ts,
      });
    } else if (role === "tool") {
      const tcId = m.tool_call_id ?? "";
      const content = asText(m.content);
      const ownerIdx = out.findIndex(
        (x) => x.role === "assistant" && x.toolCalls.some((t) => t.id === tcId),
      );
      if (ownerIdx >= 0) {
        const a = out[ownerIdx] as AssistantMsg;
        out[ownerIdx] = {
          ...a,
          toolCalls: a.toolCalls.map((t) =>
            // History carries no ok/error flag, so mark the result unknown —
            // the card renders a neutral badge instead of a green “ok”.
            t.id === tcId ? { ...t, result: { ok: true, output: content, unknown: true } } : t,
          ),
        };
      } else {
        out.push({
          id: newId("tool"),
          role: "tool",
          toolCallId: tcId,
          toolName: "",
          output: content,
          ok: true,
          ts,
        });
      }
    }
  }
  return out;
}

export function reduce(state: AgentState, ev: AgentEvent): AgentState {
  switch (ev.type) {
    // ── Synthetic events ──
    case "_user": {
      const imgs = ev.images?.length ? ev.images : undefined;
      const msg: UIMessage = {
        id: newId("u"),
        role: "user",
        text: ev.text,
        ts: Date.now(),
        steer: ev.steer,
        ...(imgs ? { images: imgs } : {}),
      };
      return {
        ...state,
        messages: [...state.messages, msg],
        selectedModel: ev.model ?? state.selectedModel,
        streaming: true,
      };
    }
    case "_select_model":
      return { ...state, selectedModel: ev.id };
    case "_set_thinking":
      return { ...state, thinkingLevel: ev.level };
    case "_dismiss_toast":
      return { ...state, toasts: state.toasts.filter((t) => t.id !== ev.id) };
    case "_set_switching":
      return { ...state, switching: ev.switching };
    case "_clear_provider_models_preview":
      return { ...state, providerModelsPreview: null, providerModelsPreviewError: null };
    case "_set_provider_models_preview_error":
      return { ...state, providerModelsPreviewError: ev.error };
    case "_set_models_refreshing":
      return { ...state, modelsRefreshing: ev.refreshing };
    case "_add_notifications": {
      // Client-only: append feed items emitted by useAgent's liveSessions diff.
      // Dedup per session+kind: refresh (bump ts) an existing UNREAD item for
      // the same session+kind rather than stacking duplicates, so a session
      // repeatedly hitting approval doesn't spam the bell.
      const merged = [...state.notifications];
      for (const item of ev.items) {
        const idx = merged.findIndex(
          (n) => !n.read && n.sessionFile === item.sessionFile && n.kind === item.kind,
        );
        if (idx >= 0) {
          merged[idx] = {
            ...merged[idx],
            ts: item.ts,
            attentionKind: item.attentionKind ?? merged[idx].attentionKind,
          };
        } else {
          merged.push(item);
        }
      }
      const trimmed = merged.length > 50 ? merged.slice(merged.length - 50) : merged;
      return { ...state, notifications: trimmed };
    }
    case "_dismiss_notification":
      return {
        ...state,
        notifications: state.notifications.map((n) =>
          n.id === ev.id ? { ...n, read: true } : n,
        ),
      };
    case "_mark_notifications_read":
      return {
        ...state,
        notifications: state.notifications.map((n) => ({ ...n, read: true })),
      };
    case "_clear_notifications":
      return { ...state, notifications: [] };
    case "_goal_approve_optimistic": {
      if (!state.goalMode || state.goalMode.phase !== "plan_ready") return state;
      const goalMode = { ...state.goalMode, auto_deploy: true };
      let next: AgentState = {
        ...state,
        goalMode,
        streaming: true,
      };
      next = appendGoalMsg(next, {
        kind: "phase",
        title: "Goal · deploying",
        text: "Plan approved — deploying…",
        ok: true,
        status: "deploying",
      });
      return next;
    }
    case "_undo_local": {
      // Optimistic undo: trim the last turn; the core `reset` that follows must
      // NOT wipe the remaining transcript (see `pendingUndo` on `reset`).
      // Idempotent: originating client + server fanout both emit this — apply once.
      if (state.pendingUndo) return state;
      return {
        ...finishTurn(state),
        messages: dropLastTurn(state.messages),
        pendingUndo: true,
        workState: null,
      };
    }
    case "_session_title": {
      const sessions = state.sessions.map((s) =>
        s.name === ev.name ? { ...s, title: ev.title || undefined } : s,
      );
      return { ...state, sessions };
    }

    // ── Core events ──
    case "ready": {
      const skipped = ev.plugins_skipped;
      let toasts = state.toasts;
      if (skipped && skipped.length > 0) {
        toasts = pushToast(
          toasts,
          "info",
          `Skipped project plugin(s): ${skipped.join(", ")} (need --trust-project-plugins or reinstall)`,
        );
      }
      // The ready payload is the source of truth for sandbox readiness: it
      // carries the effective mode, sandboxReady, image/resource limits,
      // network mode, and the effective guest shell. Normalize legacy values
      // defensively (the core already migrates "firejail"/"seatbelt" →
      // "microsandbox"); only "none"|"microsandbox" reach here.
      const sbMode: SandboxMode = ev.sandbox === "microsandbox" ? "microsandbox" : "none";
      const sbShell =
        ev.shell === "bash" || ev.shell === "powershell" ? ev.shell : null;
      const sbNetwork: SandboxNetworkMode | null =
        ev.sandboxNetworkMode === "none" ||
        ev.sandboxNetworkMode === "restricted" ||
        ev.sandboxNetworkMode === "allowlist"
          ? ev.sandboxNetworkMode
          : null;
      return {
        ...state,
        ready: ev,
        models: ev.models ?? [],
        authed: ev.authed,
        // Prefer explicit anyLoggedIn; fall back to active authed or models present
        // (discovery only runs for logged-in providers).
        anyLoggedIn:
          ev.anyLoggedIn === true ||
          (ev.anyLoggedIn !== false && (ev.authed === true || (ev.models?.length ?? 0) > 0)),
        provider: ev.provider,
        providerKind: ev.providerKind,
        approvalMode: ev.approval,
        workspace: ev.workspace,
        providerPresets: ev.providerPresets ?? state.providerPresets,
        sandbox: {
          mode: sbMode,
          ready: ev.sandboxReady === true,
          image: ev.sandboxImage ?? null,
          cpus: ev.sandboxCpus ?? null,
          memoryMb: ev.sandboxMemoryMb ?? null,
          networkMode: sbNetwork,
          shell: sbShell,
          report: state.sandbox.report,
          // A fresh ready may follow a reset; clear a stale prepare/error state.
          preparePhase: null,
          error: null,
        },
        toasts,
      };
    }
    case "models": {
      const models = ev.models ?? [];
      const stillValid =
        state.selectedModel && models.some((m) => m.id === state.selectedModel);
      return {
        ...state,
        models,
        selectedModel: stillValid ? state.selectedModel : models[0]?.id ?? null,
      };
    }
    case "models_refreshed": {
      // Terminal event for `refresh_models` (core already re-emitted `models`).
      // Clear any optimistic spinner and confirm the count — parity with TUI.
      const count =
        typeof (ev as { count?: unknown }).count === "number"
          ? (ev as { count: number }).count
          : state.models.length;
      return {
        ...state,
        modelsRefreshing: false,
        toasts: pushToast(
          state.toasts,
          "info",
          `Model list refreshed (${count} model${count === 1 ? "" : "s"})`,
        ),
      };
    }
    case "provider_presets":
      return { ...state, providerPresets: ev.presets ?? [] };
    case "provider_models_preview":
      return {
        ...state,
        // Always set (including []) so the spinner can clear on failure.
        providerModelsPreview: ev.models ?? [],
        providerModelsPreviewError: ev.error?.trim() ? ev.error : null,
      };
    case "authed":
      return {
        ...state,
        authed: ev.ok,
        anyLoggedIn: ev.ok ? true : state.anyLoggedIn,
        pendingOauth: null,
      };
    case "provider_changed":
      return {
        ...state,
        provider: ev.provider,
        providerKind: ev.kind,
        authed: ev.has_key,
        // Gaining a key unlocks multi-provider sends; losing the active key alone
        // does not clear anyLoggedIn (other providers may still be usable).
        anyLoggedIn: ev.has_key ? true : state.anyLoggedIn,
        pendingOauth: null,
      };
    case "approval_changed": {
      if (ev.mode.includes(":")) {
        const kind = ev.mode.split(":")[0];
        return {
          ...state,
          escalatedKinds: state.escalatedKinds.includes(kind)
            ? state.escalatedKinds
            : [...state.escalatedKinds, kind],
        };
      }
      return { ...state, approvalMode: ev.mode };
    }
    case "delta": {
      const s = beginAssistant({ ...state, retrying: false });
      return updateCurrentAssistant(s, (m) => ({ ...m, text: m.text + ev.text }));
    }
    case "thinking": {
      const s = beginAssistant({ ...state, retrying: false });
      return updateCurrentAssistant(s, (m) => ({ ...m, thinking: m.thinking + ev.text }));
    }
    case "tool_call_start":
      // The SDK ignores tool_call_start/name/args deltas — the full card arrives
      // with the finalized `tool_call` event (beginAssistant there). Avoids an
      // empty bubble during args streaming.
      return state;
    case "tool_call_name":
    case "tool_call_args":
      return state;
    case "tool_call":
      return onToolCall(state, ev);
    case "tool_result":
      return onToolResult(state, ev);
    case "bash_execution": {
      const msg: BashMsg = {
        id: newId("bash"),
        role: "bash",
        command: ev.command,
        output: ev.output,
        ok: ev.ok !== false,
        excludeFromContext: !!ev.exclude_from_context,
        ts: Date.now(),
      };
      return { ...state, messages: [...state.messages, msg] };
    }
    case "umans_conc": {
      const { used, limit, provider } = ev;
      // Live account-wide Umans concurrency from the /v1/usage poll. null used
      // => not Umans / fetch failed (hide); null limit => unlimited (render ∞).
      // `provider` is the Umans provider the poll tracks; the header only shows
      // the field when the selected model routes to it.
      return { ...state, umansConc: { used: used ?? null, limit: limit ?? null, provider: provider ?? "" } };
    }
    case "metrics": {
      const { type: _t, tps_est, tps, ...rest } = ev as typeof ev & { tps_est?: number };
      // Mid-stream core emits `tps_est`; final metrics emit `tps`. Prefer final.
      const metrics = {
        ...state.metrics,
        ...rest,
        tps: tps ?? tps_est ?? state.metrics?.tps,
      };
      // Final metrics (carry elapsed_ms / prompt_tokens): attach usage to the last
      // assistant message for per-message display.
      const isFinal = ev.elapsed_ms != null || ev.prompt_tokens != null;
      let messages = state.messages;
      if (isFinal) {
        const lastA = [...messages].reverse().find((m) => m.role === "assistant");
        if (lastA) {
          messages = messages.map((m) =>
            m.id === lastA.id ? { ...m, usage: metrics ?? undefined } : m,
          );
        }
      }
      return { ...state, metrics, messages, retrying: false };
    }
    case "approval_request":
      return {
        ...state,
        pendingApproval: {
          request_id: ev.request_id,
          tool: ev.tool,
          args: ev.args,
          diff: ev.diff,
        },
      };
    case "approval_expired":
      return {
        ...state,
        pendingApproval:
          state.pendingApproval?.request_id === ev.request_id
            ? null
            : state.pendingApproval,
        toasts: pushToast(state.toasts, "info", "Tool approval request expired"),
      };
    case "run_cancelled":
      return finishTurn(state);
    case "runtime_status":
      // Requested diagnostics are currently rendered by protocol consumers and
      // logs; explicitly accept them without growing persistent UI state.
      return state;
    case "stuck_nudge":
      // Core self-corrected a stuck loop by injecting a steering system
      // message; no persistent UI state to grow (it steers the model, not the UI).
      return state;
    case "ask_request":
      return {
        ...state,
        pendingAsk: { request_id: ev.request_id, questions: ev.questions },
        toasts: pushToast(
          state.toasts,
          "info",
          `Agent asks: ${ev.questions.length} question${ev.questions.length === 1 ? "" : "s"}`,
        ),
      };
    case "sudo_request":
      return {
        ...state,
        pendingSudo: { request_id: ev.request_id, command: ev.command },
        toasts: pushToast(
          state.toasts,
          "info",
          "Sudo command requested — approve or decline",
        ),
      };
    case "compacting":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `Compacting context${ev.trigger ? ` (${ev.trigger})` : ""}…`,
        ),
      };
    case "compacted":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `Context compacted — ${ev.before_tokens.toLocaleString()} → ${ev.after_tokens.toLocaleString()} tokens`,
        ),
      };
    case "http_retry":
      return {
        ...state,
        retrying: true,
        toasts: pushToast(
          state.toasts,
          "info",
          `Retrying request${ev.status ? ` (HTTP ${ev.status})` : ""}…`,
        ),
      };
    case "sessions": {
      const sorted = [...ev.sessions].sort((a, b) => (b.mtime ?? 0) - (a.mtime ?? 0));
      return {
        ...state,
        sessions: sorted,
        currentSessionFile:
          state.currentSessionFile ?? sorted[0]?.path ?? sorted[0]?.name ?? null,
      };
    }
    case "session_status": {
      // Bridge-synthesized snapshot of every live session's status, fanned to ALL
      // sessions (cross-workspace). Keyed by absolute session-file path so the
      // sidebar can look up a session's live badge. Notifications are derived
      // client-side from transitions (useAgent), NOT here, so a fresh
      // hydration never spams the feed.
      const liveSessions: Record<string, LiveSessionStatus> = {};
      for (const s of ev.sessions) liveSessions[s.sessionFile] = s;
      return { ...state, liveSessions };
    }
    case "stats":
      return {
        ...state,
        stats: ev,
        currentSessionFile: ev.session_file || state.currentSessionFile,
      };
    case "usage": {
      // Provider plan/rate-limit snapshot for the selected model. Keep the full
      // payload for the Diagnostics panel (toasts stay short).
      const avail = ev.available;
      const plan = ev.plan;
      const provider = ev.provider || "provider";
      const windows = ev.windows ?? [];
      const msg = ev.message;
      const snapshot = {
        provider: ev.provider,
        provider_kind: ev.provider_kind,
        model: ev.model,
        base_url: ev.base_url,
        available: ev.available,
        plan: ev.plan,
        message: ev.message,
        windows,
      };
      if (!avail) {
        return {
          ...state,
          usageSnapshot: snapshot,
          toasts: pushToast(state.toasts, "info", msg || `${provider}: usage not available`),
        };
      }
      const summary = windows
        .slice(0, 3)
        .map((w) => {
          const label = w.label || "limit";
          if (w.unit === "percent" && typeof w.used === "number") {
            return `${label} ${Math.round(w.used)}%`;
          }
          if (
            typeof w.used === "number" &&
            typeof w.limit === "number" &&
            w.limit > 0
          ) {
            const pct = Math.round((w.used / w.limit) * 100);
            return `${label} ${w.used}/${w.limit} (${pct}%)`;
          }
          if (typeof w.used === "number") return `${label} ${w.used}`;
          return label;
        })
        .join(" · ");
      const head = plan ? `${provider} (${plan})` : provider;
      return {
        ...state,
        usageSnapshot: snapshot,
        toasts: pushToast(
          state.toasts,
          "info",
          summary ? `${head}: ${summary}` : `${head}: usage ok`,
        ),
      };
    }
    case "context_breakdown": {
      const breakdown = {
        total_tokens: ev.total_tokens,
        context_window: ev.context_window,
        pct: ev.pct,
        messages: ev.messages,
        system_tokens: ev.system_tokens,
        digest_threshold_tokens: ev.digest_threshold_tokens,
        compact_threshold_tokens: ev.compact_threshold_tokens,
        hard_limit_tokens: ev.hard_limit_tokens,
        response_reserve_tokens: ev.response_reserve_tokens,
        safety_margin_tokens: ev.safety_margin_tokens,
        by_role: ev.by_role ?? {},
        top_consumers: ev.top_consumers ?? [],
      };
      return {
        ...state,
        contextBreakdown: breakdown,
      };
    }
    case "agents":
      return {
        ...state,
        availableAgents: ev.agents ?? [],
      };
    case "history": {
      const tokensIn = (ev as { tokens_in?: number }).tokens_in;
      return {
        ...state,
        messages: historyToMessages(ev.messages ?? []),
        currentAssistantId: null,
        streaming: false,
        followUpQueued: false,
        pendingApproval: null,
        pendingAsk: null,
        pendingSudo: null,
        pendingIntercom: null,
        stats:
          tokensIn != null
            ? {
                type: "stats",
                tokens_in: tokensIn,
                tokens_out: state.stats?.tokens_out ?? 0,
                tokens_total: tokensIn + (state.stats?.tokens_out ?? 0),
                cached_tokens: state.stats?.cached_tokens ?? 0,
                turns: state.stats?.turns ?? 0,
                messages: (ev.messages ?? []).length,
                session_file: state.stats?.session_file ?? state.currentSessionFile ?? "",
              }
            : state.stats,
      };
    }
    case "reset":
      // After client `_undo_local`, keep the trimmed messages — core already
      // dropped the last turn and this reset is only a UI sync signal.
      if (state.pendingUndo) {
        return {
          ...finishTurn(state),
          pendingUndo: false,
          workState: null,
          goalMode: null,
          goalPlan: null,
          goalStepFinals: {},
          goalIterations: [],
        };
      }
      return {
        ...state,
        messages: [],
        currentAssistantId: null,
        streaming: false,
        followUpQueued: false,
        pendingUndo: false,
        pendingApproval: null,
        pendingAsk: null,
        pendingSudo: null,
        pendingIntercom: null,
        pendingOauth: null,
        workState: null,
        goalMode: null,
        goalPlan: null,
        goalStepFinals: {},
        goalIterations: [],
        subagentRuns: {},
        metrics: null,
      };
    case "done": {
      const finished = finishTurn(state);
      return goalKeepsStreaming(state.goalMode) ? { ...finished, streaming: true } : finished;
    }
    case "aborted":
      return finishTurn(state);
    case "advisor_note":
    case "advisor_status":
      // Advisors emit review feedback + state transitions for the watchdog.
      // The web UI surfaces advisor status in the toast log but does not
      // interrupt the turn — keep the current streaming state.
      return state;
    case "error": {
      // Do NOT always clear streaming — core often emits non-fatal errors mid-turn.
      // Pre-turn failures (bad skill/model): drop the optimistic user bubble + working flag.
      // Core-death toasts after `aborted` must NOT strip the last user line.
      const msg = ev.message ?? "";
      const coreDead = /core exited/i.test(msg);
      const started = turnHasStarted(state);
      let messages = state.messages;
      if (
        !coreDead &&
        !started &&
        messages.length > 0 &&
        messages[messages.length - 1].role === "user"
      ) {
        messages = messages.slice(0, -1);
      }
      return {
        ...state,
        messages,
        streaming: started || coreDead ? state.streaming : false,
        retrying: false,
        goalMode: coreDead ? null : state.goalMode,
        goalPlan: coreDead ? null : state.goalPlan,
        goalStepFinals: coreDead ? {} : state.goalStepFinals,
        toasts: pushToast(state.toasts, "error", msg),
      };
    }
    case "info": {
      const msg = ev.message.toLowerCase();
      let followUpQueued = state.followUpQueued;
      if (msg.includes("prompt queued")) followUpQueued = true;
      if (msg.includes("queue cleared") || msg.includes("queue already empty")) {
        followUpQueued = false;
      }
      let next: AgentState = {
        ...state,
        followUpQueued,
        toasts: pushToast(state.toasts, "info", ev.message),
      };
      // Parity with TUI: persist goal deploy bridge lines (not toast-only).
      if (
        msg.includes("goal deploy") ||
        msg.includes("goal plan approved") ||
        msg.includes("writing completion summary") ||
        msg.includes("snapshotting workspace")
      ) {
        next = appendGoalMsg(next, {
          kind: "phase",
          title: "Goal · progress",
          text: ev.message,
          ok: true,
          status: state.goalMode?.phase ?? "deploying",
        });
      }
      return next;
    }
    case "steer":
      return state;

    // ── Subagent / intercom ──
    case "intercom_message": {
      const msg = ev.message.length > 80 ? ev.message.slice(0, 79) + "…" : ev.message;
      const entry: IntercomEntry = {
        id: newId("ic"),
        kind: ev.reason === "need_decision" ? "ask" : "reply",
        from: ev.from,
        to: ev.to,
        message: ev.message,
        ts: Date.now(),
      };
      const log = [entry, ...state.intercomLog].slice(0, 50);
      const needsDecision = ev.reason === "need_decision";
      return {
        ...state,
        intercomLog: log,
        pendingIntercom: needsDecision
          ? { request_id: ev.id, from: ev.from || "subagent", message: ev.message, reason: ev.reason }
          : state.pendingIntercom,
        toasts: needsDecision ? pushToast(state.toasts, "info", `Subagent asks: ${msg}`) : state.toasts,
      };
    }
    case "subagent_progress": {
      // Live phase/tokens/tool counters, keyed by run_id. Also keep a log line
      // (the old reducer read `message`, which the core never emits for progress).
      const phase = ev.phase ?? "";
      const entry: IntercomEntry = {
        id: newId("sp"),
        kind: "status",
        from: ev.agent,
        message: ev.tool ? `${phase}: ${ev.tool}` : phase,
        ts: Date.now(),
      };
      const withLog = { ...state, intercomLog: [entry, ...state.intercomLog].slice(0, 50) };
      return upsertRun(withLog, ev.run_id, (r) => ({
        ...r,
        agent: ev.agent ?? r.agent,
        phase,
        tool: ev.tool ?? r.tool,
        toolCount: ev.tool_count ?? r.toolCount,
        tokensIn: ev.tokens_in ?? r.tokensIn,
        tokensOut: ev.tokens_out ?? r.tokensOut,
        elapsedMs: ev.elapsed_ms ?? r.elapsedMs,
        state: r.state === "running" && phase === "done" ? "completed" : r.state,
      }));
    }
    case "subagent_start":
      return upsertRun(state, ev.run_id, (r) => ({
        ...r,
        mode: ev.mode,
        agent: ev.agent ?? r.agent,
        agents: ev.agents ?? r.agents,
        task: ev.task ?? r.task,
        depth: ev.depth ?? r.depth,
        startedAt: ev.started_at ?? r.startedAt,
        state: "running",
      }));
    case "subagent_message": {
      const item: SubagentChatItem = {
        id: newId("sm"),
        kind: "message",
        role: ev.role === "user" || ev.role === "assistant" ? ev.role : "assistant",
        content: ev.content,
        ts: Date.now(),
      };
      return upsertRun(state, ev.run_id, (r) => ({ ...r, items: [...r.items, item] }));
    }
    case "subagent_tool_call": {
      const item: SubagentChatItem = {
        id: newId("st"),
        kind: "tool",
        callId: ev.call_id,
        name: ev.name,
        args: ev.args,
        ts: Date.now(),
      };
      return upsertRun(state, ev.run_id, (r) => ({
        ...r,
        items: [...r.items, item],
        toolCount: ev.tool_count ?? r.toolCount,
      }));
    }
    case "subagent_tool_result":
      return upsertRun(state, ev.run_id, (r) => {
        // Match by call_id when present; otherwise the most recent tool item
        // still awaiting a result (covers models that emit empty tool-call ids).
        let matched = false;
        const items = r.items.map((it) => {
          if (matched || it.kind !== "tool" || it.result !== undefined) return it;
          if (ev.call_id && it.callId === ev.call_id) {
            matched = true;
            return { ...it, result: ev.result, ok: ev.ok };
          }
          return it;
        });
        if (!matched) {
          for (let i = items.length - 1; i >= 0; i--) {
            if (items[i].kind === "tool" && items[i].result === undefined) {
              items[i] = { ...items[i], result: ev.result, ok: ev.ok };
              matched = true;
              break;
            }
          }
        }
        return { ...r, items };
      });
    case "subagent_done": {
      const runState = String(ev.state ?? "completed");
      const summaryRaw = String(ev.summary ?? "").trim();
      const summary =
        summaryRaw ||
        (runState === "failed"
          ? "(step failed with no summary)"
          : "(step finished with no written summary)");
      return pruneTerminalRuns(
        upsertRun(state, ev.run_id, (r) => {
          const items = [...r.items];
          const hasAssistantText = items.some(
            (it) =>
              it.kind === "message" &&
              it.role === "assistant" &&
              String(it.content ?? "").trim(),
          );
          if (!hasAssistantText) {
            items.push({
              id: newId("sm"),
              kind: "message",
              role: "assistant",
              content: summary,
              ts: Date.now(),
            });
          }
          return {
            ...r,
            state: runState || r.state,
            summary: summaryRaw || r.summary || summary,
            endedAt: ev.ended_at ?? r.endedAt,
            items,
          };
        }),
      );
    }

    // ── Memory ──
    case "memory_list":
      return { ...state, memories: Array.isArray(ev.entries) ? ev.entries : [] };
    case "memory_saved": {
      const ok = !ev.deleted;
      return {
        ...state,
        toasts: pushToast(state.toasts, ok ? "success" : "info", ev.message ?? (ok ? "Memory saved" : "Memory forgotten")),
      };
    }

    // ── Plugins ──
    case "plugins_list":
      return { ...state, plugins: Array.isArray(ev.plugins) ? ev.plugins : [] };
    case "plugin_installed":
    case "plugin_removed":
    case "plugin_enabled":
    case "plugin_disabled": {
      const verb = ev.type.replace("plugin_", "");
      const msg = ("message" in ev && ev.message) || `${verb}: ${ev.name}`;
      return {
        ...state,
        toasts: pushToast(state.toasts, ev.ok === false ? "error" : "success", msg),
      };
    }
    case "plugin_error":
      return { ...state, toasts: pushToast(state.toasts, "error", ev.message) };
    case "plugin_trust_prompt":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `${Array.isArray(ev.plugins) ? ev.plugins.length : 0} project plugin trust decision${Array.isArray(ev.plugins) && ev.plugins.length === 1 ? "" : "s"} required`,
        ),
      };
    case "plugin_trust_applied":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "success",
          `Plugin trust updated${ev.loaded ? `; ${ev.loaded} loaded` : ""}`,
        ),
      };

    // ── Skills ──
    case "skills":
      return { ...state, skills: Array.isArray(ev.skills) ? ev.skills : [] };
    case "skill_marketplace_state":
      return {
        ...state,
        marketplaceDisclaimerAccepted: ev.disclaimer_accepted === true,
        marketplaceInstalled: Array.isArray(ev.installed) ? ev.installed : [],
      };
    case "skill_marketplace_results":
      return { ...state, marketplaceResults: Array.isArray(ev.skills) ? ev.skills : [] };
    case "skill_marketplace_changed":
      return { ...state, toasts: pushToast(state.toasts, "success", `Skill ${ev.action}: ${ev.name}`) };
    case "skill_marketplace_error":
      return { ...state, toasts: pushToast(state.toasts, "error", ev.message || "Skill marketplace error") };

    // ── Vision ──
    case "vision_config":
      return {
        ...state,
        visionConfig: {
          enabled: ev.enabled !== false,
          vision_models: ev.vision_models ?? [],
          vision_model: ev.vision_model ?? null,
        },
      };

    // ── Projects / workspace ──
    case "projects":
      return { ...state, projects: Array.isArray(ev.projects) ? ev.projects : [] };
    case "workspace_changed":
      return {
        ...state,
        workspace: ev.workspace,
        projects: Array.isArray(ev.projects) ? ev.projects : state.projects,
        switching: false,
        messages: [],
        currentAssistantId: null,
        streaming: false,
        pendingApproval: null,
        pendingAsk: null,
        pendingSudo: null,
        pendingIntercom: null,
        pendingOauth: null,
        pendingUndo: false,
        followUpQueued: false,
        sessions: [],
        currentSessionFile: null,
        stats: null,
        workState: null,
        goalMode: null,
        goalPlan: null,
        goalStepFinals: {},
        goalIterations: [],
        subagentRuns: {},
        metrics: null,
        modelsRefreshing: false,
      };
    case "session_renamed": {
      const sessions = state.sessions.map((s) =>
        s.name === ev.name ? { ...s, title: ev.title || undefined } : s,
      );
      return { ...state, sessions };
    }

    // ── Compaction / config ──
    case "digested": {
      const n = ev.results;
      const before = ev.before_tokens;
      const after = ev.after_tokens;
      const range =
        before != null && after != null
          ? ` — ${before.toLocaleString()} → ${after.toLocaleString()} tokens`
          : after != null
            ? ` — now ${after.toLocaleString()} tokens`
            : "";
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          n > 1
            ? `Reclaimed ${n} stale tool payloads${range}`
            : `Reclaimed a stale tool payload${range}`,
        ),
      };
    }
    case "config_changed": {
      // Patch ready so Settings / header reflect the live value immediately.
      const sandboxChanged = ev.key === "sandbox";
      const nextSandboxMode: SandboxMode =
        sandboxChanged && String(ev.value) === "microsandbox"
          ? "microsandbox"
          : sandboxChanged
            ? "none"
            : state.sandbox.mode;
      const ready = state.ready
        ? {
            ...state.ready,
            ...(ev.key === "bash_timeout_secs"
              ? { bash_timeout_secs: Number(ev.value) || state.ready.bash_timeout_secs }
              : {}),
            ...(ev.key === "auto_compact"
              ? {
                  auto_compact:
                    ev.value === true ||
                    ev.value === "true" ||
                    ev.value === 1 ||
                    ev.value === "1",
                }
              : {}),
            ...(sandboxChanged ? { sandbox: String(ev.value) } : {}),
          }
        : state.ready;
      // When the mode flips, the core re-runs preflight and will re-emit
      // sandbox_ready/sandbox_status. Until then a requested-but-unready
      // microsandbox must not be treated as ready — clear readiness so the
      // Safety panel can show the setup-required state.
      const sandbox: SandboxRuntimeStatus = sandboxChanged
        ? {
            ...state.sandbox,
            mode: nextSandboxMode,
            ready: nextSandboxMode === "none" ? true : state.sandbox.ready,
            // A mode change invalidates any stale error/prepare-phase.
            preparePhase: nextSandboxMode === "none" ? null : state.sandbox.preparePhase,
            error: null,
          }
        : state.sandbox;
      return {
        ...state,
        ready,
        sandbox,
        toasts: pushToast(state.toasts, "info", `${ev.key} → ${ev.value}`),
      };
    }

    // ── Sandbox (Microsandbox) ──
    case "sandbox_status": {
      // Full preflight report + effective mode. The core is the single source
      // of truth for platform/support/readiness — store verbatim.
      const report: SandboxPreflightReport = ev.report;
      return {
        ...state,
        sandbox: {
          ...state.sandbox,
          mode: ev.mode,
          ready: report.ready,
          report,
          // A fresh status clears a transient prepare-phase / error.
          preparePhase: null,
          error: null,
        },
      };
    }
    case "sandbox_prepare_progress": {
      // Runtime/image download streaming phase. Keep the latest phase so the
      // Safety panel can show live progress; never clears readiness on its own.
      return {
        ...state,
        sandbox: { ...state.sandbox, preparePhase: ev.phase },
      };
    }
    case "sandbox_ready": {
      // Terminal prepare result (or an out-of-band readiness flip). `ready` is
      // the source of truth; carry the optional fresh report if present.
      const report = ev.report ?? state.sandbox.report;
      return {
        ...state,
        sandbox: {
          ...state.sandbox,
          ready: ev.ready,
          report,
          preparePhase: null,
          error: ev.ready ? null : state.sandbox.error,
        },
      };
    }
    case "sandbox_error": {
      // A setup/boot/runtime failure. The core fail-closed: no host fallback.
      // Surface the structured error + a toast so it is not silently missed.
      return {
        ...state,
        sandbox: {
          ...state.sandbox,
          ready: false,
          preparePhase: null,
          error: ev.error,
        },
        toasts: pushToast(state.toasts, "error", ev.error),
      };
    }

    // ── OAuth / lifecycle status ──
    case "oauth_prompt":
      // The core needs the user to visit an authorize URL (and, for the device
      // flow, paste back a code). Surface it as a blocking banner + a toast.
      return {
        ...state,
        pendingOauth: { url: ev.url, code: ev.code, message: ev.message },
        toasts: pushToast(state.toasts, "info", ev.message || "Complete the OAuth login in your browser"),
      };
    case "reflecting": {
      // Auto-reflect injected a reflection continuation turn. Mirror the TUI's
      // logInfo so the post-finish model activity isn't a mystery.
      const n = String(ev.recurrence ?? "");
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          n && n !== "0"
            ? `auto-reflect: reflecting on this turn (${n} recurring patterns)…`
            : "auto-reflect: reflecting on this turn…",
        ),
      };
    }
    case "work_state":
      // Rolling KV-cache-aware work-state summary (goal/done/doing/next/files).
      return {
        ...state,
        workState: {
          version: ev.version,
          goal: ev.goal,
          done: ev.done,
          in_progress: ev.in_progress,
          next: ev.next,
          recent_files: ev.recent_files,
          last_activity: ev.last_activity,
        },
      };
    case "goal_state": {
      if (!ev.id || ev.phase === "idle") {
        return {
          ...state,
          goalMode: null,
          goalPlan: null,
          goalStepFinals: {},
          goalIterations: [],
        };
      }
      const goalMode = {
        id: ev.id,
        goal: ev.goal,
        phase: ev.phase,
        concurrency: ev.concurrency,
        max_tasks: ev.max_tasks,
        allowed_models: ev.allowed_models ?? [],
        allowed_providers: ev.allowed_providers ?? [],
        auto_deploy: ev.auto_deploy,
        ceo_mode: !!ev.ceo_mode,
        mode: ev.mode != null ? String(ev.mode) : ev.ceo_mode ? "ceo" : "single_pass",
        iteration: typeof ev.iteration === "number" ? ev.iteration : 0,
        max_iterations: typeof ev.max_iterations === "number" ? ev.max_iterations : 0,
        plan_revision: typeof ev.plan_revision === "number" ? ev.plan_revision : 0,
        max_plan_revisions:
          typeof ev.max_plan_revisions === "number" ? ev.max_plan_revisions : 0,
        review_verdict: parseGoalVerdict(ev.review_verdict),
        verify_verdict: parseGoalVerdict(ev.verify_verdict),
        remaining_gaps: Array.isArray(ev.remaining_gaps)
          ? ev.remaining_gaps.map((g: unknown) => String(g))
          : [],
        self_review_feedback:
          ev.self_review_feedback != null ? String(ev.self_review_feedback) : null,
        certified: !!ev.certified,
        role_models: ev.role_models,
        model_concurrency: ev.model_concurrency,
        prompts: ev.prompts ?? [],
        active_run_ids: ev.active_run_ids ?? [],
        version: ev.version,
        error: ev.error ?? null,
        parent_model: ev.parent_model ?? "",
      };
      const terminal =
        ev.phase === "done" || ev.phase === "failed" || ev.phase === "cancelled";
      let goalIterations = state.goalIterations ?? [];
      if (goalMode.ceo_mode) {
        goalIterations = upsertGoalIteration(goalIterations, goalMode.iteration ?? 0, {
          plan_revision: goalMode.plan_revision,
          review_verdict: goalMode.review_verdict,
          verify_verdict: goalMode.verify_verdict,
          remaining_gaps: goalMode.remaining_gaps,
          prompts: goalMode.prompts,
          certified: goalMode.certified,
        });
      }
      let next: AgentState = {
        ...state,
        goalMode,
        goalIterations,
        streaming: goalKeepsStreaming(goalMode) ? true : terminal ? false : state.streaming,
      };
      // Belt-and-suspenders: lasting step cards from prompts when core has not
      // (yet) emitted goal_step_complete.
      next = ingestPromptFinalsFromGoalState(next, goalMode.prompts);
      return next;
    }
    case "goal_plan":
      return {
        ...state,
        goalPlan: {
          id: ev.id,
          summary: ev.summary,
          steps: ev.steps ?? [],
          risks: ev.risks ?? [],
          validation: ev.validation ?? [],
          version: ev.version,
        },
      };
    case "goal_phase": {
      const from = String(ev.from ?? "");
      const to = String(ev.to ?? "");
      const detail = String(ev.message ?? "").trim();
      const counts =
        typeof ev.done_count === "number" && typeof ev.step_count === "number"
          ? ` (${ev.done_count}/${ev.step_count})`
          : "";
      const wave =
        typeof ev.wave === "number" ? ` · wave ${ev.wave}` : "";
      const msg = detail
        ? `Goal ${from} → ${to}${wave}${counts}: ${detail}`
        : `Goal ${from} → ${to}${wave}${counts}`;
      let next: AgentState = {
        ...state,
        toasts: pushToast(state.toasts, "info", msg),
      };
      if (GOAL_LASTING_PHASES.has(to)) {
        next = appendGoalMsg(next, {
          kind: "phase",
          title: `Goal · ${to}${wave}${counts}`,
          text: detail || msg,
          status: to,
          ok: to !== "failed" && to !== "cancelled",
        });
      }
      return next;
    }
    case "goal_step_complete": {
      return ingestGoalStepFinal(state, {
        stepId: String(ev.step_id ?? ""),
        title: ev.title != null ? String(ev.title) : undefined,
        agent: ev.agent != null ? String(ev.agent) : undefined,
        ok: ev.ok,
        status: ev.status != null ? String(ev.status) : undefined,
        summary: ev.summary != null ? String(ev.summary) : undefined,
        runId: ev.run_id != null ? String(ev.run_id) : undefined,
      });
    }
    case "goal_completion_summary": {
      const raw = String(ev.text ?? ev.summary ?? "").trim();
      const text = truncateGoalSummary(
        raw || "Goal finished (no completion summary provided).",
        8000,
      );
      return appendGoalMsg(state, {
        kind: "completion_summary",
        title: "Goal complete",
        text,
        ok: true,
        status: "done",
      });
    }
    case "goal_step_verdict": {
      const ok = !!ev.ok;
      const output = String(ev.output ?? "");
      const snippet = output.trim().slice(0, 160);
      let next: AgentState = {
        ...state,
        lastGoalVerdict: { ok, output },
        toasts: pushToast(
          state.toasts,
          ok ? "success" : "error",
          ok
            ? `Verifier passed${snippet ? `: ${snippet}` : ""}`
            : `Verifier failed${snippet ? `: ${snippet}` : ""}`,
        ),
      };
      next = appendGoalMsg(next, {
        kind: "verdict",
        title: ok ? "Verifier passed" : "Verifier failed",
        text: truncateGoalSummary(output || (ok ? "passed" : "failed")),
        ok,
        status: ok ? "done" : "failed",
      });
      return next;
    }
    case "goal_iteration": {
      const iteration = typeof ev.iteration === "number" ? ev.iteration : 0;
      const goalMode = state.goalMode
        ? {
            ...state.goalMode,
            iteration,
            max_iterations:
              typeof ev.max_iterations === "number"
                ? ev.max_iterations
                : state.goalMode.max_iterations,
            plan_revision:
              typeof ev.plan_revision === "number"
                ? ev.plan_revision
                : state.goalMode.plan_revision,
            max_plan_revisions:
              typeof ev.max_plan_revisions === "number"
                ? ev.max_plan_revisions
                : state.goalMode.max_plan_revisions,
          }
        : state.goalMode;
      return {
        ...state,
        goalMode,
        goalIterations: upsertGoalIteration(state.goalIterations ?? [], iteration, {
          plan_revision:
            typeof ev.plan_revision === "number" ? ev.plan_revision : undefined,
        }),
      };
    }
    case "goal_review_verdict": {
      const verdict = {
        ok: !!ev.ok,
        summary: String(ev.summary ?? ""),
        evidence_paths: Array.isArray(ev.evidence_paths)
          ? ev.evidence_paths.map((p: unknown) => String(p))
          : undefined,
      };
      const iteration =
        typeof ev.iteration === "number"
          ? ev.iteration
          : (state.goalMode?.iteration ?? 0);
      const goalMode = state.goalMode
        ? { ...state.goalMode, review_verdict: verdict }
        : state.goalMode;
      let next: AgentState = {
        ...state,
        goalMode,
        goalIterations: upsertGoalIteration(state.goalIterations ?? [], iteration, {
          plan_revision:
            typeof ev.plan_revision === "number" ? ev.plan_revision : undefined,
          review_verdict: verdict,
        }),
        toasts: pushToast(
          state.toasts,
          verdict.ok ? "success" : "info",
          verdict.ok
            ? `Plan review passed${verdict.summary ? `: ${verdict.summary.slice(0, 120)}` : ""}`
            : `Plan review revise${verdict.summary ? `: ${verdict.summary.slice(0, 120)}` : ""}`,
        ),
      };
      next = appendGoalMsg(next, {
        kind: "verdict",
        title: verdict.ok ? "Plan review · pass" : "Plan review · revise",
        text: truncateGoalSummary(verdict.summary || (verdict.ok ? "passed" : "revise")),
        ok: verdict.ok,
        status: verdict.ok ? "done" : "failed",
      });
      return next;
    }
    case "goal_verify_verdict": {
      const gaps = Array.isArray(ev.remaining_gaps)
        ? ev.remaining_gaps.map((g: unknown) => String(g))
        : [];
      const verdict = {
        ok: !!ev.ok,
        summary: String(ev.summary ?? ""),
        evidence_paths: Array.isArray(ev.evidence_paths)
          ? ev.evidence_paths.map((p: unknown) => String(p))
          : undefined,
      };
      const iteration =
        typeof ev.iteration === "number"
          ? ev.iteration
          : (state.goalMode?.iteration ?? 0);
      const goalMode = state.goalMode
        ? {
            ...state.goalMode,
            verify_verdict: verdict,
            remaining_gaps: gaps,
            certified: verdict.ok ? true : state.goalMode.certified,
          }
        : state.goalMode;
      let next: AgentState = {
        ...state,
        goalMode,
        lastGoalVerdict: { ok: verdict.ok, output: verdict.summary },
        goalIterations: upsertGoalIteration(state.goalIterations ?? [], iteration, {
          verify_verdict: verdict,
          remaining_gaps: gaps,
          prompts: goalMode?.prompts,
          certified: verdict.ok || undefined,
        }),
        toasts: pushToast(
          state.toasts,
          verdict.ok ? "success" : "error",
          verdict.ok
            ? `Verify certified${verdict.summary ? `: ${verdict.summary.slice(0, 120)}` : ""}`
            : `Verify gaps${verdict.summary ? `: ${verdict.summary.slice(0, 120)}` : ""}`,
        ),
      };
      next = appendGoalMsg(next, {
        kind: "verdict",
        title: verdict.ok ? "Verify · certified" : "Verify · remaining gaps",
        text: truncateGoalSummary(
          [verdict.summary, gaps.length ? `Gaps: ${gaps.join("; ")}` : ""]
            .filter(Boolean)
            .join("\n") || (verdict.ok ? "certified" : "gaps remain"),
        ),
        ok: verdict.ok,
        status: verdict.ok ? "done" : "failed",
      });
      return next;
    }
    case "goal_certified": {
      const summary = String(ev.summary ?? "");
      const iteration =
        typeof ev.iteration === "number"
          ? ev.iteration
          : (state.goalMode?.iteration ?? 0);
      const goalMode = state.goalMode
        ? {
            ...state.goalMode,
            certified: true,
            verify_verdict: state.goalMode.verify_verdict ?? {
              ok: true,
              summary,
            },
          }
        : state.goalMode;
      let next: AgentState = {
        ...state,
        goalMode,
        goalIterations: upsertGoalIteration(state.goalIterations ?? [], iteration, {
          certified: true,
          verify_verdict: {
            ok: true,
            summary,
          },
        }),
        toasts: pushToast(
          state.toasts,
          "success",
          `Mission certified${summary ? `: ${summary.slice(0, 120)}` : ""}`,
        ),
      };
      next = appendGoalMsg(next, {
        kind: "completion_summary",
        title: "Mission certified",
        text: truncateGoalSummary(summary || "Goal certified complete.", 8000),
        ok: true,
        status: "done",
      });
      return next;
    }
    case "protocol_hello":
      return {
        ...state,
        protocolHello: {
          version: String(ev.version ?? ""),
          protocol_version: Number(
            (ev as unknown as Record<string, unknown>).protocol_version ?? 1,
          ),
          min_client: String(ev.min_client ?? ""),
          capabilities: Array.isArray(ev.capabilities)
            ? ev.capabilities.map(String)
            : [],
        },
      };
    case "file_change": {
      const path = String(ev.path ?? "");
      if (!path) return { ...state, fileChangeSeq: state.fileChangeSeq + 1, recentFileChanges: [] };
      const record = {
        path,
        tool: String(ev.tool ?? ""),
        unified_diff: ev.unified_diff ? String(ev.unified_diff) : undefined,
        agent_id: ev.agent_id ? String(ev.agent_id) : undefined,
        run_id: ev.run_id ? String(ev.run_id) : undefined,
        ts: Date.now(),
      };
      const recent = [record, ...state.recentFileChanges.filter((c) => c.path !== path)].slice(
        0,
        40,
      );
      return {
        ...state,
        fileChangeSeq: state.fileChangeSeq + 1,
        recentFileChanges: recent,
      };
    }
    case "checkpoint_created": {
      const entry = {
        id: String(ev.id ?? ""),
        label: String(ev.label ?? ""),
        kind: String(ev.kind ?? ""),
        auto: !!ev.auto,
        paths: Array.isArray(ev.paths) ? ev.paths.map(String) : undefined,
      };
      const checkpoints = [
        entry,
        ...state.checkpoints.filter((c) => String(c.id) !== entry.id),
      ].slice(0, 50);
      const label = entry.label || entry.id;
      return {
        ...state,
        checkpoints,
        toasts: entry.auto
          ? state.toasts
          : pushToast(state.toasts, "success", `Checkpoint saved: ${label}`),
      };
    }
    case "checkpoint_restored":
      return {
        ...state,
        fileChangeSeq: state.fileChangeSeq + 1,
        recentFileChanges: [],
        toasts: pushToast(
          state.toasts,
          "success",
          `Restored checkpoint ${String(ev.id ?? "")}`,
        ),
      };
    case "checkpoints":
      return {
        ...state,
        checkpoints: Array.isArray(ev.checkpoints) ? (ev.checkpoints as typeof state.checkpoints) : [],
      };
    case "worktree_ready": {
      const runId = String(ev.run_id ?? "");
      const path = String(ev.path ?? "");
      const branch = ev.branch ? String(ev.branch) : undefined;
      const worktrees = [
        { run_id: runId, path, branch },
        ...state.worktrees.filter((w) => w.path !== path && w.run_id !== runId),
      ];
      return {
        ...state,
        worktrees,
        toasts: pushToast(state.toasts, "info", `Worktree ready: ${path}`),
      };
    }
    case "worktree_seeded": {
      const count = Array.isArray(ev.paths) ? ev.paths.length : 0;
      return {
        ...state,
        fileChangeSeq: state.fileChangeSeq + 1,
        recentFileChanges: [],
        toasts: pushToast(
          state.toasts,
          "info",
          `Seeded ${count} file${count === 1 ? "" : "s"} into worktree`,
        ),
      };
    }
    case "worktree_cleaned": {
      const path = String(ev.path ?? "");
      return {
        ...state,
        worktrees: state.worktrees.filter((w) => w.path !== path),
      };
    }
    case "worktree_promoted": {
      const runId = String(ev.run_id ?? "");
      return {
        ...state,
        worktrees: state.worktrees.filter((w) => w.run_id !== runId),
        fileChangeSeq: state.fileChangeSeq + 1,
        recentFileChanges: [],
        toasts: pushToast(state.toasts, "success", `Promoted worktree for ${runId}`),
      };
    }
    case "audit":
      // Decision log is for operators/sidecar; avoid toast spam on every gate.
      return state;
    case "cost_update":
      return {
        ...state,
        cost: {
          tokens_in: typeof ev.tokens_in === "number" ? ev.tokens_in : state.cost?.tokens_in,
          tokens_out: typeof ev.tokens_out === "number" ? ev.tokens_out : state.cost?.tokens_out,
          cached_tokens:
            typeof ev.cached_tokens === "number" ? ev.cached_tokens : state.cost?.cached_tokens,
          cache_hit_pct:
            ev.cache_hit_pct !== undefined ? ev.cache_hit_pct : state.cost?.cache_hit_pct,
          estimated_usd:
            ev.estimated_usd !== undefined ? ev.estimated_usd : state.cost?.estimated_usd,
          model: typeof ev.model === "string" ? ev.model : state.cost?.model,
        },
      };
    case "search_key_set":
      return {
        ...state,
        searchKeys: {
          ...state.searchKeys,
          [String(ev.provider ?? "")]: !!ev.has_key,
        },
      };
    case "plugin_commands":
      return {
        ...state,
        pluginCommands: Array.isArray(ev.commands) ? ev.commands : [],
      };
    case "plugin_status":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `${String(ev.plugin ?? "plugin")}: ${String(ev.text ?? "")}`,
        ),
      };
    case "session_changed":
      return {
        ...state,
        currentSessionFile: String(ev.path ?? state.currentSessionFile ?? ""),
      };
    case "session_change_failed":
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "error",
          String(ev.message ?? `Failed to change session ${ev.path ?? ""}`),
        ),
      };
    case "session_deleted": {
      const path = String(ev.path ?? "");
      return {
        ...state,
        sessions: state.sessions.filter((s) => (s.path ?? s.name) !== path && s.name !== path),
        toasts: pushToast(state.toasts, "info", `Deleted session ${path}`),
      };
    }
    case "session_pinned": {
      const path = String(ev.path ?? "");
      const pinned = !!ev.pinned;
      return {
        ...state,
        sessions: state.sessions.map((s) =>
          (s.path ?? s.name) === path || s.name === path ? { ...s, pinned } : s,
        ),
      };
    }
    case "session_recovered": {
      const warningCount = Array.isArray(ev.warnings) ? ev.warnings.length : 0;
      const interruptedCount = Array.isArray(ev.interrupted_runs)
        ? ev.interrupted_runs.length
        : 0;
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `Recovered session (${interruptedCount} interrupted run${interruptedCount === 1 ? "" : "s"}${warningCount ? `, ${warningCount} warning${warningCount === 1 ? "" : "s"}` : ""})`,
        ),
      };
    }
    case "summary_required": {
      // Core rejected bare finish without a user-facing completion summary and
      // is nudging the model to write one (auto-reflect gate). Surface a soft
      // status so the chat doesn't look idle while the continuation turn runs.
      const attempt = typeof ev.attempt === "number" ? ev.attempt : null;
      const max = typeof ev.max_attempts === "number" ? ev.max_attempts : null;
      const suffix =
        attempt != null && max != null ? ` (${attempt}/${max})` : "";
      return {
        ...state,
        toasts: pushToast(
          state.toasts,
          "info",
          `Writing completion summary${suffix}…`,
        ),
      };
    }
    case "advisor_note": {
      // Watchdog advisor recommendation. Surface concerns/blockers as warnings;
      // nits stay informational (matches TUI handlers).
      const severity = typeof ev.severity === "string" ? ev.severity : "";
      const message = typeof ev.message === "string" ? ev.message : "";
      if (!message) return state;
      const kind =
        severity === "concern" || severity === "blocker" ? "warning" : "info";
      const prefix = severity ? `Advisor (${severity})` : "Advisor";
      return {
        ...state,
        toasts: pushToast(state.toasts, kind, `${prefix}: ${message}`),
      };
    }
    case "advisor_status":
      // Reviewing/no-key state is available to protocol clients; keep the UI
      // quiet during normal reviews and surface only actionable notes.
      return state;

    default:
      return state;
  }
}

// Compile-time anchor: `CoreEventType` from the SDK is the authoritative
// catalog of every known core wire-event `type` string. The exhaustive
// switch in `reduce()` covers all 78 entries; the test in reducer.test.ts
// cross-checks against the SDK's `CORE_EVENT_TYPES` at runtime.
//
// Two bridge-generated events handled here that are NOT core wire events
// (and correctly absent from CORE_EVENT_TYPES):
//   - "projects"           (emitted by core-bridge.ts on project list/switch)
//   - "workspace_changed"  (emitted by core-bridge.ts on workspace switch)
//
// This import is type-only — zero runtime cost, browser-safe.
export type { CoreEventType };
