"use client";

import { useMemo, useState } from "react";
import type { PluginTrustEntry } from "@/lib/types";
import { ShieldIcon } from "./icons";

interface Props {
  plugins: PluginTrustEntry[];
  onDecide: (decisions: Record<string, "trust" | "deny">) => Promise<void>;
}

export function PluginTrustPrompt({ plugins, onDecide }: Props) {
  const initial = useMemo(
    () => Object.fromEntries(plugins.map((p) => [p.name, p.decision])) as Record<string, "" | "trust" | "deny">,
    [plugins],
  );
  const [decisions, setDecisions] = useState(initial);
  const [saving, setSaving] = useState(false);
  const undecided = plugins.filter((p) => !decisions[p.name]).length;

  const setDecision = (name: string, decision: "trust" | "deny") =>
    setDecisions((current) => ({ ...current, [name]: decision }));

  const apply = async () => {
    const selected = Object.fromEntries(
      Object.entries(decisions).filter(([, decision]) => decision === "trust" || decision === "deny"),
    ) as Record<string, "trust" | "deny">;
    if (!Object.keys(selected).length) return;
    setSaving(true);
    try {
      await onDecide(selected);
    } finally {
      setSaving(false);
    }
  };

  return (
    <section className="rounded-sm border border-accent/40 bg-ink-900 px-4 py-3 shadow-sm" role="dialog" aria-label="Plugin trust decisions">
      <div className="flex items-start gap-3">
        <ShieldIcon width={17} height={17} className="mt-0.5 shrink-0 text-accent-soft" />
        <div className="min-w-0 flex-1">
          <h2 className="text-[13px] font-semibold text-ink-100">Review project plugins</h2>
          <p className="mt-1 text-[11px] leading-relaxed text-ink-400">
            These plugins came from this workspace and can run code or inspect files. Choose what to trust before they load.
          </p>
          <div className="mt-3 space-y-2">
            {plugins.map((plugin) => {
              const decision = decisions[plugin.name] ?? "";
              return (
                <div key={plugin.name} className="rounded-sm border border-ink-800 bg-ink-950 px-3 py-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <div className="min-w-0">
                      <div className="truncate font-mono text-[12px] text-ink-100">{plugin.name}{plugin.version ? ` v${plugin.version}` : ""}</div>
                      {plugin.description && <div className="mt-0.5 text-[11px] text-ink-500">{plugin.description}</div>}
                    </div>
                    <div className="flex shrink-0 gap-1">
                      <button type="button" onClick={() => setDecision(plugin.name, "deny")} aria-pressed={decision === "deny"} className={`focus-ring rounded-sm border px-2 py-1 text-[11px] ${decision === "deny" ? "border-danger bg-danger/10 text-danger" : "border-ink-700 text-ink-400 hover:bg-ink-800"}`}>Deny</button>
                      <button type="button" onClick={() => setDecision(plugin.name, "trust")} aria-pressed={decision === "trust"} className={`focus-ring rounded-sm border px-2 py-1 text-[11px] ${decision === "trust" ? "border-success bg-success/10 text-success" : "border-ink-700 text-ink-400 hover:bg-ink-800"}`}>Trust</button>
                    </div>
                  </div>
                  {plugin.path && <div className="mt-1 break-all font-mono text-[10px] text-ink-600">{plugin.path}</div>}
                </div>
              );
            })}
          </div>
          <div className="mt-3 flex items-center justify-between gap-3">
            <span className="text-[10px] text-ink-500">{undecided ? `${undecided} undecided` : "All plugins decided"}</span>
            <button type="button" disabled={saving || undecided === plugins.length} onClick={() => void apply()} className="focus-ring rounded-sm bg-accent px-3 py-1.5 text-[11px] font-medium text-ink-950 disabled:cursor-not-allowed disabled:opacity-50">{saving ? "Applying…" : "Apply decisions"}</button>
          </div>
        </div>
      </div>
    </section>
  );
}
