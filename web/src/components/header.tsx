"use client";

// Header — the top control bar. Model selector, thinking-level selector,
// approval-mode toggle, live metrics, and a connection indicator. Collapses
// the secondary controls on small screens.

import { useState } from "react";
import { buildSharedSessionUrl } from "@/lib/session-share";
import type { Metrics, UmansConc, ModelInfo, CostUpdate, NotificationItem } from "@/lib/types";
import { formatTokens, formatTps, formatMs, basename } from "@/lib/format";
import { useOutsideClose } from "@/lib/use-outside-close";
import { ModelPicker } from "./model-picker";
import { NotificationCenter } from "./notification-center";
import {
  ChevronDown,
  ModelIcon,
  BrainIcon,
  ShieldIcon,
  FolderIcon,
  MenuIcon,
  RefreshIcon,
  LayoutIdeIcon,
  BoltIcon,
  CopyIcon,
} from "./icons";

interface Props {
  /** Use container-friendly chrome when hosted in an IDE dock. */
  compact?: boolean;
  connected: boolean;
  workspace: string;
  provider: string;
  models: ModelInfo[];
  selectedModel: string | null;
  /** True while an on-demand model-list refresh is in flight. */
  modelsRefreshing?: boolean;
  thinkingLevel: string;
  approvalMode: string;
  metrics: Metrics | null;
  /** Session cost estimate from core `cost_update`. */
  cost?: CostUpdate | null;
  /** Live Umans concurrency (used/limit); shown ahead of tps when present. */
  umansConc?: UmansConc | null;
  streaming: boolean;
  retrying: boolean;
  sessionFile: string | null;
  sessionTitle?: string;
  switching?: boolean;
  theme?: string;
  onMenuClick?: () => void;
  onSelectModel: (id: string) => void;
  /** Force-refresh multi-provider model cache. */
  onRefreshModels?: () => void;
  onSelectThinking: (level: string) => void;
  onSetApproval: (mode: "never" | "destructive" | "always") => void;
  onReconnect?: () => void;
  onToggleTheme?: () => void;
  /** When chat-only: open full IDE chrome without remounting the agent. */
  onOpenIde?: () => void;
  /** Chat-only: open Settings (activity bar is hidden in this mode). */
  onOpenSettings?: () => void;
  /** Chat-only: open project switcher (activity bar is hidden in this mode). */
  onOpenProjects?: () => void;
  /** Open Control Center mission panel. */
  onOpenControl?: () => void;
  // ── Cross-session notifications ──
  notifications: NotificationItem[];
  onOpenNotification: (n: NotificationItem) => void;
  onDismissNotification: (id: string) => void;
  onMarkAllNotificationsRead: () => void;
  onClearNotifications: () => void;
}

const ALL_LEVELS = ["off", "low", "medium", "high", "xhigh", "max"];

export function Header(props: Props) {
  const [modelOpen, setModelOpen] = useState(false);
  const [shareCopied, setShareCopied] = useState(false);
  const [thinkOpen, setThinkOpen] = useState(false);
  const [approvOpen, setApprovOpen] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const modelRef = useOutsideClose(() => setModelOpen(false), modelOpen);
  const thinkRef = useOutsideClose(() => setThinkOpen(false), thinkOpen);
  const approvRef = useOutsideClose(() => setApprovOpen(false), approvOpen);
  const configRef = useOutsideClose(() => setConfigOpen(false), configOpen);

  const current = props.models.find((m) => m.id === props.selectedModel) ?? props.models[0];
  // Prefer the model's advertised thinking_levels (includes xhigh/max when supported).
  const levels =
    current?.thinking_levels && current.thinking_levels.length > 0
      ? current.thinking_levels
      : ALL_LEVELS;
  const effLevels = current?.reasoning ? levels : ["off"];

  const m = props.metrics;
  const cost = props.cost;
  const conc = props.umansConc;
  const approvalModes: Array<"never" | "destructive" | "always"> = ["never", "destructive", "always"];

  return (
    <header
      className={`relative z-20 flex min-w-0 items-center gap-1.5 border-b border-ink-800 bg-ink-900 px-2 ${props.compact ? "flex-wrap py-1.5" : "h-10 sm:px-3"}`}
    >
      {/* Mobile sidebar toggle */}
      <button
        onClick={props.onMenuClick}
        className={`focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11 ${props.compact ? "" : "lg:hidden"}`}
        aria-label="Open sessions"
      >
        <MenuIcon />
      </button>

      {/* Brand + workspace / active conversation */}
      <div className={`min-w-0 items-center gap-2.5 ${props.compact ? "flex flex-1" : "flex"}`}>
        {props.compact ? (
          <div className="min-w-0 border-l-2 border-accent pl-2">
            <div className="flex items-center gap-2">
              {props.streaming && (
                <span
                  className="h-1.5 w-1.5 shrink-0 animate-pulse rounded-none bg-accent"
                  title="Streaming"
                  aria-hidden="true"
                />
              )}
              <div className="truncate text-[12.5px] font-semibold leading-tight tracking-tight text-ink-100">
                {props.sessionTitle || "New chat"}
              </div>
            </div>
            <div className="mt-0.5 flex items-center gap-1 font-mono text-[10px] leading-tight text-ink-500">
              <FolderIcon width={9} height={9} className="shrink-0" />
              <span className="truncate">{basename(props.workspace) || props.workspace}</span>
            </div>
          </div>
        ) : (
          <>
            <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-sm bg-accent text-[12px] font-bold text-white">
              c
            </span>
            <div className="hidden min-w-0 sm:block">
              <div className="flex items-center gap-1.5 text-[12.5px] font-semibold leading-tight tracking-tight text-ink-100">
                Catalyst Code
                {props.streaming && (
                  <span className="font-mono text-[9px] font-medium uppercase tracking-[0.14em] text-accent-soft">
                    live
                  </span>
                )}
              </div>
              <div className="flex items-center gap-1 truncate font-mono text-[10px] text-ink-500">
                <FolderIcon width={10} height={10} />
                <span className="truncate">{basename(props.workspace) || props.workspace}</span>
              </div>
            </div>
          </>
        )}
      </div>

      {/* Model selector */}
      <div className={`relative ${props.compact ? "" : "ml-auto"}`} ref={modelRef}>
        <button
          onClick={() => setModelOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={modelOpen}
          className="focus-ring flex h-8 items-center gap-1.5 rounded-sm px-2 text-[11px] font-mono text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11"
        >
          <ModelIcon width={12} height={12} className="text-accent-soft" />
          <span className={`${props.compact ? "max-w-[82px]" : "max-w-[120px]"} truncate`}>{current?.name || current?.id || "no model"}</span>
          <ChevronDown width={11} height={11} className="text-ink-500" />
        </button>
        {modelOpen && (
          <div role="menu" className={`absolute right-0 z-30 mt-1 max-h-[min(70vh,28rem)] overflow-hidden rounded-sm border border-ink-700 bg-ink-900 py-1 shadow-elev-2 animate-fade-in ${props.compact ? "w-[min(18rem,calc(100vw-1rem))]" : "w-[min(20rem,calc(100vw-1rem))] sm:w-80"}`}>
            <ModelPicker
              models={props.models}
              selectedModel={props.selectedModel}
              onSelect={props.onSelectModel}
              variant="popover"
              onClose={() => setModelOpen(false)}
              onRefresh={props.onRefreshModels}
              refreshing={props.modelsRefreshing}
            />
          </div>
        )}
      </div>

      {/* Thinking selector — icon-only on xs, full on sm+ */}
      <div className={props.compact ? "hidden" : "relative"} ref={thinkRef}>
        <button
          onClick={() => setThinkOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={thinkOpen}
          className="focus-ring flex h-8 items-center gap-1.5 rounded-sm px-2 text-[11px] font-mono text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11"
          title={`Thinking: ${props.thinkingLevel}`}
        >
          <BrainIcon width={12} height={12} className="text-accent-soft" />
          <span className={props.compact ? "hidden" : "hidden capitalize sm:inline"}>{props.thinkingLevel}</span>
          <ChevronDown width={11} height={11} className="text-ink-500" />
        </button>
        {thinkOpen && (
          <div role="menu" className="absolute right-0 z-30 mt-1 w-40 overflow-hidden rounded-sm border border-ink-700 bg-ink-900 py-1 shadow-elev-2 animate-fade-in">
            {effLevels.map((lv) => (
              <button
                key={lv}
                role="menuitem"
                onClick={() => {
                  props.onSelectThinking(lv);
                  setThinkOpen(false);
                }}
                className={`flex w-full items-center border-l-2 px-2 py-1 text-left text-[11px] font-mono capitalize transition-colors hover:bg-ink-800 ${
                  props.thinkingLevel === lv ? "border-accent text-ink-100" : "border-transparent text-ink-300"
                }`}
              >
                {lv}
              </button>
            ))}
          </div>
        )}
      </div>

      {/* Approval mode */}
      <div className={props.compact ? "hidden" : "relative"} ref={approvRef}>
        <button
          onClick={() => setApprovOpen((o) => !o)}
          aria-haspopup="menu"
          aria-expanded={approvOpen}
          className={`focus-ring flex h-8 items-center gap-1.5 rounded-sm px-2 text-[11px] font-mono transition-colors hover:bg-ink-850 [@media(pointer:coarse)]:h-11 ${
            props.approvalMode === "always"
              ? "text-success"
              : props.approvalMode === "never"
                ? "text-ink-300 hover:text-ink-100"
                : "text-warning"
          }`}
          title="Approval mode"
        >
          <ShieldIcon width={12} height={12} />
          <span className={props.compact ? "hidden" : "hidden capitalize md:inline"}>{props.approvalMode}</span>
          <ChevronDown width={11} height={11} className="text-ink-500" />
        </button>
        {approvOpen && (
          <div role="menu" className="absolute right-0 z-30 mt-1 w-40 overflow-hidden rounded-sm border border-ink-700 bg-ink-900 py-1 shadow-elev-2 animate-fade-in">
            {approvalModes.map((mode) => (
              <button
                key={mode}
                role="menuitem"
                onClick={() => {
                  props.onSetApproval(mode);
                  setApprovOpen(false);
                }}
                className={`flex w-full items-center border-l-2 px-2 py-1 text-left text-[11px] font-mono capitalize transition-colors hover:bg-ink-800 ${
                  props.approvalMode === mode ? "border-accent text-ink-100" : "border-transparent text-ink-300"
                }`}
              >
                {mode}
              </button>
            ))}
          </div>
        )}
      </div>

      {props.compact && (
        <div className="relative" ref={configRef}>
          <button
            type="button"
            onClick={() => setConfigOpen((open) => !open)}
            aria-haspopup="menu"
            aria-expanded={configOpen}
            aria-label="Chat configuration"
            title="Chat configuration"
            className="focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
          >
            <svg width={14} height={14} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round">
              <path d="M4 7h10M18 7h2M4 17h2M10 17h10" />
              <circle cx="16" cy="7" r="2" /><circle cx="8" cy="17" r="2" />
            </svg>
          </button>
          {configOpen && (
            <div role="menu" className="absolute right-0 z-30 mt-1 w-56 rounded-sm border border-ink-700 bg-ink-900 py-1 shadow-elev-2 animate-fade-in">
              <div className="px-2 pb-1 pt-1 text-[10px] font-mono uppercase tracking-wider text-ink-500">Thinking</div>
              <div className="mb-1 grid grid-cols-2 gap-px px-1">
                {effLevels.map((level) => (
                  <button key={level} role="menuitemradio" aria-checked={props.thinkingLevel === level} onClick={() => props.onSelectThinking(level)} className={`rounded-sm border-l-2 px-2 py-1 text-left text-[11px] font-mono capitalize transition-colors ${props.thinkingLevel === level ? "border-accent bg-ink-800 text-ink-100" : "border-transparent text-ink-300 hover:bg-ink-800"}`}>
                    {level}
                  </button>
                ))}
              </div>
              <div className="px-2 pb-1 text-[10px] font-mono uppercase tracking-wider text-ink-500">Approvals</div>
              <div className="grid grid-cols-3 gap-px px-1">
                {approvalModes.map((mode) => (
                  <button key={mode} role="menuitemradio" aria-checked={props.approvalMode === mode} onClick={() => props.onSetApproval(mode)} className={`rounded-sm border-l-2 px-1.5 py-1 text-[10px] font-mono capitalize transition-colors ${props.approvalMode === mode ? "border-accent bg-ink-800 text-ink-100" : "border-transparent text-ink-300 hover:bg-ink-800"}`}>
                    {mode}
                  </button>
                ))}
              </div>
              {props.onOpenIde && (
                <button
                  role="menuitem"
                  onClick={() => {
                    props.onOpenIde?.();
                    setConfigOpen(false);
                  }}
                  className="mt-1 flex w-full items-center gap-2 border-t border-ink-800 px-2 py-1.5 text-[11px] text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100"
                >
                  <LayoutIdeIcon width={13} height={13} className="text-accent-soft" />
                  Open IDE
                </button>
              )}
              {props.onOpenProjects && (
                <button
                  role="menuitem"
                  onClick={() => {
                    props.onOpenProjects?.();
                    setConfigOpen(false);
                  }}
                  className="mt-1 flex w-full items-center gap-2 border-t border-ink-800 px-2 py-1.5 text-[11px] text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100"
                >
                  <FolderIcon width={13} height={13} className="text-accent-soft" />
                  Switch project
                </button>
              )}
              {props.onOpenControl && (
                <button
                  type="button"
                  onClick={props.onOpenControl}
                  className="focus-ring mt-1 flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
                  title="Control Center"
                  aria-label="Control Center"
                >
                  <BoltIcon width={14} height={14} />
                </button>
              )}
              {props.onOpenSettings && (
                <button
                  role="menuitem"
                  onClick={() => {
                    props.onOpenSettings?.();
                    setConfigOpen(false);
                  }}
                  className="mt-1 flex w-full items-center gap-2 border-t border-ink-800 px-2 py-1.5 text-[11px] text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100"
                >
                  <BoltIcon width={13} height={13} className="text-accent-soft" />
                  Settings
                </button>
              )}
              {props.onToggleTheme && (
                <button role="menuitem" onClick={props.onToggleTheme} className="mt-1 flex w-full items-center justify-between border-t border-ink-800 px-2 py-1.5 text-[11px] text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100">
                  <span>Appearance</span><span className="font-mono text-[10px] uppercase text-ink-500">{props.theme ?? "dark"}</span>
                </button>
              )}
            </div>
          )}
        </div>
      )}

      {/* Metrics + connection */}
      <div className="flex items-center gap-1 pl-1">
        {props.sessionFile && props.workspace && (
          <button
            type="button"
            onClick={() => {
              const url = buildSharedSessionUrl(
                window.location.origin,
                props.sessionFile!,
                props.workspace,
              );
              void navigator.clipboard?.writeText(url).then(() => {
                setShareCopied(true);
                window.setTimeout(() => setShareCopied(false), 1400);
              });
            }}
            className="focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
            title={shareCopied ? "Session link copied" : "Copy authenticated session link"}
            aria-label={shareCopied ? "Session link copied" : "Copy session link"}
          >
            <CopyIcon width={13} height={13} className={shareCopied ? "text-success" : undefined} />
          </button>
        )}
        <NotificationCenter
          notifications={props.notifications}
          currentWorkspace={props.workspace}
          onOpen={props.onOpenNotification}
          onDismiss={props.onDismissNotification}
          onMarkAllRead={props.onMarkAllNotificationsRead}
          onClear={props.onClearNotifications}
        />
        {(m || cost) && (
          <div className={`${props.compact ? "hidden" : "hidden lg:flex"} items-center gap-2.5 px-1 font-mono text-[10px] text-ink-500`}>
            {m?.prompt_tokens != null && <span title="input tokens">↑{formatTokens(m.prompt_tokens)}</span>}
            {m?.tokens_out != null && <span title="output tokens">↓{formatTokens(m.tokens_out)}</span>}
            {conc && conc.used != null && current?.provider === conc.provider && (
              <span title="live account-wide concurrency in use / plan limit" className="text-accent-soft">
                {conc.limit == null
                  ? `Conc ${conc.used}/∞`
                  : `Conc ${conc.used}/${conc.limit}`}
              </span>
            )}
            {m?.tps != null && <span title="tokens/sec" className="text-accent-soft">{formatTps(m.tps)}</span>}
            {m?.ttft_ms != null && <span title="time to first token">ttft {formatMs(m.ttft_ms)}</span>}
            {cost?.estimated_usd != null && (
              <span title="estimated session cost" className="text-ink-400">
                ${cost.estimated_usd.toFixed(cost.estimated_usd < 0.01 ? 4 : 3)}
              </span>
            )}
          </div>
        )}
        {props.onOpenIde && (
          <button
            type="button"
            onClick={props.onOpenIde}
            className="focus-ring flex h-8 items-center gap-1.5 rounded-sm px-2 text-[11px] font-mono text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11"
            title="Open IDE layout"
            aria-label="Open IDE layout"
          >
            <LayoutIdeIcon width={13} height={13} className="text-accent-soft" />
            <span className="hidden sm:inline">Open IDE</span>
          </button>
        )}
        {props.onOpenProjects && (
          <button
            type="button"
            onClick={props.onOpenProjects}
            className="focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
            title="Switch project"
            aria-label="Switch project"
          >
            <FolderIcon width={13} height={13} />
          </button>
        )}
        {props.onOpenSettings && (
          <button
            type="button"
            onClick={props.onOpenSettings}
            className="focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
            title="Settings"
            aria-label="Settings"
          >
            <BoltIcon width={13} height={13} />
          </button>
        )}
        {/* Theme toggle */}
        {props.onToggleTheme && !props.compact && (
          <button
            onClick={props.onToggleTheme}
            className="focus-ring flex h-8 w-8 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11 [@media(pointer:coarse)]:w-11"
            title={`Theme: ${props.theme ?? "dark"} (click to toggle)`}
            aria-label="Toggle theme"
          >
            {props.theme === "light" ? (
              <svg width={13} height={13} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round">
                <circle cx="12" cy="12" r="5" />
                <path d="M12 1v2M12 21v2M4.22 4.22l1.42 1.42M18.36 18.36l1.42 1.42M1 12h2M21 12h2M4.22 19.78l1.42-1.42M18.36 5.64l1.42-1.42" />
              </svg>
            ) : (
              <svg width={13} height={13} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round">
                <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
              </svg>
            )}
          </button>
        )}
        <span
          className="flex items-center gap-1.5 px-1"
          title={props.connected ? "connected" : "disconnected"}
        >
          <span
            aria-hidden="true"
            className={`h-1.5 w-1.5 rounded-none ${
              props.retrying
                ? "animate-pulse bg-warning"
                : props.connected
                  ? `bg-success${props.streaming || props.switching ? " animate-pulse" : ""}`
                  : "bg-danger"
            }`}
          />
          <span className={`font-mono text-[10px] uppercase tracking-wider text-ink-500 ${props.compact ? "hidden" : "hidden sm:inline"}`}>
            {props.switching
              ? "switching"
              : props.retrying
                ? "retrying"
                : props.streaming
                  ? "working"
                  : props.connected
                    ? "ready"
                    : "offline"}
          </span>
        </span>
        {/* Reconnect button when disconnected */}
        {!props.connected && props.onReconnect && (
          <button
            onClick={props.onReconnect}
            className="focus-ring flex h-8 items-center gap-1 rounded-sm px-2 font-mono text-[10px] uppercase tracking-wider text-ink-300 transition-colors hover:bg-ink-850 hover:text-ink-100 [@media(pointer:coarse)]:h-11"
            title="Reconnect to catcode-core"
          >
            <RefreshIcon width={11} height={11} /> Reconnect
          </button>
        )}
      </div>
    </header>
  );
}
