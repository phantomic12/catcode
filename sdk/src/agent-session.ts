// AgentSession — the PI-compatible session wrapper. Spawns a catalyst-code core
// process and translates its JSONL events into `AgentSessionEvent`s, mapping the
// harness protocol (`send`/`delta`/`tool_call`/`tool_result`/`done`/`aborted`/
// `compacted`) onto PI's `agent_start`/`turn_start`/`message_start`/
// `message_update`/`message_end`/`tool_execution_*`/`turn_end`/`agent_end`
// sequence. The agent loop itself runs in the Rust core; this class only adapts
// the surface so consumers written against `@earendil-works/pi-coding-agent`
// work unchanged.

import { writeFileSync } from "node:fs";
import { join } from "node:path";

import { CoreProcess, type CoreEvent, type CoreCommand, type ReadyPayload } from "./core-process.js";
import { ExtensionRunner, type ExtensionBindings, type ExtensionUIContext } from "./extension-runner.js";
import type { ModelRegistry } from "./model-registry.js";
import type { SessionManager } from "./session-manager.js";
import type { SettingsManager } from "./settings-manager.js";
import type { ResourceLoader, Skill } from "./resource-loader.js";
import type { AuthStorage } from "./auth-storage.js";
import { getSupportedThinkingLevels } from "./ai.js";
import {
  type AgentMessage,
  type AgentState,
  type AssistantMessage,
  type CompactionResult,
  type ContextUsage,
  type ImageContent,
  type Model,
  type SessionStats,
  type TextContent,
  type ThinkingContent,
  type ThinkingLevel,
  type ToolCall,
  type ToolResultMessage,
  type Usage,
} from "./types.js";
import {
  type AgentSessionEvent,
  type AgentSessionEventListener,
  type CoreEventListener,
  type ModelCycleResult,
  type PromptOptions,
} from "./events.js";

export interface AgentSessionServices {
  cwd: string;
  agentDir: string;
  authStorage: AuthStorage;
  settingsManager: SettingsManager;
  modelRegistry: ModelRegistry;
  resourceLoader: ResourceLoader;
  diagnostics: AgentSessionRuntimeDiagnostic[];
}

export interface AgentSessionRuntimeDiagnostic {
  type: "info" | "warning" | "error";
  message: string;
}

export interface AgentSessionConfig {
  services: AgentSessionServices;
  sessionManager: SessionManager;
  model?: Model<any>;
  thinkingLevel?: ThinkingLevel;
  tools?: string[];
  customTools?: any[];
  sessionStartEvent?: any;
}

export interface BashResult {
  output: string;
  exitCode: number | undefined;
  cancelled: boolean;
  truncated: boolean;
  fullOutputPath?: string;
}

export interface ReplacedSessionContext {
  sessionPath: string;
}

const EMPTY_USAGE: Usage = {
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
  totalTokens: 0,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

/** Map a harness `ModelInfo` (core `ready`/`models` event) to a PI `Model`. */
function toModel(info: any, provider: string): Model<any> {
  return {
    id: info.id,
    name: info.name ?? info.id,
    provider,
    api: "openai",
    contextWindow: info.context_window ?? 0,
    maxTokens: info.max_tokens ?? 0,
    reasoning: !!info.reasoning,
    vision: !!info.vision,
    thinkingLevels: (info.thinking_levels ?? []) as ThinkingLevel[],
    input: {
      text: (info.input ?? ["text"]).includes("text"),
      image: (info.input ?? []).includes("image") || !!info.vision,
    },
  };
}

function nowMs(): number {
  return Date.now();
}

function newId(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

function extractText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((b: any) => (b && typeof b === "object" && b.type === "text" ? b.text : ""))
      .join("");
  }
  return "";
}

/** Map a PI ThinkingLevel to the harness `reasoning_effort` (low/medium/high). */
function toReasoningEffort(level: ThinkingLevel | undefined): string | undefined {
  switch (level) {
    case "off":
    case "minimal":
      return "low";
    case "low":
      return "low";
    case "medium":
      return "medium";
    case "high":
    case "xhigh":
      return "high";
    default:
      return undefined;
  }
}

export class AgentSession {
  readonly services: AgentSessionServices;
  readonly sessionManager: SessionManager;
  readonly extensionRunner: ExtensionRunner;

  private core: CoreProcess;
  private listeners = new Set<AgentSessionEventListener>();
  private unsubCore: (() => void) | null = null;
  private authUnsub: (() => void) | null = null;
  private uiContext?: ExtensionUIContext;
  private onError?: (err: any) => void;
  private disposed = false;

  // ── session state ──
  private _messages: AgentMessage[] = [];
  private _isStreaming = false;
  private _isCompacting = false;
  private _isRetrying = false;
  private _model: Model<any> | undefined;
  private _thinkingLevel: ThinkingLevel;
  private _sessionFile: string | undefined;
  private _sessionId: string;
  private _sessionName: string | undefined;
  private _steering: string[] = [];
  private _followUp: string[] = [];
  private _pendingMessageCount = 0;
  private _errorMessage: string | undefined;
  private _systemPrompt = "";
  private _provider = "default";
  private _autoCompactionEnabled = true;
  private _autoRetryEnabled = true;

  // ── streaming/turn tracking ──
  private agentStarted = false;
  private turnActive = false;
  private currentAssistant: AssistantMessage | null = null;
  private lastAssistant: AssistantMessage | null = null;
  private currentTurnToolResults: ToolResultMessage[] = [];
  private pendingToolCalls = new Set<string>();
  private retryPending = false;
  private manualCompaction = false;

  // ── pending promise resolvers ──
  private turnResolver: { resolve: () => void; reject: (e: Error) => void } | null = null;
  private preflightCb: ((ok: boolean) => void) | null = null;
  private steerResolver: (() => void) | null = null;
  private statsResolver: ((s: SessionStats) => void) | null = null;
  private compactResolver: (() => void) | null = null;

  constructor(config: AgentSessionConfig) {
    this.services = config.services;
    this.sessionManager = config.sessionManager;
    this.extensionRunner = new ExtensionRunner();
    this._model = config.model;
    this._thinkingLevel = config.thinkingLevel ?? "medium";
    this._sessionFile = config.sessionManager.getSessionFile() ?? undefined;
    this._sessionId = config.sessionManager.getSessionId();

    this.core = new CoreProcess({
      cwd: config.services.cwd,
      sessionFile: this._sessionFile,
      approval: "destructive",
    });
  }

  /** Spawn the core, await `ready`, populate the registry, and subscribe. */
  async init(initialModel?: Model<any>, provider?: string, apiKey?: string): Promise<void> {
    const ready = await this.core.start();
    this.onReady(ready, provider, apiKey);
    this.unsubCore = this.core.on((ev) => this.handleCoreEvent(ev));

    // Forward runtime key updates from AuthStorage to the core.
    this.authUnsub = this.services.authStorage._addSink((p, key) => {
      this.core.send({ type: "set_key", api_key: key, provider: p });
    });

    if (initialModel) {
      await this.setModel(initialModel);
    } else if (this._model) {
      await this.setModel(this._model);
    }
  }

  private onReady(ready: ReadyPayload, provider?: string, apiKey?: string): void {
    const providerName = provider ?? ready.provider ?? "default";
    this._provider = providerName;
    const models = (ready.models ?? []).map((m: any) => toModel(m, providerName));
    this.services.modelRegistry._setModels(models);

    if (ready.workspace) this.services.cwd = ready.workspace;
    if (ready.resumed_messages > 0) {
      // History will be replayed via the `history` event right after ready.
    }

    if (apiKey) {
      this.core.send({ type: "set_key", api_key: apiKey, provider: providerName });
    } else {
      // Push any runtime keys already on the shared AuthStorage.
      for (const p of this.services.authStorage.list()) {
        const cred = this.services.authStorage.get(p);
        if (cred && cred.type === "api_key") {
          this.core.send({ type: "set_key", api_key: cred.key, provider: p });
        }
      }
    }

    if (provider && provider !== ready.provider) {
      this.core.send({ type: "set_provider", name: provider });
    }
  }

  // ── Event subscription ──
  subscribe(listener: AgentSessionEventListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /**
   * Subscribe to raw core JSONL events (every `Event::new` kind from the
   * harness). Prefer this when you need protocol fidelity; use `subscribe`
   * for the PI-compatible stream (which also re-emits each raw event as
   * `{ type: "core_event", event }`).
   */
  subscribeCore(listener: CoreEventListener): () => void {
    return this.core.on(listener);
  }

  /** Low-level access to the spawned core process (commands + raw events). */
  get coreProcess(): CoreProcess {
    return this.core;
  }

  private emit(event: AgentSessionEvent): void {
    for (const fn of this.listeners) {
      try {
        fn(event);
      } catch {
        /* listener errors are non-fatal */
      }
    }
  }

  // ── Prompting ──
  async prompt(text: string, options: PromptOptions = {}): Promise<void> {
    this.assertNotDisposed();
    const images = options.images?.map((img) => img.data.startsWith("data:") ? img.data : `data:${img.mimeType};base64,${img.data}`);
    const streamingBehavior = options.streamingBehavior;

    // Preflight: optimistically accept (the core queues synchronously).
    this.preflightCb = options.preflightResult ?? null;

    const send = (): void => {
      const cmd: CoreCommand = { type: "send", prompt: text };
      if (this._model) cmd.model = this._model.id;
      const effort = toReasoningEffort(this._thinkingLevel);
      if (effort) cmd.reasoning_effort = effort;
      if (images && images.length) cmd.images = images;
      this.core.send(cmd);
    };

    if (streamingBehavior === "steer") {
      return this.steer(text, options.images);
    }

    if (this._isStreaming) {
      // The core buffers one follow-up; track it.
      this._pendingMessageCount++;
      this.emit({ type: "queue_update", steering: [...this._steering], followUp: [...this._followUp] });
    }

    // Begin a turn: set up the resolver, emit agent_start lazily on first delta.
    this.agentStarted = false;
    this.turnActive = false;
    this.currentAssistant = null;
    this.lastAssistant = null;
    this.currentTurnToolResults = [];
    this.pendingToolCalls.clear();
    this._errorMessage = undefined;
    this._isStreaming = true;

    // Push the user message immediately so `messages` reflects it.
    this._messages.push({
      role: "user",
      content: text,
      timestamp: nowMs(),
    });

    await new Promise<void>((resolve, reject) => {
      this.turnResolver = { resolve, reject };
      try {
        send();
        // Preflight success: the core accepted the command.
        this.preflightCb?.(true);
        this.preflightCb = null;
      } catch (err: any) {
        this.preflightCb?.(false);
        this.preflightCb = null;
        reject(err);
      }
    });
  }

  async steer(text: string, images?: ImageContent[]): Promise<void> {
    this.assertNotDisposed();
    const imgs = images?.map((img) => (img.data.startsWith("data:") ? img.data : `data:${img.mimeType};base64,${img.data}`));
    const cmd: CoreCommand = { type: "steer", prompt: text };
    if (this._model) cmd.model = this._model.id;
    const effort = toReasoningEffort(this._thinkingLevel);
    if (effort) cmd.reasoning_effort = effort;
    if (imgs && imgs.length) cmd.images = imgs;

    // Reset turn state for the steered turn.
    this.agentStarted = false;
    this.turnActive = false;
    this.currentAssistant = null;
    this.currentTurnToolResults = [];
    this._isStreaming = true;

    if (!this.turnResolver) {
      // Nothing running — behave like send.
      this._messages.push({ role: "user", content: text, timestamp: nowMs() });
      await new Promise<void>((resolve, reject) => {
        this.turnResolver = { resolve, reject };
        this.core.send(cmd);
      });
      return;
    }

    // Interrupt the running turn; resolve on the `steer` ack.
    await new Promise<void>((resolve) => {
      this.steerResolver = resolve;
      this.core.send(cmd);
    });
  }

  async followUp(text: string, images?: ImageContent[]): Promise<void> {
    // The core buffers follow-ups one-deep; sending `send` while busy queues it.
    await this.prompt(text, { images, streamingBehavior: "followUp" });
  }

  sendUserMessage(content: string | (TextContent | ImageContent)[], options?: { deliverAs?: "steer" | "followUp" }): Promise<void> {
    const text = typeof content === "string" ? content : extractText(content);
    if (options?.deliverAs === "steer") return this.steer(text);
    return this.followUp(text);
  }

  clearQueue(): { steering: string[]; followUp: string[] } {
    // The harness core buffers one-deep and has no explicit clear; abort drains it.
    const steering = [...this._steering];
    const followUp = [...this._followUp];
    this._steering = [];
    this._followUp = [];
    this._pendingMessageCount = 0;
    this.emit({ type: "queue_update", steering: [], followUp: [] });
    return { steering, followUp };
  }

  getSteeringMessages(): readonly string[] {
    return this._steering;
  }
  getFollowUpMessages(): readonly string[] {
    return this._followUp;
  }

  async abort(): Promise<void> {
    this.assertNotDisposed();
    this.core.send({ type: "abort" });
    // The aborted/done event resolves the pending turn.
    if (this.turnResolver) {
      await new Promise<void>((resolve) => {
        const prev = this.turnResolver;
        const wrap = { resolve: () => { resolve(); }, reject: (e: Error) => { resolve(); } };
        this.turnResolver = wrap;
        void prev; // previous resolver is superseded by the abort path
      });
    }
  }

  // ── Model management ──
  async setModel(model: Model<any>): Promise<void> {
    this.assertNotDisposed();
    this._model = model;
    // Surface as an extension event (pi-web intercepts model_select).
    await this.extensionRunner.emit({ type: "model_select", model: { provider: model.provider, id: model.id, name: model.name } });
  }

  async cycleModel(direction: "forward" | "backward" = "forward"): Promise<ModelCycleResult | undefined> {
    const available = this.modelRegistry.getAvailable();
    if (available.length === 0) return undefined;
    const currentIdx = this._model ? available.findIndex((m) => m.id === this._model!.id && m.provider === this._model!.provider) : -1;
    const nextIdx =
      direction === "forward"
        ? currentIdx < 0
          ? 0
          : (currentIdx + 1) % available.length
        : currentIdx <= 0
          ? available.length - 1
          : currentIdx - 1;
    const next = available[nextIdx];
    await this.setModel(next);
    return { model: next, thinkingLevel: this._thinkingLevel, isScoped: false };
  }

  // ── Thinking ──
  setThinkingLevel(level: ThinkingLevel): void {
    this.assertNotDisposed();
    this._thinkingLevel = level;
    void this.extensionRunner.emit({ type: "thinking_level_select", level });
  }

  cycleThinkingLevel(): ThinkingLevel | undefined {
    if (!this._model) return undefined;
    const levels = getSupportedThinkingLevels(this._model);
    if (levels.length === 0) return undefined;
    const idx = levels.indexOf(this._thinkingLevel as any);
    const next = levels[(idx + 1) % levels.length];
    this.setThinkingLevel(next);
    return next;
  }

  getAvailableThinkingLevels(): ThinkingLevel[] {
    return this._model ? getSupportedThinkingLevels(this._model) : ["medium"];
  }
  supportsThinking(): boolean {
    return !!this._model?.reasoning;
  }

  // ── Queue modes (no core command; tracked locally) ──
  setSteeringMode(_mode: "all" | "one-at-a-time"): void {}
  setFollowUpMode(_mode: "all" | "one-at-a-time"): void {}
  get steeringMode(): "all" | "one-at-a-time" {
    return "all";
  }
  get followUpMode(): "all" | "one-at-a-time" {
    return "all";
  }

  // ── Compaction ──
  async compact(customInstructions?: string): Promise<CompactionResult> {
    this.assertNotDisposed();
    this.manualCompaction = true;
    this._isCompacting = true;
    this.emit({ type: "compaction_start", reason: "manual" });
    const cmd: CoreCommand = { type: "compact" };
    if (customInstructions) cmd.customInstructions = customInstructions;
    this.core.send(cmd);
    return new Promise<CompactionResult>((resolve) => {
      this.compactResolver = () => {
        resolve({
          summary: "",
          tokensBefore: 0,
          tokensAfter: 0,
        });
      };
    });
  }
  abortCompaction(): void {
    this._isCompacting = false;
  }
  abortBranchSummary(): void {}
  setAutoCompactionEnabled(enabled: boolean): void {
    this._autoCompactionEnabled = enabled;
  }

  // ── Retry ──
  setAutoRetryEnabled(enabled: boolean): void {
    this._autoRetryEnabled = enabled;
  }
  abortRetry(): void {}

  // ── Bash (user-initiated; PI `!cmd` / `!!cmd`) ──
  async executeBash(
    command: string,
    onChunk?: (chunk: string) => void,
    options?: { excludeFromContext?: boolean },
  ): Promise<BashResult> {
    this.assertNotDisposed();
    try {
      const ev = await this._awaitFirst(
        ["bash_execution", "error"],
        () => {
          this.core.send({
            type: "user_bash",
            command,
            exclude_from_context: !!options?.excludeFromContext,
          });
        },
        120_000,
      );
      if (ev.type === "error") {
        const result: BashResult = {
          output: String((ev as any).message ?? "bash failed"),
          exitCode: 1,
          cancelled: false,
          truncated: false,
        };
        this.recordBashResult(command, result, options);
        return result;
      }
      const output = String((ev as any).output ?? "");
      if (onChunk && output) onChunk(output);
      const result: BashResult = {
        output,
        exitCode: (ev as any).ok === false ? 1 : 0,
        cancelled: false,
        truncated: false,
      };
      this.recordBashResult(command, result, options);
      return result;
    } catch (err: any) {
      // Do not fall back to a second local exec — the core may still be running
      // the command (e.g. waiting on sudo). Surface the wait failure instead.
      const result: BashResult = {
        output: err?.message ?? "bash failed",
        exitCode: 1,
        cancelled: false,
        truncated: false,
      };
      this.recordBashResult(command, result, options);
      return result;
    }
  }
  recordBashResult(
    command: string,
    result: BashResult,
    options?: { excludeFromContext?: boolean },
  ): void {
    // PI BashExecutionMessage shape — kept in session.messages for consumers
    // that walk the transcript. Core already injects into its own conversation
    // when excludeFromContext is false; this mirrors that for the SDK list.
    const msg = {
      role: "bashExecution" as const,
      command,
      output: result.output,
      exitCode: result.exitCode,
      cancelled: result.cancelled,
      truncated: result.truncated,
      fullOutputPath: result.fullOutputPath,
      timestamp: nowMs(),
      excludeFromContext: options?.excludeFromContext,
    };
    this._messages.push(msg as any);
  }
  abortBash(): void {}

  // ── Session / stats / export ──
  setSessionName(name: string): void {
    this._sessionName = name;
    this.emit({ type: "session_info_changed", name });
  }

  async navigateTree(_targetId: string): Promise<{ cancelled: boolean }> {
    // The harness has no branch tree; navigation is a no-op.
    return { cancelled: true };
  }

  getUserMessagesForForking(): Array<{ entryId: string; text: string }> {
    return this._messages
      .map((m, i) => ({ m, i }))
      .filter(({ m }) => m.role === "user")
      .map(({ m, i }) => ({ entryId: `msg-${i}`, text: extractText((m as any).content) }));
  }

  async getSessionStats(): Promise<SessionStats> {
    this.assertNotDisposed();
    const ev = await this.core.request<any>({ type: "stats" }, "stats");
    const stats = ev as any;
    return {
      sessionFile: stats.session_file ?? this._sessionFile,
      sessionId: this._sessionId,
      userMessages: this._messages.filter((m) => m.role === "user").length,
      assistantMessages: this._messages.filter((m) => m.role === "assistant").length,
      toolCalls: this._messages.filter((m) => m.role === "toolResult").length,
      toolResults: this._messages.filter((m) => m.role === "toolResult").length,
      totalMessages: this._messages.length,
      tokens: {
        input: stats.tokens_in ?? 0,
        output: stats.tokens_out ?? 0,
        cacheRead: stats.cached_tokens ?? 0,
        cacheWrite: 0,
        total: stats.tokens_total ?? 0,
      },
      cost: 0,
    };
  }

  getContextUsage(): ContextUsage | undefined {
    if (!this._model) return undefined;
    return {
      inputTokens: 0,
      outputTokens: 0,
      totalTokens: 0,
      contextWindow: this._model.contextWindow,
      percentage: 0,
    };
  }

  async exportToHtml(outputPath?: string): Promise<string> {
    const path = outputPath ?? join(this.services.cwd, `${this._sessionId}.html`);
    const body = this._messages
      .map((m) => {
        const role = (m as any).role;
        const text = extractText((m as any).content).replace(/[<>&]/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;" })[c]!);
        return `<div class="msg ${role}"><div class="role">${role}</div><pre>${text}</pre></div>`;
      })
      .join("\n");
    const html = `<!doctype html><html><head><meta charset="utf-8"><title>${this._sessionId}</title>
<style>body{font-family:system-ui;margin:2em}.msg{margin:1em 0;padding:1em;border:1px solid #ddd;border-radius:6px}.role{font-weight:700;text-transform:uppercase;opacity:.7}</style>
</head><body><h1>${this._sessionId}</h1>${body}</body></html>`;
    writeFileSync(path, html, "utf8");
    return path;
  }

  exportToJsonl(outputPath?: string): string {
    const path = outputPath ?? join(this.services.cwd, `${this._sessionId}.jsonl`);
    const lines = this._messages.map((m) => JSON.stringify(m)).join("\n");
    writeFileSync(path, lines + "\n", "utf8");
    return path;
  }

  getLastAssistantText(): string | undefined {
    for (let i = this._messages.length - 1; i >= 0; i--) {
      const m = this._messages[i];
      if (m.role === "assistant") return extractText((m as AssistantMessage).content);
    }
    return undefined;
  }

  createReplacedSessionContext(): ReplacedSessionContext {
    return { sessionPath: this._sessionFile ?? "" };
  }
  hasExtensionHandlers(_eventType: string): boolean {
    return false;
  }

  // ── Extensions / reload ──
  async bindExtensions(bindings: ExtensionBindings): Promise<void> {
    this.uiContext = bindings.uiContext;
    this.onError = bindings.onError;
  }
  async reload(): Promise<void> {
    /* no-op: the core owns resource discovery */
  }

  // ── Internal session management (used by AgentSessionRuntime) ──
  /** Wait for the first of `types` after issuing `send`. */
  private _awaitFirst(types: string[], send: () => void, timeoutMs = 15000): Promise<CoreEvent> {
    return new Promise<CoreEvent>((resolve, reject) => {
      const unsub = this.core.on((ev) => {
        if (types.includes(ev.type)) {
          unsub();
          resolve(ev);
        }
      });
      const t = setTimeout(() => {
        unsub();
        reject(new Error(`Timed out waiting for ${types.join("/")}`));
      }, timeoutMs);
      try {
        send();
      } catch (e: any) {
        clearTimeout(t);
        unsub();
        reject(e);
      }
    });
  }

  /** Switch the core to an existing session file and sync `sessionFile`. */
  async _switchSession(path: string): Promise<void> {
    this.assertNotDisposed();
    this._messages = [];
    await this._awaitFirst(["history", "info", "error"], () => this.core.send({ type: "load_session", path }));
    await this._syncSessionFile();
  }

  /** Start a fresh session (optionally named) and sync `sessionFile`. */
  async _newSession(path?: string): Promise<void> {
    this.assertNotDisposed();
    this._messages = [];
    await this._awaitFirst(["info", "error"], () => this.core.send({ type: "new_session", path }));
    await this._syncSessionFile();
  }

  /** Fork: the harness has no branch tree, so this creates a fresh session. */
  async _forkSession(): Promise<void> {
    await this._newSession();
  }

  private async _syncSessionFile(): Promise<void> {
    try {
      const stats = await this.core.request<any>({ type: "stats" }, "stats", 10000);
      if (stats.session_file) {
        this._sessionFile = stats.session_file;
        this._sessionId = stats.session_file.split(/[\\/]/).pop()?.replace(/\.jsonl$/i, "") ?? this._sessionId;
      }
    } catch {
      /* best-effort */
    }
  }

  // ── Tool registry (fixed core toolset) ──
  private static readonly CORE_TOOLS = [
    "read_file", "edit", "write_file", "grep", "glob", "bash", "bulk",
    "finish", "patch", "diagnostics", "fetch", "spawn", "subagent",
  ];
  getActiveToolNames(): string[] {
    return [...AgentSession.CORE_TOOLS];
  }
  getAllTools(): any[] {
    return AgentSession.CORE_TOOLS.map((name) => ({ name }));
  }
  getToolDefinition(name: string): any {
    return AgentSession.CORE_TOOLS.includes(name) ? { name } : undefined;
  }
  setActiveToolsByName(_toolNames: string[]): void {}
  setScopedModels(_scopedModels: Array<{ model: Model<any>; thinkingLevel?: ThinkingLevel }>): void {}

  // ── Lifecycle ──
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.unsubCore?.();
    this.authUnsub?.();
    void this.core.dispose();
  }

  // ── Public getters (PI-compatible) ──
  get modelRegistry(): ModelRegistry {
    return this.services.modelRegistry;
  }
  get settingsManager(): SettingsManager {
    return this.services.settingsManager;
  }
  get resourceLoader(): ResourceLoader {
    return this.services.resourceLoader;
  }
  get agent(): any {
    return { convertToLlm: (msgs: AgentMessage[]) => msgs };
  }
  get state(): AgentState {
    return {
      systemPrompt: this._systemPrompt,
      model: this._model as Model<any>,
      thinkingLevel: this._thinkingLevel,
      tools: [],
      messages: this._messages,
      isStreaming: this._isStreaming,
      streamingMessage: this._isStreaming ? (this.currentAssistant as AgentMessage | undefined) ?? undefined : undefined,
      pendingToolCalls: this.pendingToolCalls,
      errorMessage: this._errorMessage,
    } as AgentState;
  }
  get model(): Model<any> | undefined {
    return this._model;
  }
  get thinkingLevel(): ThinkingLevel {
    return this._thinkingLevel;
  }
  get isStreaming(): boolean {
    return this._isStreaming;
  }
  get isCompacting(): boolean {
    return this._isCompacting;
  }
  get isRetrying(): boolean {
    return this._isRetrying;
  }
  get systemPrompt(): string {
    return this._systemPrompt;
  }
  get retryAttempt(): number {
    return 0;
  }
  get messages(): AgentMessage[] {
    return this._messages;
  }
  get sessionFile(): string | undefined {
    return this._sessionFile;
  }
  get sessionId(): string {
    return this._sessionId;
  }
  get sessionName(): string | undefined {
    return this._sessionName;
  }
  get scopedModels(): ReadonlyArray<{ model: Model<any>; thinkingLevel?: ThinkingLevel }> {
    return [];
  }
  get promptTemplates(): ReadonlyArray<any> {
    return [];
  }
  get autoCompactionEnabled(): boolean {
    return this._autoCompactionEnabled;
  }
  get autoRetryEnabled(): boolean {
    return this._autoRetryEnabled;
  }
  get isBashRunning(): boolean {
    return false;
  }
  get hasPendingBashMessages(): boolean {
    return false;
  }
  get pendingMessageCount(): number {
    return this._pendingMessageCount;
  }

  // ── Core event translation ──
  private handleCoreEvent(ev: CoreEvent): void {
    // Always re-emit the raw harness event so every core kind is available via
    // `subscribe()` as `{ type: "core_event", event }` (and via `subscribeCore`).
    this.emit({ type: "core_event", event: ev });

    switch (ev.type) {
      case "ready":
      case "authed":
      case "provider_changed":
        break;
      case "models": {
        const models = ((ev.models as any[]) ?? []).map((m: any) => toModel(m, this._provider));
        this.services.modelRegistry._setModels(models);
        break;
      }
      case "delta":
        this.onDelta(String(ev.text ?? ""));
        break;
      case "thinking":
        this.onThinking(String(ev.thinking ?? ev.text ?? ""));
        break;
      case "tool_call_start":
      case "tool_call_name":
      case "tool_call_args":
        // Granular streaming events — ignored for PI mapping (canonical `tool_call`).
        break;
      case "tool_call":
        this.onToolCall(ev);
        break;
      case "tool_result":
        this.onToolResult(ev);
        break;
      case "metrics":
        this.onMetrics(ev);
        break;
      case "http_retry":
        this.onHttpRetry(ev);
        break;
      case "done":
        this.onDone();
        break;
      case "aborted":
        this.onAborted();
        break;
      case "steer":
        this.onSteerAck();
        break;
      case "error":
        this.onError2(String(ev.message ?? "unknown error"));
        break;
      case "info":
        break;
      case "history":
        this.onHistory(ev);
        break;
      case "reset":
        this._messages = [];
        this.emit({ type: "queue_update", steering: [], followUp: [] });
        break;
      case "compacted":
        this.onCompacted(ev);
        break;
      case "approval_request":
        void this.onApprovalRequest(ev);
        break;
      case "ask_request":
        void this.onAskRequest(ev);
        break;
      case "sudo_request":
        void this.onSudoRequest(ev);
        break;
      case "intercom_message":
        void this.onIntercomMessage(ev);
        break;
      // Remaining core events are available via `core_event` / `subscribeCore`
      // (protocol_hello, file_change, checkpoints, worktrees, goals, plugins, …).
      default:
        break;
    }
  }

  private beginAssistantIfNeeded(): void {
    if (this.currentAssistant) return;
    // A new model request begins: close out the previous turn, if any.
    if (this.turnActive) {
      this.emit({
        type: "turn_end",
        message: (this.lastAssistant as AgentMessage) ?? this.currentAssistant!,
        toolResults: this.currentTurnToolResults,
      });
    }
    if (!this.agentStarted) {
      this.emit({ type: "agent_start" });
      this.agentStarted = true;
    }
    this.emit({ type: "turn_start" });
    this.currentAssistant = {
      role: "assistant",
      content: [],
      api: "openai",
      provider: this._provider,
      model: this._model?.id ?? "",
      usage: { ...EMPTY_USAGE },
      stopReason: "stop",
      timestamp: nowMs(),
    };
    this.currentTurnToolResults = [];
    this.turnActive = true;
    this.emit({ type: "message_start", message: this.currentAssistant });
  }

  private onDelta(text: string): void {
    this.beginAssistantIfNeeded();
    if (this.retryPending) {
      this.retryPending = false;
      this.emit({ type: "auto_retry_end", success: true, attempt: 1 });
    }
    const a = this.currentAssistant!;
    let textBlock = a.content.find((b): b is TextContent => b.type === "text");
    if (!textBlock) {
      textBlock = { type: "text", text: "" };
      a.content.push(textBlock);
    }
    textBlock.text += text;
    this.emit({
      type: "message_update",
      message: a,
      assistantMessageEvent: {
        type: "text_delta",
        contentIndex: a.content.indexOf(textBlock),
        delta: text,
        partial: a,
      },
    });
  }

  private onThinking(text: string): void {
    this.beginAssistantIfNeeded();
    const a = this.currentAssistant!;
    let block = a.content.find((b): b is ThinkingContent => b.type === "thinking");
    if (!block) {
      block = { type: "thinking", thinking: "" };
      a.content.push(block);
    }
    block.thinking += text;
    this.emit({
      type: "message_update",
      message: a,
      assistantMessageEvent: {
        type: "thinking_delta",
        contentIndex: a.content.indexOf(block),
        delta: text,
        partial: a,
      },
    });
  }

  private onToolCall(ev: CoreEvent): void {
    // The assistant message is complete; ensure it exists (text-less tool turn).
    if (!this.currentAssistant) {
      this.beginAssistantIfNeeded();
    }
    const a = this.currentAssistant!;
    const id = String(ev.id ?? newId("call"));
    const name = String(ev.name ?? "");
    let args: Record<string, any> = {};
    try {
      args = ev.args ? (typeof ev.args === "string" ? JSON.parse(ev.args) : ev.args) : {};
    } catch {
      args = { raw: String(ev.args ?? "") };
    }
    const toolCall: ToolCall = { type: "toolCall", id, name, arguments: args };
    const contentIndex = a.content.length;
    a.content.push(toolCall);

    // Emit toolcall_start/end deltas (pi-web reads toolCall from toolcall_end).
    this.emit({
      type: "message_update",
      message: a,
      assistantMessageEvent: { type: "toolcall_start", contentIndex, partial: a },
    });
    this.emit({
      type: "message_update",
      message: a,
      assistantMessageEvent: { type: "toolcall_end", contentIndex, toolCall, partial: a },
    });

    // Finalize the assistant message and begin tool execution.
    this.emit({ type: "message_end", message: a });
    this.lastAssistant = a;
    this.currentAssistant = null;
    this._messages.push(a);
    this.pendingToolCalls.add(id);
    this.emit({
      type: "tool_execution_start",
      toolCallId: id,
      toolName: name,
      args,
    });
  }

  private onToolResult(ev: CoreEvent): void {
    const id = String(ev.id ?? "");
    const ok = ev.ok !== false;
    const output = String(ev.output ?? "");
    const toolName = this.lookupToolName(id);
    const result = {
      content: [{ type: "text", text: output } as TextContent],
      details: undefined,
    };
    this.emit({
      type: "tool_execution_end",
      toolCallId: id,
      toolName,
      result,
      isError: !ok,
    });
    const tr: ToolResultMessage = {
      role: "toolResult",
      toolCallId: id,
      toolName,
      content: [{ type: "text", text: output }],
      isError: !ok,
      timestamp: nowMs(),
    };
    this._messages.push(tr);
    this.currentTurnToolResults.push(tr);
    this.pendingToolCalls.delete(id);
  }

  private lookupToolName(id: string): string {
    for (let i = this._messages.length - 1; i >= 0; i--) {
      const m = this._messages[i];
      if (m.role === "assistant") {
        const tc = (m as AssistantMessage).content.find((b) => b.type === "toolCall" && (b as ToolCall).id === id);
        if (tc) return (tc as ToolCall).name;
      }
    }
    return "";
  }

  private onMetrics(ev: CoreEvent): void {
    // Final metrics carry usage for the last assistant message.
    if (this.lastAssistant) {
      this.lastAssistant.usage = {
        input: Number(ev.prompt_tokens ?? ev.tokens_in ?? 0),
        output: Number(ev.tokens_out ?? 0),
        cacheRead: Number(ev.cached_tokens ?? 0),
        cacheWrite: 0,
        totalTokens: Number(ev.tokens_in ?? 0) + Number(ev.tokens_out ?? 0),
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
      };
    }
  }

  private onHttpRetry(ev: CoreEvent): void {
    this._isRetrying = true;
    this.retryPending = true;
    this.emit({
      type: "auto_retry_start",
      attempt: Number(ev.attempt ?? 1),
      maxAttempts: 3,
      delayMs: Number(ev.backoff_ms ?? 0),
      errorMessage: ev.reason ? String(ev.reason) : `HTTP ${ev.status ?? ""}`.trim(),
    });
  }

  private onDone(): void {
    this.finishTurn(false);
  }

  private onAborted(): void {
    this.finishTurn(true);
  }

  private finishTurn(aborted: boolean): void {
    if (this.turnActive && this.currentAssistant) {
      this.emit({ type: "message_end", message: this.currentAssistant });
      this.lastAssistant = this.currentAssistant;
      this._messages.push(this.currentAssistant);
      this.currentAssistant = null;
    }
    if (this.turnActive) {
      this.emit({
        type: "turn_end",
        message: (this.lastAssistant as AgentMessage) ?? { role: "assistant", content: [], api: "openai", provider: this._provider, model: this._model?.id ?? "", usage: { ...EMPTY_USAGE }, stopReason: aborted ? "aborted" : "stop", timestamp: nowMs() },
        toolResults: this.currentTurnToolResults,
      });
    }
    this.emit({ type: "agent_end", messages: this._messages, willRetry: false });
    if (this.retryPending) {
      this.retryPending = false;
      this.emit({ type: "auto_retry_end", success: !aborted, attempt: 1 });
    }
    this._isStreaming = false;
    this._isRetrying = false;
    this.turnActive = false;
    this.agentStarted = false;
    this.currentTurnToolResults = [];
    this.pendingToolCalls.clear();
    if (this.turnResolver) {
      const r = this.turnResolver;
      this.turnResolver = null;
      r.resolve();
    }
  }

  private onError2(message: string): void {
    this._errorMessage = message;
    if (this.turnResolver && !this.agentStarted) {
      // Error before streaming (e.g. unknown model): preflight failed.
      this.preflightCb?.(false);
      this.preflightCb = null;
      this._isStreaming = false;
      const r = this.turnResolver;
      this.turnResolver = null;
      r.reject(new Error(message));
      return;
    }
    // Mid-stream error: surface via extension onError if bound, end the turn.
    this.onError?.({ error: message });
    this.finishTurn(false);
  }

  private onSteerAck(): void {
    if (this.steerResolver) {
      const r = this.steerResolver;
      this.steerResolver = null;
      r();
    }
  }

  private onHistory(ev: CoreEvent): void {
    const msgs = (ev.messages ?? []) as any[];
    this._messages = msgs.map((m) => this.mapHistoryMessage(m));
  }

  private mapHistoryMessage(m: any): AgentMessage {
    const role = m.role;
    const ts = m.timestamp ?? nowMs();
    if (role === "user") {
      return { role: "user", content: typeof m.content === "string" ? m.content : this.mapContentParts(m.content), timestamp: ts };
    }
    if (role === "assistant") {
      const content: any[] = [];
      if (m.content) content.push({ type: "text", text: typeof m.content === "string" ? m.content : extractText(m.content) });
      for (const tc of m.tool_calls ?? []) {
        content.push({
          type: "toolCall",
          id: tc.id ?? "",
          name: tc.function?.name ?? tc.name ?? "",
          arguments: safeParseArgs(tc.function?.arguments ?? tc.arguments),
        });
      }
      return {
        role: "assistant",
        content,
        api: "openai",
        provider: this._provider,
        model: this._model?.id ?? "",
        usage: { ...EMPTY_USAGE },
        stopReason: "stop",
        timestamp: ts,
      };
    }
    if (role === "tool") {
      return {
        role: "toolResult",
        toolCallId: m.tool_call_id ?? "",
        toolName: m.name ?? "",
        content: [{ type: "text", text: typeof m.content === "string" ? m.content : extractText(m.content) }],
        isError: false,
        timestamp: ts,
      };
    }
    // Fallback: treat unknown roles as user text.
    return { role: "user", content: extractText(m.content), timestamp: ts };
  }

  private mapContentParts(content: any): (TextContent | ImageContent)[] {
    if (!Array.isArray(content)) return [{ type: "text", text: String(content ?? "") }];
    return content.map((b: any) =>
      b?.type === "image"
        ? { type: "image", data: b.data ?? "", mimeType: b.mimeType ?? "image/png" }
        : { type: "text", text: b?.text ?? "" },
    );
  }

  private onCompacted(ev: CoreEvent): void {
    const reason = this.manualCompaction ? "manual" : "threshold";
    this.manualCompaction = false;
    const result: CompactionResult = {
      summary: "",
      tokensBefore: Number(ev.before_tokens ?? 0),
      tokensAfter: Number(ev.after_tokens ?? 0),
    };
    this.emit({ type: "compaction_start", reason });
    this.emit({
      type: "compaction_end",
      reason,
      result,
      aborted: false,
      willRetry: false,
    });
    this._isCompacting = false;
    if (this.compactResolver) {
      const r = this.compactResolver;
      this.compactResolver = null;
      r();
    }
  }

  private async onApprovalRequest(ev: CoreEvent): Promise<void> {
    const requestId = String(ev.request_id ?? "");
    const tool = String(ev.tool ?? "");
    const args = String(ev.args ?? "");
    const diff = ev.diff ? `\n\n${String(ev.diff)}` : "";
    let decision: "yes" | "no" | "always" = "yes";
    if (this.uiContext) {
      try {
        const confirmed = await this.uiContext.confirm(`Approve ${tool}?`, `${args}${diff}`);
        decision = confirmed ? "yes" : "no";
      } catch {
        decision = "yes"; // default-allow on UI failure to avoid deadlock.
      }
    }
    this.core.send({ type: "approve", request_id: requestId, decision });
  }

  private async onAskRequest(ev: CoreEvent): Promise<void> {
    const requestId = String(ev.request_id ?? "");
    const questions = ev.questions;
    let answers: Record<string, unknown> | null = {};
    if (this.uiContext) {
      try {
        const summary = Array.isArray(questions)
          ? questions
              .map((q: any) => String(q?.prompt ?? q?.id ?? ""))
              .filter(Boolean)
              .join("\n")
          : String(questions ?? "");
        const reply = await this.uiContext.input("Agent question", summary || "Answer the agent");
        if (reply === undefined) {
          answers = null; // skip
        } else if (Array.isArray(questions) && questions.length === 1 && questions[0]?.id) {
          answers = { [String(questions[0].id)]: reply };
        } else {
          answers = { answer: reply };
        }
      } catch {
        answers = null;
      }
    }
    this.core.send({
      type: "ask_reply",
      request_id: requestId,
      answers: answers === null ? null : answers,
    });
  }

  private async onSudoRequest(ev: CoreEvent): Promise<void> {
    const requestId = String(ev.request_id ?? "");
    const command = String(ev.command ?? "");
    let approved = false;
    let password: string | undefined;
    if (this.uiContext) {
      try {
        approved = await this.uiContext.confirm(
          "Approve sudo?",
          command || "Agent requested elevated privileges",
        );
        if (approved) {
          const pw = await this.uiContext.input("sudo password", "Password (not stored)");
          if (pw === undefined) {
            approved = false;
          } else {
            password = pw;
          }
        }
      } catch {
        approved = false;
      }
    }
    this.core.send({
      type: "sudo_reply",
      request_id: requestId,
      approved,
      ...(password !== undefined ? { password } : {}),
    });
  }

  private async onIntercomMessage(ev: CoreEvent): Promise<void> {
    const requestId = String(ev.id ?? "");
    const reason = String(ev.reason ?? "");
    if (reason !== "need_decision") return; // progress_update is non-blocking.
    let reply = "[no reply — proceed with your best judgment]";
    if (this.uiContext) {
      try {
        const input = await this.uiContext.input(
          String(ev.from ?? "subagent"),
          String(ev.message ?? ""),
        );
        if (input) reply = input;
      } catch {
        /* keep default reply */
      }
    }
    this.core.send({ type: "intercom_reply", request_id: requestId, reply });
  }

  private assertNotDisposed(): void {
    if (this.disposed) throw new Error("AgentSession has been disposed");
  }
}

function safeParseArgs(raw: any): Record<string, any> {
  if (typeof raw !== "string") return raw ?? {};
  try {
    return JSON.parse(raw);
  } catch {
    return { raw };
  }
}

export { type Skill };
