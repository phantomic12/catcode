// Server-side hub layout store — account-scoped persistence for project tabs,
// last-viewed chat sessions, and panel chrome (git left / preview right /
// terminal bottom sizes + visibility).
//
// Chat sessions themselves live as on-disk JSONL + live catcode-core processes
// in the HarnessBridge. This store only remembers which projects are open and
// which session each project last viewed, so every signed-in device reopens
// the same chats and reattaches the live SSE feed.
//
// Single-account install: one file under the shared config dir. The owning
// userId is recorded so a future multi-user deploy can partition cleanly.

import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const HUB_LAYOUT_VERSION = 3 as const;

export interface HubTerminalSession {
  id: string;
  title: string;
  cwd: string;
  alive: boolean;
  exitCode: number | null;
}

export interface HubPersistState {
  version: typeof HUB_LAYOUT_VERSION;
  /** Open project tabs, in order (absolute workspace paths). */
  tabPaths: string[];
  /** Display name per project path (basename at add time). */
  names: Record<string, string>;
  /** Active tab path (must be in tabPaths, else null). */
  active: string | null;
  /** Last-viewed chat session file (.jsonl) per project path. */
  sessions: Record<string, string>;
  /** Git sidebar (LEFT) visibility + width. */
  gitOpen: boolean;
  gitWidth: number;
  /** Terminal panel (BOTTOM) visibility + height. */
  terminalOpen: boolean;
  terminalHeight: number;
  /** Preview panel (RIGHT) visibility + width. */
  previewOpen: boolean;
  previewWidth: number;
  /** Per-project terminal session chrome (ids only; PTYs live in the server). */
  terminalSessions: Record<string, HubTerminalSession[]>;
  activeTerminal: Record<string, string | null>;
  /** Per-project preview URL. */
  previewUrls: Record<string, string>;
}

interface HubLayoutFile {
  version: 1;
  /** better-auth user id that last wrote this layout. */
  userId: string;
  /** ms epoch — multi-device last-write-wins. */
  updatedAt: number;
  state: HubPersistState;
}

function configDir(): string {
  // Prefer HOME so tests (and rare overrides) can redirect the store without
  // monkey-patching os.homedir(). Fall back to the real home directory.
  const home = process.env.HOME || process.env.USERPROFILE || homedir() || ".";
  return join(home, ".config", "catalyst-code");
}

function layoutFile(): string {
  if (process.env.CATCODE_HUB_LAYOUT_PATH) {
    return process.env.CATCODE_HUB_LAYOUT_PATH;
  }
  return join(configDir(), "hub-layout.json");
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

/** Sanitize untrusted JSON into a HubPersistState (v1/v2 → v3 panel chrome). */
export function sanitizeHubState(raw: unknown): HubPersistState {
  const base = defaultHubState();
  if (!raw || typeof raw !== "object") return base;
  const parsed = raw as Partial<HubPersistState> & {
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
  for (const [k, v] of Object.entries(parsed.sessions ?? {})) {
    if (typeof k === "string" && typeof v === "string" && v.endsWith(".jsonl")) {
      sessions[k] = v;
    }
  }

  const terminalSessions: Record<string, HubTerminalSession[]> = {};
  for (const [workspace, value] of Object.entries(parsed.terminalSessions ?? {})) {
    if (!Array.isArray(value)) continue;
    terminalSessions[workspace] = value
      .filter((s): s is HubTerminalSession =>
        !!s &&
        typeof s === "object" &&
        typeof (s as HubTerminalSession).id === "string" &&
        typeof (s as HubTerminalSession).title === "string" &&
        typeof (s as HubTerminalSession).cwd === "string" &&
        typeof (s as HubTerminalSession).alive === "boolean" &&
        ((s as HubTerminalSession).exitCode === null ||
          typeof (s as HubTerminalSession).exitCode === "number"),
      )
      .slice(0, 16);
  }

  const activeTerminal: Record<string, string | null> = {};
  for (const [k, v] of Object.entries(parsed.activeTerminal ?? {})) {
    if (v === null || typeof v === "string") activeTerminal[k] = v;
  }

  const previewUrls: Record<string, string> = {};
  for (const [k, v] of Object.entries(parsed.previewUrls ?? {})) {
    if (typeof v === "string" && v.length <= 2048) previewUrls[k] = v;
  }

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

export interface LoadedHubLayout {
  state: HubPersistState;
  updatedAt: number;
  userId: string;
}

/** Load the account hub layout, or null when nothing has been saved yet. */
export function loadHubLayout(): LoadedHubLayout | null {
  const p = layoutFile();
  try {
    if (!existsSync(p)) return null;
    const raw = JSON.parse(readFileSync(p, "utf8")) as Partial<HubLayoutFile>;
    if (!raw || typeof raw !== "object" || !raw.state) return null;
    const state = sanitizeHubState(raw.state);
    return {
      state,
      updatedAt: typeof raw.updatedAt === "number" ? raw.updatedAt : 0,
      userId: typeof raw.userId === "string" ? raw.userId : "",
    };
  } catch {
    return null;
  }
}

/** Persist the layout for `userId`. Returns the written record. */
export function saveHubLayout(userId: string, raw: unknown): LoadedHubLayout {
  const state = sanitizeHubState(raw);
  const record: HubLayoutFile = {
    version: 1,
    userId,
    updatedAt: Date.now(),
    state,
  };
  const dir = configDir();
  if (!existsSync(dir)) mkdirSync(dir, { recursive: true });
  const target = layoutFile();
  const tmp = `${target}.${process.pid}.${Date.now()}.tmp`;
  // Atomic replace: write temp then rename so a crash mid-write cannot leave a
  // truncated hub-layout.json that loadHubLayout would treat as empty.
  writeFileSync(tmp, JSON.stringify(record, null, 2), "utf8");
  renameSync(tmp, target);
  return { state, updatedAt: record.updatedAt, userId };
}
