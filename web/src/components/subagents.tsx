"use client";

// SubagentsPanel — the live subagent supervisor surface.
//
// Replaces the old flat intercom log with two views:
//  (1) a runs list: every subagent run (single / parallel-batch / chain), each
//      card showing agent, task, state, elapsed, tokens, current phase/tool —
//      with a pulsing indicator while running.
//  (2) a drill-in live chat: click a run to see what it's doing — its task,
//      each assistant turn's text, and every tool call (name + args) with its
//      result. This is the per-run equivalent of the main chat, assembled from
//      the core's run_id-tagged subagent_message / subagent_tool_call /
//      subagent_tool_result events.
//
// The blocking `need_decision` ask still uses the inline IntercomPrompt banner
// (intercom.tsx); this panel is for observation, not reply.

import { useEffect, useState } from "react";
import type { AgentInfo, ProcessLogsView, ProcessStatusView, SessionTreeSnapshot, SubagentChatItem, SubagentRunView } from "@/lib/types";
import {
  formatMs,
  formatTokens,
  relativeTime,
  truncate,
  prettyArgs,
  toolIcon,
  isDangerousTool,
} from "@/lib/format";
import {
  XIcon,
  ChevronRight,
  DotIcon,
  CheckIcon,
  WarningIcon,
  RefreshIcon,
} from "./icons";
import { useOutsideClose, mergeRefs } from "@/lib/use-outside-close";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { useBodyScrollLock } from "@/lib/use-body-scroll-lock";

/** Fallback builtins if the core hasn't emitted an `agents` event yet. */
const FALLBACK_AGENTS: AgentInfo[] = [
  { name: "scout", description: "Fast code exploration", source: "builtin" },
  { name: "reviewer", description: "Evidence-backed review", source: "builtin" },
  { name: "worker", description: "Implementation writer", source: "builtin" },
  { name: "oracle", description: "Decision consistency", source: "builtin" },
  { name: "planner", description: "Implementation planning", source: "builtin" },
  { name: "researcher", description: "Deep research", source: "builtin" },
  { name: "context-builder", description: "Gather handoff context", source: "builtin" },
  { name: "delegate", description: "General delegated work", source: "builtin" },
];

/** @deprecated Prefer live `availableAgents` from core. Kept for external imports. */
export const AVAILABLE_AGENTS = FALLBACK_AGENTS.map((a) => a.name);

const STATE_STYLE: Record<string, { dot: string; text: string; label: string }> = {
  running: { dot: "bg-warning", text: "text-warning", label: "running" },
  completed: { dot: "bg-success", text: "text-success", label: "done" },
  failed: { dot: "bg-danger", text: "text-danger", label: "failed" },
  paused: { dot: "bg-ink-500", text: "text-ink-400", label: "paused" },
};

function StateBadge({ state }: { state: string }) {
  const s = STATE_STYLE[state] ?? STATE_STYLE.paused;
  const pulse = state === "running";
  return (
    <span className={`inline-flex items-center gap-1.5 ${s.text}`}>
      <span
        className={`h-1.5 w-1.5 rounded-none ${s.dot} ${pulse ? "animate-pulse" : ""}`}
      />
      <span className="text-[11px] font-medium">{s.label}</span>
    </span>
  );
}

function sessionTreeSnapshot(tree: unknown): SessionTreeSnapshot | null {
  if (!tree || typeof tree !== "object") return null;
  const value = tree as Partial<SessionTreeSnapshot>;
  return Array.isArray(value.entries) ? value as SessionTreeSnapshot : null;
}

function SessionTreeView({ tree, onBranch }: { tree: unknown; onBranch?: (entryId: string) => void }) {
  const snapshot = sessionTreeSnapshot(tree);
  if (!snapshot) return null;
  const active = snapshot.leaf ?? null;
  const ancestry = new Set(snapshot.ancestry ?? []);
  const siblings = new Set(snapshot.siblings ?? []);
  return (
    <section className="mb-3 rounded-sm border border-ink-800 bg-ink-900 px-3 py-2.5" aria-label="Session tree">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-[10px] font-mono uppercase tracking-wider text-ink-500">Session tree</span>
        {active && <span className="font-mono text-[10px] text-accent-soft">active {active}</span>}
      </div>
      <div className="space-y-1">
        {snapshot.entries.map((entry) => {
          const isActive = entry.id === active;
          const role = isActive ? "active leaf" : ancestry.has(entry.id) ? "ancestry" : siblings.has(entry.id) ? "sibling" : "branch";
          return (
            <button key={entry.id} onClick={() => onBranch?.(entry.id)} className={`flex w-full items-center gap-2 rounded-sm px-2 py-1 text-left text-[11px] ${isActive ? "bg-accent/15 text-accent-soft" : "text-ink-300 hover:bg-ink-850"}`}>
              <span className="w-14 shrink-0 font-mono text-[9px] uppercase text-ink-600">{role}</span>
              <span className="min-w-0 flex-1 truncate">{entry.title || entry.id}</span>
              {entry.summary && <span className="max-w-[45%] truncate text-ink-600">{entry.summary}</span>}
            </button>
          );
        })}
      </div>
    </section>
  );
}
export function ProcessPanel({ processes, logs, onLogs, onStop }: { processes: Record<string, ProcessStatusView>; logs: Record<string, ProcessLogsView>; onLogs?: (name: string) => void; onStop?: (name: string) => void }) {
  const list = Object.values(processes);
  if (list.length === 0) return null;
  return (
    <section className="mb-3 rounded-sm border border-ink-800 bg-ink-900 px-3 py-2.5" aria-label="Project processes">
      <div className="mb-2 text-[10px] font-mono uppercase tracking-wider text-ink-500">Project processes</div>
      <div className="space-y-2">
        {list.map((process) => (
          <div key={process.name} className="border-l-2 border-ink-700 pl-2">
            <div className="flex items-center gap-2 text-[11px]">
              <span className={`h-1.5 w-1.5 ${process.state === "ready" ? "bg-success" : process.state === "starting" ? "bg-warning" : "bg-ink-600"}`} />
              <code className="text-ink-200">{process.name}</code>
              <span className="text-ink-600">pid {process.pid} · {process.state}</span>
              <button className="ml-auto text-accent-soft" onClick={() => onLogs?.(process.name)}>request logs</button>
              {process.state !== "exited" && <button className="text-danger" onClick={() => onStop?.(process.name)}>request stop</button>}
            </div>
            {logs[process.name] && <pre className="mt-1 max-h-24 overflow-auto whitespace-pre-wrap bg-ink-950 p-2 font-mono text-[10px] text-ink-400">{logs[process.name].truncated && "[earlier output truncated]\n"}{logs[process.name].text}</pre>}
          </div>
        ))}
      </div>
    </section>
  );
}

export function RunCard({ run, onClick }: { run: SubagentRunView; onClick: () => void }) {
  const isContainer = run.mode === "parallel" || run.mode === "chain";
  const title = run.agent ?? (isContainer ? run.mode : "subagent");
  const phaseHint =
    run.state === "running" && run.phase
      ? run.tool
        ? `${run.phase} · ${run.tool}`
        : run.phase
      : run.summary
        ? truncate(run.summary, 90)
        : run.state === "running"
          ? "working…"
          : "finished";
  return (
    <button
      onClick={onClick}
      className="group w-full rounded-sm border border-ink-800 bg-ink-900 px-3.5 py-3 text-left transition-colors hover:border-ink-600 hover:bg-ink-850"
    >
      <div className="flex items-center gap-2">
        <span className="font-mono text-[12px] font-semibold text-accent-soft">
          {isContainer ? "❯" : "↳"} {title}
        </span>
        {run.mode !== "single" && (
          <span className="rounded-sm bg-ink-850 px-1.5 py-0.5 text-[10px] uppercase tracking-wide text-ink-400">
            {run.mode}
          </span>
        )}
        <span className="ml-auto">
          <StateBadge state={run.state} />
        </span>
      </div>
      <div className="mt-1 line-clamp-2 text-[12px] leading-relaxed text-ink-300">
        {run.task || (isContainer ? `${run.agents.length} agent(s)` : "—")}
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-ink-500">
        <span>{formatMs(run.elapsedMs)}</span>
        <span className="text-ink-700">·</span>
        <span>↑{formatTokens(run.tokensOut)}</span>
        <span className="text-ink-700">·</span>
        <span>{run.toolCount} tool{run.toolCount === 1 ? "" : "s"}</span>
        <span className="ml-auto truncate text-ink-600">{phaseHint}</span>
      </div>
    </button>
  );
}

function ChatMessage({ item }: { item: SubagentChatItem }) {
  const isUser = item.role === "user";
  return (
    <div className={isUser ? "" : "mt-3"}>
      <div className="mb-1 flex items-center gap-1.5">
        <span
          className={`text-[11px] font-semibold uppercase tracking-wide ${
            isUser ? "text-accent-soft" : "text-ink-400"
          }`}
        >
          {isUser ? "Task" : "Assistant"}
        </span>
      </div>
      <pre className="whitespace-pre-wrap break-words rounded-sm border border-ink-800 bg-ink-950 p-2.5 text-[12.5px] leading-relaxed text-ink-200">
        <code>{item.content}</code>
      </pre>
    </div>
  );
}

function ToolBlock({ item }: { item: SubagentChatItem }) {
  const [open, setOpen] = useState(false);
  const name = item.name ?? "";
  const dangerous = isDangerousTool(name);
  const pending = item.result === undefined;
  const argStr = item.args ? prettyArgs(item.args) : "";
  const showArgs = argStr && argStr !== "{}";
  const out = item.result ?? "";
  const displayOut =
    out || (!pending && name === "finish" ? "This turn has finished" : "");

  return (
    <div className="mt-3 rounded-sm border border-ink-800 bg-ink-900">
      <button
        onClick={() => showArgs && setOpen((o) => !o)}
        className={`flex w-full items-center gap-2 px-3 py-2 text-left ${
          showArgs ? "hover:bg-ink-850" : "cursor-default"
        }`}
      >
        <span className="text-[13px]">{toolIcon(name)}</span>
        <span className="font-mono text-[12px] font-medium text-ink-200">{name || "tool"}</span>
        {dangerous && (
          <span className="rounded-sm bg-ink-900 px-1.5 py-0.5 text-[10px] font-medium text-danger">
            destructive
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          {pending ? (
            <span className="flex items-center gap-1 text-[11px] text-warning">
              <DotIcon className="text-warning" /> running
            </span>
          ) : item.ok ? (
            <span className="flex items-center gap-1 text-[11px] text-success">
              <CheckIcon width={12} height={12} /> ok
            </span>
          ) : (
            <span className="flex items-center gap-1 text-[11px] text-danger">
              <WarningIcon width={12} height={12} /> error
            </span>
          )}
          {showArgs && (
            <ChevronRight
              width={14}
              height={14}
              className={`text-ink-500 transition-transform ${open ? "rotate-90" : ""}`}
            />
          )}
        </span>
      </button>
      {showArgs && open && (
        <pre className="mx-3 mb-2 max-h-48 overflow-auto whitespace-pre-wrap break-words rounded border border-ink-800 bg-ink-950 p-2 text-[11.5px] leading-relaxed text-ink-300">
          <code>{argStr}</code>
        </pre>
      )}
      {displayOut && (
        <pre className="mx-3 mb-3 mt-1 max-h-64 overflow-auto whitespace-pre-wrap break-words rounded border border-ink-800 bg-ink-950 p-2 text-[11.5px] leading-relaxed text-ink-400">
          <code>{displayOut}</code>
        </pre>
      )}
    </div>
  );
}

export function RunDetail({ run, onBack }: { run: SubagentRunView; onBack: () => void }) {
  const title = run.agent ?? run.mode;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2 border-b border-ink-800 px-4 py-3">
        <button
          onClick={onBack}
          className="flex items-center gap-1 rounded-sm px-1.5 py-0.5 text-[13px] text-ink-300 transition-colors hover:bg-ink-800 hover:text-ink-100"
        >
          <ChevronRight width={14} height={14} className="rotate-180" />
          Back
        </button>
        <span className="font-mono text-[13px] font-semibold text-accent-soft">↳ {title}</span>
        <span className="ml-auto">
          <StateBadge state={run.state} />
        </span>
      </div>
      <div className="border-b border-ink-800 px-4 py-2.5">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-ink-500">
          <span>{formatMs(run.elapsedMs)}</span>
          <span className="text-ink-700">·</span>
          <span>↑{formatTokens(run.tokensOut)} ↓{formatTokens(run.tokensIn)}</span>
          <span className="text-ink-700">·</span>
          <span>{run.toolCount} tool{run.toolCount === 1 ? "" : "s"}</span>
          <span className="text-ink-700">·</span>
          <span>{relativeTime(run.startedAt)}</span>
        </div>
        {run.task && (
          <p className="mt-1.5 line-clamp-2 text-[12px] leading-relaxed text-ink-300">{run.task}</p>
        )}
        {run.state !== "running" && run.summary && (
          <div className="mt-2 rounded-sm border border-ink-800 bg-ink-900 px-2.5 py-2">
            <div className="font-mono text-[10px] uppercase tracking-wider text-ink-500">
              final
            </div>
            <pre className="mt-1 max-h-40 overflow-auto whitespace-pre-wrap break-words text-[12px] leading-relaxed text-ink-200">
              {run.summary}
            </pre>
          </div>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
        {run.items.length === 0 ? (
          <div className="px-3 py-10 text-center text-[12px] text-ink-600">
            {run.mode === "parallel" || run.mode === "chain" ? (
              <>
                This is a container run ({run.mode}). Its child runs appear in the list —
                each has its own live chat.
              </>
            ) : run.state === "running" ? (
              "Waiting for the subagent's first response…"
            ) : (
              "No transcript captured for this run."
            )}
          </div>
        ) : (
          <div className="space-y-0">
            {run.items.map((item) =>
              item.kind === "message" ? (
                <ChatMessage key={item.id} item={item} />
              ) : (
                <ToolBlock key={item.id} item={item} />
              ),
            )}
            {run.state === "running" && (
              <div className="mt-3 flex items-center gap-1.5 text-[11px] text-warning">
                <DotIcon className="text-warning" /> working…
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

interface PanelProps {
  runs: Record<string, SubagentRunView>;
  jobs?: Record<string, { runId: string; parentRunId?: string | null; state: string; summary?: string | null }>;
  processes?: Record<string, ProcessStatusView>;
  processLogs?: Record<string, ProcessLogsView>;
  sessionTree?: unknown | null;
  agents?: AgentInfo[];
  onRefreshAgents?: () => void;
  onRefreshJobs?: () => void;
  onJobStatus?: (runId: string) => void;
  onJobWait?: (runId: string) => void;
  onJobCancel?: (runId: string) => void;
  onBranch?: (entryId: string) => void;
  onProcessLogs?: (name: string) => void;
  onProcessStop?: (name: string) => void;
  onClose: () => void;
}

export function SubagentsPanel({ runs, jobs = {}, processes = {}, processLogs = {}, sessionTree, agents, onRefreshAgents, onRefreshJobs, onJobStatus, onJobWait, onJobCancel, onBranch, onProcessLogs, onProcessStop, onClose }: PanelProps) {
  const closeRef = useOutsideClose(onClose);
  const trapRef = useFocusTrap<HTMLDivElement>();
  useBodyScrollLock();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  useEffect(() => {
    onRefreshAgents?.();
    onRefreshJobs?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const list = Object.values(runs).sort((a, b) => {
    const ar = a.state === "running" ? 1 : 0;
    const br = b.state === "running" ? 1 : 0;
    if (ar !== br) return br - ar;
    return (b.startedAt || 0) - (a.startedAt || 0);
  });
  const runningCount = list.filter((r) => r.state === "running").length;
  const selected = selectedId ? runs[selectedId] : null;
  const agentList = agents && agents.length > 0 ? agents : FALLBACK_AGENTS;

  return (
    <div className="modal-backdrop">
      <div
        ref={mergeRefs(closeRef, trapRef)}
        className="modal-sheet max-w-2xl"
        role="dialog"
        aria-modal="true"
        aria-label="Subagents"
      >
        <div className="flex min-h-11 items-center justify-between border-b border-ink-800 px-5 py-3.5">
          <div className="flex items-center gap-2">
            <span className="text-[15px] font-semibold text-ink-100">Subagents</span>
            {runningCount > 0 && (
              <span className="flex items-center gap-1.5 rounded-sm border border-warning bg-ink-900 px-2 py-0.5 text-[11px] font-medium text-warning">
                <DotIcon className="text-warning" /> {runningCount} running
              </span>
            )}
          </div>
          <div className="flex items-center gap-1">
            {onRefreshAgents && (
              <button
                onClick={onRefreshAgents}
                className="focus-ring flex h-11 w-11 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:h-7 sm:w-7"
                aria-label="Refresh agents"
                title="Refresh available agents"
              >
                <RefreshIcon width={15} height={15} />
              </button>
            )}
            <button
              onClick={onClose}
              className="focus-ring flex h-11 w-11 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:h-7 sm:w-7"
              aria-label="Close"
            >
              <XIcon width={16} height={16} />
            </button>
          </div>
        </div>

        {selected ? (
          <RunDetail run={selected} onBack={() => setSelectedId(null)} />
        ) : (
          <div className="flex-1 overflow-y-auto p-3">
            <div className="mb-3 rounded-sm border border-ink-800 bg-ink-900 px-3 py-2.5">
              <div className="mb-1.5 text-[10px] font-mono uppercase tracking-wider text-ink-500">
                Available agents
              </div>
              <div className="flex flex-wrap gap-1.5">
                {agentList.map((a) => (
                  <code
                    key={a.name}
                    title={a.description || a.source}
                    className="rounded-sm bg-ink-850 px-1.5 py-0.5 font-mono text-[11px] text-accent-soft"
                  >
                    {a.name}
                  </code>
                ))}
              </div>

              <p className="mt-2 text-[11px] text-ink-600">
                Use <code className="font-mono text-ink-500">/run</code>,{" "}
                <code className="font-mono text-ink-500">/parallel</code>, or{" "}
                <code className="font-mono text-ink-500">/chain</code> — or the{" "}
                <code className="font-mono text-ink-500">subagent</code> tool.
              </p>
            </div>
            {Object.keys(jobs).length > 0 && (
              <div className="mb-3 border-b border-ink-800 pb-3">
                <div className="mb-2 flex items-center justify-between text-[10px] font-mono uppercase tracking-wider text-ink-500">
                  <span>Durable jobs</span>
                  <button className="text-accent-soft" onClick={onRefreshJobs}>Refresh</button>
                </div>
                {Object.values(jobs).map((job) => (
                  <div key={job.runId} className="flex items-center gap-2 py-1 text-[12px]" style={{ paddingLeft: job.parentRunId ? 16 : 0 }}>
                    <code className="text-ink-300">{job.runId}</code>
                    <span className="text-ink-500">{job.state}</span>
                    <button className="text-accent-soft" onClick={() => onJobStatus?.(job.runId)}>status</button>
                    {job.state === "running" && <button className="text-warning" onClick={() => onJobWait?.(job.runId)}>wait</button>}
                    {job.state === "running" && <button className="text-danger" onClick={() => onJobCancel?.(job.runId)}>cancel</button>}
                  </div>
                ))}
              </div>
            )}
            <ProcessPanel
              processes={processes}
              logs={processLogs}
              onLogs={onProcessLogs}
              onStop={onProcessStop}
            />
            <SessionTreeView tree={sessionTree} onBranch={onBranch} />
            {list.length === 0 ? (
              <div className="px-3 py-8 text-center text-[12px] text-ink-600">
                No runs yet. Delegated work will appear here live.
              </div>
            ) : (
              <div className="space-y-2">
                {list.map((r) => (
                  <RunCard key={r.id} run={r} onClick={() => setSelectedId(r.id)} />
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
