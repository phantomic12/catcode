"use client";

import { useEffect, useRef, useState } from "react";
import { EyeIcon, PlusIcon, SendIcon } from "@/components/icons";
import { useIdeContext } from "@/lib/ide-context";

export function PreviewPanel({ url, onUrl }: { url: string; onUrl: (url: string) => void }) {
  const { attachToChat } = useIdeContext();
  const frameRef = useRef<HTMLIFrameElement>(null);
  const [draft, setDraft] = useState(url);
  const [picking, setPicking] = useState(false);
  const [picked, setPicked] = useState<string | null>(null);

  useEffect(() => setDraft(url), [url]);
  useEffect(() => {
    if (!picking) return;
    const frame = frameRef.current;
    let cleanup = () => {};
    try {
      const doc = frame?.contentDocument;
      if (!doc) throw new Error("cross-origin");
      const onOver = (e: Event) => { (e.target as HTMLElement).style.outline = "2px solid #d68e58"; };
      const onClick = (e: Event) => {
        e.preventDefault(); e.stopPropagation();
        const el = e.target as HTMLElement;
        const selector = el.id ? `#${el.id}` : el.tagName.toLowerCase() + (el.className && typeof el.className === "string" ? `.${el.className.trim().split(/\\s+/).slice(0, 2).join(".")}` : "");
        setPicked(selector);
        setPicking(false);
      };
      doc.addEventListener("mouseover", onOver, true); doc.addEventListener("click", onClick, true);
      cleanup = () => { doc.removeEventListener("mouseover", onOver, true); doc.removeEventListener("click", onClick, true); };
    } catch { setPicking(false); }
    return cleanup;
  }, [picking]);

  const sendPick = () => { if (picked) attachToChat({ text: `Please inspect this preview element: ${picked}\nPreview: ${url}` }); };
  return <section className="flex h-full min-h-0 flex-col bg-ink-925" aria-label="Preview">
    <div className="flex shrink-0 items-center gap-2 border-b border-ink-800 bg-ink-900 px-2 py-1.5">
      <EyeIcon width={14} height={14} className="text-accent-soft" /><span className="font-mono text-[10px] uppercase tracking-[0.14em] text-ink-400">Preview</span>
      <form className="flex min-w-0 flex-1 gap-1" onSubmit={(e) => { e.preventDefault(); onUrl(draft.trim()); }}>
        <input value={draft} onChange={(e) => setDraft(e.target.value)} placeholder="http://localhost:3000" aria-label="Preview URL" className="min-w-0 flex-1 border border-ink-700 bg-ink-950 px-2 py-1 font-mono text-[11px] text-ink-200 outline-none focus:border-accent" />
        <button className="rounded-sm bg-accent px-2 font-mono text-[10px] text-white" type="submit">Load</button>
      </form>
      <button type="button" onClick={() => setPicking((v) => !v)} aria-pressed={picking} className={`rounded-sm border px-2 py-1 font-mono text-[10px] ${picking ? "border-accent text-accent-soft" : "border-ink-700 text-ink-400 hover:text-ink-100"}`}>{picking ? "Picking…" : "Pick element"}</button>
    </div>
    {picked && <div className="flex items-center gap-2 border-b border-accent/30 bg-ink-900 px-2 py-1 font-mono text-[10px] text-accent-soft"><span className="truncate">{picked}</span><button type="button" onClick={sendPick} className="ml-auto flex shrink-0 items-center gap-1 rounded-sm bg-accent px-2 py-1 text-white"><SendIcon width={11} height={11} /> Send to chat</button></div>}
    <div className="relative min-h-0 flex-1 bg-white"><iframe ref={frameRef} title="Development server preview" src={url || "about:blank"} className="h-full w-full border-0" sandbox="allow-forms allow-modals allow-popups allow-presentation allow-same-origin allow-scripts" />{picking && <div className="pointer-events-none absolute inset-x-0 top-0 bg-accent px-3 py-1 text-center font-mono text-[10px] text-white">Click an element in the preview to send it to chat</div>}</div>
  </section>;
}
