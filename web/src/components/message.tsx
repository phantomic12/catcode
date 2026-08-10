"use client";

// Message — one turn on the session rail.
//   user      → YOU tick + ember quote well
//   assistant → AGENT tick + markdown + tools + metrics (streaming pulses the tick)
//   tool/bash/goal → meta ticks on the same continuous spine

import { memo, useState, useEffect, useRef, type ReactNode } from "react";
import type { ComponentPropsWithoutRef } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { AdvisorMsg, AssistantMsg, BashMsg, GoalMsg, ToolMsg, UserMsg, UIMessage } from "@/lib/types";
import { formatTokens } from "@/lib/format";
import { Markdown } from "./markdown";
import { Thinking } from "./thinking";
import { ToolCallCard } from "./tool-call";
import { CopyIcon, CheckIcon, PencilIcon, RefreshIcon } from "./icons";

function StreamingMarkdown({ children }: { children: string }) {
  return (
    <div className="prose-catalyst stream-caret">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={streamComponents}>
        {children}
      </ReactMarkdown>
    </div>
  );
}

const streamComponents: ComponentPropsWithoutRef<typeof ReactMarkdown>["components"] = {
  pre({ children }) {
    return <pre className="!my-2 !rounded-sm !border !border-ink-800 !bg-ink-950">{children}</pre>;
  },
};

function CopyBtn({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1400);
      },
      () => {},
    );
  };
  return (
    <button
      onClick={copy}
      className={`focus-ring flex h-8 w-8 min-h-11 min-w-11 items-center justify-center rounded-sm transition-colors hover:bg-ink-800 sm:min-h-0 sm:min-w-0 ${
        copied ? "text-success" : "text-ink-500 hover:text-ink-100"
      }`}
      aria-label="Copy message"
      title={copied ? "Copied" : "Copy"}
    >
      {copied ? <CheckIcon width={13} height={13} /> : <CopyIcon width={13} height={13} />}
    </button>
  );
}

function RoleLabel({
  children,
  tone = "muted",
}: {
  children: ReactNode;
  tone?: "muted" | "ember" | "live";
}) {
  const color =
    tone === "ember"
      ? "text-accent-soft"
      : tone === "live"
        ? "text-accent-soft"
        : "text-ink-500";
  return (
    <span className={`chat-role-label text-[10px] font-mono font-medium uppercase tracking-[0.14em] ${color}`}>
      {children}
    </span>
  );
}

function UserMessage({
  m,
  canEdit,
  onEdit,
  compact,
}: {
  m: UserMsg;
  canEdit?: boolean;
  onEdit?: (text: string, images?: string[]) => void;
  compact?: boolean;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(m.text);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const cancelledRef = useRef(false);
  useEffect(() => {
    if (editing) {
      setDraft(m.text);
      cancelledRef.current = false;
      requestAnimationFrame(() => {
        const el = inputRef.current;
        if (el) {
          el.focus();
          el.setSelectionRange(el.value.length, el.value.length);
        }
      });
    }
  }, [editing, m.text]);

  const commit = () => {
    const t = draft.trim();
    if (t && t !== m.text && onEdit) onEdit(t, m.images);
    setEditing(false);
  };

  return (
    <article
      className={`chat-turn chat-turn--user group chat-msg-enter ${compact ? "pr-2" : "pr-4 sm:pr-6"}`}
    >
      <span className="chat-turn-tick" aria-hidden="true" />
      <div className="chat-turn-header flex items-center gap-2 pb-2 pt-5">
        <RoleLabel tone="ember">You</RoleLabel>
        {m.steer && (
          <span className="rounded-sm border border-warning px-1.5 py-0.5 text-[9px] font-mono uppercase tracking-wider text-warning">
            steer
          </span>
        )}
        <span className="ml-auto flex items-center gap-0.5 opacity-100 transition-opacity sm:opacity-0 sm:group-hover:opacity-100 sm:focus-within:opacity-100">
          <CopyBtn text={m.text} />
          {canEdit && onEdit && !editing && (
            <button
              onClick={() => setEditing(true)}
              className="focus-ring flex h-8 w-8 min-h-11 min-w-11 items-center justify-center rounded-sm text-ink-500 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:min-h-0 sm:min-w-0"
              aria-label="Edit message"
              title="Edit & resend"
            >
              <PencilIcon width={13} height={13} />
            </button>
          )}
        </span>
      </div>
      {editing ? (
        <div className="chat-user-well mb-5 px-4 py-3.5 sm:px-5">
          <textarea
            ref={inputRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.shiftKey) {
                e.preventDefault();
                commit();
              } else if (e.key === "Escape") {
                e.preventDefault();
                cancelledRef.current = true;
                setEditing(false);
              }
            }}
            onBlur={() => {
              if (cancelledRef.current) {
                cancelledRef.current = false;
                return;
              }
              commit();
            }}
            rows={Math.min(8, draft.split("\n").length + 1)}
            className="w-full resize-none bg-transparent text-[13.5px] leading-relaxed text-ink-100 focus:outline-none"
          />
          <div className="flex justify-end gap-1.5 pt-1 font-mono text-[10px] text-ink-500">
            <kbd className="rounded-sm border border-ink-800 bg-ink-900 px-1">↵</kbd> save{" "}
            <kbd className="rounded-sm border border-ink-800 bg-ink-900 px-1">Esc</kbd> cancel
          </div>
        </div>
      ) : (
        <div className="chat-user-well mb-5 whitespace-pre-wrap break-words px-4 py-3.5 text-[14px] leading-relaxed text-ink-100 sm:px-5">
          {m.images && m.images.length > 0 && (
            <div className="mb-2 flex flex-wrap gap-1.5">
              {m.images.map((src, i) => (
                // eslint-disable-next-line @next/next/no-img-element
                <img
                  key={i}
                  src={src}
                  alt={`attachment ${i + 1}`}
                  className="h-16 w-16 rounded-sm border border-ink-800 object-cover"
                />
              ))}
            </div>
          )}
          {m.text}
        </div>
      )}
    </article>
  );
}

function AssistantMessage({
  m,
  canRegenerate,
  onRegenerate,
  compact,
}: {
  m: AssistantMsg;
  canRegenerate?: boolean;
  onRegenerate?: () => void;
  compact?: boolean;
}) {
  return (
    <article
      className={`chat-turn chat-turn--agent group chat-msg-enter ${
        m.streaming ? "chat-turn--streaming" : ""
      } ${compact ? "pr-2" : "pr-4 sm:pr-6"}`}
    >
      <span className="chat-turn-tick" aria-hidden="true" />
      <div className="chat-turn-header flex items-center gap-2 pb-2 pt-5">
        <RoleLabel tone={m.streaming ? "live" : "muted"}>Agent</RoleLabel>
        {m.model && (
          <span className="truncate font-mono text-[10px] text-ink-600">{m.model}</span>
        )}
        {m.streaming && (
          <span className="shimmer-text font-mono text-[10px] uppercase tracking-[0.12em] text-accent-soft">
            live
          </span>
        )}
        <span className="ml-auto flex items-center gap-0.5 opacity-100 transition-opacity sm:opacity-0 sm:group-hover:opacity-100 sm:focus-within:opacity-100">
          {canRegenerate && onRegenerate && !m.streaming && (
            <button
              onClick={onRegenerate}
              className="focus-ring flex h-8 w-8 min-h-11 min-w-11 items-center justify-center rounded-sm text-ink-500 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:min-h-0 sm:min-w-0"
              aria-label="Regenerate response"
              title="Regenerate"
            >
              <RefreshIcon width={13} height={13} />
            </button>
          )}
          <CopyBtn text={m.text} />
        </span>
      </div>
      <div className="chat-agent-body pb-6">
        <Thinking text={m.thinking} active={m.streaming} />
        {m.text ? (
          m.streaming ? (
            <StreamingMarkdown>{m.text}</StreamingMarkdown>
          ) : (
            <Markdown>{m.text}</Markdown>
          )
        ) : (
          !m.thinking &&
          m.toolCalls.length === 0 &&
          m.streaming && (
            <div className="flex items-center gap-2 py-1.5 text-[13px]">
              <span className="h-1.5 w-1.5 animate-pulse rounded-none bg-accent-soft" aria-hidden="true" />
              <span className="shimmer-text font-medium text-ink-300">Thinking…</span>
            </div>
          )
        )}
        {m.toolCalls.length > 0 && (
          <div className="chat-run-stack mt-4 space-y-2">
            {m.toolCalls.map((tc) => (
              <ToolCallCard key={tc.id} tc={tc} />
            ))}
          </div>
        )}
        {m.usage && (
          <div className="mt-2.5 flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[10px] tabular-nums text-ink-600">
            {m.usage.prompt_tokens != null && <span>↑ {formatTokens(m.usage.prompt_tokens)}</span>}
            {m.usage.tokens_out != null && <span>↓ {formatTokens(m.usage.tokens_out)}</span>}
            {m.usage.cached_tokens ? <span>cached {formatTokens(m.usage.cached_tokens)}</span> : null}
          </div>
        )}
      </div>
    </article>
  );
}

function ToolMessage({ m }: { m: ToolMsg }) {
  return (
    <article className="chat-turn chat-turn--meta chat-msg-enter pr-2 sm:pr-4">
      <span className="chat-turn-tick" aria-hidden="true" />
      <span className="sr-only">Run</span>
      <div className="chat-run-record my-2 overflow-hidden border border-ink-800 bg-ink-950">
        <div className="flex items-center gap-2 border-b border-ink-800 bg-ink-925 px-2.5 py-1.5">
          <span className={`h-1.5 w-1.5 rounded-none ${m.ok ? "bg-success" : "bg-danger"}`} aria-hidden="true" />
          <span className="font-mono text-[10px] uppercase tracking-[0.12em] text-ink-500">
            {m.toolName || "tool"} · {m.ok ? "ok" : "error"}
          </span>
        </div>
        <pre
          className={`max-h-60 overflow-auto whitespace-pre-wrap break-words px-3 py-2 font-mono text-[11px] leading-relaxed ${
            m.ok ? "text-ink-300" : "text-danger"
          }`}
        >
          {m.output}
        </pre>
      </div>
    </article>
  );
}

function BashMessage({ m }: { m: BashMsg }) {
  const prefix = m.excludeFromContext ? "!!" : "!";
  return (
    <article className="chat-turn chat-turn--meta chat-msg-enter pr-2 sm:pr-4">
      <span className="chat-turn-tick" aria-hidden="true" />
      <span className="sr-only">Run</span>
      <div className="chat-run-record my-2 overflow-hidden border border-ink-800 bg-ink-950">
        <div className="flex items-center gap-2 border-b border-ink-800 bg-ink-925 px-2.5 py-1.5">
          <span className={`h-1.5 w-1.5 rounded-none ${m.ok ? "bg-success" : "bg-danger"}`} aria-hidden="true" />
          <span className="font-mono text-[10px] uppercase tracking-[0.12em] text-ink-500">
            {prefix} bash · {m.ok ? "ok" : "error"}
            {m.excludeFromContext ? " · no context" : ""}
          </span>
        </div>
        <div className="px-3 py-2">
          <div className="font-mono text-[12px] text-accent-soft">$ {m.command}</div>
          <pre
            className={`mt-1.5 max-h-60 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed ${
              m.ok ? "text-ink-300" : "text-danger"
            }`}
          >
            {m.output}
          </pre>
        </div>
      </div>
    </article>
  );
}

function AdvisorMessage({ m }: { m: AdvisorMsg }) {
  const failed = ["failed", "no_key", "invalid_response"].includes(m.state);
  const actionable = m.severity === "concern" || m.severity === "blocker";
  const tone = failed
    ? { bar: "border-danger", dot: "bg-danger", label: "text-danger" }
    : actionable
      ? { bar: "border-warning", dot: "bg-warning", label: "text-warning" }
      : m.state === "clear"
        ? { bar: "border-success", dot: "bg-success", label: "text-success" }
        : { bar: "border-accent", dot: "bg-accent-soft", label: "text-accent-soft" };
  const label = m.severity || m.state || "reviewing";
  const text = m.text || (m.state === "reviewing"
    ? "Checking the completed work against the request…"
    : m.state === "clear"
      ? "No actionable finding."
      : failed
        ? "Reviewer was unavailable; the executor continued."
        : "");
  return (
    <article className="chat-turn chat-turn--meta chat-msg-enter pr-2 sm:pr-4">
      <span className="chat-turn-tick" aria-hidden="true" />
      <span className="sr-only">Advisor review</span>
      <div className={`chat-run-record my-2 border border-ink-800 border-l-2 bg-ink-925 px-3.5 py-2.5 ${tone.bar}`}>
        <div className="flex min-w-0 items-center gap-2 font-mono text-[10px] uppercase tracking-[0.12em]">
          <span className={`h-1.5 w-1.5 shrink-0 rounded-none ${tone.dot}`} aria-hidden="true" />
          <span className={tone.label}>advisor</span>
          <span className="truncate text-ink-300">{m.advisor || "default"}</span>
          {m.model && <span className="truncate text-ink-600 normal-case">{m.model}</span>}
          <span className="ml-auto shrink-0 text-ink-500">{label}</span>
          {m.elapsedMs != null && <span className="shrink-0 text-ink-600">{(m.elapsedMs / 1000).toFixed(1)}s</span>}
        </div>
        {text && (
          <pre className={`mt-1.5 max-h-48 overflow-auto whitespace-pre-wrap break-words text-[12px] leading-relaxed ${failed ? "text-danger" : "text-ink-300"}`}>
            {text}
          </pre>
        )}
      </div>
    </article>
  );
}

function GoalMessage({ m }: { m: GoalMsg }) {
  const tone =
    m.ok === false || m.status === "failed"
      ? { bar: "border-danger", dot: "bg-danger", label: "text-danger" }
      : m.status === "skipped"
        ? { bar: "border-ink-700", dot: "bg-ink-600", label: "text-ink-500" }
        : m.kind === "phase"
          ? { bar: "border-accent", dot: "bg-accent-soft", label: "text-accent-soft" }
          : { bar: "border-success", dot: "bg-success", label: "text-success" };
  const kindLabel =
    m.kind === "step_complete"
      ? "step"
      : m.kind === "completion_summary"
        ? "summary"
        : m.kind === "verdict"
          ? "verdict"
          : "phase";
  return (
    <article className="chat-turn chat-turn--meta chat-msg-enter pr-2 sm:pr-4">
      <span className="chat-turn-tick" aria-hidden="true" />
      <span className="sr-only">Run</span>
      <div className={`chat-run-record my-2 border border-ink-800 border-l-2 bg-ink-925 px-3.5 py-2.5 ${tone.bar}`}>
        <div className="flex items-center gap-2 font-mono text-[10px] uppercase tracking-[0.12em]">
          <span className={`h-1.5 w-1.5 rounded-none ${tone.dot}`} aria-hidden="true" />
          <span className={tone.label}>goal · {kindLabel}</span>
          {m.status && <span className="text-ink-500">{m.status}</span>}
        </div>
        <div className="mt-1 text-[13px] font-semibold tracking-tight text-ink-100">{m.title}</div>
        {m.text && (
          <pre className="mt-1.5 max-h-48 overflow-auto whitespace-pre-wrap break-words text-[12px] leading-relaxed text-ink-300">
            {m.text}
          </pre>
        )}
      </div>
    </article>
  );
}

export const Message = memo(function Message({
  m,
  onEditUser,
  onRegenerate,
  canEdit,
  canRegenerate,
  compact,
}: {
  m: UIMessage;
  onEditUser?: (text: string, images?: string[]) => void;
  onRegenerate?: () => void;
  canEdit?: boolean;
  canRegenerate?: boolean;
  compact?: boolean;
}) {
  if (m.role === "user")
    return <UserMessage m={m} canEdit={canEdit} onEdit={onEditUser} compact={compact} />;
  if (m.role === "assistant")
    return (
      <AssistantMessage
        m={m}
        canRegenerate={canRegenerate}
        onRegenerate={onRegenerate}
        compact={compact}
      />
    );
  if (m.role === "bash") return <BashMessage m={m} />;
  if (m.role === "advisor") return <AdvisorMessage m={m} />;
  if (m.role === "goal") return <GoalMessage m={m} />;
  return <ToolMessage m={m} />;
});
