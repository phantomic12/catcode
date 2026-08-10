// Reducer unit tests — exercises the pure reduce() function for the key
// message-assembly rules and state transitions. Run with `bun test`.
import { test, expect, describe } from "bun:test";
import { reduce, initialState, MAX_TERMINAL_RUNS } from "./reducer";
import type { AgentEvent, SandboxPreflightReport, UIMessage } from "./types";

const ev = (e: AgentEvent) => reduce(initialState, e);

// Type guards so we can access role-specific fields without casts.
function isUser(m: UIMessage): m is import("./types").UserMsg {
  return m.role === "user";
}
function isAssistant(m: UIMessage): m is import("./types").AssistantMsg {
  return m.role === "assistant";
}

describe("initial state", () => {
  test("has sensible defaults", () => {
    expect(initialState.ready).toBeNull();
    expect(initialState.messages).toEqual([]);
    expect(initialState.streaming).toBe(false);
    expect(initialState.switching).toBe(false);
    expect(initialState.projects).toEqual([]);
  });
});

describe("synthetic events", () => {
  test("_user adds a user message and sets streaming", () => {
    const s = ev({ type: "_user", text: "hello" });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0].role).toBe("user");
    expect(isUser(s.messages[0]) ? s.messages[0].text : "").toBe("hello");
    expect(s.streaming).toBe(true);
  });

  test("_user with steer flag marks the message", () => {
    const s = ev({ type: "_user", text: "redirect", steer: true });
    expect(s.messages[0].role).toBe("user");
    expect(isUser(s.messages[0]) ? s.messages[0].steer : false).toBe(true);
  });

  test("_select_model sets the selected model", () => {
    const s = ev({ type: "_select_model", id: "glm-5.2" });
    expect(s.selectedModel).toBe("glm-5.2");
  });

  test("_set_switching toggles the switching flag", () => {
    const s = ev({ type: "_set_switching", switching: true });
    expect(s.switching).toBe(true);
    const s2 = reduce(s, { type: "_set_switching", switching: false });
    expect(s2.switching).toBe(false);
  });

  test("_dismiss_toast removes a toast by id", () => {
    let s = ev({ type: "error", message: "boom" });
    const id = s.toasts[0].id;
    s = reduce(s, { type: "_dismiss_toast", id });
    expect(s.toasts).toHaveLength(0);
  });
});

describe("plugin trust", () => {
  test("prompt becomes a visible pending decision gate", () => {
    const s = ev({
      type: "plugin_trust_prompt",
      plugins: [{ name: "workspace-hook", version: "1.0", decision: "" }],
    });
    expect(s.pendingPluginTrust?.[0]?.name).toBe("workspace-hook");
    expect(s.toasts.at(-1)?.message).toContain("1 project plugin trust decision");
  });

  test("applied decision clears the pending gate", () => {
    let s = ev({
      type: "plugin_trust_prompt",
      plugins: [{ name: "workspace-hook", decision: "" }],
    });
    s = reduce(s, { type: "plugin_trust_applied", trusted: ["workspace-hook"], denied: [], loaded: 1 });
    expect(s.pendingPluginTrust).toBeNull();
  });
});

describe("assistant message assembly", () => {
  test("delta lazily begins an assistant message", () => {
    const s = ev({ type: "delta", text: "Hi" });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0].role).toBe("assistant");
    expect(isAssistant(s.messages[0]) ? s.messages[0].text : "").toBe("Hi");
    expect(s.currentAssistantId).not.toBeNull();
    expect(s.streaming).toBe(true);
  });

  test("multiple deltas append to the same assistant message", () => {
    let s = ev({ type: "delta", text: "Hello" });
    s = reduce(s, { type: "delta", text: " world" });
    expect(s.messages).toHaveLength(1);
    expect(isAssistant(s.messages[0]) ? s.messages[0].text : "").toBe("Hello world");
  });

  test("done finalizes the assistant and clears streaming", () => {
    let s = ev({ type: "delta", text: "Hi" });
    expect(s.streaming).toBe(true);
    s = reduce(s, { type: "done" });
    expect(s.streaming).toBe(false);
    expect(s.currentAssistantId).toBeNull();
    expect(isAssistant(s.messages[0]) ? s.messages[0].streaming : false).toBe(false);
  });

  test("thinking accumulates into the assistant message", () => {
    let s = ev({ type: "thinking", text: "hmm" });
    s = reduce(s, { type: "thinking", text: "..." });
    expect(isAssistant(s.messages[0]) ? s.messages[0].thinking : "").toBe("hmm...");
  });
});

describe("tool calls", () => {
  test("tool_call finalizes the current assistant and attaches the call", () => {
    let s = ev({ type: "delta", text: "Let me check" });
    s = reduce(s, { type: "tool_call", id: "t1", name: "read_file", args: '{"path":"a.ts"}' });
    const a = s.messages[0];
    expect(a.role).toBe("assistant");
    if (!isAssistant(a)) throw new Error("expected assistant");
    expect(a.toolCalls).toHaveLength(1);
    expect(a.toolCalls[0].name).toBe("read_file");
    expect(a.toolCalls[0].args.path).toBe("a.ts");
  });

  test("tool_result matches its tool call by id", () => {
    let s = ev({ type: "delta", text: "" });
    s = reduce(s, { type: "tool_call", id: "t1", name: "bash", args: "{}" });
    s = reduce(s, { type: "tool_result", id: "t1", ok: true, output: "done" });
    const a = s.messages[0];
    if (!isAssistant(a)) throw new Error("expected assistant");
    expect(a.toolCalls[0].result?.ok).toBe(true);
    expect(a.toolCalls[0].result?.output).toBe("done");
  });

  test("a delta after tool_call begins a fresh assistant message", () => {
    let s = ev({ type: "delta", text: "first" });
    s = reduce(s, { type: "tool_call", id: "t1", name: "bash", args: "{}" });
    s = reduce(s, { type: "tool_result", id: "t1", ok: true, output: "ok" });
    s = reduce(s, { type: "delta", text: "second" });
    expect(s.messages).toHaveLength(2);
    expect(s.messages[1].role).toBe("assistant");
    expect(isAssistant(s.messages[1]) ? s.messages[1].text : "").toBe("second");
  });
});

describe("sessions", () => {
  test("sorted by mtime descending", () => {
    const s = ev({
      type: "sessions",
      sessions: [
        { name: "old.jsonl", mtime: 100 },
        { name: "new.jsonl", mtime: 200 },
      ],
      files: [],
    });
    expect(s.sessions[0].name).toBe("new.jsonl");
    expect(s.sessions[1].name).toBe("old.jsonl");
  });

  test("preserves title field", () => {
    const s = ev({
      type: "sessions",
      sessions: [{ name: "a.jsonl", mtime: 1, title: "My Session", messages: 5 }],
      files: [],
    });
    expect(s.sessions[0].title).toBe("My Session");
    expect(s.sessions[0].messages).toBe(5);
  });

  test("indexes bridge session-status snapshots by absolute session path", () => {
    const state = reduce(initialState, {
      type: "session_status",
      sessions: [
        {
          sessionFile: "/sessions/a.jsonl",
          workspace: "/project-a",
          title: "Background task",
          streaming: true,
          running: true,
          needsAttention: false,
          viewers: 0,
          lastEventAt: 123,
        },
      ],
    });
    expect(state.liveSessions["/sessions/a.jsonl"]).toMatchObject({
      workspace: "/project-a",
      streaming: true,
      running: true,
    });
  });
});

describe("notifications", () => {
  test("_add_notifications appends feed items", () => {
    const s = reduce(initialState, {
      type: "_add_notifications",
      items: [
        {
          id: "n1",
          sessionFile: "/a.jsonl",
          workspace: "/p",
          title: "Session A",
          kind: "attention",
          attentionKind: "approval",
          ts: 1,
          read: false,
        },
      ],
    });
    expect(s.notifications).toHaveLength(1);
    expect(s.notifications[0].title).toBe("Session A");
  });

  test("_add_notifications dedups by session+kind (refresh, not stack)", () => {
    let s = reduce(initialState, {
      type: "_add_notifications",
      items: [
        { id: "n1", sessionFile: "/a.jsonl", workspace: "/p", title: "A", kind: "attention", ts: 1, read: false },
      ],
    });
    s = reduce(s, {
      type: "_add_notifications",
      items: [
        { id: "n2", sessionFile: "/a.jsonl", workspace: "/p", title: "A", kind: "attention", ts: 2, read: false },
      ],
    });
    // Same session+kind while still unread -> refresh (bump ts), not a duplicate.
    expect(s.notifications.filter((n) => n.sessionFile === "/a.jsonl")).toHaveLength(1);
    expect(s.notifications[0].ts).toBe(2);
  });

  test("_dismiss_notification marks one read", () => {
    let s = reduce(initialState, {
      type: "_add_notifications",
      items: [
        { id: "n1", sessionFile: "/a.jsonl", workspace: "/p", title: "A", kind: "finished", ts: 1, read: false },
        { id: "n2", sessionFile: "/b.jsonl", workspace: "/p", title: "B", kind: "attention", ts: 2, read: false },
      ],
    });
    s = reduce(s, { type: "_dismiss_notification", id: "n1" });
    expect(s.notifications.find((n) => n.id === "n1")?.read).toBe(true);
    expect(s.notifications.find((n) => n.id === "n2")?.read).toBe(false);
  });

  test("_mark_notifications_read marks all read", () => {
    let s = reduce(initialState, {
      type: "_add_notifications",
      items: [
        { id: "n1", sessionFile: "/a.jsonl", workspace: "/p", title: "A", kind: "finished", ts: 1, read: false },
        { id: "n2", sessionFile: "/b.jsonl", workspace: "/p", title: "B", kind: "attention", ts: 2, read: false },
      ],
    });
    s = reduce(s, { type: "_mark_notifications_read" });
    expect(s.notifications.every((n) => n.read)).toBe(true);
  });

  test("_clear_notifications empties the feed", () => {
    let s = reduce(initialState, {
      type: "_add_notifications",
      items: [
        { id: "n1", sessionFile: "/a.jsonl", workspace: "/p", title: "A", kind: "finished", ts: 1, read: false },
      ],
    });
    s = reduce(s, { type: "_clear_notifications" });
    expect(s.notifications).toEqual([]);
  });
});

describe("session rename overlay", () => {
  test("_session_title updates a session's title", () => {
    let s = ev({
      type: "sessions",
      sessions: [{ name: "a.jsonl", mtime: 1, title: "old" }],
      files: [],
    });
    s = reduce(s, { type: "_session_title", name: "a.jsonl", title: "New Name" });
    expect(s.sessions[0].title).toBe("New Name");
  });

  test("_session_title with empty title clears it", () => {
    let s = ev({
      type: "sessions",
      sessions: [{ name: "a.jsonl", mtime: 1, title: "old" }],
      files: [],
    });
    s = reduce(s, { type: "_session_title", name: "a.jsonl", title: "" });
    expect(s.sessions[0].title).toBeUndefined();
  });
});

describe("workspace switching", () => {
  test("workspace_changed resets per-session state and clears switching", () => {
    let s = ev({ type: "_user", text: "hi" });
    s = reduce(s, { type: "_set_switching", switching: true });
    s = reduce(s, { type: "workspace_changed", workspace: "/new/path", projects: [] });
    expect(s.workspace).toBe("/new/path");
    expect(s.switching).toBe(false);
    expect(s.messages).toEqual([]);
    expect(s.currentSessionFile).toBeNull();
    expect(s.stats).toBeNull();
  });

  test("projects event sets the project list", () => {
    const s = ev({
      type: "projects",
      projects: [{ path: "/a", name: "a", lastUsed: 1 }],
    });
    expect(s.projects).toHaveLength(1);
    expect(s.projects[0].name).toBe("a");
  });
});

describe("toasts", () => {
  test("error pushes an error toast", () => {
    const s = ev({ type: "error", message: "failed" });
    expect(s.toasts).toHaveLength(1);
    expect(s.toasts[0].kind).toBe("error");
    expect(s.toasts[0].message).toBe("failed");
  });

  test("compacted pushes an info toast", () => {
    const s = ev({ type: "compacted", before_tokens: 1000, after_tokens: 500 });
    expect(s.toasts[0].kind).toBe("info");
    expect(s.toasts[0].message).toContain("1,000");
  });

  test("toasts are capped at 6", () => {
    let s = initialState;
    for (let i = 0; i < 10; i++) s = reduce(s, { type: "info", message: `msg ${i}` });
    expect(s.toasts.length).toBeLessThanOrEqual(6);
  });
});

describe("reset", () => {
  test("clears messages and streaming", () => {
    let s = ev({ type: "_user", text: "hi" });
    s = reduce(s, { type: "delta", text: "hey" });
    s = reduce(s, { type: "reset" });
    expect(s.messages).toEqual([]);
    expect(s.streaming).toBe(false);
    expect(s.currentAssistantId).toBeNull();
  });
});

describe("history replay", () => {
  test("extracts images from multimodal user messages", () => {
    const s = ev({
      type: "history",
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "What is this?" },
            { type: "image_url", image_url: { url: "data:image/png;base64,abc" } },
          ],
        },
      ],
    });
    expect(s.messages).toHaveLength(1);
    const u = s.messages[0];
    expect(u.role).toBe("user");
    if (!isUser(u)) throw new Error("expected user");
    expect(u.text).toBe("What is this?");
    expect(u.images).toEqual(["data:image/png;base64,abc"]);
  });

  test("marks replayed tool results as unknown (no live ok/error)", () => {
    const s = ev({
      type: "history",
      messages: [
        { role: "user", content: "list files" },
        {
          role: "assistant",
          content: null,
          tool_calls: [{ id: "t1", type: "function", function: { name: "list_dir", arguments: "{}" } }],
        },
        { role: "tool", tool_call_id: "t1", content: "a\nb\nc" },
      ],
    });
    const a = s.messages.find((m) => m.role === "assistant");
    if (!a || !isAssistant(a)) throw new Error("expected assistant");
    expect(a.toolCalls).toHaveLength(1);
    expect(a.toolCalls[0].result?.output).toBe("a\nb\nc");
    expect(a.toolCalls[0].result?.unknown).toBe(true);
  });

  test("replays persisted advisor findings as transcript cards", () => {
    const s = ev({
      type: "history",
      messages: [
        {
          role: "system",
          content: '<advisory advisor="Correctness" severity="concern" scope="main">Missing focused regression coverage</advisory>',
        },
      ],
    });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0]).toMatchObject({
      role: "advisor",
      advisor: "Correctness",
      severity: "concern",
      state: "finding",
      text: "Missing focused regression coverage",
    });
  });

  test("peels MiniMax <think> tags from assistant content on history load", () => {
    const s = ev({
      type: "history",
      messages: [
        {
          role: "assistant",
          content:
            "<think>\nstep one\n</think>\n\n</think>\n\n## Answer\nHi there",
        },
      ],
    });
    const a = s.messages[0];
    if (!isAssistant(a)) throw new Error("expected assistant");
    expect(a.thinking).toBe("step one");
    expect(a.text).toBe("## Answer\nHi there");
    expect(a.text.includes("<think>")).toBe(false);
    expect(a.text.includes("</think>")).toBe(false);
  });

});

describe("reset clears stale prompts", () => {
  test("reset drops a pending intercom ask", () => {
    let s = ev({
      type: "intercom_message",
      id: "ic1",
      from: "worker",
      message: "decide?",
      reason: "need_decision",
    });
    expect(s.pendingIntercom).not.toBeNull();
    s = reduce(s, { type: "reset" });
    expect(s.pendingIntercom).toBeNull();
  });
});

describe("subagent run pruning", () => {
  test("terminal runs are pruned past the cap (no unbounded growth)", () => {
    let s = initialState;
    const N = MAX_TERMINAL_RUNS + 40;
    for (let i = 0; i < N; i++) {
      const id = `run-${i}`;
      s = reduce(s, {
        type: "subagent_start",
        run_id: id,
        mode: "single",
        agent: "worker",
        agents: ["worker"],
        task: "t",
        depth: 0,
        started_at: 1000 + i,
      });
      s = reduce(s, {
        type: "subagent_done",
        run_id: id,
        state: "completed",
        ended_at: 2000 + i,
      });
    }
    expect(Object.keys(s.subagentRuns).length).toBeLessThanOrEqual(MAX_TERMINAL_RUNS);
    // Most-recent terminal run is retained; oldest was pruned.
    expect(s.subagentRuns[`run-${N - 1}`]).toBeDefined();
    expect(s.subagentRuns["run-0"]).toBeUndefined();
  });

  test("running runs are never pruned even past the cap", () => {
    let s = initialState;
    const N = MAX_TERMINAL_RUNS + 12;
    for (let i = 0; i < N; i++) {
      const id = `run-${i}`;
      s = reduce(s, {
        type: "subagent_start",
        run_id: id,
        mode: "single",
        agents: [],
        task: "t",
        depth: 0,
        started_at: i,
      });
      // leave odd-indexed runs running; complete even ones.
      if (i % 2 === 0)
        s = reduce(s, { type: "subagent_done", run_id: id, state: "completed", ended_at: i });
    }
    for (let i = 1; i < N; i += 2) {
      expect(s.subagentRuns[`run-${i}`]).toBeDefined();
      expect(s.subagentRuns[`run-${i}`].state).toBe("running");
    }
  });
});

describe("metrics tps_est", () => {
  test("maps mid-stream tps_est onto tps", () => {
    const s = reduce(initialState, {
      type: "metrics",
      tps_est: 42.5,
    } as AgentEvent);
    expect(s.metrics?.tps).toBe(42.5);
    expect(s.retrying).toBe(false);
  });

  test("prefers final tps over tps_est", () => {
    let s = reduce(initialState, { type: "metrics", tps_est: 10 } as AgentEvent);
    s = reduce(s, { type: "metrics", tps: 55, elapsed_ms: 1000 } as AgentEvent);
    expect(s.metrics?.tps).toBe(55);
  });
});

describe("intercom need_decision", () => {
  test("empty reason is log-only (not a blocking ask)", () => {
    const s = reduce(initialState, {
      type: "intercom_message",
      id: "m1",
      from: "explorer",
      to: "orchestrator",
      reason: "",
      message: "progress note",
    } as AgentEvent);
    expect(s.pendingIntercom).toBeNull();
    expect(s.intercomLog[0]?.kind).toBe("reply");
  });

  test("need_decision opens pendingIntercom", () => {
    const s = reduce(initialState, {
      type: "intercom_message",
      id: "m2",
      from: "explorer",
      to: "orchestrator",
      reason: "need_decision",
      message: "which path?",
    } as AgentEvent);
    expect(s.pendingIntercom?.request_id).toBe("m2");
    expect(s.pendingIntercom?.message).toBe("which path?");
  });
});

describe("config_changed", () => {
  test("patches ready.bash_timeout_secs and auto_compact", () => {
    let s = reduce(initialState, {
      type: "ready",
      models: [],
      authed: true,
      workspace: "/tmp",
      approval: "destructive",
      base_url: "",
      provider: "x",
      providerKind: "openai",
      providers: ["x"],
      bash_timeout_secs: 30,
      auto_compact: true,
      resumed_messages: 0,
    } as AgentEvent);
    s = reduce(s, { type: "config_changed", key: "bash_timeout_secs", value: 90 });
    expect(s.ready?.bash_timeout_secs).toBe(90);
    s = reduce(s, { type: "config_changed", key: "auto_compact", value: false });
    expect(s.ready?.auto_compact).toBe(false);
  });

  test("patches ready.sandbox", () => {
    let s = reduce(initialState, {
      type: "ready",
      models: [],
      authed: true,
      workspace: "/tmp",
      approval: "destructive",
      base_url: "",
      provider: "x",
      providerKind: "openai",
      providers: ["x"],
      bash_timeout_secs: 30,
      sandbox: "none",
      resumed_messages: 0,
    } as AgentEvent);
    s = reduce(s, { type: "config_changed", key: "sandbox", value: "microsandbox" });
    expect(s.ready?.sandbox).toBe("microsandbox");
    // The reducer must also mirror the mode into the sandbox runtime status and
    // clear readiness for a requested-but-unready microsandbox (fail-closed).
    expect(s.sandbox.mode).toBe("microsandbox");
    expect(s.sandbox.ready).toBe(false);
  });
});

describe("sandbox (Microsandbox)", () => {
  const readyReady = {
    type: "ready",
    models: [],
    authed: true,
    workspace: "/tmp",
    approval: "destructive",
    base_url: "",
    provider: "x",
    providerKind: "openai",
    providers: ["x"],
    bash_timeout_secs: 30,
    sandbox: "microsandbox",
    shell: "bash",
    sandboxImage: "ghcr.io/catalystctl/catcode-sandbox:1",
    sandboxCpus: 2,
    sandboxMemoryMb: 2048,
    sandboxNetworkMode: "restricted",
    sandboxReady: true,
    resumed_messages: 0,
  } as AgentEvent;

  test("ready payload seeds sandbox runtime status (source of truth)", () => {
    const s = reduce(initialState, readyReady);
    expect(s.sandbox.mode).toBe("microsandbox");
    expect(s.sandbox.ready).toBe(true);
    expect(s.sandbox.image).toBe("ghcr.io/catalystctl/catcode-sandbox:1");
    expect(s.sandbox.cpus).toBe(2);
    expect(s.sandbox.memoryMb).toBe(2048);
    expect(s.sandbox.networkMode).toBe("restricted");
    // Guest is always Linux bash inside the microsandbox, even on Windows.
    expect(s.sandbox.shell).toBe("bash");
  });

  test("ready normalizes legacy/unknown sandbox values to none", () => {
    const s = reduce(
      initialState,
      { ...readyReady, sandbox: "firejail", sandboxReady: false, shell: "powershell" } as AgentEvent,
    );
    // The core migrates legacy values before emitting, but the reducer stays
    // defensive: anything that is not exactly "microsandbox" is treated as off.
    expect(s.sandbox.mode).toBe("none");
    expect(s.sandbox.ready).toBe(false);
    expect(s.sandbox.shell).toBe("powershell");
  });

  test("sandbox_status stores the preflight report and readiness", () => {
    let s = reduce(initialState, readyReady);
    const report: SandboxPreflightReport = {
      requested: true,
      supported: true,
      ready: false,
      platform: "linux",
      architecture: "x86_64",
      checks: [
        { code: "kvm_device_missing", title: "KVM device", status: "fail", detail: "/dev/kvm absent" },
        { code: "runtime_missing", title: "Runtime", status: "warn", detail: "not downloaded" },
      ],
      actions: [
        { title: "Load KVM", explanation: "modprobe", command: "sudo modprobe kvm", requires_admin: true, requires_reboot: false },
      ],
    };
    s = reduce(s, { type: "sandbox_status", mode: "microsandbox", report } as AgentEvent);
    expect(s.sandbox.report).toEqual(report);
    // report.ready is the source of truth — overrides the prior ready:true.
    expect(s.sandbox.ready).toBe(false);
    expect(s.sandbox.mode).toBe("microsandbox");
  });

  test("sandbox_prepare_progress tracks the phase without clearing readiness", () => {
    let s = reduce(initialState, readyReady);
    s = reduce(s, { type: "sandbox_prepare_progress", phase: "downloading_runtime" } as AgentEvent);
    expect(s.sandbox.preparePhase).toBe("downloading_runtime");
    // Progress alone must not flip readiness either way.
    expect(s.sandbox.ready).toBe(true);
  });

  test("sandbox_ready flips readiness and clears transient state", () => {
    let s = reduce(initialState, { ...readyReady, sandboxReady: false } as AgentEvent);
    expect(s.sandbox.ready).toBe(false);
    s = reduce(s, { type: "sandbox_prepare_progress", phase: "pulling_image" } as AgentEvent);
    s = reduce(s, { type: "sandbox_ready", ready: true } as AgentEvent);
    expect(s.sandbox.ready).toBe(true);
    expect(s.sandbox.preparePhase).toBeNull();
  });

  test("sandbox_error is fail-closed: clears readiness + prepare + toasts", () => {
    let s = reduce(initialState, readyReady);
    s = reduce(s, { type: "sandbox_prepare_progress", phase: "booting" } as AgentEvent);
    s = reduce(s, { type: "sandbox_error", error: "sandbox_boot_failed" } as AgentEvent);
    expect(s.sandbox.ready).toBe(false);
    expect(s.sandbox.preparePhase).toBeNull();
    expect(s.sandbox.error).toBe("sandbox_boot_failed");
    expect(s.toasts.some((t) => t.kind === "error" && t.message.includes("sandbox_boot_failed"))).toBe(true);
  });

  test("requested-but-not-ready microsandbox never reports ready (fail-closed)", () => {
    // Enabling microsandbox via config_changed must not pretend to be ready.
    let s = reduce(initialState, { ...readyReady, sandbox: "none", sandboxReady: false } as AgentEvent);
    s = reduce(s, { type: "config_changed", key: "sandbox", value: "microsandbox" });
    expect(s.sandbox.mode).toBe("microsandbox");
    expect(s.sandbox.ready).toBe(false);
    // Disabling returns to ready (host execution is always available).
    s = reduce(s, { type: "config_changed", key: "sandbox", value: "none" });
    expect(s.sandbox.mode).toBe("none");
    expect(s.sandbox.ready).toBe(true);
  });
});

describe("diagnostics + agents", () => {
  test("stores context breakdown and agents list", () => {
    let s = reduce(initialState, {
      type: "context_breakdown",
      total_tokens: 1000,
      context_window: 8000,
      pct: 12,
      messages: 4,
      system_tokens: 200,
      by_role: { user: 300 },
      top_consumers: [{ index: 1, role: "user", tokens: 300, preview: "hi" }],
    } as AgentEvent);
    expect(s.contextBreakdown?.total_tokens).toBe(1000);
    expect(s.toasts).toHaveLength(0);
    s = reduce(s, {
      type: "agents",
      agents: [{ name: "scout", description: "explore", source: "builtin" }],
    } as AgentEvent);
    expect(s.availableAgents).toEqual([
      { name: "scout", description: "explore", source: "builtin" },
    ]);
  });
});

describe("http_retry clears on delta", () => {
  test("retrying flag clears when streaming resumes", () => {
    let s = reduce(initialState, { type: "http_retry", status: 429 } as AgentEvent);
    expect(s.retrying).toBe(true);
    s = reduce(s, { type: "delta", text: "hi" });
    expect(s.retrying).toBe(false);
  });
});

describe("finishTurn clears gates", () => {
  test("aborted drops pending approval/ask/sudo/intercom and follow-up queue", () => {
    let s = reduce(initialState, {
      type: "approval_request",
      request_id: "a1",
      tool: "bash",
      args: "{}",
    } as AgentEvent);
    s = reduce(s, {
      type: "info",
      message: "prompt queued; will run after the current turn",
    } as AgentEvent);
    expect(s.followUpQueued).toBe(true);
    expect(s.pendingApproval).not.toBeNull();
    s = reduce(s, { type: "aborted" });
    expect(s.pendingApproval).toBeNull();
    expect(s.followUpQueued).toBe(false);
    expect(s.streaming).toBe(false);
  });
});

describe("error does not end the turn", () => {
  // Covered by "pre-turn error clears streaming" — kept as a pointer in history.
});

describe("pre-turn error clears streaming", () => {
  test("error after optimistic user with no assistant clears streaming", () => {
    let s = reduce(initialState, { type: "_user", text: "/skill:missing" });
    expect(s.streaming).toBe(true);
    s = reduce(s, { type: "error", message: "unknown skill" });
    expect(s.streaming).toBe(false);
  });

  test("error mid-turn keeps streaming", () => {
    let s = reduce(initialState, { type: "_user", text: "hi" });
    s = reduce(s, { type: "delta", text: "hello" });
    expect(s.streaming).toBe(true);
    s = reduce(s, { type: "error", message: "no pending approval" });
    expect(s.streaming).toBe(true);
  });
});

describe("digested without before_tokens", () => {
  test("subagent digested does not throw", () => {
    const s = reduce(initialState, {
      type: "digested",
      results: 2,
      after_tokens: 1000,
    } as AgentEvent);
    expect(s.toasts[0].message).toContain("Reclaimed");
    expect(s.toasts[0].message).toContain("1,000");
  });
});

describe("pre-turn error drops ghost user", () => {
  test("unknown skill error removes optimistic user line", () => {
    let s = reduce(initialState, { type: "_user", text: "/skill:missing" });
    expect(s.messages).toHaveLength(1);
    s = reduce(s, { type: "error", message: "unknown skill" });
    expect(s.messages).toHaveLength(0);
    expect(s.streaming).toBe(false);
  });

  test("core exited error keeps the last user line", () => {
    let s = reduce(initialState, { type: "_user", text: "hi" });
    s = reduce(s, { type: "aborted" });
    expect(s.messages).toHaveLength(1);
    s = reduce(s, {
      type: "error",
      message: "This session's core exited. Sending a message will restart it.",
    });
    expect(s.messages).toHaveLength(1);
    expect(s.goalMode).toBeNull();
  });
});

describe("models rebinds selectedModel", () => {
  test("clears selection when model disappears from list", () => {
    let s = reduce(initialState, {
      type: "models",
      models: [
        { id: "a", name: "A", provider: "p" },
        { id: "b", name: "B", provider: "p" },
      ],
    } as never);
    s = reduce(s, { type: "_select_model", id: "b" });
    expect(s.selectedModel).toBe("b");
    s = reduce(s, {
      type: "models",
      models: [{ id: "a", name: "A", provider: "p" }],
    } as never);
    expect(s.selectedModel).toBe("a");
  });
});

describe("models_refreshed", () => {
  test("clears modelsRefreshing and toasts the count", () => {
    let s = reduce(initialState, {
      type: "_set_models_refreshing",
      refreshing: true,
    });
    expect(s.modelsRefreshing).toBe(true);
    s = reduce(s, {
      type: "models_refreshed",
      count: 3,
      providers: { umans: ["a", "b", "c"] },
    });
    expect(s.modelsRefreshing).toBe(false);
    expect(s.toasts.some((t) => t.message.includes("3 models"))).toBe(true);
  });
});

describe("history tokens_in", () => {
  test("history with tokens_in seeds stats", () => {
    const s = reduce(initialState, {
      type: "history",
      messages: [{ role: "user", content: "hi" }],
      tokens_in: 42,
    } as never);
    expect(s.stats?.tokens_in).toBe(42);
    expect(s.messages).toHaveLength(1);
  });
});

describe("goal_state idle clears plan", () => {
  test("idle clears goalMode and goalPlan", () => {
    const seeded = {
      ...initialState,
      goalMode: {
        id: "g1",
        goal: "x",
        phase: "running",
        concurrency: 1,
        max_tasks: 1,
        allowed_models: [] as string[],
        allowed_providers: [] as string[],
        auto_deploy: false,
        prompts: [],
        active_run_ids: [],
        version: 1,
        error: null,
        parent_model: "",
      },
      goalPlan: {
        id: "g1",
        summary: "plan",
        steps: [],
        risks: [],
        validation: [],
        version: 1,
      },
    };
    const s = reduce(seeded, { type: "goal_state", id: "", phase: "idle" } as never);
    expect(s.goalMode).toBeNull();
    expect(s.goalPlan).toBeNull();
    expect(s.goalIterations).toEqual([]);
  });
});

describe("goal deployment streaming lifecycle", () => {
  const goalState = (phase: string, autoDeploy = true) =>
    ({
      type: "goal_state",
      id: "g1",
      goal: "ship it",
      phase,
      concurrency: 2,
      max_tasks: 4,
      allowed_models: [],
      allowed_providers: [],
      auto_deploy: autoDeploy,
      prompts: [],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "model",
    }) as AgentEvent;

  test("planning done stays live while an auto-deploy plan is handed off", () => {
    let s = reduce(initialState, { type: "_user", text: "ship it" });
    s = reduce(s, goalState("plan_ready", true));
    s = reduce(s, { type: "done" });
    expect(s.streaming).toBe(true);
  });

  test("deploying re-arms streaming even when it arrives after planning done", () => {
    let s = reduce(initialState, { type: "done" });
    expect(s.streaming).toBe(false);
    s = reduce(s, goalState("deploying"));
    expect(s.streaming).toBe(true);
    s = reduce(s, goalState("running"));
    expect(s.streaming).toBe(true);
  });

  test("manual plan review and terminal goal phases are idle", () => {
    let s = reduce(initialState, { type: "_user", text: "ship it" });
    s = reduce(s, goalState("plan_ready", false));
    s = reduce(s, { type: "done" });
    expect(s.streaming).toBe(false);

    s = reduce(s, goalState("deploying"));
    expect(s.streaming).toBe(true);
    s = reduce(s, goalState("synthesizing"));
    expect(s.streaming).toBe(true);
    s = reduce(s, { type: "done" });
    expect(s.streaming).toBe(true);
    s = reduce(s, goalState("done"));
    expect(s.streaming).toBe(false);
  });
});

describe("undo local + pendingUndo reset", () => {
  test("_undo_local trims last turn and sets pendingUndo", () => {
    let s = reduce(initialState, { type: "_user", text: "one" });
    s = reduce(s, { type: "delta", text: "a1" });
    s = reduce(s, { type: "done" });
    s = reduce(s, { type: "_user", text: "two" });
    s = reduce(s, { type: "delta", text: "a2" });
    s = reduce(s, { type: "done" });
    expect(s.messages.length).toBeGreaterThanOrEqual(4);
    s = reduce(s, { type: "_undo_local" });
    expect(s.pendingUndo).toBe(true);
    expect(s.messages.some((m) => m.role === "user" && (m as { text: string }).text === "two")).toBe(false);
    expect(s.messages.some((m) => m.role === "user" && (m as { text: string }).text === "one")).toBe(true);
    s = reduce(s, { type: "reset" });
    expect(s.pendingUndo).toBe(false);
    expect(s.messages.some((m) => m.role === "user" && (m as { text: string }).text === "one")).toBe(true);
  });

  test("plain reset still clears messages", () => {
    let s = reduce(initialState, { type: "_user", text: "x" });
    s = reduce(s, { type: "reset" });
    expect(s.messages).toHaveLength(0);
  });

  test("_undo_local is idempotent (client + fanout)", () => {
    let s = reduce(initialState, { type: "_user", text: "one" });
    s = reduce(s, { type: "delta", text: "a1" });
    s = reduce(s, { type: "done" });
    s = reduce(s, { type: "_user", text: "two" });
    s = reduce(s, { type: "delta", text: "a2" });
    s = reduce(s, { type: "done" });
    const afterFirst = reduce(s, { type: "_undo_local" });
    const afterSecond = reduce(afterFirst, { type: "_undo_local" });
    expect(afterSecond.messages.length).toBe(afterFirst.messages.length);
    expect(afterSecond.pendingUndo).toBe(true);
  });
});

describe("core protocol events", () => {
  test("cost_update stores session cost", () => {
    const s = ev({
      type: "cost_update",
      tokens_in: 100,
      tokens_out: 40,
      estimated_usd: 0.0123,
      model: "glm-5.2",
    });
    expect(s.cost?.estimated_usd).toBe(0.0123);
    expect(s.cost?.tokens_in).toBe(100);
  });

  test("file_change bumps seq and records path", () => {
    const s = ev({
      type: "file_change",
      path: "src/a.ts",
      tool: "edit",
    });
    expect(s.fileChangeSeq).toBe(1);
    expect(s.recentFileChanges[0]?.path).toBe("src/a.ts");
  });

  test("checkpoint_created + checkpoints list", () => {
    let s = ev({
      type: "checkpoint_created",
      id: "cp1",
      label: "manual",
      kind: "file",
      auto: false,
    });
    expect(s.checkpoints[0]?.id).toBe("cp1");
    expect(s.toasts.some((t) => t.message.includes("Checkpoint"))).toBe(true);
    s = reduce(s, {
      type: "checkpoints",
      checkpoints: [{ id: "cp2", label: "other", kind: "git" }],
    });
    expect(s.checkpoints).toHaveLength(1);
    expect(s.checkpoints[0].id).toBe("cp2");
  });

  test("protocol_hello stores capabilities", () => {
    const s = ev({
      type: "protocol_hello",
      version: "1",
      min_client: "1",
      capabilities: ["checkpoints", "worktrees"],
    });
    expect(s.protocolHello?.capabilities).toContain("checkpoints");
  });

  test("goal_step_verdict toasts, stores result, and lasting line", () => {
    const s = ev({ type: "goal_step_verdict", ok: false, output: "tests failed" });
    expect(s.lastGoalVerdict?.ok).toBe(false);
    expect(s.toasts.some((t) => t.kind === "error")).toBe(true);
    expect(s.messages.some((m) => m.role === "goal" && m.kind === "verdict")).toBe(true);
  });

  test("goal_step_complete appends lasting message and stores final", () => {
    const s = ev({
      type: "goal_step_complete",
      step_id: "s1",
      title: "Implement X",
      agent: "worker",
      ok: true,
      status: "done",
      summary: "Implemented X cleanly.",
      run_id: "r1",
    });
    expect(s.goalStepFinals.s1?.summary).toContain("Implemented X");
    const card = s.messages.find((m) => m.role === "goal" && m.kind === "step_complete");
    expect(card).toBeTruthy();
    if (card && card.role === "goal") {
      expect(card.text).toContain("Implemented X");
      expect(card.stepId).toBe("s1");
    }
  });

  test("goal_step_complete dedupes when goal_state also reports the step", () => {
    let s = ev({
      type: "goal_step_complete",
      step_id: "s1",
      title: "A",
      agent: "worker",
      ok: true,
      status: "done",
      summary: "done via event",
    });
    s = reduce(s, {
      type: "goal_state",
      id: "g1",
      goal: "ship it",
      phase: "running",
      concurrency: 2,
      max_tasks: 8,
      allowed_models: [],
      allowed_providers: [],
      auto_deploy: true,
      prompts: [
        {
          step_id: "s1",
          agent: "worker",
          title: "A",
          task: "do A",
          status: "done",
          summary: "done via event",
        },
      ],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
    } as never);
    const cards = s.messages.filter((m) => m.role === "goal" && m.kind === "step_complete");
    expect(cards).toHaveLength(1);
  });

  test("goal_state backfills lasting step card when discrete event missing", () => {
    const s = reduce(initialState, {
      type: "goal_state",
      id: "g1",
      goal: "ship it",
      phase: "running",
      concurrency: 2,
      max_tasks: 8,
      allowed_models: [],
      allowed_providers: [],
      auto_deploy: true,
      prompts: [
        {
          step_id: "s2",
          agent: "reviewer",
          title: "Review",
          task: "review",
          status: "done",
          summary: "Looks good",
        },
      ],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
    } as never);
    expect(s.goalStepFinals.s2?.summary).toBe("Looks good");
    expect(s.messages.some((m) => m.role === "goal" && m.kind === "step_complete")).toBe(true);
  });

  test("goal_phase synthesizing adds lasting bridge line", () => {
    const s = ev({
      type: "goal_phase",
      from: "running",
      to: "synthesizing",
      message: "writing completion summary",
    });
    expect(s.toasts.some((t) => t.message.includes("synthesizing"))).toBe(true);
    const phaseCard = s.messages.find((m) => m.role === "goal" && m.kind === "phase");
    expect(phaseCard).toBeTruthy();
    if (phaseCard && phaseCard.role === "goal") {
      expect(phaseCard.title).toContain("synthesizing");
    }
  });

  test("goal_completion_summary injects lasting completion card", () => {
    const s = ev({
      type: "goal_completion_summary",
      text: "All steps succeeded.\n- A: ok",
    });
    const card = s.messages.find((m) => m.role === "goal" && m.kind === "completion_summary");
    expect(card).toBeTruthy();
    if (card && card.role === "goal") {
      expect(card.text).toContain("All steps succeeded");
    }
  });

  test("goal_completion_summary empty uses goal stub not step stub", () => {
    const s = ev({ type: "goal_completion_summary", text: "" });
    const card = s.messages.find((m) => m.role === "goal" && m.kind === "completion_summary");
    expect(card).toBeTruthy();
    if (card && card.role === "goal") {
      expect(card.text).toContain("Goal finished");
      expect(card.text).not.toContain("step finished");
    }
  });

  test("goal_step_complete verifier remap appends failed update card", () => {
    let s = ev({
      type: "goal_step_complete",
      step_id: "s1",
      title: "Implement X",
      agent: "worker",
      ok: true,
      status: "done",
      summary: "Implemented X",
      run_id: "r1",
    });
    s = reduce(s, {
      type: "goal_step_complete",
      step_id: "s1",
      title: "Implement X",
      agent: "worker",
      ok: false,
      status: "failed",
      summary: "verifier failed: tests red",
      run_id: "r1",
    } as never);
    const cards = s.messages.filter((m) => m.role === "goal" && m.kind === "step_complete");
    expect(cards.length).toBe(2);
    const last = cards[cards.length - 1];
    expect(last && last.role === "goal" && last.status).toBe("failed");
    expect(last && last.role === "goal" && last.ok).toBe(false);
    expect(s.goalStepFinals.s1?.status).toBe("failed");
  });

  test("goal deploy info lines become lasting goal messages", () => {
    const s = ev({
      type: "info",
      message: "Goal plan approved — deploying (snapshotting workspace…)",
    });
    expect(s.toasts.some((t) => t.message.includes("approved"))).toBe(true);
    expect(
      s.messages.some(
        (m) => m.role === "goal" && m.kind === "phase" && m.text.includes("approved"),
      ),
    ).toBe(true);
  });

  test("_goal_approve_optimistic arms auto_deploy and lasting card", () => {
    let s = reduce(initialState, {
      type: "goal_state",
      id: "g1",
      goal: "ship it",
      phase: "plan_ready",
      concurrency: 2,
      max_tasks: 8,
      allowed_models: [],
      allowed_providers: [],
      auto_deploy: false,
      prompts: [],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
    } as never);
    s = reduce(s, { type: "_goal_approve_optimistic" });
    expect(s.goalMode?.auto_deploy).toBe(true);
    expect(s.streaming).toBe(true);
    expect(
      s.messages.some(
        (m) => m.role === "goal" && String(m.text).includes("Plan approved"),
      ),
    ).toBe(true);
  });

  test("subagent_done injects final summary into run transcript when empty", () => {
    let s = reduce(initialState, {
      type: "subagent_start",
      run_id: "run-a",
      mode: "single",
      agent: "worker",
      task: "do work",
      started_at: 1,
    } as never);
    s = reduce(s, {
      type: "subagent_done",
      run_id: "run-a",
      state: "completed",
      summary: "Finished the task.",
      ended_at: 2,
    } as never);
    const run = s.subagentRuns["run-a"];
    expect(run.summary).toBe("Finished the task.");
    expect(
      run.items.some((it) => it.kind === "message" && String(it.content).includes("Finished")),
    ).toBe(true);
  });

  test("finish tool_result keeps non-empty FINISH_MESSAGE", () => {
    let s = reduce(initialState, { type: "_user", text: "hi" } as never);
    s = reduce(s, { type: "delta", text: "working" } as never);
    s = reduce(s, {
      type: "tool_call",
      id: "tc1",
      name: "finish",
      args: "{}",
    } as never);
    s = reduce(s, {
      type: "tool_result",
      id: "tc1",
      ok: true,
      output: "This turn has finished",
    } as never);
    const withFinish = s.messages.some((m) => {
      if (m.role === "assistant") {
        return m.toolCalls.some(
          (tc) => tc.name === "finish" && (tc.result?.output ?? "").includes("finished"),
        );
      }
      if (m.role === "tool") {
        return m.toolName === "finish" && m.output.includes("finished");
      }
      return false;
    });
    expect(withFinish).toBe(true);
  });

  test("session_pinned updates session list", () => {
    let s = reduce(initialState, {
      type: "sessions",
      sessions: [{ name: "a.jsonl", mtime: 1, path: "/tmp/a.jsonl" }],
      files: [],
    });
    s = reduce(s, { type: "session_pinned", path: "/tmp/a.jsonl", pinned: true });
    expect(s.sessions[0].pinned).toBe(true);
  });
});


describe("CEO / Control Center goal events", () => {
  test("goal_state maps ceo fields and seeds goalIterations", () => {
    const s = reduce(initialState, {
      type: "goal_state",
      id: "goal-1",
      goal: "ship control center",
      phase: "reviewing",
      concurrency: 4,
      max_tasks: 8,
      allowed_models: [],
      allowed_providers: [],
      auto_deploy: true,
      ceo_mode: true,
      mode: "ceo",
      iteration: 1,
      max_iterations: 3,
      plan_revision: 0,
      max_plan_revisions: 2,
      review_verdict: { ok: true, summary: "plan looks good", evidence_paths: [], at: 1 },
      verify_verdict: null,
      remaining_gaps: [],
      certified: false,
      prompts: [],
      active_run_ids: [],
      version: 2,
      error: null,
      parent_model: "m",
    } as never);
    expect(s.goalMode?.ceo_mode).toBe(true);
    expect(s.goalMode?.phase).toBe("reviewing");
    expect(s.goalMode?.iteration).toBe(1);
    expect(s.goalMode?.review_verdict?.ok).toBe(true);
    expect(s.goalIterations).toHaveLength(1);
    expect(s.goalIterations[0].iteration).toBe(1);
    expect(s.streaming).toBe(true);
  });

  test("goal_iteration upserts budget counters", () => {
    let s = reduce(initialState, {
      type: "goal_state",
      id: "goal-1",
      goal: "x",
      phase: "verifying",
      concurrency: 2,
      max_tasks: 4,
      auto_deploy: true,
      ceo_mode: true,
      iteration: 0,
      max_iterations: 3,
      prompts: [],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
      allowed_models: [],
      allowed_providers: [],
    } as never);
    s = reduce(s, {
      type: "goal_iteration",
      id: "goal-1",
      iteration: 2,
      max_iterations: 3,
      plan_revision: 1,
      max_plan_revisions: 2,
    });
    expect(s.goalMode?.iteration).toBe(2);
    expect(s.goalMode?.plan_revision).toBe(1);
    expect(s.goalIterations.some((r) => r.iteration === 2)).toBe(true);
  });

  test("goal_review_verdict / goal_verify_verdict / goal_certified", () => {
    let s = reduce(initialState, {
      type: "goal_state",
      id: "goal-1",
      goal: "x",
      phase: "verifying",
      concurrency: 2,
      max_tasks: 4,
      auto_deploy: true,
      ceo_mode: true,
      iteration: 1,
      max_iterations: 3,
      prompts: [],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
      allowed_models: [],
      allowed_providers: [],
    } as never);
    s = reduce(s, {
      type: "goal_review_verdict",
      ok: false,
      summary: "needs tighter validation",
      iteration: 1,
      plan_revision: 1,
      evidence_paths: ["plan.md"],
    });
    expect(s.goalMode?.review_verdict?.ok).toBe(false);
    s = reduce(s, {
      type: "goal_verify_verdict",
      ok: false,
      summary: "gaps remain",
      iteration: 1,
      remaining_gaps: ["missing abort button"],
      evidence_paths: ["SUMMARY.md"],
    });
    expect(s.goalMode?.remaining_gaps).toEqual(["missing abort button"]);
    expect(s.lastGoalVerdict?.ok).toBe(false);
    s = reduce(s, {
      type: "goal_certified",
      summary: "all criteria met",
      iteration: 1,
      certified: true,
    });
    expect(s.goalMode?.certified).toBe(true);
    expect(s.goalIterations.find((r) => r.iteration === 1)?.certified).toBe(true);
  });

  test("idle goal_state clears iterations", () => {
    let s = reduce(initialState, {
      type: "goal_state",
      id: "goal-1",
      goal: "x",
      phase: "planning",
      concurrency: 1,
      max_tasks: 2,
      auto_deploy: true,
      ceo_mode: true,
      iteration: 0,
      prompts: [],
      active_run_ids: [],
      version: 1,
      error: null,
      parent_model: "",
      allowed_models: [],
      allowed_providers: [],
    } as never);
    s = reduce(s, { type: "goal_state", id: "", phase: "idle" } as never);
    expect(s.goalMode).toBeNull();
    expect(s.goalIterations).toEqual([]);
  });
});

describe("durable job and session tree events", () => {
  test("job list captures parent-child state and cancellation", () => {
    let state = ev({ type: "job_list", runs: [{ run_id: "child", parent_run_id: "parent", state: "completed", summary: "done" }] } as AgentEvent);
    expect(state.jobTree.child).toEqual({ runId: "child", parentRunId: "parent", state: "completed", summary: "done" });
    state = reduce(state, { type: "job_cancel_requested", run_id: "child" } as AgentEvent);
    expect(state.jobTree.child.state).toBe("cancelling");
  });

  test("session tree is retained for branch navigation", () => {
    const tree = { leaf: "entry-2", entries: [{ id: "entry-2" }] };
    const state = ev({ type: "session_tree", tree } as AgentEvent);
    expect(state.sessionTree).toEqual(tree);
  });

  test("session tree retains ancestry, siblings, and active leaf", () => {
    const tree = { leaf: "leaf", ancestry: ["root"], siblings: ["peer"], entries: [{ id: "root" }, { id: "leaf" }, { id: "peer" }] };
    const state = ev({ type: "session_tree", tree } as AgentEvent);
    expect(state.sessionTree).toEqual(tree);
  });

  test("process tool results populate status and logs without dedicated events", () => {
    const status = ev({ type: "tool_result", id: "p1", tool: "process", ok: true, output: JSON.stringify({ name: "web", pid: 42, argv: ["bun", "dev"], cwd: "/repo", started_at_ms: 100, state: "ready" }) } as AgentEvent);
    expect(status.processes.web.state).toBe("ready");
    const logs = reduce(status, { type: "tool_result", id: "p2", tool: "process", ok: true, output: JSON.stringify({ name: "web", text: "ready\n", truncated: false }) } as AgentEvent);
    expect(logs.processLogs.web.text).toBe("ready\n");
  });
});


describe("advisor status", () => {
  test("updates one durable reviewer record across its lifecycle", () => {
    let s = reduce(initialState, {
      type: "advisor_status",
      scope: "main",
      advisor: "Correctness",
      model: "reviewer",
      state: "reviewing",
    });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0]).toMatchObject({ role: "advisor", state: "reviewing" });
    expect(s.toasts).toEqual([]);

    s = reduce(s, {
      type: "advisor_status",
      scope: "main",
      advisor: "Correctness",
      model: "reviewer",
      state: "clear",
      elapsed_ms: 2500,
    });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0]).toMatchObject({ role: "advisor", state: "clear", elapsedMs: 2500 });

    s = reduce(s, {
      type: "advisor_note",
      scope: "main",
      advisor: "Correctness",
      model: "reviewer",
      severity: "concern",
      finding: "Missing focused regression coverage",
    });
    expect(s.messages).toHaveLength(1);
    expect(s.messages[0]).toMatchObject({
      role: "advisor",
      state: "finding",
      severity: "concern",
      text: "Missing focused regression coverage",
    });

    s = reduce(s, {
      type: "advisor_status",
      scope: "main",
      advisor: "Correctness",
      model: "reviewer",
      state: "reviewing",
    });
    s = reduce(s, {
      type: "advisor_status",
      scope: "main",
      advisor: "Correctness",
      model: "reviewer",
      state: "clear",
    });
    expect(s.messages).toHaveLength(2);
    expect(s.messages[0]).toMatchObject({ role: "advisor", state: "finding" });
    expect(s.messages[1]).toMatchObject({ role: "advisor", state: "clear" });
  });
});

describe("CORE_EVENT_TYPES coverage", () => {
  test("checked-in event fixtures match the web SDK catalog", async () => {
    const { CORE_EVENT_TYPES } = await import("@catalyst-code/coding-agent");
    const fixtures = (await Bun.file(
      new URL("../../../protocol/fixtures/events-v2.jsonl", import.meta.url),
    ).text())
      .trim()
      .split(/\r?\n/)
      .map((line) => JSON.parse(line));
    expect(fixtures.map((event) => event.type)).toEqual([...CORE_EVENT_TYPES]);
    expect(fixtures.every((event) => event.protocol_version === 2)).toBe(true);
  });

  test("reducer has an explicit case for every SDK core event type", async () => {
    const { CORE_EVENT_TYPES } = await import("@catalyst-code/coding-agent");
    const src = await Bun.file(new URL("./reducer.ts", import.meta.url)).text();
    const cases = new Set(
      [...src.matchAll(/case\s+"([a-z0-9_]+)":/g)].map((m) => m[1]),
    );
    // Every event the SDK catalog knows about must be handled.
    const missing = CORE_EVENT_TYPES.filter((t) => !cases.has(t));
    expect(missing).toEqual([]);
  });

  test("every reducer-handled core event is in the SDK catalog (or documented as a gap)", async () => {
    const { CORE_EVENT_TYPES } = await import("@catalyst-code/coding-agent");
    const sdkSet: Set<string> = new Set(CORE_EVENT_TYPES);
    const src = await Bun.file(new URL("./reducer.ts", import.meta.url)).text();
    const allCases = [...src.matchAll(/case\s+"([a-z0-9_]+)":/g)].map((m) => m[1]);
    // Synthetic events (underscore-prefixed) are not core wire events.
    const coreEvents = allCases.filter((c) => !c.startsWith("_"));
    // Bridge-generated events (not core wire events); correctly absent from CORE_EVENT_TYPES.
    const bridgeEvents = new Set(["projects", "workspace_changed", "session_status"]);
    // Advisor state values are switch cases inside the advisor_status handler,
    // not additional wire event types.
    const advisorStates = new Set([
      "reviewing", "clear", "no_key", "failed", "invalid_response",
    ]);
    // Core wire events emitted by the new Microsandbox subsystem. The SDK
    // catalog (@catalyst-code/coding-agent CORE_EVENT_TYPES) lags the core
    // until the SDK is updated; documented here as a known gap so this coverage
    // gate stays green without weakening the exhaustive-switch invariant.
    const sandboxEvents = new Set([
      "sandbox_status", "sandbox_prepare_progress", "sandbox_ready", "sandbox_error",
    ]);
    const extra = coreEvents.filter(
      (c) =>
        !sdkSet.has(c) &&
        !bridgeEvents.has(c) &&
        !sandboxEvents.has(c) &&
        !advisorStates.has(c),
    );
    expect(extra).toEqual([]);
  });
});
