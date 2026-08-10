// Small display/format helpers (no deps).

export function formatTokens(n: number | undefined | null): string {
  if (n == null) return "—";
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

export function formatMs(ms: number | undefined | null): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  const rem = Math.round(s % 60);
  return `${m}m ${rem}s`;
}

export function formatTps(tps: number | undefined | null): string {
  if (tps == null) return "—";
  return `${tps.toFixed(1)} t/s`;
}

export function relativeTime(ts: number): string {
  const diff = Date.now() - ts;
  if (diff < 60_000) return "just now";
  const mins = Math.floor(diff / 60_000);
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  const days = Math.floor(hrs / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(ts).toLocaleDateString();
}

export function shortPath(p: string | null | undefined): string {
  if (!p) return "—";
  const parts = p.split("/");
  if (parts.length <= 2) return p;
  return ".../" + parts.slice(-2).join("/");
}

export function basename(p: string | null | undefined): string {
  if (!p) return "";
  const parts = p.replace(/\\/g, "/").split("/");
  return parts[parts.length - 1] || p;
}

/** Truncate a long string for one-line previews. */
export function truncate(s: string, n = 80): string {
  const flat = s.replace(/\s+/g, " ").trim();
  return flat.length > n ? flat.slice(0, n - 1) + "…" : flat;
}

/** Flatten internal whitespace so a preview stays one physical line. */
function collapseWS(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

/** True when a line is only a directory change (no chained operators). */
function isCdOnly(line: string): boolean {
  const fields = line.trim().split(/\s+/).filter(Boolean);
  if (fields.length === 0 || fields[0] !== "cd") return false;
  if (/[|&;]/.test(line) || line.includes("&&")) return false;
  return true;
}

/**
 * Collapse a multi-line bash script to a one-line activity preview.
 * Mirrors tui/tool_blocks.go summarizeBashCommand: first meaningful line,
 * pair bare `cd` with the next work line, append total line count.
 */
export function summarizeBashCommand(cmd: string): string {
  const trimmed = cmd.trim();
  if (!trimmed) return "";
  const rawLines = trimmed.split("\n");
  const totalLines = rawLines.length;
  const meaningful = rawLines
    .map((l) => l.trim())
    .filter((t) => t !== "" && !t.startsWith("#"))
    .map(collapseWS);
  if (meaningful.length === 0) return collapseWS(trimmed);
  let lead = meaningful[0];
  if (totalLines === 1) return lead;
  if (isCdOnly(lead) && meaningful.length > 1) {
    lead = `${lead} · ${meaningful[1]}`;
  }
  return `${lead} · ${totalLines} lines`;
}

/** One-line preview for a collapsed tool-call header. */
export function toolArgPreview(
  name: string,
  args: Record<string, unknown>,
  argString?: string,
  max = 72,
): string {
  if (name === "bash") {
    const cmd = typeof args.command === "string" ? args.command : "";
    const summary = summarizeBashCommand(cmd);
    if (summary) return truncate(summary, max);
  }
  if (name === "read_file" || name === "edit" || name === "write_file" || name === "patch") {
    if (typeof args.path === "string" && args.path) return truncate(args.path, max);
  }
  if (name === "grep" || name === "glob") {
    if (typeof args.pattern === "string" && args.pattern) return truncate(args.pattern, max);
  }
  if (name === "fetch" && typeof args.url === "string") return truncate(args.url, max);
  return truncate(argString || JSON.stringify(args), max);
}

// Mirrors the core's ToolKind::Destructive classification (core/src/tools.rs
// classify()). Read-only tools (read_file, grep, glob, …) are intentionally
// absent — only tools that write/execute get the destructive badge + amber
// approval styling.
const DANGEROUS_TOOLS = new Set([
  // Keep in sync with tools::classify() — everything NOT ReadOnly.
  "bash",
  "write_file",
  "edit",
  "patch",
  "delete",
  "rename",
  "mkdir",
  "bulk",
  "bulk_write",
  "bulk_edit",
  "todo_write",
  "spawn",
  "subagent",
  "fetch",
  "git_add",
  "git_commit",
  "git_push",
  "git_pull",
  "git_branch",
  "test_env",
]);

export function isDangerousTool(name: string): boolean {
  return DANGEROUS_TOOLS.has(name);
}

const TOOL_ICONS: Record<string, string> = {
  read_file: "📄",
  list_dir: "📁",
  grep: "🔍",
  glob: "🗂",
  bulk_read: "📚",
  write_file: "✍️",
  edit: "✎",
  patch: "✎",
  bulk_write: "📝",
  bulk_edit: "📝",
  bash: "⌘",
  diagnostics: "✦",
  todo_write: "☑",
  todo_read: "☑",
  finish: "✓",
  spawn: "↳",
  subagent: "↳",
  contact_supervisor: "💬",
  intercom: "💬",
  memory: "🧠",
  fetch: "🌐",
  web_search: "🔎",
  git_status: "⎇",
  git_diff: "⎇",
  git_log: "⎇",
  git_show: "⎇",
  git_add: "⎇",
  git_commit: "⎇",
  git_push: "⎇",
  git_pull: "⎇",
  git_branch: "⎇",
  delete: "🗑",
  rename: "↔",
  mkdir: "📁",
  ask: "❓",
  load_tools: "🧰",
  workspace_activity: "📡",
  goal_write_plan: "📋",
  test_env: "🖥",
  bulk: "📦",
};

export function toolIcon(name: string): string {
  return TOOL_ICONS[name] ?? "🔧";
}

/** Pretty-print a tool's args JSON, capped to keep the card compact. */
export function prettyArgs(args: Record<string, unknown>): string {
  try {
    const json = JSON.stringify(args, null, 2);
    return json.length > 4000 ? json.slice(0, 4000) + "\n…(truncated)" : json;
  } catch {
    return String(args);
  }
}
