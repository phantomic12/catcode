"use client";

// Chat — the application shell. Owns the useAgent hook and wires it to the
// sidebar, header, message list, approval gate, composer, and toasts. Handles
// auto-scroll, the empty-state hero, the API-key overlay, theme toggle,
// message edit/regenerate, transcript export, and the slash-command dispatch.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useAgent, type AgentApi } from "@/lib/use-agent";
import { useIdeContext } from "@/lib/ide-context";
import { basename } from "@/lib/format";
import { Sidebar } from "./sidebar";
import { Header } from "./header";
import { Message } from "./message";
import { Composer, type ComposerHandle } from "./composer";
import { Toasts } from "./toasts";
import { Approval } from "./approval";
import { IntercomPrompt } from "./intercom";
import { AskFlyout } from "./ask";
import { SudoPrompt } from "./sudo-prompt";
import { OauthPromptBanner } from "./oauth-prompt";
import { WorkStatePanel } from "./work-state";
import { SubagentsPanel } from "./subagents";
import { MemoryPanel } from "./memory";
import { PluginsPanel } from "./plugins";
import { SkillsMarketplace } from "./skills-marketplace";
import { HelpModal } from "./help-modal";
import { GoalModal, GoalPlanBanner, GoalProgressPanel } from "./goal-modal";
import { PluginTrustPrompt } from "./plugin-trust";
import { ControlCenterPanel } from "./control-center";
import { ProviderLoginModal } from "./provider-login-modal";
import { CustomProviderModal } from "./custom-provider-modal";
import { DiagnosticsModal } from "./diagnostics-modal";
import { ErrorBoundary } from "./error-boundary";
import { AppDialogHost, useAppDialog } from "./app-dialog";
import { useOutsideClose, mergeRefs } from "@/lib/use-outside-close";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { ShieldIcon, SendIcon, BrandMark } from "./icons";

/** Parse `agent "task"` / `agent 'task'` / `agent bare-task` pairs from slash args. */
function parseAgentTasks(args: string): Array<{ agent: string; task: string }> {
  const out: Array<{ agent: string; task: string }> = [];
  const re = /(\S+)\s+(?:"([^"]*)"|'([^']*)'|(\S+))/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(args))) {
    out.push({ agent: m[1], task: m[2] ?? m[3] ?? m[4] ?? "" });
  }
  return out;
}

function buildRunPrompt(agent: string, task: string): string {
  return `Run the subagent tool: agent="${agent}", task="${task}". Return its result.`;
}

function buildParallelPrompt(tasks: Array<{ agent: string; task: string }>): string {
  const lines = tasks.map((t) => `- agent="${t.agent}" task="${t.task}"`).join("\n");
  return `Run the subagent tool in parallel mode with these tasks:\n${lines}`;
}

function buildChainPrompt(tasks: Array<{ agent: string; task: string }>): string {
  const lines = tasks.map((t) => `- agent="${t.agent}" task="${t.task}"`).join("\n");
  return `Run the subagent tool as a chain with these steps (use {previous} to pass the prior step's output):\n${lines}`;
}

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

export function ChatInner({ agent, docked }: { agent: AgentApi; docked?: boolean }) {
  const { openSettings, openProjects, ide, registerAttachToChat } = useIdeContext();
  const { state } = agent;
  const { confirm, prompt, dialog } = useAppDialog();
  const dialogApi = useRef({ confirm, prompt });
  useEffect(() => {
    dialogApi.current = { confirm, prompt };
  }, [confirm, prompt]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [restoredDrawerWorkspace, setRestoredDrawerWorkspace] = useState<string | null>(null);
  const [keyInput, setKeyInput] = useState("");
  const [keyBusy, setKeyBusy] = useState(false);
  const [keyDismissed, setKeyDismissed] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);
  const composerRef = useRef<ComposerHandle>(null);
  const [autoScroll, setAutoScroll] = useState(true);
  const [modal, setModal] = useState<
    null | "memory" | "plugins" | "skills-marketplace" | "subagents" | "help" | "goal" | "control" | "login" | "logout" | "custom-provider" | "diagnostics"
  >(null);
  const [images, setImages] = useState<string[]>([]);
  // True while `discover_provider_models` is in flight (custom-provider modal).
  // A generation counter lets Cancel / late previews ignore stale results.
  const [discovering, setDiscovering] = useState(false);
  const discoverGenRef = useRef(0);
  // Keep the server and first client render deterministic. The inline script in
  // layout.tsx applies the persisted value before paint; after hydration we
  // adopt that value into React state. Reading localStorage in the initializer
  // made the theme button's server/client markup disagree in light mode.
  const [theme, setTheme] = useState<string>("dark");
  const themeMountedRef = useRef(false);

  useEffect(() => {
    const workspace = state.workspace;
    if (!workspace) return;
    try {
      setSidebarOpen(localStorage.getItem(`catalyst:chat-drawer:${encodeURIComponent(workspace)}`) === "open");
    } catch {
      setSidebarOpen(false);
    }
    setRestoredDrawerWorkspace(workspace);
  }, [state.workspace]);

  useEffect(() => {
    if (!state.workspace || restoredDrawerWorkspace !== state.workspace) return;
    try {
      localStorage.setItem(
        `catalyst:chat-drawer:${encodeURIComponent(state.workspace)}`,
        sidebarOpen ? "open" : "closed",
      );
    } catch {
      // Storage may be unavailable; drawer behavior remains functional.
    }
  }, [restoredDrawerWorkspace, sidebarOpen, state.workspace]);

  // A discovery result landed (including empty/error) → stop the spinner.
  // Null means no preview yet / cleared — don't clear discovering on open.
  useEffect(() => {
    if (state.providerModelsPreview !== null) setDiscovering(false);
  }, [state.providerModelsPreview]);

  // Refs so the edit/regenerate/command callbacks can stay stable (empty deps)
  // — this keeps <Message> memoized: only the streaming message re-renders on
  // each token, not the whole conversation.
  const agentRef = useRef(agent);
  useEffect(() => {
    agentRef.current = agent;
  }, [agent]);
  const openSettingsRef = useRef(openSettings);
  useEffect(() => {
    openSettingsRef.current = openSettings;
  }, [openSettings]);
  const msgsRef = useRef(state.messages);
  useEffect(() => {
    msgsRef.current = state.messages;
  }, [state.messages]);
  const streamingRef = useRef(state.streaming);
  useEffect(() => {
    streamingRef.current = state.streaming;
  }, [state.streaming]);

  // Theme: toggle a data-theme attribute + persist. CSS variables adjust.
  useEffect(() => {
    if (!themeMountedRef.current) {
      themeMountedRef.current = true;
      const initial = document.documentElement.dataset.theme === "light" ? "light" : "dark";
      if (initial !== theme) setTheme(initial);
      return;
    }
    document.documentElement.setAttribute("data-theme", theme);
    lsSet("catalyst:theme", theme);
  }, [theme]);

  // Re-fetch goal status once the core is ready (covers reconnect / mid-goal resume).
  useEffect(() => {
    if (state.ready) void agent.goalStatus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.ready != null]);

  const openDiagnostics = useCallback(() => {
    const a = agentRef.current;
    void a.stats();
    void a.context();
    void a.usage(a.state.selectedModel ?? undefined);
    setModal("diagnostics");
  }, []);

  const hitlGateRef = useRef<HTMLDivElement>(null);
  const hadHitlGateRef = useRef(false);

  // Stick to bottom while streaming / new messages — exclude HITL gates so
  // they don't fight the user by yanking the viewport to the end.
  // streamTick tracks in-message deltas (tokens / tool args) that don't change array identity.
  const streamTick = useMemo(() => {
    const last = state.messages[state.messages.length - 1];
    if (!last || last.role !== "assistant") return 0;
    const tools = last.toolCalls ?? [];
    let toolWeight = tools.length * 1000;
    for (const tc of tools) {
      toolWeight += (tc.argString?.length ?? 0) + (tc.result?.output?.length ?? 0);
    }
    return (last.text?.length ?? 0) + (last.thinking?.length ?? 0) + toolWeight;
  }, [state.messages]);

  useEffect(() => {
    if (!autoScroll) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [state.messages, state.goalMode, state.goalPlan, autoScroll, streamTick]);

  // When a HITL gate appears: stop stick-to-bottom. The gate is pinned above
  // the composer (outside the scroll area) so it's always visible — no need to
  // scroll it into view. When the gate is dismissed (e.g. the user approved),
  // resume following the agent's output so the response isn't stranded off-screen.
  useEffect(() => {
    const hasGate = !!(
      state.pendingApproval ||
      state.pendingAsk ||
      state.pendingSudo ||
      state.pendingPluginTrust ||
      state.pendingIntercom ||
      (state.goalMode &&
        state.goalMode.phase === "plan_ready" &&
        !state.goalMode.auto_deploy)
    );
    if (hasGate && !hadHitlGateRef.current) {
      setAutoScroll(false);
    } else if (!hasGate && hadHitlGateRef.current) {
      setAutoScroll(true);
      const el = scrollRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    }
    hadHitlGateRef.current = hasGate;
  }, [
    state.pendingApproval,
    state.pendingAsk,
    state.pendingSudo,
    state.pendingIntercom,
    state.pendingPluginTrust,
    state.goalMode,
  ]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 90;
    setAutoScroll(atBottom);
  };

  // ── Export transcript as a downloadable markdown file ──
  const doExport = useCallback(() => {
    const md = agentRef.current.exportTranscript();
    const blob = new Blob([md], { type: "text/markdown" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `catalyst-code-transcript-${new Date().toISOString().slice(0, 10)}.md`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  }, []);

  // ── Slash-command dispatch (single switch; the catalog is the source of truth) ──
  // Uses refs so this callback is stable — the Composer never re-renders from it.
  // `args` is the remainder after the slash token (e.g. `/run scout "bugs"` →
  // action "run", args `scout "bugs"`).
  const onCommand = useCallback((name: string, args?: string) => {
    void (async () => {
      const a = agentRef.current;
      const d = dialogApi.current;
      switch (name) {
        case "reset": {
          const ok = await d.confirm({
            title: "Reset session",
            message: "Reset the conversation and session file? This cannot be undone.",
            confirmLabel: "Reset",
            danger: true,
          });
          if (ok) await a.reset();
          return;
        }
        case "compact": {
          let instr = args?.trim();
          if (!instr) {
            const typed = await d.prompt({
              title: "Compact context",
              message:
                "Optional: what should compaction preserve? Leave blank for the default summary.",
              placeholder: "e.g. Focus on code samples and API usage",
              multiline: true,
              confirmLabel: "Compact",
            });
            if (typed === null) return;
            instr = typed.trim() || undefined;
          }
          await a.compact(instr || undefined);
          return;
        }
        case "context":
        case "usage":
        case "stats":
          openDiagnostics();
          return;
        case "new":
          return void a.newSession();
        case "abort":
          return void a.abort();
        case "sessions":
          return void a.listSessions();
        case "undo":
          return void a.undo();
        case "clear": {
          const ok = await d.confirm({
            title: "Clear view",
            message: "Clear the conversation view? The session file is kept.",
            confirmLabel: "Clear",
          });
          if (ok) await a.clear();
          return;
        }
        case "memory":
          a.listMemory();
          return setModal("memory");
        case "remember": {
          let note = args?.trim();
          if (!note) {
            const typed = await d.prompt({
              title: "Remember",
              message: "A durable note for future sessions.",
              placeholder: "Remember what?",
              multiline: true,
              required: true,
              confirmLabel: "Save",
            });
            if (!typed?.trim()) return;
            note = typed.trim();
          }
          void a.saveMemory(note);
          a.listMemory();
          return setModal("memory");
        }
        case "forget":
          a.listMemory();
          return setModal("memory");
        case "plugins":
          a.listPlugins();
          return setModal("plugins");
        case "skills":
          a.listMarketplaceSkills();
          return setModal("skills-marketplace");
        case "auto-compact": {
          const on = !(a.state.ready?.auto_compact ?? true);
          return void a.setConfig("auto_compact", on);
        }
        case "sandbox": {
          const mode = (args ?? "").trim().toLowerCase();
          // Accept the new aliases. Legacy "firejail"/"seatbelt"/"fj"/"macos"/
          // "sandbox-exec" map to microsandbox (the core migrates + warns);
          // "none"/"off"/"disabled"/"false" disable it.
          const disabled =
            mode === "none" || mode === "off" || mode === "disabled" || mode === "false";
          const microsandbox =
            mode === "microsandbox" ||
            mode === "msb" ||
            mode === "on" ||
            mode === "true" ||
            mode === "enabled" ||
            mode === "firejail" ||
            mode === "fj" ||
            mode === "seatbelt" ||
            mode === "macos" ||
            mode === "sandbox-exec";
          if (disabled) return void a.setConfig("sandbox", "none");
          if (microsandbox) return void a.setConfig("sandbox", "microsandbox");
          void a.getVisionConfig();
          return openSettingsRef.current();
        }
        case "settings":
        case "vision":
          void a.getVisionConfig();
          return openSettingsRef.current();
        case "model": {
          const id = args?.trim();
          if (id) {
            const match =
              a.state.models.find((m) => m.id === id) ||
              a.state.models.find((m) => m.id.endsWith(`/${id}`) || m.name === id);
            if (match) {
              a.setModel(match.id);
              return;
            }
          }
          void a.getVisionConfig();
          return openSettingsRef.current();
        }
        case "reasoning": {
          const level = args?.trim().toLowerCase();
          if (level && ["off", "low", "medium", "high", "xhigh", "max"].includes(level)) {
            a.setThinking(level);
            return;
          }
          void a.getVisionConfig();
          return openSettingsRef.current();
        }
        case "approval": {
          const mode = args?.trim().toLowerCase();
          if (mode === "never" || mode === "destructive" || mode === "always") {
            return void a.setApproval(mode);
          }
          void a.getVisionConfig();
          return openSettingsRef.current();
        }
        case "subagents":
          void a.listAgents();
          return setModal("subagents");
        case "help":
          return setModal("help");
        case "copy":
          return a.copyLastReply();
        case "export":
          return doExport();
        case "theme":
          return setTheme((t) => (t === "dark" ? "light" : "dark"));
        case "key": {
          const key = await d.prompt({
            title: "API key",
            message: "Enter an API key for the current provider.",
            placeholder: "sk-…",
            password: true,
            required: true,
            confirmLabel: "Save key",
          });
          if (key?.trim()) void a.setKey(key.trim());
          return;
        }
                case "oauth-code": {
          const code = await d.prompt({
            title: "OAuth code",
            message: "Paste the OAuth code or final callback URL.",
            placeholder: "code or https://…",
            required: true,
            confirmLabel: "Submit",
          });
          if (code?.trim()) void a.submitOauthCode(code.trim());
          return;
        }
        case "search-key": {
          let provider = (args ?? "").trim().toLowerCase();
          if (provider !== "exa" && provider !== "tavily") {
            const picked = await d.prompt({
              title: "Search API key provider",
              message: "Which search provider? Type exa or tavily.",
              placeholder: "exa",
              confirmLabel: "Next",
            });
            provider = (picked ?? "").trim().toLowerCase();
            if (provider !== "exa" && provider !== "tavily") return;
          }
          const key = await d.prompt({
            title: `${provider === "exa" ? "Exa" : "Tavily"} API key`,
            message: `Paste your ${provider} search API key (leave blank to clear).`,
            placeholder: provider === "exa" ? "exa-key…" : "tvly-key…",
            password: true,
            confirmLabel: "Save",
          });
          if (key !== null) void a.setSearchKey(provider, key.trim());
          return;
        }
        case "login":
          void a.listProviderPresets();
          return setModal("login");
        case "logout":
          void a.listProviderPresets();
          return setModal("logout");
        case "steer": {
          const msg = args?.trim();
          if (msg) {
            void a.steer(msg);
            return;
          }
          setSidebarOpen(false);
          return composerRef.current?.focus();
        }
        case "attach":
          return composerRef.current?.openAttach();
        case "goal":
          void a.goalStatus();
          return setModal("goal");
        case "control":
          void a.goalStatus();
          return setModal("control");
        case "cancel-goal":
          return void a.cancelGoal();
        case "index": {
          const incremental =
            /\b(--incremental|-i)\b/.test(args ?? "") ||
            (args ?? "").trim() === "incremental";
          const task = incremental
            ? "Run an incremental knowledge index of this repository. Use `git status` + `git diff --name-only` to find files changed since the last index; for each changed area, read it and use the `memory` tool (action: append) to UPDATE the relevant existing memories — architecture, conventions, APIs, gotchas — rather than creating duplicates. If a changed file reveals a new subsystem with no memory yet, save a new one. Then list the memories you touched. Be concise: only persist what genuinely changed."
            : "Run a full knowledge index of this repository to bootstrap learning. Walk the top-level layout, read README/package-manifest/entry points/config/tests, and identify the architecture, major subsystems, conventions, reusable patterns, build/test/deploy steps, and gotchas. Use the `memory` tool (action: save) to persist each as a durable, named memory (types: architecture/convention/api/gotcha/build). Then use `list_dir .catalyst-code/skills/` and, for any reusable workflow you solved 2+ times that has no skill yet, write a candidate SKILL.md under `.catalyst-code/skills/<name>/` with write_file (frontmatter: name/description; body: when-to-use + steps + example). End by listing the memories and any candidate skills you created, and name one area you are least confident about.";
          return void a.prompt(task);
        }
        case "reflect":
          return void a.prompt(
            "Reflect on the work done in this session so far. Identify: (1) any convention, architecture fact, decision, or gotcha worth persisting so future sessions don't rediscover it, and (2) any repetitive pattern you performed more than once that should become a reusable skill under `.catalyst-code/skills/`. Use the `memory` tool (action: append if a topic memory exists, else save) to persist durable facts only — skip transient task state. If you wrote a skill, name it. Finish with a two-line summary: what you learned and what you persisted.",
          );
        case "bash-timeout": {
          const n = Number((args ?? "").trim());
          if (Number.isFinite(n) && n > 0) return void a.setConfig("bash_timeout_secs", n);
          void a.getVisionConfig();
          return openSettingsRef.current();
        }
        case "run": {
          let raw = args?.trim();
          if (!raw) {
            raw =
              (await d.prompt({
                title: "Delegate to a subagent",
                message: 'Format: agent "task"',
                placeholder: 'scout "find bugs"',
                required: true,
                confirmLabel: "Run",
              })) ?? "";
          }
          const parsed = parseAgentTasks(raw.trim());
          if (parsed.length === 0) {
            if (raw.trim()) {
              const sp = raw.trim().indexOf(" ");
              if (sp > 0) {
                void a.prompt(
                  buildRunPrompt(
                    raw.trim().slice(0, sp),
                    raw.trim().slice(sp + 1).replace(/^["']|["']$/g, ""),
                  ),
                );
              }
            }
            return;
          }
          void a.prompt(buildRunPrompt(parsed[0].agent, parsed[0].task));
          return;
        }
        case "parallel": {
          let raw = args?.trim();
          if (!raw) {
            raw =
              (await d.prompt({
                title: "Run subagents in parallel",
                message: 'Format: a1 "t1" a2 "t2"',
                placeholder: 'scout "scan" worker "fix"',
                required: true,
                confirmLabel: "Run",
              })) ?? "";
          }
          const parsed = parseAgentTasks(raw.trim());
          if (parsed.length === 0) return;
          void a.prompt(buildParallelPrompt(parsed));
          return;
        }
        case "chain": {
          let raw = args?.trim();
          if (!raw) {
            raw =
              (await d.prompt({
                title: "Run a subagent chain",
                message: 'Format: a1 "t1" a2 "t2" — use {previous} in later tasks',
                placeholder: 'scout "map auth" worker "fix using {previous}"',
                required: true,
                confirmLabel: "Run",
              })) ?? "";
          }
          const parsed = parseAgentTasks(raw.trim());
          if (parsed.length === 0) return;
          void a.prompt(buildChainPrompt(parsed));
          return;
        }
        default:
          return;
      }
    })();
  }, [doExport, openDiagnostics]);

  const onAddImage = (url: string) => setImages((prev) => [...prev, url]);
  const onRemoveImage = (i: number) => setImages((prev) => prev.filter((_, idx) => idx !== i));
  const sendPrompt = async (text: string, imgs?: string[]) => {
    const snapshot = imgs?.length ? imgs : images.length ? [...images] : undefined;
    setImages([]);
    try {
      const ok = await agent.prompt(text, snapshot);
      if (!ok && snapshot?.length) setImages(snapshot);
      return ok;
    } catch (error) {
      if (snapshot?.length) setImages(snapshot);
      throw error;
    }
  };

  // Preview (and other IDE panels) attach context into the composer via IdeContext.
  useEffect(() => {
    registerAttachToChat((payload) => {
      if (payload.text) composerRef.current?.append(payload.text);
      if (payload.image) setImages((prev) => [...prev, payload.image!]);
      composerRef.current?.focus();
    });
    return () => registerAttachToChat(null);
  }, [registerAttachToChat]);

  const submitKey = async () => {
    if (!keyInput.trim()) return;
    setKeyBusy(true);
    await agent.setKey(keyInput.trim());
    setKeyBusy(false);
    setKeyInput("");
  };

  // ── Edit a user message: undo the last turn, then re-send the edited text ──
  // Stable (empty deps) via refs so <Message> memo isn't defeated.
  const onEditUser = useCallback((newText: string, imgs?: string[]) => {
    const a = agentRef.current;
    void a.undo().then((ok) => {
      if (ok) void a.prompt(newText, imgs);
    });
  }, []);

  const onRegenerate = useCallback(() => {
    const a = agentRef.current;
    const msgs = msgsRef.current;
    let lastUserText = "";
    let lastUserImages: string[] | undefined;
    for (let i = msgs.length - 1; i >= 0; i--) {
      if (msgs[i].role === "user") {
        const u = msgs[i] as { text: string; images?: string[] };
        lastUserText = u.text;
        lastUserImages = u.images;
        break;
      }
    }
    if (lastUserText) {
      void a.undo().then((ok) => {
        if (ok) void a.prompt(lastUserText, lastUserImages);
      });
    }
  }, []);

  // Compute indices for edit/regenerate affordances (only the latest of each).
  const messages = state.messages;
  let lastUserIdx = -1;
  let lastAssistantIdx = -1;
  for (let i = messages.length - 1; i >= 0; i--) {
    if (lastUserIdx < 0 && messages[i].role === "user") lastUserIdx = i;
    if (lastAssistantIdx < 0 && messages[i].role === "assistant") lastAssistantIdx = i;
    if (lastUserIdx >= 0 && lastAssistantIdx >= 0) break;
  }

  // Block the composer only when NO provider can serve a turn. Active-provider
  // authed=false is fine in multi-login if anyLoggedIn / models are present.
  const canDispatch =
    state.authed === true ||
    state.anyLoggedIn === true ||
    (state.models?.length ?? 0) > 0;
  const needKey = state.ready != null && !canDispatch && !keyDismissed;
  const currentModel = state.models.find((m) => m.id === state.selectedModel) ?? state.models[0];
  const modelLabel = currentModel?.name ?? currentModel?.id ?? "no model";
  const currentSession = state.sessions.find((session) => {
    const current = state.currentSessionFile;
    return !!current && (
      current === session.path ||
      current === session.name ||
      current.endsWith("/" + session.name) ||
      current.endsWith("\\" + session.name)
    );
  });
  const currentSessionTitle = currentSession
    ? currentSession.title || basename(currentSession.name) || currentSession.name
    : undefined;
  const draftId = `${state.workspace}\n${state.currentSessionFile || "new-session"}`;
  const manualPlanReady =
    state.goalMode?.phase === "plan_ready" && !state.goalMode.auto_deploy;
  const hitlOpen = !!(
    state.pendingApproval ||
    state.pendingAsk ||
    state.pendingSudo ||
    state.pendingIntercom ||
    state.pendingPluginTrust ||
    manualPlanReady
  );
  const empty = state.messages.length === 0;
  const switching = state.switching;
  const activeFile =
    ide.state.openTabs?.find((tab) => tab.id === ide.state.activeTabId && tab.kind === "file")?.target ??
    null;

  return (
    <div className={`chat-panel relative flex min-h-0 min-w-0 ${docked ? "h-full" : "h-[100dvh]"} w-full overflow-hidden bg-ink-950 text-ink-100`}>
      <Sidebar
        embedded={docked}
        open={sidebarOpen}
        onClose={() => setSidebarOpen(false)}
        switching={switching}
        workspace={state.workspace}
        streamingSessionFile={state.streaming ? state.currentSessionFile : null}
        liveSessions={state.liveSessions}
        sessions={state.sessions}
        currentSessionFile={state.currentSessionFile}
        stats={state.stats}
        onNewSession={() => {
          agent.newSession();
          setSidebarOpen(false);
        }}
        onLoadSession={(p) => {
          agent.loadSession(p);
          setSidebarOpen(false);
        }}
        onReset={() => void onCommand("reset")}
        onCompact={() => void onCommand("compact")}
        onStats={openDiagnostics}
        onOpenPanel={(p) => {
          if (p === "memory") agent.listMemory();
          if (p === "plugins") agent.listPlugins();
          if (p === "subagents") void agent.listAgents();
          if (p === "control") void agent.goalStatus();
          setModal(p as "memory" | "plugins" | "subagents" | "help" | "control");
        }}
        onDeleteSession={(p) => agent.deleteSession(p)}
        onRenameSession={(name, title) => agent.renameSession(name, title)}
        onConfirmDelete={async (title) =>
          dialogApi.current.confirm({
            title: "Delete session",
            message: `Delete session "${title}"? The .jsonl file will be permanently removed.`,
            confirmLabel: "Delete",
            danger: true,
          })
        }
      />

      <div className="relative flex min-w-0 flex-1 flex-col">
        <Header
          compact={docked}
          connected={agent.connected}
          workspace={state.workspace}
          provider={state.provider}
          models={state.models}
          selectedModel={state.selectedModel}
          modelsRefreshing={state.modelsRefreshing}
          thinkingLevel={state.thinkingLevel}
          approvalMode={state.approvalMode}
          metrics={state.metrics}
          cost={state.cost}
          umansConc={state.umansConc}
          streaming={state.streaming}
          retrying={state.retrying}
          sessionFile={state.currentSessionFile}
          sessionTitle={currentSessionTitle}
          switching={switching}
          theme={theme}
          onMenuClick={() => setSidebarOpen(true)}
          onSelectModel={agent.setModel}
          onRefreshModels={() => void agent.refreshModels()}
          onSelectThinking={agent.setThinking}
          onSetApproval={agent.setApproval}
          onReconnect={agent.reconnect}
          onToggleTheme={() => setTheme((t) => (t === "dark" ? "light" : "dark"))}
          onOpenControl={() => {
            void agent.goalStatus();
            setModal("control");
          }}
          onOpenIde={!docked ? () => ide.setUiMode("ide") : undefined}
          onOpenSettings={!docked ? openSettings : undefined}
          onOpenProjects={!docked ? openProjects : undefined}
          notifications={state.notifications}
          onOpenNotification={(n) => agent.loadSession(n.sessionFile, n.workspace)}
          onDismissNotification={agent.dismissNotification}
          onMarkAllNotificationsRead={agent.markAllNotificationsRead}
          onClearNotifications={agent.clearNotifications}
        />

        {state.workState && <WorkStatePanel ws={state.workState} compact={!!docked} />}
        {state.streaming && !state.workState && (
          <AgentActivity compact={!!docked} activity={undefined} hasWorkState={false} />
        )}

        {/* Messages — flex sibling; composer stays below so it never clips */}
        <div className="relative min-h-0 flex-1">
        <div ref={scrollRef} onScroll={onScroll} className="h-full overflow-y-auto overflow-x-hidden">
          {empty || switching ? (
            <EmptyState
              workspace={state.workspace}
              connected={agent.connected}
              switching={switching}
              canSend={!!currentModel && agent.connected}
              compact={hitlOpen || !!state.goalMode}
              adaptive={!!docked}
              onPick={(t) => agent.prompt(t)}
            />
          ) : (
            <div className={`chat-thread w-full ${docked ? "chat-thread--wide px-3 sm:px-5" : "px-3 sm:px-6"}`}>
              <ErrorBoundary label="message list">
                {state.messages.map((m, i) => (
                  <Message
                    key={m.id}
                    m={m}
                    compact={!!docked}
                    canEdit={i === lastUserIdx && !state.streaming}
                    canRegenerate={i === lastAssistantIdx && !state.streaming}
                    onEditUser={onEditUser}
                    onRegenerate={onRegenerate}
                  />
                ))}
              </ErrorBoundary>
              <div className="h-2" />
            </div>
          )}

        </div>
          {!autoScroll && !empty && !switching && (
            <button
              type="button"
              onClick={() => {
                setAutoScroll(true);
                const el = scrollRef.current;
                if (el) el.scrollTop = el.scrollHeight;
              }}
              className="pointer-events-auto absolute bottom-3 left-1/2 z-10 flex min-h-11 -translate-x-1/2 items-center gap-1.5 rounded-sm border border-ink-700 bg-ink-900 px-3.5 py-1.5 font-mono text-[11px] text-ink-200 transition-colors hover:border-accent hover:text-ink-100"
            >
              <span className="text-accent-soft" aria-hidden="true">↓</span> Jump to latest
            </button>
          )}
        </div>

        {/* HITL first so empty-session OAuth/sudo/ask aren't below a full-height hero. */}
        {!switching && (
          <div ref={hitlGateRef} className="mx-auto w-full max-w-[60rem] shrink-0 px-3 sm:px-6">
            {state.pendingPluginTrust && (
              <div className="mb-2 mt-2">
                <PluginTrustPrompt
                  plugins={state.pendingPluginTrust}
                  onDecide={agent.pluginTrustDecisions}
                />
              </div>
            )}
            {state.pendingApproval && (
              <div className="mb-2 mt-2">
                <Approval approval={state.pendingApproval} onApprove={agent.approve} />
              </div>
            )}
            {state.pendingIntercom && (
              <div className="mb-2 mt-2">
                <IntercomPrompt
                  prompt={state.pendingIntercom}
                  onReply={agent.intercomReply}
                  onDismiss={() => agent.intercomReply("(skipped — no decision provided)")}
                />
              </div>
            )}
            {state.pendingAsk && (
              <div className="mb-2 mt-2">
                <AskFlyout
                  prompt={state.pendingAsk}
                  onSubmit={(answers) => agent.askReply(answers)}
                  onSkip={() => agent.askReply(null)}
                />
              </div>
            )}
            {state.pendingSudo && (
              <div className="mb-2 mt-2">
                <SudoPrompt
                  prompt={state.pendingSudo}
                  onApprove={(password) => agent.sudoReply(true, password)}
                  onDecline={() => agent.sudoReply(false)}
                />
              </div>
            )}
            {state.pendingOauth && (
              <div className="mb-2 mt-2">
                <OauthPromptBanner
                  prompt={state.pendingOauth}
                  onSubmit={agent.submitOauthCode}
                  onDismiss={agent.dismissOauth}
                />
              </div>
            )}
            {state.goalMode &&
              state.goalMode.phase === "plan_ready" &&
              !state.goalMode.auto_deploy && (
                <div className="mb-2 mt-2">
                  <GoalPlanBanner
                    goal={state.goalMode.goal}
                    summary={state.goalPlan?.summary}
                    steps={
                      state.goalMode.prompts.map((p) => ({
                        agent: p.agent,
                        title: p.title || p.step_id,
                      })) || []
                    }
                    onApprove={() => void agent.approveGoalPlan()}
                    onRevise={() => {
                      void dialogApi.current
                        .prompt({
                          title: "Revise plan",
                          message: "What should change in the plan?",
                          multiline: true,
                          required: true,
                          confirmLabel: "Revise",
                        })
                        .then((fb) => {
                          if (fb?.trim()) void agent.reviseGoal(fb.trim());
                        });
                    }}
                    onCancel={() => void agent.cancelGoal()}
                  />
                </div>
              )}
            {state.goalMode &&
              state.goalMode.phase !== "idle" &&
              (state.goalMode.phase !== "plan_ready" ||
                state.goalMode.auto_deploy) && (
                <div className="mb-2 mt-2">
                  <GoalProgressPanel
                    goalMode={state.goalMode}
                    onCancel={() => void agent.cancelGoal()}
                  />
                </div>
              )}
          </div>
        )}

        <Composer
          key={draftId}
          ref={composerRef}
          compact={docked}
          streaming={state.streaming}
          followUpQueued={state.followUpQueued}
          hitlOpen={hitlOpen}
          connected={agent.connected}
          canSend={!!currentModel && !switching}
          thinkingLevel={state.thinkingLevel}
          modelLabel={modelLabel}
          images={images}
          workspace={state.workspace}
          draftId={draftId}
          activeFile={activeFile}
          onAddImage={onAddImage}
          onRemoveImage={onRemoveImage}
          onRestoreImages={setImages}
          onPrompt={sendPrompt}
          onSteer={(t) => agent.steer(t)}
          onAbort={agent.abort}
          onClearQueue={agent.clearQueue}
          onCommand={onCommand}
          skills={state.skills}
          onSkill={(name, task) => agent.applySkill(name, task)}
          onBash={(command, exclude) => void agent.userBash(command, exclude)}
        />
      </div>

      <Toasts toasts={state.toasts} onDismiss={agent.dismissToast} docked={!!docked} />
      <AppDialogHost dialog={dialog} />

      {modal === "memory" && (
        <MemoryPanel
          memories={state.memories}
          onSave={agent.saveMemory}
          onForget={agent.forgetMemory}
          onRefresh={() => void agent.refreshMemory()}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "plugins" && (
        <PluginsPanel
          plugins={state.plugins}
          onInstall={agent.installPlugin}
          onRemove={agent.removePlugin}
          onEnable={agent.enablePlugin}
          onDisable={agent.disablePlugin}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "skills-marketplace" && (
        <SkillsMarketplace
          accepted={state.marketplaceDisclaimerAccepted}
          results={state.marketplaceResults}
          installed={state.marketplaceInstalled}
          onAccept={agent.acceptSkillDisclaimer}
          onSearch={agent.searchMarketplaceSkills}
          onInstall={agent.installMarketplaceSkill}
          onUpdate={agent.updateMarketplaceSkill}
          onRemove={agent.removeMarketplaceSkill}
          onRefresh={agent.listMarketplaceSkills}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "subagents" && (
        <SubagentsPanel
          runs={state.subagentRuns}
          jobs={state.jobTree}
          processes={state.processes}
          processLogs={state.processLogs}
          sessionTree={state.sessionTree}
          agents={state.availableAgents}
          onRefreshAgents={() => void agent.listAgents()}
          onRefreshJobs={() => void agent.send({ type: "job_list" })}
          onJobStatus={(runId) => void agent.send({ type: "job_status", run_id: runId })}
          onJobWait={(runId) => void agent.send({ type: "job_wait", run_id: runId })}
          onJobCancel={(runId) => void agent.send({ type: "job_cancel", run_id: runId })}
          onProcessLogs={(name) => void agent.prompt(`Use the process tool to show logs for the named process ${name}.`)}
          onProcessStop={(name) => void agent.prompt(`Use the process tool to stop the named process ${name}.`)}
          onBranch={(entryId) => void agent.send({ type: "session_branch", entry_id: entryId })}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "diagnostics" && (
        <DiagnosticsModal
          stats={state.stats}
          context={state.contextBreakdown}
          usage={state.usageSnapshot}
          cost={state.cost}
          checkpoints={state.checkpoints}
          protocolHello={state.protocolHello}
          onRefresh={() => {
            void agent.stats();
            void agent.context();
            void agent.usage(agent.state.selectedModel ?? undefined);
            void agent.listCheckpoints();
          }}
          onCreateCheckpoint={() => void agent.createCheckpoint()}
          onRestoreCheckpoint={(id) => void agent.restoreCheckpoint(id)}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "help" && <HelpModal onClose={() => setModal(null)} />}
      {modal === "goal" && (
        <GoalModal
          models={state.models}
          providerPresets={state.providerPresets}
          providers={state.ready?.providers ?? []}
          onStart={(opts) => void agent.startGoal(opts)}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "control" && (
        <ControlCenterPanel
          models={state.models}
          providerPresets={state.providerPresets}
          providers={state.ready?.providers ?? []}
          goalMode={state.goalMode}
          goalPlan={state.goalPlan}
          goalIterations={state.goalIterations}
          subagentRuns={state.subagentRuns}
          onStart={(opts) => void agent.startGoal(opts)}
          onAbort={() => void agent.cancelGoal()}
          onClose={() => setModal(null)}
        />
      )}
      {(modal === "login" || modal === "logout") && (
        <ProviderLoginModal
          presets={state.providerPresets}
          mode={modal}
          onLoginKey={(id, key) => void agent.login(id, key)}
          onLoginOauth={(id) => void agent.loginOauth(id)}
          onLoginSaved={(id) => void agent.login(id)}
          onSwitchProvider={(id) => void agent.setProvider(id)}
          onLogout={(id) => void agent.logout(id)}
          onAddCustom={() => setModal("custom-provider")}
          onClose={() => setModal(null)}
        />
      )}
      {modal === "custom-provider" && (
        <CustomProviderModal
          previewModels={agent.state.providerModelsPreview}
          discoverError={agent.state.providerModelsPreviewError}
          discovering={discovering}
          onDiscover={(url, kind, key, headers) => {
            discoverGenRef.current += 1;
            agent.clearProviderModelsPreview();
            setDiscovering(true);
            void agent.discoverProviderModels(url, kind, key, headers);
          }}
          onCancelDiscover={() => {
            discoverGenRef.current += 1;
            setDiscovering(false);
            agent.clearProviderModelsPreview();
          }}
          onSubmit={(draft) => void agent.addCustomProvider(draft)}
          onClose={() => {
            discoverGenRef.current += 1;
            setModal(null);
            agent.clearProviderModelsPreview();
            setDiscovering(false);
          }}
        />
      )}

      {needKey && (
        <KeyOverlay
          value={keyInput}
          busy={keyBusy}
          onChange={setKeyInput}
          onSubmit={submitKey}
          onDismiss={() => setKeyDismissed(true)}
        />
      )}
    </div>
  );
}

// Thin wrapper: calls useAgent for standalone (non-IdeShell) usage.
export function Chat(props: { docked?: boolean } = {}) {
  const agent = useAgent();
  return <ChatInner agent={agent} docked={props.docked} />;
}

function EmptyState({
  workspace,
  connected,
  switching,
  canSend,
  compact,
  adaptive,
  onPick,
}: {
  workspace: string;
  connected: boolean;
  switching: boolean;
  canSend: boolean;
  compact?: boolean;
  adaptive?: boolean;
  onPick: (t: string) => void;
}) {
  // HITL + empty: shrink the panel so approval/ask stay visible without scrolling past chrome.
  const hitlCompact = !!compact;
  const dockedEmpty = !!adaptive;
  const padX = dockedEmpty ? "px-3" : "px-4 sm:px-6";
  const kbd =
    "rounded-sm border border-ink-700 bg-ink-900 px-1.5 py-0.5 font-mono text-[10px] text-ink-400";
  return (
    <div
      className={`chat-empty relative flex min-w-0 flex-col ${
        dockedEmpty
          ? hitlCompact
            ? "min-h-0"
            : "min-h-full"
          : hitlCompact
            ? "min-h-0"
            : "h-full"
      }`}
    >
      <div
        className={`relative mx-auto flex w-full flex-1 flex-col justify-center ${padX} ${
          hitlCompact ? "py-3" : dockedEmpty ? "py-6" : "py-10"
        } ${dockedEmpty ? "max-w-none" : "max-w-[40rem]"}`}
      >
        <div className={`border-l border-ink-800 ${hitlCompact ? "py-1 pl-3" : "py-2 pl-5 sm:pl-6"}`}>
          <div className="flex items-center gap-3">
            <BrandMark size={22} />
            <div className="min-w-0">
              <p className="font-mono text-[10px] uppercase tracking-[0.14em] text-ink-500">
                Session ready
              </p>
              <p className="mt-0.5 truncate font-mono text-[11px] text-accent-soft">
                {basename(workspace) || workspace || "this workspace"}
                {!connected && <span className="ml-2 text-ink-600">· connecting…</span>}
              </p>
            </div>
          </div>

          {switching ? (
            <p className="mt-4 font-mono text-[12px] text-ink-400">Loading session…</p>
          ) : (
            <>
              <h2 className="mt-5 font-display text-[1.15rem] font-semibold text-ink-100 sm:text-[1.25rem]">
                Start with the repository
              </h2>
              <p className="mt-2 max-w-lg text-[13px] leading-relaxed text-ink-400">
                Ask CatCode to inspect the project before choosing the first change, or write a specific task below.
              </p>

              {!(dockedEmpty && hitlCompact) && (
                <button
                  type="button"
                  disabled={!canSend}
                  onClick={() => {
                    if (!canSend) return;
                    onPick(
                      "Inspect this project and summarize its structure, current state, and the most useful next task.",
                    );
                  }}
                  className="focus-ring mt-5 inline-flex min-h-11 items-center gap-2 rounded-sm border border-ink-700 bg-ink-900 px-3.5 py-2.5 text-[13px] font-medium text-ink-100 transition-colors hover:border-accent hover:text-accent-soft disabled:cursor-not-allowed disabled:opacity-40"
                >
                  Inspect project
                  <SendIcon width={13} height={13} />
                </button>
              )}

              {!hitlCompact && (
                <p className="mt-5 font-mono text-[10px] leading-relaxed text-ink-600">
                  <kbd className={kbd}>/</kbd> commands{" "}
                  <span className="text-ink-700">·</span>{" "}
                  <kbd className={kbd}>@</kbd> files{" "}
                  <span className="text-ink-700">·</span> sessions stay live across devices
                </p>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function AgentActivity({ compact, activity, hasWorkState }: { compact: boolean; activity?: string; hasWorkState: boolean }) {
  const label = activity?.trim() || (hasWorkState ? "Working through the plan" : "Understanding your request");
  return (
    <div
      className={`flex h-8 w-full items-center gap-2 border-b border-ink-800 bg-ink-925 ${
        compact ? "px-2" : "px-3 sm:px-6"
      }`}
      role="status"
      aria-live="polite"
    >
      <div className="mx-auto flex w-full max-w-[60rem] items-center gap-2">
        <span className="h-1.5 w-1.5 shrink-0 animate-pulse rounded-none bg-accent-soft" aria-hidden="true" />
        <span className="min-w-0 flex-1 truncate font-mono text-[10px] uppercase tracking-[0.12em] text-ink-400">{label}</span>
        {!compact && (
          <span className="ml-auto hidden items-center gap-1.5 font-mono text-[10px] text-ink-600 sm:flex" aria-label="Agent activity stages">
            <span className="text-accent-soft">Understand</span>
            <span className="text-ink-700">→</span>
            <span className={hasWorkState ? "text-accent-soft" : ""}>Work</span>
            <span className="text-ink-700">→</span>
            <span>Respond</span>
          </span>
        )}
      </div>
    </div>
  );
}

function KeyOverlay({
  value,
  busy,
  onChange,
  onSubmit,
  onDismiss,
}: {
  value: string;
  busy: boolean;
  onChange: (v: string) => void;
  onSubmit: () => void;
  onDismiss: () => void;
}) {
  const closeRef = useOutsideClose(onDismiss);
  const trapRef = useFocusTrap<HTMLDivElement>();
  return (
    <div className="modal-backdrop">
      <div
        ref={mergeRefs(closeRef, trapRef)}
        className="modal-sheet modal-sheet-auto relative max-w-md p-6"
        role="dialog"
        aria-modal="true"
        aria-label="Connect your provider"
      >
        <button
          onClick={onDismiss}
          className="absolute right-3 top-3 rounded-sm p-1 text-ink-500 transition-colors hover:bg-ink-800 hover:text-ink-100"
          aria-label="Dismiss"
          title="Dismiss (use /login to enter a key later)"
        >
          <svg width={16} height={16} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round"><path d="M18 6L6 18M6 6l12 12" /></svg>
        </button>
        <div className="mb-4 flex items-center gap-3">
          <span className="flex h-10 w-10 items-center justify-center rounded-sm border border-ink-800 bg-ink-900 text-accent-soft">
            <ShieldIcon width={18} height={18} />
          </span>
          <div>
            <h2 className="text-[15px] font-semibold text-ink-100">Connect your provider</h2>
            <p className="text-[12px] text-ink-400">Enter an API key to start chatting.</p>
          </div>
        </div>
        <input
          type="password"
          autoFocus
          value={value}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onSubmit();
          }}
          placeholder="sk-..."
          className="w-full rounded-sm border border-ink-700 bg-ink-950 px-3.5 py-2.5 font-mono text-[13px] text-ink-100 placeholder:text-ink-600 focus:border-accent focus:outline-none"
        />
        <button
          onClick={onSubmit}
          disabled={busy || !value.trim()}
          className="mt-3 w-full rounded-sm bg-accent py-2.5 text-[13px] font-semibold text-white transition-colors hover:bg-accent-soft disabled:cursor-not-allowed disabled:bg-ink-800 disabled:text-ink-500"
        >
          {busy ? "Connecting…" : "Connect"}
        </button>
        <p className="mt-3 text-center text-[11px] text-ink-500">
          Paste a key here, or use <code className="font-mono text-ink-400">/login</code> for OAuth.
        </p>
      </div>
    </div>
  );
}
