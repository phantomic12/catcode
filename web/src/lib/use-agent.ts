"use client";

// useAgent — the client hook that owns AgentState.
//
// Opens an EventSource to /api/stream, hydrates from the server snapshot, then
// reduces every live core event. Exposes typed actions (prompt, steer, abort,
// approve, setKey, …) that POST a raw core command to /api/command and apply the
// optimistic `_user` event locally (the bridge tracks it for snapshots).

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { reduce, initialState } from "./reducer";
import {
  getSandboxStatusCommand,
  prepareSandboxCommand,
  resetSandboxCommand,
} from "./commands";
import type { AgentState, ApproveDecision, CoreCommand, CoreEvent, CustomProviderDraft, LiveSessionStatus, ModelInfo, NotificationItem } from "./types";
import { setTabBadge, desktopNotify, isDesktopEnabled, attentionLabel } from "./notifications";
import { parseSharedSession } from "./session-share";

let notifSeq = 0;

// ponytail: localStorage persistence for UI preferences (model/thinking/approval).
// try/catch guards SSR + disabled-storage; failing silently is fine.
function lsGet(k: string): string | null {
  try {
    return typeof localStorage !== "undefined" ? localStorage.getItem(k) : null;
  } catch {
    return null;
  }
}
function lsSet(k: string, v: string): void {
  try {
    if (typeof localStorage !== "undefined") localStorage.setItem(k, v);
  } catch {
    /* ignore */
  }
}

function pickPreferredModel(models: ModelInfo[]): string | null {
  if (models.length === 0) return null;
  const preferred =
    models.find((m) => /glm[-_. ]?5/i.test(m.id)) ??
    models.find((m) => /5\.2|glm/i.test(m.id)) ??
    models[0];
  return preferred.id;
}

export interface AgentApi {
  state: AgentState;
  connected: boolean;
  /** Forward any core command (optimistic for send/steer). Returns false on failure. */
  send: (cmd: CoreCommand) => Promise<boolean>;
  prompt: (text: string, images?: string[]) => Promise<boolean>;
  steer: (text: string) => Promise<boolean>;
  /** PI-compatible bang bash (`!cmd` / `!!cmd`). */
  userBash: (command: string, excludeFromContext?: boolean) => Promise<void>;
  abort: () => Promise<void>;
  /** Drop a queued follow-up/steer without aborting the running turn. */
  clearQueue: () => Promise<void>;
  approve: (decision: ApproveDecision, opts?: { pattern?: string }) => Promise<void>;
  setKey: (key: string) => Promise<void>;
  setProvider: (name: string) => Promise<void>;
  login: (preset: string, key?: string) => Promise<void>;
  /** Add or update a custom provider with full config.json parity. */
  addCustomProvider: (draft: CustomProviderDraft) => Promise<void>;
  /** Discover models from an endpoint (preview, no persist) for the custom-provider form. */
  discoverProviderModels: (
    base_url: string,
    kind: "openai" | "anthropic",
    api_key?: string,
    headers?: Record<string, string>,
  ) => Promise<void>;
  /** Clear the provider-models preview (when the modal closes). */
  clearProviderModelsPreview: () => void;
  loginOauth: (preset: string) => Promise<void>;
  logout: (provider: string) => Promise<void>;
  listProviderPresets: () => Promise<void>;
  setModel: (id: string) => void;
  setThinking: (level: string) => void;
  setApproval: (mode: "never" | "destructive" | "always") => Promise<void>;
  newSession: () => Promise<void>;
  loadSession: (path: string, workspace?: string) => Promise<void>;
  listSessions: () => Promise<void>;
  compact: (instructions?: string) => Promise<void>;
  reset: () => Promise<void>;
  stats: () => Promise<void>;
  context: () => Promise<void>;
  dismissToast: (id: string) => void;
  // ── Cross-session notifications ──
  /** Mark a single feed notification read (e.g. on click). */
  dismissNotification: (id: string) => void;
  /** Mark every feed notification read (e.g. when the bell opens). */
  markAllNotificationsRead: () => void;
  /** Clear the entire feed. */
  clearNotifications: () => void;
  // ── Subagent / intercom ──
  intercomReply: (reply: string) => Promise<void>;
  // ── Ask tool ──
  askReply: (answers: Record<string, string> | null) => Promise<void>;
  // ── Sudo passthrough ──
  sudoReply: (approved: boolean, password?: string) => Promise<void>;
  // ── OAuth ──
  submitOauthCode: (code: string) => Promise<void>;
  // ── Search tool API keys (Exa / Tavily) ──
  setSearchKey: (provider: string, apiKey: string) => Promise<void>;
  dismissOauth: () => void;
  // ── Turn / history ──
  undo: () => Promise<boolean>;
  clear: () => Promise<void>;
  // ── Memory ──
  saveMemory: (text: string, tags?: string[], scope?: "workspace" | "global") => Promise<void>;
  listMemory: () => Promise<void>;
  forgetMemory: (id: string) => Promise<void>;
  // ── Plugins ──
  installPlugin: (path: string, scope?: "workspace" | "global") => Promise<void>;
  removePlugin: (name: string) => Promise<void>;
  enablePlugin: (name: string) => Promise<void>;
  disablePlugin: (name: string) => Promise<void>;
  listPlugins: () => Promise<void>;
  pluginTrustDecisions: (decisions: Record<string, "trust" | "deny">) => Promise<void>;
  listAgents: () => Promise<void>;
  // ── Usage ──
  usage: (model?: string) => Promise<void>;
  // ── Skills ──
  listSkills: () => Promise<void>;
  applySkill: (name: string, task?: string) => Promise<void>;
  listMarketplaceSkills: () => Promise<void>;
  acceptSkillDisclaimer: () => Promise<void>;
  searchMarketplaceSkills: (query: string) => Promise<void>;
  installMarketplaceSkill: (source: string, name: string, scope: "project" | "global") => Promise<void>;
  updateMarketplaceSkill: (name: string, scope: "project" | "global") => Promise<void>;
  removeMarketplaceSkill: (name: string, scope: "project" | "global") => Promise<void>;
  // ── Goal mode ──
  startGoal: (opts: {
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
  }) => Promise<void>;
  cancelGoal: () => Promise<void>;
  approveGoalPlan: () => Promise<void>;
  reviseGoal: (feedback: string) => Promise<void>;
  goalStatus: () => Promise<void>;
  // ── Vision ──
  getVisionConfig: () => Promise<void>;
  setVisionConfig: (
    vision_model: string | null,
    vision_models?: string[],
    enabled?: boolean,
  ) => Promise<void>;
  // ── Config ──
  setConfig: (key: string, value: string | number | boolean) => Promise<void>;
  // ── Memory extras ──
  refreshMemory: () => Promise<void>;
  // ── Models ──
  /** Force-refresh the multi-provider model cache (`refresh_models`). */
  refreshModels: () => Promise<void>;
  // ── Projects / workspace ──
  switchWorkspace: (path: string) => Promise<void>;
  renameSession: (name: string, title: string) => Promise<void>;
  listProjects: () => Promise<void>;
  addProject: (path: string) => Promise<void>;
  removeProject: (path: string) => Promise<void>;
  // ── Session lifecycle ──
  deleteSession: (path: string) => Promise<void>;
  pinSession: (path: string, pinned: boolean) => Promise<void>;
  // ── Checkpoints ──
  createCheckpoint: (label?: string, paths?: string[]) => Promise<void>;
  listCheckpoints: () => Promise<void>;
  restoreCheckpoint: (id: string) => Promise<void>;
  // ── Sandbox (Microsandbox) ──
  /** Re-run preflight; the core replies with a `sandbox_status` event. */
  getSandboxStatus: () => Promise<void>;
  /** Begin user-space runtime/image prep (`prepare_sandbox`). */
  prepareSandbox: () => Promise<void>;
  /** Reset an unhealthy sandbox (`reset_sandbox`). */
  resetSandbox: () => Promise<void>;
  // ── Connection ──
  reconnect: () => void;
  // ── Utility ──
  copyLastReply: () => void;
  exportTranscript: () => string;
}

export function useAgent(): AgentApi {
  const [state, setState] = useState<AgentState>(initialState);
  const [connected, setConnected] = useState(false);
  const [reconnectKey, setReconnectKey] = useState(0);
  const stateRef = useRef(state);
  useEffect(() => {
    stateRef.current = state;
  }, [state]);

  // Multi-session: the client views ONE session at a time but many can be live
  // server-side. `streamSessionId` is the session the SSE stream is connected to
  // (null = the default workspace session); changing it reopens the stream.
  // `activeRef`/`workspaceRef` route commands to the viewed session and are kept
  // in sync from snapshots WITHOUT reopening the stream.
  const sharedTarget = useMemo(
    () => (typeof window === "undefined" ? null : parseSharedSession(window.location.search)),
    [],
  );
  const [streamSessionId, setStreamSessionId] = useState<string | null>(
    sharedTarget?.session ?? null,
  );
  const activeRef = useRef<string | null>(sharedTarget?.session ?? null);
  const workspaceRef = useRef<string>(sharedTarget?.workspace ?? "");
  /** Bumps on each stream effect so stale EventSource handlers are ignored. */
  const streamGenRef = useRef(0);
  /** Serializes undo so rapid double-/undo can't soft+hard wipe. */
  const undoBusyRef = useRef(false);
  /** Suppress repeated 502 toasts while EventSource hammers reconnect. */
  const lastStreamErrToastRef = useRef(0);
  /** Previous liveSessions map — used to detect background-session transitions
   *  (finished / needs attention) for the notification feed. Reset to null on
   *  (re)connect so the first status after a snapshot primes without flooding. */
  const prevLiveRef = useRef<Record<string, LiveSessionStatus> | null>(null);
  /** Notification ids already surfaced to the desktop, so a refreshed (deduped)
   *  unread item doesn't re-fire an OS notification. */
  const seenNotifIdsRef = useRef<Set<string>>(new Set());
  /** Stable ref to loadSession so desktop-notification click handlers navigate
   *  without re-running the notifications effect. */
  const loadSessionRef = useRef<(path: string, workspace?: string) => void>(() => {});
  useEffect(() => {
    if (state.workspace) workspaceRef.current = state.workspace;
    if (state.currentSessionFile) activeRef.current = state.currentSessionFile;
  }, [state.workspace, state.currentSessionFile]);

  // Stream connection + reducer. Re-runs when the viewed session changes
  // (streamSessionId) or on manual reconnect. EventSource auto-reconnects on
  // transient drops; reconnectKey gives the user an explicit force-fresh option.
  useEffect(() => {
    let url = "/api/stream";
    if (streamSessionId) {
      const params = new URLSearchParams();
      params.set("session", streamSessionId);
      if (workspaceRef.current) params.set("workspace", workspaceRef.current);
      url = `/api/stream?${params.toString()}`;
    }
    const gen = ++streamGenRef.current;
    const es = new EventSource(url);
    es.onopen = () => {
      if (streamGenRef.current !== gen) return;
      setConnected(true);
    };
    es.onerror = () => {
      if (streamGenRef.current !== gen) return;
      setConnected(false);
      setState((s) => (s.switching ? { ...s, switching: false } : s));
      void fetch(url, { method: "GET", headers: { Accept: "text/event-stream" }, cache: "no-store" })
        .then(async (res) => {
          if (streamGenRef.current !== gen) {
            void res.body?.cancel();
            return;
          }
          if (res.status === 401 && typeof window !== "undefined") {
            void res.body?.cancel();
            es.close();
            window.location.href = "/login";
            return;
          }
          const ct = res.headers.get("content-type") || "";
          // A live SSE body means EventSource already has a healthy stream —
          // cancel this probe immediately so we don't leak a second subscriber.
          if (ct.includes("text/event-stream")) {
            void res.body?.cancel();
            return;
          }
          if (res.status >= 400) {
            const now = Date.now();
            if (now - lastStreamErrToastRef.current < 8000) return;
            lastStreamErrToastRef.current = now;
            let msg = `Connection failed (${res.status})`;
            try {
              const body = (await res.json()) as { error?: string };
              if (body.error) msg = body.error;
            } catch {
              /* keep status message */
            }
            setState((s) => reduce(s, { type: "error", message: msg }));
          } else {
            void res.body?.cancel();
          }
        })
        .catch(() => {
          /* transient — EventSource retries */
        });
    };
    es.onmessage = (e) => {
      if (streamGenRef.current !== gen) return;
      let ev: CoreEvent | { type: "_snapshot"; state: AgentState };
      try {
        ev = JSON.parse(e.data);
      } catch {
        return;
      }
      if (ev.type === "_snapshot") {
        const snap = (ev as { state: AgentState }).state;
        const t = lsGet("umans:thinking");
        const m = lsGet("umans:model");
        // Functional merge: preserve in-flight client undo (and its trimmed
        // transcript) so a reconnect mid-undo can't lose pendingUndo and then
        // wipe messages on the following reset.
        // Prime the liveSessions diff so reconnect/switch doesn't flood the feed
        // with transitions that happened while disconnected.
        prevLiveRef.current = null;
        setState((s) => {
          // Keep client trim only while undo is still in flight on the server
          // (snapshot still has the longer pre-undo transcript). Once the server
          // has applied undo, trust snap.pendingUndo so Reset isn't soft-stuck.
          const undoInFlight =
            s.pendingUndo && snap.messages.length > s.messages.length;
          return {
            ...snap,
            goalStepFinals: snap.goalStepFinals ?? {},
            pendingUndo: undoInFlight || snap.pendingUndo,
            messages: undoInFlight ? s.messages : snap.messages,
            thinkingLevel: t ?? snap.thinkingLevel,
            selectedModel:
              m && snap.models.some((x) => x.id === m) ? m : snap.selectedModel,
            // Preserve the client-only notification feed (snap has none — the
            // server never populates it). liveSessions comes from snap (the
            // bridge seeds it on subscribe).
            notifications: s.notifications,
          };
        });
      } else {
        setState((s) => reduce(s, ev as CoreEvent));
      }
    };
    return () => {
      es.close();
    };
  }, [streamSessionId, reconnectKey]);

  // Auto-select a model once they arrive and none is chosen. Prefer a saved
  // selection from localStorage so the user's last model persists across reloads.
  useEffect(() => {
    if (stateRef.current.selectedModel || stateRef.current.models.length === 0) return;
    const saved = lsGet("umans:model");
    const id =
      saved && stateRef.current.models.some((m) => m.id === saved)
        ? saved
        : pickPreferredModel(stateRef.current.models);
    if (id) setState((s) => reduce(s, { type: "_select_model", id }));
  }, [state.models]);

  // Restore thinking preference on mount.
  useEffect(() => {
    const t = lsGet("umans:thinking");
    if (t) setState((s) => reduce(s, { type: "_set_thinking", level: t }));
  }, []);

  // ── Cross-session notifications ──
  // Detect background-session transitions from the liveSessions map and append
  // feed items (a background session finished its turn, or now needs the user).
  // The currently-viewed session is never notified — its attention UI is already
  // inline. prevLiveRef is reset on (re)connect so the first status after a
  // snapshot primes without emitting a flood of stale transitions.
  useEffect(() => {
    const next = state.liveSessions ?? {};
    const prev = prevLiveRef.current;
    prevLiveRef.current = next;
    if (!prev) return; // priming after (re)connect
    const viewed = state.currentSessionFile;
    const items: NotificationItem[] = [];
    for (const [file, s] of Object.entries(next)) {
      const before = prev[file];
      if (!before) continue; // newly observed this run — prime
      if (file === viewed) continue; // never notify about the viewed session
      const attentionEntered = !before.needsAttention && s.needsAttention;
      const finished = before.streaming && !s.streaming && !s.needsAttention;
      if (!attentionEntered && !finished) continue;
      items.push({
        id: `n_${Date.now().toString(36)}_${(notifSeq++).toString(36)}`,
        sessionFile: file,
        workspace: s.workspace,
        title: s.title ?? file.split(/[\\/]/).pop() ?? file,
        kind: attentionEntered ? "attention" : "finished",
        attentionKind: attentionEntered ? s.attentionKind : undefined,
        ts: Date.now(),
        read: false,
      });
    }
    if (items.length) {
      setState((st) => reduce(st, { type: "_add_notifications", items }));
    }
  }, [state.currentSessionFile, state.liveSessions]);

  // Drive the tab badge + OS desktop notifications from the feed. New (unseen)
  // items fire a desktop notification when the user opted in; the tab badge
  // always reflects the unread count (no permission needed).
  useEffect(() => {
    const unread = state.notifications.filter((n) => !n.read);
    const hasAttention = unread.some((n) => n.kind === "attention");
    setTabBadge(unread.length, hasAttention);

    const seen = seenNotifIdsRef.current;
    const fresh = state.notifications.filter((n) => !seen.has(n.id));
    if (fresh.length && isDesktopEnabled()) {
      for (const n of fresh) {
        const body =
          n.kind === "attention"
            ? attentionLabel(n.attentionKind)
            : "finished its turn";
        desktopNotify({
          title: n.title || "Session",
          body,
          tag: `session:${n.sessionFile}:${n.kind}`,
          requireInteraction: n.kind === "attention",
          onClick: () => loadSessionRef.current(n.sessionFile, n.workspace),
        });
      }
    }
    seenNotifIdsRef.current = new Set(state.notifications.map((n) => n.id));
  }, [state.notifications]);

  const post = useCallback(
    async (
      cmd: CoreCommand,
    ): Promise<{ ok?: boolean; session?: string; workspace?: string; error?: string }> => {
      // Attach routing metadata so the bridge targets the viewed session.
      const body: Record<string, unknown> = { ...cmd };
      if (activeRef.current) body.session = activeRef.current;
      if (workspaceRef.current) body.workspace = workspaceRef.current;
      try {
        const res = await fetch("/api/command", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        });
        const data = (await res.json().catch(() => ({}))) as {
          ok?: boolean;
          session?: string;
          workspace?: string;
          error?: string;
        };
        if (!res.ok) {
          return {
            ...data,
            ok: false,
            error: data.error || `Request failed (${res.status})`,
          };
        }
        if (data.ok === false || data.error) {
          return { ...data, ok: false, error: data.error || "Command failed" };
        }
        return { ...data, ok: true };
      } catch (e) {
        return {
          ok: false,
          error: e instanceof Error ? e.message : "Network error",
        };
      }
    },
    [],
  );

  // Mirror the live gate into localStorage once the session is ready. Do NOT
  // push localStorage → set_approval here: that fought settings.json (TUI
  // writes never; a stale umans:approval=destructive would reset the core on
  // every tab open / session switch). Spawn already boots from settings.json.
  useEffect(() => {
    const a = state.approvalMode;
    if (a === "never" || a === "destructive" || a === "always") {
      lsSet("umans:approval", a);
    }
  }, [state.approvalMode]);

  // Switch the viewed session: reopen the SSE stream for it (the bridge starts
  // its core if it isn't already live). Other live sessions keep running.
  // Approval comes from settings.json on spawn — no localStorage override.
  const switchToSession = useCallback(
    (sessionFile: string, workspace?: string) => {
      // Clicking the already-active session must no-op — otherwise we set
      // switching:true without reopening the SSE stream (same streamSessionId)
      // and the UI wedges on "Loading session…".
      if (
        sessionFile === activeRef.current &&
        (!workspace || workspace === workspaceRef.current) &&
        !stateRef.current.switching
      ) {
        return;
      }
      activeRef.current = sessionFile;
      if (workspace) workspaceRef.current = workspace;
      setState((s) => ({
        ...s,
        messages: [],
        currentAssistantId: null,
        streaming: false,
        retrying: false,
        followUpQueued: false,
        pendingUndo: false,
        pendingApproval: null,
        pendingPluginTrust: null,
        pendingIntercom: null,
        pendingSudo: null,
        pendingOauth: null,
        workState: null,
        goalMode: null,
        goalPlan: null,
        goalIterations: [],
        subagentRuns: {},
        metrics: null,
        currentSessionFile: sessionFile,
        switching: true,
        workspace: workspace ?? s.workspace,
      }));
      setStreamSessionId(sessionFile);
    },
    [],
  );

  const send = useCallback(
    async (cmd: CoreCommand): Promise<boolean> => {
      const optimistic = cmd.type === "send" || cmd.type === "steer";
      if (optimistic) {
        setState((s) =>
          reduce(s, {
            type: "_user",
            text: cmd.prompt,
            model: cmd.model,
            steer: cmd.type === "steer",
            ...(cmd.type === "send" && cmd.images?.length ? { images: cmd.images } : {}),
          }),
        );
      }
      const r = await post(cmd);
      if (r.ok === false || r.error) {
        setState((s) => {
          let messages = s.messages;
          if (optimistic) {
            // Roll back the optimistic bubble (last matching user line).
            for (let i = messages.length - 1; i >= 0; i--) {
              const m = messages[i];
              if (m.role === "user" && m.text === cmd.prompt) {
                messages = [...messages.slice(0, i), ...messages.slice(i + 1)];
                break;
              }
            }
          }
          return reduce(
            { ...s, messages },
            {
              type: "error",
              message: optimistic ? "Failed to send command" : r.error || "Command failed",
            },
          );
        });
        return false;
      }
      return true;
    },
    [post],
  );

  // Refresh the discoverable-skills list when a turn ends (streaming → idle)
  // so a skill created mid-session (e.g. by /reflect or /index) shows up in the
  // /skill:<name> autocomplete without a reconnect. Skips the initial false→false.
  const wasStreamingRef = useRef(false);
  useEffect(() => {
    if (wasStreamingRef.current && !state.streaming) {
      post({ type: "list_skills" }).catch(() => {});
    }
    wasStreamingRef.current = state.streaming;
  }, [state.streaming, post]);

  const effortFor = useCallback((level: string): string | undefined => {
    return level === "off" ? undefined : level;
  }, []);

  const prompt = useCallback(
    async (text: string, images?: string[]): Promise<boolean> => {
      const s = stateRef.current;
      const model = s.selectedModel ?? s.models[0]?.id ?? "";
      const cmd: CoreCommand = { type: "send", prompt: text, model };
      const eff = effortFor(s.thinkingLevel);
      if (eff) cmd.reasoning_effort = eff;
      if (images?.length) cmd.images = images;
      return send(cmd);
    },
    [send, effortFor],
  );

  const steer = useCallback(
    async (text: string): Promise<boolean> => {
      const s = stateRef.current;
      const model = s.selectedModel ?? s.models[0]?.id ?? "";
      const cmd: CoreCommand = { type: "steer", prompt: text, model };
      const eff = effortFor(s.thinkingLevel);
      if (eff) cmd.reasoning_effort = eff;
      return send(cmd);
    },
    [send, effortFor],
  );

  const userBash = useCallback(
    async (command: string, excludeFromContext = false) => {
      await send({
        type: "user_bash",
        command,
        exclude_from_context: excludeFromContext,
      });
    },
    [send],
  );

  const abort = useCallback(async (): Promise<void> => {
    await send({ type: "abort" });
  }, [send]);
  const clearQueue = useCallback(async (): Promise<void> => {
    await send({ type: "clear_queue" });
  }, [send]);

  /** Fire-and-forget wrapper so AgentApi void methods don't leak Promise<boolean>. */
  const fire = useCallback(async (cmd: CoreCommand): Promise<void> => {
    await send(cmd);
  }, [send]);

  const approve = useCallback(
    async (decision: ApproveDecision, opts?: { pattern?: string }) => {
      const s = stateRef.current;
      const req = s.pendingApproval;
      if (!req) return;
      const sessionAtClick = s.currentSessionFile;
      setState((st) => ({ ...st, pendingApproval: null }));
      const r = await post({
        type: "approve",
        request_id: req.request_id,
        decision,
        ...(opts?.pattern ? { pattern: opts.pattern } : {}),
      });
      if (r.ok === false || r.error) {
        // Restore only if still on the same session (don't poison a switch).
        setState((st) => {
          if (st.currentSessionFile !== sessionAtClick) {
            return reduce(st, { type: "error", message: r.error || "Failed to send approval" });
          }
          return reduce(
            { ...st, pendingApproval: st.pendingApproval ?? req },
            { type: "error", message: r.error || "Failed to send approval" },
          );
        });
      }
    },
    [post],
  );

  const setKey = useCallback(
    async (key: string) => {
      const s = stateRef.current;
      await send({ type: "set_key", api_key: key, provider: s.provider || undefined });
      // Re-discover models now that a key is set (the core doesn't on set_key).
      if (s.models.length === 0 && s.provider) {
        await post({ type: "set_provider", name: s.provider });
      }
    },
    [send, post],
  );

  // Log in to a first-party provider. Requires an explicitly pasted API key
  // (or stored OAuth creds from a prior login in this app). Does not scan env.
  const login = useCallback(
    async (preset: string, key?: string) => {
      await send({ type: "login", preset, api_key: key });
    },
    [send],
  );

  // Add or update a custom provider (name/kind/base_url/key|env/headers/ctx)
  // — the same fields hand-editing config.json supports. The core persists to
  // config.json, makes it the active provider, and re-discovers models.
  const addCustomProvider = useCallback(
    async (d: CustomProviderDraft) => {
      const headers: Record<string, string> = {};
      for (const line of d.headersText.split(/\n+/)) {
        const i = line.indexOf(":");
        if (i <= 0) continue;
        const k = line.slice(0, i).trim();
        const v = line.slice(i + 1).trim();
        if (k && v) headers[k] = v;
      }
      const ctx = parseInt(d.contextWindow.trim(), 10);
      await send({
        type: "add_custom_provider",
        name: d.name.trim(),
        base_url: d.base_url.trim(),
        kind: d.kind,
        api_key: d.apiKey.trim() || undefined,
        api_key_env: d.apiKeyEnv.trim() || undefined,
        headers: Object.keys(headers).length ? headers : undefined,
        context_window: Number.isFinite(ctx) && ctx > 0 ? ctx : undefined,
        models_override: d.modelsOverride.length ? d.modelsOverride : undefined,
      });
      // Refresh the preset/picker list (logged-in/configured flags).
      await post({ type: "list_provider_presets" });
    },
    [send, post],
  );

  // Discover models from an endpoint WITHOUT persisting a provider — a preview
  // for the add-custom-provider form. The core emits provider_models_preview.
  const discoverProviderModels = useCallback(
    async (
      base_url: string,
      kind: "openai" | "anthropic",
      api_key?: string,
      headers?: Record<string, string>,
    ) => {
      await send({
        type: "discover_provider_models",
        base_url: base_url.trim(),
        kind,
        api_key: api_key?.trim() || undefined,
        headers: headers && Object.keys(headers).length ? headers : undefined,
      });
    },
    [send],
  );

  const clearProviderModelsPreview = useCallback(() => {
    setState((s) => reduce(s, { type: "_clear_provider_models_preview" }));
  }, []);

  // Start this app's OAuth subscription login (browser / device-code).
  const loginOauth = useCallback(
    async (preset: string) => {
      await send({ type: "login_oauth", preset });
    },
    [send],
  );

  // Switch the default/fallback provider (a turn still routes per-model, but
  // compaction/legacy fallback uses this one).
  const setProvider = useCallback(async (name: string) => {
    await send({ type: "set_provider", name });
  }, [send]);

  // Log out of a provider: the core drops its key/config and re-aggregates.
  const logout = useCallback(async (provider: string) => {
    await send({ type: "logout", provider });
  }, [send]);

  // Ask the core for the latest preset list (configured/hasKey/loggedIn flags).
  const listProviderPresets = useCallback(async () => {
    await post({ type: "list_provider_presets" });
  }, [post]);

  const setModel = useCallback((id: string) => {
    lsSet("umans:model", id);
    setState((s) => reduce(s, { type: "_select_model", id }));
  }, []);

  const setThinking = useCallback((level: string) => {
    lsSet("umans:thinking", level);
    setState((s) => reduce(s, { type: "_set_thinking", level }));
  }, []);

  const setApproval = useCallback(
    async (mode: "never" | "destructive" | "always") => {
      lsSet("umans:approval", mode);
      await send({ type: "set_approval", mode });
    },
    [send],
  );

  const newSession = useCallback(async () => {
    const r = await post({ type: "new_session" });
    if (r.ok === false || r.error) {
      setState((s) => reduce(s, { type: "error", message: r.error || "Failed to create session" }));
      return;
    }
    if (r.session) switchToSession(r.session, r.workspace);
  }, [post, switchToSession]);
  const loadSession = useCallback(
    (path: string, workspace?: string): Promise<void> => {
      switchToSession(path, workspace);
      // Opening a session clears its pending notifications (they're now seen).
      setState((s) => ({
        ...s,
        notifications: s.notifications.map((n) =>
          n.sessionFile === path ? { ...n, read: true } : n,
        ),
      }));
      return Promise.resolve();
    },
    [switchToSession],
  );
  // Keep the ref current so desktop-notification click handlers can navigate.
  useEffect(() => {
    loadSessionRef.current = loadSession;
  }, [loadSession]);
  const listSessions = useCallback(() => fire({ type: "list_sessions" }), [fire]);
  const compact = useCallback(
    (instructions?: string) =>
      fire({ type: "compact", ...(instructions ? { instructions } : {}) }),
    [fire],
  );
  const wipeLocalTranscript = useCallback(() => {
    setState((s) => ({
      ...s,
      pendingUndo: false,
      messages: [],
      currentAssistantId: null,
      streaming: false,
      followUpQueued: false,
      pendingApproval: null,
      pendingAsk: null,
      pendingSudo: null,
      pendingIntercom: null,
      pendingOauth: null,
      pendingPluginTrust: null,
      workState: null,
      goalMode: null,
      goalPlan: null,
      goalIterations: [],
      subagentRuns: {},
      metrics: null,
    }));
  }, []);

  const reset = useCallback(async () => {
    if (undoBusyRef.current) {
      setState((st) =>
        reduce(st, { type: "error", message: "Wait for undo to finish before resetting" }),
      );
      return;
    }
    const s0 = stateRef.current;
    if (
      s0.streaming ||
      s0.pendingApproval ||
      s0.pendingAsk ||
      s0.pendingSudo ||
      s0.pendingIntercom
    ) {
      setState((st) =>
        reduce(st, {
          type: "error",
          message: "Stop or resolve the pending turn before resetting",
        }),
      );
      return;
    }
    // Drop pendingUndo so the following core `reset` hard-clears (undo soft-path
    // must not swallow an intentional wipe).
    wipeLocalTranscript();
    await send({ type: "reset" });
  }, [send, wipeLocalTranscript]);
  const stats = useCallback(() => fire({ type: "stats" }), [fire]);
  const context = useCallback(() => fire({ type: "context" }), [fire]);

  const dismissToast = useCallback((id: string) => {
    setState((s) => reduce(s, { type: "_dismiss_toast", id }));
  }, []);

  // ── Cross-session notifications ──
  const dismissNotification = useCallback((id: string) => {
    setState((s) => reduce(s, { type: "_dismiss_notification", id }));
  }, []);
  const markAllNotificationsRead = useCallback(() => {
    setState((s) => reduce(s, { type: "_mark_notifications_read" }));
  }, []);
  const clearNotifications = useCallback(() => {
    setState((s) => reduce(s, { type: "_clear_notifications" }));
  }, []);

  // ── Subagent / intercom ──
  const intercomReply = useCallback(
    async (reply: string) => {
      const s = stateRef.current;
      const req = s.pendingIntercom;
      if (!req) return;
      const sessionAtClick = s.currentSessionFile;
      setState((st) => ({ ...st, pendingIntercom: null }));
      const r = await post({ type: "intercom_reply", request_id: req.request_id, reply });
      if (r.ok === false || r.error) {
        setState((st) =>
          reduce(
            {
              ...st,
              pendingIntercom:
                st.currentSessionFile === sessionAtClick
                  ? st.pendingIntercom ?? req
                  : st.pendingIntercom,
            },
            { type: "error", message: r.error || "Failed to send reply" },
          ),
        );
      }
    },
    [post],
  );

  const askReply = useCallback(
    async (answers: Record<string, string> | null) => {
      const s = stateRef.current;
      const req = s.pendingAsk;
      if (!req) return;
      const sessionAtClick = s.currentSessionFile;
      setState((st) => ({ ...st, pendingAsk: null }));
      const r = await post({ type: "ask_reply", request_id: req.request_id, answers });
      if (r.ok === false || r.error) {
        setState((st) =>
          reduce(
            {
              ...st,
              pendingAsk:
                st.currentSessionFile === sessionAtClick
                  ? st.pendingAsk ?? req
                  : st.pendingAsk,
            },
            { type: "error", message: r.error || "Failed to send answers" },
          ),
        );
      }
    },
    [post],
  );

  const sudoReply = useCallback(
    async (approved: boolean, password?: string) => {
      const s = stateRef.current;
      const req = s.pendingSudo;
      if (!req) return;
      const sessionAtClick = s.currentSessionFile;
      setState((st) => ({ ...st, pendingSudo: null }));
      const r = await post({
        type: "sudo_reply",
        request_id: req.request_id,
        approved,
        ...(password ? { password } : {}),
      });
      if (r.ok === false || r.error) {
        setState((st) =>
          reduce(
            {
              ...st,
              pendingSudo:
                st.currentSessionFile === sessionAtClick
                  ? st.pendingSudo ?? req
                  : st.pendingSudo,
            },
            { type: "error", message: r.error || "Failed to send sudo reply" },
          ),
        );
      }
    },
    [post],
  );

  // ── OAuth ──
    // ── Search tool ──
  // Set or clear a web_search API key (Exa/Tavily). Empty apiKey clears it.
  const setSearchKey = useCallback(
    async (provider: string, apiKey: string) => {
      await send({ type: "set_search_key", provider, api_key: apiKey });
    },
    [send],
  );

  // Complete a no-browser (manual-code) OAuth login by pasting the code or
  // final callback URL the provider returned. Mirrors the TUI's /oauth-code.
  const submitOauthCode = useCallback(
    async (code: string) => {
      const v = code.trim();
      if (!v) return;
      const r = await post({ type: "oauth_code", code: v });
      if (r.ok === false || r.error) {
        setState((s) =>
          reduce(s, { type: "error", message: r.error || "Failed to submit OAuth code" }),
        );
      }
    },
    [post],
  );
  const dismissOauth = useCallback(() => {
    // Hide the banner only — the core keeps its pending login until oauth_code
    // or a fresh /login. Surface a hint so the user can finish via /oauth-code.
    setState((s) =>
      reduce(
        { ...s, pendingOauth: null },
        {
          type: "info",
          message: "OAuth still pending in the core — paste the code with /oauth-code when ready",
        },
      ),
    );
  }, []);

  // ── Turn / history ──
  const undo = useCallback(async () => {
    if (undoBusyRef.current) return false;
    const s0 = stateRef.current;
    // Core stays blocked on HITL / mid-turn until approve…/abort —
    // undoing would clear Stop/banners and wedge or corrupt the live turn.
    if (
      s0.streaming ||
      s0.pendingApproval ||
      s0.pendingAsk ||
      s0.pendingSudo ||
      s0.pendingIntercom
    ) {
      setState((st) =>
        reduce(st, {
          type: "error",
          message: s0.streaming
            ? "Stop the running turn before undoing"
            : "Resolve or abort the pending prompt before undoing",
        }),
      );
      return false;
    }
    undoBusyRef.current = true;
    const before = s0.messages;
    const sessionAtClick = s0.currentSessionFile;
    setState((s) => reduce(s, { type: "_undo_local" }));
    try {
      const r = await post({ type: "undo" });
      // Session switched mid-flight — don't claim success (edit/regen would
      // re-prompt the wrong session) and don't restore A's transcript into B.
      if (stateRef.current.currentSessionFile !== sessionAtClick) {
        setState((s) => ({ ...s, pendingUndo: false }));
        return false;
      }
      if (r.ok === false || r.error) {
        setState((s) =>
          reduce(
            { ...s, messages: before, pendingUndo: false },
            { type: "error", message: r.error || "Undo failed" },
          ),
        );
        return false;
      }
      return true;
    } finally {
      undoBusyRef.current = false;
    }
  }, [post]);

  const clear = useCallback(async () => {
    if (undoBusyRef.current) {
      setState((st) =>
        reduce(st, { type: "error", message: "Wait for undo to finish before clearing" }),
      );
      return;
    }
    const s0 = stateRef.current;
    if (
      s0.streaming ||
      s0.pendingApproval ||
      s0.pendingAsk ||
      s0.pendingSudo ||
      s0.pendingIntercom
    ) {
      setState((st) =>
        reduce(st, {
          type: "error",
          message: "Stop or resolve the pending turn before clearing",
        }),
      );
      return;
    }
    wipeLocalTranscript();
    await send({ type: "clear" });
  }, [send, wipeLocalTranscript]);

  // ── Memory ──
  const saveMemory = useCallback(
    async (text: string, tags?: string[], scope?: "workspace" | "global") => {
      await send({ type: "save_memory", text, tags, ...(scope ? { scope } : {}) });
      await post({ type: "list_memory" });
    },
    [send, post],
  );
  const listMemory = useCallback(() => fire({ type: "list_memory" }), [fire]);
  const forgetMemory = useCallback(
    async (id: string) => {
      await send({ type: "forget_memory", id });
      await post({ type: "list_memory" });
    },
    [send, post],
  );

  // ── Plugins ──
  const installPlugin = useCallback(
    async (path: string, scope?: "workspace" | "global") => {
      await send({ type: "install_plugin", path, ...(scope ? { scope } : {}) });
      await post({ type: "list_plugins" });
    },
    [send, post],
  );
  const removePlugin = useCallback(
    async (name: string) => {
      await send({ type: "remove_plugin", name });
      await post({ type: "list_plugins" });
    },
    [send, post],
  );
  const enablePlugin = useCallback(
    async (name: string) => {
      await send({ type: "enable_plugin", name });
      await post({ type: "list_plugins" });
    },
    [send, post],
  );
  const disablePlugin = useCallback(
    async (name: string) => {
      await send({ type: "disable_plugin", name });
      await post({ type: "list_plugins" });
    },
    [send, post],
  );
  const pluginTrustDecisions = useCallback(
    async (decisions: Record<string, "trust" | "deny">) => {
      await send({ type: "plugin_trust_decisions", decisions });
    },
    [send],
  );
  const listPlugins = useCallback(() => fire({ type: "list_plugins" }), [fire]);
  const listAgents = useCallback(() => fire({ type: "list_agents" }), [fire]);
  const refreshMemory = useCallback(() => fire({ type: "refresh_memory" }), [fire]);
  const refreshModels = useCallback(async () => {
    // Optimistic spinner; cleared by the terminal `models_refreshed` event
    // (or immediately if the post fails before core sees the command).
    setState((s) => reduce(s, { type: "_set_models_refreshing", refreshing: true }));
    try {
      const ok = await send({ type: "refresh_models" });
      if (!ok) {
        setState((s) => reduce(s, { type: "_set_models_refreshing", refreshing: false }));
      }
    } catch {
      setState((s) => reduce(s, { type: "_set_models_refreshing", refreshing: false }));
    }
  }, [send]);

  // ── Skills ──
  const listSkills = useCallback(() => fire({ type: "list_skills" }), [fire]);
  const listMarketplaceSkills = useCallback(() => fire({ type: "list_marketplace_skills" }), [fire]);
  const acceptSkillDisclaimer = useCallback(async () => { await send({ type: "accept_skill_disclaimer" }); }, [send]);
  const searchMarketplaceSkills = useCallback(async (query: string) => { await send({ type: "search_marketplace_skills", query }); }, [send]);
  const installMarketplaceSkill = useCallback(async (source: string, name: string, scope: "project" | "global") => { await send({ type: "install_marketplace_skill", source, name, scope }); await fire({ type: "list_marketplace_skills" }); }, [send, fire]);
  const updateMarketplaceSkill = useCallback(async (name: string, scope: "project" | "global") => { await send({ type: "update_marketplace_skill", name, scope }); await fire({ type: "list_marketplace_skills" }); }, [send, fire]);
  const removeMarketplaceSkill = useCallback(async (name: string, scope: "project" | "global") => { await send({ type: "remove_marketplace_skill", name, scope }); await fire({ type: "list_marketplace_skills" }); }, [send, fire]);
  const applySkill = useCallback(
    async (name: string, task?: string) => {
      const s = stateRef.current;
      const model = s.selectedModel ?? s.models[0]?.id ?? "";
      const cmd: CoreCommand = { type: "apply_skill", name, model };
      const eff = effortFor(s.thinkingLevel);
      if (eff) cmd.reasoning_effort = eff;
      if (task && task.trim()) cmd.task = task.trim();
      // Optimistic user line: show the concise /skill:<name> [task] the user
      // invoked (the core inlines the full skill body into the actual prompt).
      const display = task && task.trim() ? `/skill:${name} ${task.trim()}` : `/skill:${name}`;
      setState((st) => reduce(st, { type: "_user", text: display, model, steer: false }));
      const r = await post(cmd);
      if (r.ok === false || r.error) {
        setState((st) => {
          let messages = st.messages;
          for (let i = messages.length - 1; i >= 0; i--) {
            const m = messages[i];
            if (m.role === "user" && m.text === display) {
              messages = [...messages.slice(0, i), ...messages.slice(i + 1)];
              break;
            }
          }
          return reduce({ ...st, messages }, { type: "error", message: r.error || "Failed to apply skill" });
        });
      }
    },
    [post, effortFor],
  );

  // ── Goal mode ──
  const startGoal = useCallback(
    async (opts: {
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
    }) => {
      const s = stateRef.current;
      const model = s.selectedModel ?? s.models[0]?.id ?? "";
      const cmd: CoreCommand = {
        type: "start_goal",
        goal: opts.goal,
        model,
        concurrency: opts.concurrency,
        max_tasks: opts.max_tasks,
        allowed_models: opts.allowed_models,
        allowed_providers: opts.allowed_providers,
        auto_deploy: opts.auto_deploy,
        ceo_mode: opts.ceo_mode,
        max_iterations: opts.max_iterations,
        max_plan_revisions: opts.max_plan_revisions,
        planner_model: opts.planner_model,
        worker_model: opts.worker_model,
        reviewer_model: opts.reviewer_model,
        model_concurrency: opts.model_concurrency,
      };
      const eff = effortFor(s.thinkingLevel);
      if (eff) cmd.reasoning_effort = eff;
      const display = opts.ceo_mode ? `🎯 Mission: ${opts.goal}` : `🎯 Goal: ${opts.goal}`;
      setState((st) =>
        reduce(st, {
          type: "_user",
          text: display,
          model,
          steer: false,
        }),
      );
      const r = await post(cmd);
      if (r.ok === false || r.error) {
        setState((st) => {
          let messages = st.messages;
          for (let i = messages.length - 1; i >= 0; i--) {
            const m = messages[i];
            if (m.role === "user" && m.text === display) {
              messages = [...messages.slice(0, i), ...messages.slice(i + 1)];
              break;
            }
          }
          return reduce({ ...st, messages }, { type: "error", message: r.error || "Failed to start goal" });
        });
      }
    },
    [post, effortFor],
  );
  const cancelGoal = useCallback(() => fire({ type: "cancel_goal" }), [fire]);
  const approveGoalPlan = useCallback(() => {
    setState((s) => reduce(s, { type: "_goal_approve_optimistic" }));
    return fire({ type: "approve_goal_plan" });
  }, [fire]);
  const goalStatus = useCallback(() => fire({ type: "goal_status" }), [fire]);
  const reviseGoal = useCallback(
    async (feedback: string) => {
      const s = stateRef.current;
      const model = s.selectedModel ?? s.models[0]?.id ?? "";
      const cmd: CoreCommand = { type: "revise_goal", feedback, model };
      const eff = effortFor(s.thinkingLevel);
      if (eff) cmd.reasoning_effort = eff;
      const r = await post(cmd);
      if (r.ok === false || r.error) {
        setState((st) =>
          reduce(st, { type: "error", message: r.error || "Failed to revise goal" }),
        );
      }
    },
    [post, effortFor],
  );

  // ── Vision ──
  const getVisionConfig = useCallback(() => fire({ type: "get_vision_config" }), [fire]);
  const setVisionConfig = useCallback(
    (vision_model: string | null, vision_models?: string[], enabled?: boolean) =>
      fire({
        type: "set_vision_config",
        vision_model,
        vision_models,
        enabled: enabled ?? true,
      }),
    [fire],
  );

  // ── Sandbox (Microsandbox) ──
  // Thin fire-and-forget wrappers over the typed command builders in
  // commands.ts. The core replies asynchronously via sandbox_status /
  // sandbox_prepare_progress / sandbox_ready / sandbox_error events, which the
  // reducer folds into state.sandbox.
  const getSandboxStatus = useCallback(
    () => fire(getSandboxStatusCommand()),
    [fire],
  );
  const prepareSandbox = useCallback(() => fire(prepareSandboxCommand()), [fire]);
  const resetSandbox = useCallback(() => fire(resetSandboxCommand()), [fire]);

  // ── Usage ──
  const usage = useCallback(
    (model?: string) => fire({ type: "usage", ...(model ? { model } : {}) }),
    [fire],
  );

  // ── Config ──
  const setConfig = useCallback(
    (key: string, value: string | number | boolean) => fire({ type: "set_config", key, value }),
    [fire],
  );

  // ── Projects / workspace ──
  const switchWorkspace = useCallback(
    async (path: string) => {
      const r = await post({ type: "switch_workspace", path });
      if (r.ok === false || r.error) {
        setState((s) => reduce(s, { type: "error", message: r.error || "Failed to switch workspace" }));
        return;
      }
      if (r.session) switchToSession(r.session, r.workspace ?? path);
    },
    [post, switchToSession],
  );
  const renameSession = useCallback(
    (name: string, title: string) => fire({ type: "rename_session", name, title }),
    [fire],
  );
  const listProjects = useCallback(() => fire({ type: "list_projects" }), [fire]);
  const addProject = useCallback((path: string) => fire({ type: "add_project", path }), [fire]);
  const removeProject = useCallback(
    (path: string) => fire({ type: "remove_project", path }),
    [fire],
  );

  // ── Session lifecycle ──
  const deleteSession = useCallback(
    async (path: string) => {
      const r = await post({ type: "delete_session", path });
      if (r.ok === false || r.error) {
        setState((s) => reduce(s, { type: "error", message: r.error || "Failed to delete session" }));
        return;
      }
      if (activeRef.current && activeRef.current === path && r.session) {
        switchToSession(r.session, r.workspace);
      }
    },
    [post, switchToSession],
  );
  const pinSession = useCallback(
    (path: string, pinned: boolean) => fire({ type: "pin_session", path, pinned }),
    [fire],
  );

  // ── Checkpoints ──
  const createCheckpoint = useCallback(
    (label?: string, paths?: string[]) =>
      fire({ type: "create_checkpoint", ...(label ? { label } : {}), ...(paths ? { paths } : {}) }),
    [fire],
  );
  const listCheckpoints = useCallback(() => fire({ type: "list_checkpoints" }), [fire]);
  const restoreCheckpoint = useCallback(
    (id: string) => fire({ type: "restore_checkpoint", id }),
    [fire],
  );

  // ── Connection ──
  const reconnect = useCallback(() => setReconnectKey((k) => k + 1), []);

  // ── Utility ──
  const copyLastReply = useCallback(() => {
    const msgs = stateRef.current.messages;
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      if (m.role === "assistant" && m.text.trim()) {
        if (!navigator.clipboard?.writeText) {
          setState((s) =>
            reduce(s, { type: "error", message: "Clipboard unavailable in this browser" }),
          );
          return;
        }
        void navigator.clipboard.writeText(m.text).then(
          () => {
            setState((s) =>
              reduce(s, { type: "info", message: "Copied last reply to clipboard" }),
            );
          },
          () => {
            setState((s) =>
              reduce(s, { type: "error", message: "Failed to copy — clipboard permission denied" }),
            );
          },
        );
        return;
      }
    }
    setState((s) =>
      reduce(s, { type: "info", message: "No assistant reply to copy yet" }),
    );
  }, []);

  const exportTranscript = useCallback((): string => {
    const msgs = stateRef.current.messages;
    const lines: string[] = [`# Catalyst Code Transcript`, ``];
    for (const m of msgs) {
      if (m.role === "user") {
        lines.push(`## You`, ``, m.text, ``);
      } else if (m.role === "assistant") {
        lines.push(`## Assistant${m.model ? ` (${m.model})` : ""}`, ``);
        if (m.text) lines.push(m.text, ``);
        for (const tc of m.toolCalls) {
          lines.push(
            `> **Tool: ${tc.name}**`,
            `> \`\`\``,
            `> ${(tc.argString || "").split("\n").join("\n> ")}`,
            `> \`\`\``,
            tc.result ? `> _${tc.result.ok ? "ok" : "error"}_` : `> _running_`,
            ``,
          );
          if (tc.result?.output) {
            lines.push(`> \`\`\``, `> ${tc.result.output.split("\n").join("\n> ")}`, `> \`\`\``, ``);
          }
        }
      }
    }
    return lines.join("\n");
  }, []);

  return useMemo(
    () => ({
      state,
      connected,
      send,
      prompt,
      steer,
      userBash,
      abort,
      clearQueue,
      approve,
      setKey,
      setProvider,
      login,
      addCustomProvider,
      discoverProviderModels,
      clearProviderModelsPreview,
      loginOauth,
      logout,
      listProviderPresets,
      setModel,
      setThinking,
      setApproval,
      newSession,
      loadSession,
      listSessions,
      compact,
      reset,
      stats,
      context,
      dismissToast,
      dismissNotification,
      markAllNotificationsRead,
      clearNotifications,
      intercomReply,
      askReply,
      sudoReply,
      submitOauthCode,
      setSearchKey,
      dismissOauth,
      undo,
      clear,
      pluginTrustDecisions,
      saveMemory,
      listMemory,
      forgetMemory,
      installPlugin,
      removePlugin,
      enablePlugin,
      disablePlugin,
      listPlugins,
      listAgents,
      refreshMemory,
      refreshModels,
      listSkills,
      applySkill,
      listMarketplaceSkills,
      acceptSkillDisclaimer,
      searchMarketplaceSkills,
      installMarketplaceSkill,
      updateMarketplaceSkill,
      removeMarketplaceSkill,
      startGoal,
      cancelGoal,
      approveGoalPlan,
      reviseGoal,
      goalStatus,
      getVisionConfig,
      setVisionConfig,
      usage,
      setConfig,
      switchWorkspace,
      renameSession,
      listProjects,
      addProject,
      removeProject,
      deleteSession,
      pinSession,
      createCheckpoint,
      listCheckpoints,
      restoreCheckpoint,
      getSandboxStatus,
      prepareSandbox,
      resetSandbox,
      reconnect,
      copyLastReply,
      exportTranscript,
    }),
    // Every action above is a useCallback with stable deps (refs/empty), so the
    // returned object identity only changes when `state` or `connected` do.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [state, connected],
  );
}
