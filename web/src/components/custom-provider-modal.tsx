"use client";

// CustomProviderModal — add (or update) a custom provider with full
// config.json parity: name, wire kind, base URL, API key or env var, extra
// headers, and a context-window override. PLUS a discover step: once the
// endpoint is entered, the harness fetches the models it exposes and the user
// can refine per-model caps (reasoning levels, context length, output tokens)
// — anything left at the discovered/default value (200k/8k flat default for
// unknown ids) is not written, so the config stays clean.
//
// Discovery is optional. Failures surface inline without locking the form;
// the user can always add without discovering, or type model ids by hand.

import { useEffect, useState } from "react";
import type { CustomProviderDraft, ModelInfo, ModelOverride } from "@/lib/types";
import { useOutsideClose, mergeRefs } from "@/lib/use-outside-close";
import { useFocusTrap } from "@/lib/use-focus-trap";
import { useBodyScrollLock } from "@/lib/use-body-scroll-lock";
import { XIcon, ShieldIcon, ArrowLeftIcon } from "./icons";

interface Props {
  /** Models discovered by `discover_provider_models` (null until a preview lands). */
  previewModels: ModelInfo[] | null;
  /** Error string from the latest discover attempt (empty list / hard failure). */
  discoverError?: string | null;
  /** True while a discovery request is in flight. */
  discovering: boolean;
  onDiscover: (
    base_url: string,
    kind: CustomProviderDraft["kind"],
    api_key?: string,
    headers?: Record<string, string>,
  ) => void;
  /** Cancel an in-flight discover (clears spinner; late preview is ignored). */
  onCancelDiscover?: () => void;
  onSubmit: (draft: CustomProviderDraft) => void;
  onClose: () => void;
}

const KINDS: { id: CustomProviderDraft["kind"]; label: string; hint: string }[] = [
  { id: "openai", label: "OpenAI-compatible", hint: "/chat/completions · Authorization: Bearer" },
  { id: "anthropic", label: "Anthropic", hint: "/v1/messages · x-api-key" },
];

/** Editable per-model capability metadata. */
interface ModelCaps {
  context_window: string;
  max_tokens: string;
  reasoning: boolean;
  thinking_levels: string;
  input: string;
  output: string;
  tool_call: boolean;
  structured_output: boolean;
}

function parseHeaders(text: string): Record<string, string> {
  const headers: Record<string, string> = {};
  for (const line of text.split(/\n+/)) {
    const i = line.indexOf(":");
    if (i <= 0) continue;
    const k = line.slice(0, i).trim();
    const v = line.slice(i + 1).trim();
    if (k && v) headers[k] = v;
  }
  return headers;
}

export function CustomProviderModal({
  previewModels,
  discoverError = null,
  discovering,
  onDiscover,
  onCancelDiscover,
  onSubmit,
  onClose,
}: Props) {
  const closeRef = useOutsideClose(onClose);
  const trapRef = useFocusTrap<HTMLDivElement>();
  useBodyScrollLock();
  const [step, setStep] = useState<"endpoint" | "models">("endpoint");
  const [draft, setDraft] = useState<CustomProviderDraft>({
    name: "",
    kind: "openai",
    base_url: "",
    apiKey: "",
    apiKeyEnv: "",
    headersText: "",
    contextWindow: "",
    modelsOverride: [],
  });
  const [touched, setTouched] = useState(false);
  // Per-model editable caps, keyed by model id. Initialized from the preview
  // or from manual model-id entries.
  const [caps, setCaps] = useState<Record<string, ModelCaps>>({});
  // Manual model ids (one per line) for endpoints that don't expose /models.
  const [manualIds, setManualIds] = useState("");
  // Baseline ModelInfo for each id so buildOverrides can detect changes.
  const [baselines, setBaselines] = useState<ModelInfo[]>([]);

  // When a non-empty preview lands, seed caps and advance to the models step.
  // Empty previews stay on the endpoint step so the user can fix the URL/key
  // or add without discovering — the spinner is cleared by the parent.
  useEffect(() => {
    if (previewModels && previewModels.length > 0) {
      const next: Record<string, ModelCaps> = {};
      for (const m of previewModels) {
        next[m.id] = {
          context_window: String(m.context_window), max_tokens: String(m.max_tokens),
          reasoning: m.reasoning, thinking_levels: (m.thinking_levels ?? []).join(", "),
          input: (m.input ?? (m.vision ? ["text", "image"] : ["text"])).join(", "),
          output: (m.output ?? ["text"]).join(", "),
          tool_call: m.tool_call ?? false, structured_output: m.structured_output ?? false,
        };
      }
      setCaps(next);
      setBaselines(previewModels);
      setStep("models");
    }
  }, [previewModels]);

  const set = (patch: Partial<CustomProviderDraft>) =>
    setDraft((d) => ({ ...d, ...patch }));

  const nameOk = draft.name.trim().length > 0;
  const urlOk = /^https?:\/\/.+/.test(draft.base_url.trim());
  const ctxOk = draft.contextWindow.trim() === "" || /^[0-9]+$/.test(draft.contextWindow.trim());
  const headersOk = draft.headersText
    .split(/\n+/)
    .filter((l) => l.trim() !== "")
    .every((l) => l.indexOf(":") > 0);
  const endpointValid = nameOk && urlOk && ctxOk && headersOk;

  const fieldErr = (ok: boolean, msg: string) =>
    touched && !ok ? <p className="mt-1 text-[11px] text-danger">{msg}</p> : null;

  const inputCls =
    "w-full rounded-sm border border-ink-700 bg-ink-950 px-3 py-2 text-[13px] text-ink-100 placeholder:text-ink-600 focus:border-accent focus:outline-none";

  const discover = () => {
    setTouched(true);
    if (!endpointValid) return;
    const headers = parseHeaders(draft.headersText);
    onDiscover(
      draft.base_url.trim(),
      draft.kind,
      draft.apiKey.trim() || undefined,
      Object.keys(headers).length ? headers : undefined,
    );
  };

  /** Seed caps from manual model-id lines and jump to the refine step. */
  const useManualModels = () => {
    const ids = manualIds
      .split(/[\n,]+/)
      .map((s) => s.trim())
      .filter(Boolean);
    if (ids.length === 0) return;
    const next: Record<string, ModelCaps> = {};
    const base: ModelInfo[] = [];
    for (const id of ids) {
      // Flat defaults for unknown ids (200k / 8k) — same as core discovery.
      const m: ModelInfo = {
        id, name: id, context_window: 200_000, max_tokens: 8_192,
        reasoning: false, thinking_levels: [], vision: false,
        input: ["text"], output: ["text"], tool_call: false, structured_output: false,
        provider: draft.name.trim() || "custom",
      };
      base.push(m);
      next[id] = {
        context_window: "200000", max_tokens: "8192", reasoning: false, thinking_levels: "",
        input: "text", output: "text", tool_call: false, structured_output: false,
      };
    }
    setBaselines(base);
    setCaps(next);
    setStep("models");
  };

  // Build the models_override payload: only include a model when the user
  // changed a field from its discovered/manual baseline. Unchanged models fall
  // through to the discovered/curated/flat-default caps.
  const buildOverrides = (): ModelOverride[] => {
    const out: ModelOverride[] = [];
    for (const m of baselines) {
      const c = caps[m.id];
      if (!c) continue;
      const ctx = parseInt(c.context_window.trim(), 10);
      const max = parseInt(c.max_tokens.trim(), 10);
      const split = (value: string) => value.split(/[;,\s]+/).map((s) => s.trim()).filter(Boolean);
      const levels = split(c.thinking_levels), input = split(c.input), output = split(c.output);
      const baselineInput = m.input ?? (m.vision ? ["text", "image"] : ["text"]);
      const baselineOutput = m.output ?? ["text"];
      const ctxChanged = Number.isFinite(ctx) && ctx !== m.context_window;
      const maxChanged = Number.isFinite(max) && max !== m.max_tokens;
      const reasonChanged = c.reasoning !== m.reasoning;
      const levelsChanged = levels.join(",") !== (m.thinking_levels ?? []).join(",");
      const inputChanged = input.join(",") !== baselineInput.join(",");
      const outputChanged = output.join(",") !== baselineOutput.join(",");
      const toolChanged = c.tool_call !== (m.tool_call ?? false);
      const structuredChanged = c.structured_output !== (m.structured_output ?? false);
      const alwaysWrite = !(previewModels && previewModels.some((p) => p.id === m.id));
      if (!alwaysWrite && !ctxChanged && !maxChanged && !reasonChanged && !levelsChanged && !inputChanged && !outputChanged && !toolChanged && !structuredChanged) continue;
      out.push({ id: m.id,
        context_window: (alwaysWrite || ctxChanged) && Number.isFinite(ctx) ? ctx : undefined,
        max_tokens: (alwaysWrite || maxChanged) && Number.isFinite(max) ? max : undefined,
        reasoning: alwaysWrite || reasonChanged ? c.reasoning : undefined,
        thinking_levels: alwaysWrite || levelsChanged ? levels : undefined,
        input: alwaysWrite || inputChanged ? input : undefined,
        output: alwaysWrite || outputChanged ? output : undefined,
        tool_call: alwaysWrite || toolChanged ? c.tool_call : undefined,
        structured_output: alwaysWrite || structuredChanged ? c.structured_output : undefined,
      });
    }
    return out;
  };

  const submit = () => {
    setTouched(true);
    if (!endpointValid) {
      setStep("endpoint");
      return;
    }
    onSubmit({ ...draft, modelsOverride: buildOverrides() });
    onClose();
  };

  return (
    <div className="modal-backdrop">
      <div
        ref={mergeRefs(closeRef, trapRef)}
        className="modal-sheet max-w-lg"
        role="dialog"
        aria-modal="true"
        aria-label="Add custom provider"
      >
        <div className="flex min-h-11 items-center justify-between border-b border-ink-800 px-5 py-3.5">
          <div className="flex items-center gap-2">
            {step === "models" && (
              <button
                onClick={() => setStep("endpoint")}
                className="focus-ring flex h-11 w-11 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:h-7 sm:w-7"
                aria-label="Back to endpoint"
              >
                <ArrowLeftIcon width={15} height={15} />
              </button>
            )}
            <ShieldIcon width={16} height={16} className="text-accent-soft" />
            <h2 className="text-[15px] font-semibold text-ink-100">
              {step === "models" ? "Refine model caps" : "Add custom provider"}
            </h2>
            <span className="ml-1 rounded-sm bg-ink-800 px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-wider text-ink-400">
              {step === "models" ? "2 / 2" : "1 / 2"}
            </span>
          </div>
          <button
            onClick={onClose}
            className="focus-ring flex h-11 w-11 items-center justify-center rounded-sm text-ink-400 transition-colors hover:bg-ink-800 hover:text-ink-100 sm:h-7 sm:w-7"
            aria-label="Close"
          >
            <XIcon width={16} height={16} />
          </button>
        </div>

        {step === "endpoint" && (
          <div className="min-h-0 flex-1 space-y-4 overflow-y-auto px-5 py-4">
            <div className="grid grid-cols-2 gap-3">
              <div>
                <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                  Name <span className="text-danger">*</span>
                </label>
                <input
                  autoFocus
                  value={draft.name}
                  onChange={(e) => set({ name: e.target.value })}
                  placeholder="my-provider"
                  className={inputCls + " font-mono"}
                />
                {fieldErr(nameOk, "A unique slug (e.g. my-provider).")}
              </div>
              <div>
                <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                  Wire protocol
                </label>
                <div className="flex overflow-hidden rounded-sm border border-ink-700">
                  {KINDS.map((k) => (
                    <button
                      key={k.id}
                      type="button"
                      onClick={() => set({ kind: k.id })}
                      title={k.hint}
                      className={`flex-1 px-2 py-2 text-[12px] font-medium transition-colors ${
                        draft.kind === k.id
                          ? "bg-ink-800 text-accent-soft"
                          : "bg-ink-950 text-ink-400 hover:bg-ink-850"
                      }`}
                    >
                      {k.label}
                    </button>
                  ))}
                </div>
                <p className="mt-1 text-[11px] text-ink-600">
                  {KINDS.find((k) => k.id === draft.kind)?.hint}
                </p>
              </div>
            </div>

            <div>
              <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                Base URL <span className="text-danger">*</span>
              </label>
              <input
                value={draft.base_url}
                onChange={(e) => set({ base_url: e.target.value })}
                placeholder="https://api.example.com/v1"
                className={inputCls + " font-mono"}
              />
              <p className="mt-1 text-[11px] text-ink-600">
                Include the version segment — paths are appended directly (e.g. /chat/completions).
              </p>
              {fieldErr(urlOk, "Must be an http(s) URL, including the version segment (e.g. /v1).")}
            </div>

            <div className="grid grid-cols-2 gap-3">
              <div>
                <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                  API key
                </label>
                <input
                  type="password"
                  autoComplete="off"
                  value={draft.apiKey}
                  onChange={(e) => set({ apiKey: e.target.value })}
                  placeholder="sk-…"
                  className={inputCls + " font-mono"}
                />
                <p className="mt-1 text-[11px] text-ink-600">
                  Stored in the 0600 user config. Wins over the env var.
                </p>
              </div>
              <div>
                <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                  …or env var name
                </label>
                <input
                  value={draft.apiKeyEnv}
                  onChange={(e) => set({ apiKeyEnv: e.target.value })}
                  placeholder="MY_PROVIDER_API_KEY"
                  className={inputCls + " font-mono"}
                />
                <p className="mt-1 text-[11px] text-ink-600">
                  Secret stays in your environment; read at request time.
                </p>
              </div>
            </div>

            <div>
              <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                Extra headers <span className="normal-case text-ink-600">(optional)</span>
              </label>
              <textarea
                rows={2}
                value={draft.headersText}
                onChange={(e) => set({ headersText: e.target.value })}
                placeholder={"HTTP-Referer: https://myapp.example\nX-Title: My App"}
                className={inputCls + " resize-y font-mono"}
              />
              <p className="mt-1 text-[11px] text-ink-600">One Key: value per line.</p>
              {fieldErr(headersOk, "Each line must be Key: value.")}
            </div>

            <div>
              <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                Context window <span className="normal-case text-ink-600">(optional, tokens)</span>
              </label>
              <input
                inputMode="numeric"
                value={draft.contextWindow}
                onChange={(e) => set({ contextWindow: e.target.value })}
                placeholder="e.g. 128000"
                className={inputCls + " font-mono"}
              />
              <p className="mt-1 text-[11px] text-ink-600">
                Force every discovered model to this window — useful for local servers (LM Studio)
                that return bare model ids.
              </p>
              {fieldErr(ctxOk, "Digits only, or leave blank.")}
            </div>

            <div>
              <label className="mb-1.5 block font-mono text-[10px] uppercase tracking-wider text-ink-500">
                Model ids <span className="normal-case text-ink-600">(optional, if /models is empty)</span>
              </label>
              <textarea
                rows={2}
                value={manualIds}
                onChange={(e) => setManualIds(e.target.value)}
                placeholder={"my-model-a\nmy-model-b"}
                className={inputCls + " resize-y font-mono"}
                disabled={discovering}
              />
              <p className="mt-1 text-[11px] text-ink-600">
                One id per line when the endpoint doesn&apos;t list models. Then use
                &quot;Use these models →&quot; or discover first.
              </p>
            </div>

            {discoverError && (
              <div className="rounded-sm border border-danger bg-ink-900 px-3 py-2 text-[12px] text-danger">
                {discoverError}
                <p className="mt-1 text-[11px] text-ink-400">
                  You can still add the provider without discovering, or enter model ids above.
                </p>
              </div>
            )}
          </div>
        )}

        {step === "models" && (
          <div className="min-h-0 flex-1 space-y-3 overflow-y-auto px-5 py-4">
            <p className="text-[12px] text-ink-400">
              {baselines.length} model{baselines.length === 1 ? "" : "s"} for{" "}
              <span className="font-mono text-ink-300">{draft.base_url}</span>. Refine any
              caps below — fields left at the baseline aren&apos;t written for discovered
              models (200k context / 8k output for unknown ids).
            </p>
            {baselines.map((m) => {
              const c = caps[m.id];
              if (!c) return null;
              return (
                <div
                  key={m.id}
                  className="rounded-sm border border-ink-800 bg-ink-900 px-3.5 py-3"
                >
                  <div className="flex items-center justify-between gap-2">
                    <div className="min-w-0">
                      <div className="truncate font-mono text-[13px] text-ink-100">{m.id}</div>
                      <div className="truncate text-[11px] text-ink-500">{m.name}</div>
                    </div>
                    <label className="flex shrink-0 cursor-pointer items-center gap-1.5 text-[11px] text-ink-400">
                      <input
                        type="checkbox"
                        checked={c.reasoning}
                        onChange={(e) =>
                          setCaps((s) => ({ ...s, [m.id]: { ...c, reasoning: e.target.checked } }))
                        }
                        className="h-3.5 w-3.5 accent-accent"
                      />
                      reasoning
                    </label>
                  </div>
                  <div className="mt-2.5 grid grid-cols-2 gap-2">
                    <div><label className="mb-1 block font-mono text-[10px] uppercase tracking-wider text-ink-500">Context</label><input inputMode="numeric" value={c.context_window} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, context_window: e.target.value } }))} className={inputCls + " px-2 py-1.5 font-mono text-[12px]"} /></div>
                    <div><label className="mb-1 block font-mono text-[10px] uppercase tracking-wider text-ink-500">Output tokens</label><input inputMode="numeric" value={c.max_tokens} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, max_tokens: e.target.value } }))} className={inputCls + " px-2 py-1.5 font-mono text-[12px]"} /></div>
                    <div><label className="mb-1 block font-mono text-[10px] uppercase tracking-wider text-ink-500">Input modalities</label><input value={c.input} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, input: e.target.value } }))} placeholder="text, image" className={inputCls + " px-2 py-1.5 font-mono text-[12px]"} /></div>
                    <div><label className="mb-1 block font-mono text-[10px] uppercase tracking-wider text-ink-500">Output modalities</label><input value={c.output} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, output: e.target.value } }))} placeholder="text" className={inputCls + " px-2 py-1.5 font-mono text-[12px]"} /></div>
                    <div><label className="mb-1 block font-mono text-[10px] uppercase tracking-wider text-ink-500">Reasoning levels</label><input value={c.thinking_levels} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, thinking_levels: e.target.value } }))} placeholder="low, medium, high" className={inputCls + " px-2 py-1.5 font-mono text-[12px]"} /></div>
                    <div className="flex items-end gap-3 pb-1 text-[11px] text-ink-400">
                      <label className="flex items-center gap-1.5"><input type="checkbox" checked={c.tool_call} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, tool_call: e.target.checked } }))} className="accent-accent" /> tool calls</label>
                      <label className="flex items-center gap-1.5"><input type="checkbox" checked={c.structured_output} onChange={(e) => setCaps((s) => ({ ...s, [m.id]: { ...c, structured_output: e.target.checked } }))} className="accent-accent" /> JSON output</label>
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}

        <div className="flex min-h-11 items-center justify-between gap-3 border-t border-ink-800 px-5 py-3.5">
          <p className="text-[11px] text-ink-600">Saved to ~/.config/catalyst-code/config.json</p>
          {step === "endpoint" ? (
            <div className="flex flex-wrap items-center justify-end gap-2">
              <button
                type="button"
                onClick={submit}
                disabled={discovering}
                className="rounded-sm border border-ink-700 px-2.5 py-1 text-[11px] text-ink-300 hover:bg-ink-800 disabled:cursor-not-allowed disabled:border-ink-800 disabled:bg-ink-900 disabled:text-ink-500"
              >
                Add without discovering
              </button>
              {manualIds.trim() !== "" && (
                <button
                  type="button"
                  onClick={useManualModels}
                  disabled={!endpointValid || discovering}
                  className="rounded-sm border border-ink-700 px-2.5 py-1 text-[11px] text-ink-200 hover:bg-ink-800 disabled:cursor-not-allowed disabled:border-ink-800 disabled:bg-ink-900 disabled:text-ink-500"
                >
                  Use these models →
                </button>
              )}
              {discovering ? (
                <button
                  type="button"
                  onClick={() => onCancelDiscover?.()}
                  className="rounded-sm border border-ink-700 px-2.5 py-1 text-[11px] text-ink-300 hover:bg-ink-800"
                >
                  Cancel discover
                </button>
              ) : null}
              <button
                type="button"
                onClick={discover}
                disabled={!endpointValid || discovering}
                className="rounded-sm bg-accent px-2.5 py-1 text-[11px] font-medium text-white hover:bg-accent-soft disabled:cursor-not-allowed disabled:border-ink-800 disabled:bg-ink-900 disabled:text-ink-500"
              >
                {discovering ? "Discovering…" : "Discover models →"}
              </button>
            </div>
          ) : (
            <button
              type="button"
              onClick={submit}
              className="rounded-sm bg-accent px-2.5 py-1 text-[11px] font-medium text-white hover:bg-accent-soft"
            >
              Add provider
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
