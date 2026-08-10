// Hub layout state helpers — pure sanitize + client fetch/push against the
// account-scoped server store (`/api/hub/layout`).
//
// Chat sessions live server-side (one catcode-core per session file via the
// HarnessBridge). The account layout only remembers which projects are open,
// which session was last viewed per project, and panel chrome (git left /
// preview right / terminal bottom) — so any signed-in device reopens the same
// chats and reattaches the live feed.
// localStorage is only a same-device cache / one-shot migration source.

/** Legacy same-device cache key (pre multi-device sync + pre chat-hub). */
export const HUB_STORAGE_KEY = "catcode:hub:v1";

/** Current account-layout schema (chat, terminal and preview chrome). */
export const HUB_LAYOUT_VERSION = 3 as const;

export interface HubTerminalSession { id: string; title: string; cwd: string; alive: boolean; exitCode: number | null; }
export interface HubPersistState {
  version: typeof HUB_LAYOUT_VERSION;
  tabPaths: string[];
  names: Record<string, string>;
  active: string | null;
  sessions: Record<string, string>;
  /** Git sidebar on the LEFT. */
  gitOpen: boolean;
  gitWidth: number;
  /** Terminal panel along the BOTTOM. */
  terminalOpen: boolean;
  terminalHeight: number;
  /** Preview panel on the RIGHT. */
  previewOpen: boolean;
  previewWidth: number;
  terminalSessions: Record<string, HubTerminalSession[]>;
  activeTerminal: Record<string, string | null>;
  previewUrls: Record<string, string>;
}

export function defaultHubState(): HubPersistState {
  return {
    version: HUB_LAYOUT_VERSION,
    tabPaths: [],
    names: {},
    active: null,
    sessions: {},
    gitOpen: true,
    gitWidth: 280,
    terminalOpen: false,
    terminalHeight: 260,
    previewOpen: false,
    previewWidth: 420,
    terminalSessions: {},
    activeTerminal: {},
    previewUrls: {},
  };
}

/** Basename tolerant of both POSIX and Windows separators. */
export function pathBasename(abs: string): string {
  return abs.split(/[\\/]/).filter(Boolean).pop() ?? abs;
}

/** Sanitize untrusted layout JSON into a HubPersistState (v1/v2 → v3). */
export function sanitizeHubState(raw: unknown): HubPersistState {
  const base = defaultHubState();
  if (!raw || typeof raw !== "object") return base;
  const parsed = raw as Partial<HubPersistState> & {
    /** v1 terminal hub fields — ignored but tolerated for migration. */
    layouts?: unknown;
    focused?: unknown;
  };

  const tabPaths = Array.isArray(parsed.tabPaths)
    ? parsed.tabPaths.filter((p): p is string => typeof p === "string" && p.length > 0)
    : [];

  const names: Record<string, string> = {};
  for (const [k, v] of Object.entries(parsed.names ?? {})) {
    if (typeof k === "string" && typeof v === "string" && v) names[k] = v;
  }
  const sessions: Record<string, string> = {};
  for (const [k, v] of Object.entries(parsed.sessions ?? {})) if (typeof v === "string" && v.endsWith(".jsonl")) sessions[k] = v;
  const terminalSessions: Record<string, HubTerminalSession[]> = {};
  for (const [workspace, value] of Object.entries(parsed.terminalSessions ?? {})) {
    if (Array.isArray(value)) terminalSessions[workspace] = value.filter((s): s is HubTerminalSession => !!s && typeof s === "object" && typeof (s as HubTerminalSession).id === "string" && typeof (s as HubTerminalSession).title === "string" && typeof (s as HubTerminalSession).cwd === "string" && typeof (s as HubTerminalSession).alive === "boolean" && ((s as HubTerminalSession).exitCode === null || typeof (s as HubTerminalSession).exitCode === "number")).slice(0, 16);
  }
  const activeTerminal: Record<string, string | null> = {};
  for (const [k, v] of Object.entries(parsed.activeTerminal ?? {})) if (v === null || typeof v === "string") activeTerminal[k] = v;
  const previewUrls: Record<string, string> = {};
  for (const [k, v] of Object.entries(parsed.previewUrls ?? {})) if (typeof v === "string" && v.length <= 2048) previewUrls[k] = v;

  const active =
    typeof parsed.active === "string" && tabPaths.includes(parsed.active)
      ? parsed.active
      : (tabPaths[0] ?? null);

  return {
    version: HUB_LAYOUT_VERSION,
    tabPaths,
    names,
    active,
    sessions,
    gitOpen: typeof parsed.gitOpen === "boolean" ? parsed.gitOpen : true,
    gitWidth:
      typeof parsed.gitWidth === "number" && Number.isFinite(parsed.gitWidth)
        ? Math.min(480, Math.max(200, parsed.gitWidth))
        : 280,
    terminalOpen: typeof parsed.terminalOpen === "boolean" ? parsed.terminalOpen : false,
    terminalHeight:
      typeof parsed.terminalHeight === "number" && Number.isFinite(parsed.terminalHeight)
        ? Math.min(560, Math.max(140, parsed.terminalHeight))
        : 260,
    previewOpen: typeof parsed.previewOpen === "boolean" ? parsed.previewOpen : false,
    previewWidth:
      typeof parsed.previewWidth === "number" && Number.isFinite(parsed.previewWidth)
        ? Math.min(900, Math.max(280, parsed.previewWidth))
        : 420,
    terminalSessions,
    activeTerminal,
    previewUrls,
  };
}

/** Read the legacy localStorage cache (may be empty / corrupt). */
export function readLocalHubCache(): HubPersistState | null {
  try {
    const raw = window.localStorage.getItem(HUB_STORAGE_KEY);
    if (!raw) return null;
    return sanitizeHubState(JSON.parse(raw));
  } catch {
    return null;
  }
}

/** Best-effort same-device cache so a hard refresh paints faster. */
export function writeLocalHubCache(state: HubPersistState): void {
  try {
    window.localStorage.setItem(HUB_STORAGE_KEY, JSON.stringify(state));
  } catch {
    /* storage full / unavailable — server is source of truth */
  }
}

export interface HubLayoutFetchResult {
  layout: HubPersistState;
  updatedAt: number;
  /** True when we pushed a localStorage migration to the server. */
  migrated: boolean;
}

/**
 * Load the account layout from the server. If the server has never stored a
 * layout but this browser still has the legacy localStorage key, migrate it
 * once so the first multi-device session picks up existing tabs.
 */
export async function fetchHubLayout(): Promise<HubLayoutFetchResult> {
  const res = await fetch("/api/hub/layout", { cache: "no-store" });
  if (!res.ok) {
    const cached = readLocalHubCache();
    return {
      layout: cached ?? defaultHubState(),
      updatedAt: 0,
      migrated: false,
    };
  }
  const data = (await res.json()) as {
    layout?: unknown;
    updatedAt?: number;
  };
  const serverLayout = sanitizeHubState(data.layout);
  const updatedAt = typeof data.updatedAt === "number" ? data.updatedAt : 0;

  const isEmpty =
    serverLayout.tabPaths.length === 0 &&
    Object.keys(serverLayout.sessions).length === 0 &&
    updatedAt === 0;
  if (isEmpty) {
    const local = readLocalHubCache();
    if (local && (local.tabPaths.length > 0 || Object.keys(local.sessions).length > 0)) {
      const pushed = await pushHubLayout(local);
      return { layout: pushed.layout, updatedAt: pushed.updatedAt, migrated: true };
    }
  }

  writeLocalHubCache(serverLayout);
  return { layout: serverLayout, updatedAt, migrated: false };
}

export interface HubLayoutPushResult {
  layout: HubPersistState;
  updatedAt: number;
}

/** Persist the layout to the account store (and refresh the local cache). */
export async function pushHubLayout(state: HubPersistState): Promise<HubLayoutPushResult> {
  writeLocalHubCache(state);
  const res = await fetch("/api/hub/layout", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(state),
  });
  if (!res.ok) {
    return { layout: state, updatedAt: 0 };
  }
  const data = (await res.json().catch(() => ({}))) as {
    layout?: unknown;
    updatedAt?: number;
  };
  const layout = data.layout ? sanitizeHubState(data.layout) : state;
  writeLocalHubCache(layout);
  return {
    layout,
    updatedAt: typeof data.updatedAt === "number" ? data.updatedAt : 0,
  };
}
