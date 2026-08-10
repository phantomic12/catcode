"use client";

// ToolCall — a flat panel section for a single tool invocation: name + args
// (syntax-highlighted JSON) + the execution result (mono, scrollable, with an
// optional diff pane). A square status dot marks running / ok / error.

import { useEffect, useRef, useState } from "react";
import type { UIToolCall } from "@/lib/types";
import { isDangerousTool, prettyArgs, toolArgPreview, toolIcon } from "@/lib/format";
import { ChevronRight, CheckIcon, CopyIcon } from "./icons";
import { Diff } from "./diff";

export function ToolCallCard({ tc }: { tc: UIToolCall }) {
  const running = !tc.result;
  const ok = tc.result?.ok;
  const unknown = tc.result?.unknown;
  const isError = !!tc.result && !ok && !unknown;
  const [open, setOpen] = useState(running || isError);
  const [copied, setCopied] = useState(false);
  const userToggledRef = useRef(false);
  const danger = isDangerousTool(tc.name);

  // Auto-expand while running; keep errors expanded; collapse successes unless user toggled.
  useEffect(() => {
    if (userToggledRef.current) return;
    if (running || isError) setOpen(true);
    else setOpen(false);
  }, [running, isError]);

  const copy = () => {
    navigator.clipboard?.writeText(tc.result?.output ?? "").then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), 1400);
      },
      () => {},
    );
  };

  return (
    <div className="chat-run-record overflow-hidden rounded-sm border border-ink-800 bg-ink-950 transition-colors duration-150">
      <button
        onClick={() => {
          userToggledRef.current = true;
          setOpen((o) => !o);
        }}
        aria-expanded={open}
        className="flex w-full items-center gap-2 bg-ink-925 px-2.5 py-1.5 text-left transition-colors hover:bg-ink-900"
      >
        <ChevronRight
          width={11}
          height={11}
          className={`shrink-0 text-ink-600 transition-transform duration-150 ${open ? "rotate-90" : ""}`}
        />
        <span className="shrink-0 text-[12px] leading-none text-ink-300">{toolIcon(tc.name)}</span>
        <span className="font-mono text-[11px] text-ink-200">{tc.name || "tool"}</span>
        {danger && (
          <span className="rounded-sm border border-warning px-1.5 py-px font-mono text-[9px] uppercase tracking-wider text-warning">
            destructive
          </span>
        )}
        <span className="min-w-0 flex-1 truncate font-mono text-[10px] text-ink-500">
          {toolArgPreview(tc.name, tc.args, tc.argString, 72)}
        </span>
        <span className="ml-auto flex shrink-0 items-center">
          {running ? (
            <span className="flex items-center gap-1.5 font-mono text-[10px] uppercase tracking-wider text-warning">
              <span className="h-1.5 w-1.5 animate-pulse rounded-none bg-warning" aria-hidden="true" />
              running
            </span>
          ) : unknown ? (
            <span className="flex items-center gap-1.5 font-mono text-[10px] uppercase tracking-wider text-ink-500">
              <span className="h-1.5 w-1.5 rounded-none bg-ink-600" aria-hidden="true" />
              loaded
            </span>
          ) : ok ? (
            <span className="flex items-center gap-1.5 font-mono text-[10px] uppercase tracking-wider text-success">
              <span className="h-1.5 w-1.5 rounded-none bg-success" aria-hidden="true" />
              ok
            </span>
          ) : (
            <span className="flex items-center gap-1.5 font-mono text-[10px] uppercase tracking-wider text-danger">
              <span className="h-1.5 w-1.5 rounded-none bg-danger" aria-hidden="true" />
              error
            </span>
          )}
        </span>
      </button>

      {open && (
        <div className="border-t border-ink-800 bg-ink-950">
          {tc.argString && (
            <div className="px-3 py-2">
              <div className="mb-1 font-mono text-[10px] uppercase tracking-wider text-ink-500">
                arguments
              </div>
              <pre className="overflow-x-auto font-mono text-[11px] leading-relaxed text-ink-300">
                <code>{prettyArgs(tc.args)}</code>
              </pre>
            </div>
          )}
          {tc.result && (
            <div className={`px-3 py-2 ${tc.argString ? "border-t border-ink-800" : ""}`}>
              <div className="mb-1 flex items-center justify-between">
                <span className="font-mono text-[10px] uppercase tracking-wider text-ink-500">
                  result
                </span>
                <button
                  onClick={copy}
                  className={`flex min-h-11 items-center gap-1 rounded-sm px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wider transition-colors sm:min-h-0 ${
                    copied ? "text-success" : "text-ink-500 hover:bg-ink-800 hover:text-ink-100"
                  }`}
                >
                  {copied ? <CheckIcon width={11} height={11} /> : <CopyIcon width={11} height={11} />}
                  {copied ? "Copied" : "Copy"}
                </button>
              </div>
              {tc.result.diff && <Diff diff={tc.result.diff} />}
              <pre
                className={`mt-1 max-h-80 overflow-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-relaxed ${
                  ok || unknown ? "text-ink-300" : "text-danger"
                }`}
              >
                {tc.result.output ||
                  (tc.name === "finish" ? "This turn has finished" : "(no output)")}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
