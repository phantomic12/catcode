# Hub — the project-centric chat frontend

The web UI is a deliberately minimal frontend for the harness, served by the
custom Next server (`web/src/server/server.ts`). It answers one workflow: jump
between projects, chat with the agent across multiple live sessions, and keep
git in view — including when you switch devices mid-turn.

- **URL**: `http://localhost:49283/` (installed service) or `:3000/` (dev).
  `/hub` is kept as a permanent alias of the same shell.
- Terminal panes have been removed; hub is a **chat + git** surface driven by
  a multi-session SSE bridge to `catcode-core`.

## What it does

| Feature | How |
| --- | --- |
| Project switching | A tab bar at the top; one tab per open project. `Ctrl/Cmd+1..9` jumps between tabs. Closing a tab leaves the chat session on disk (and its live core if mid-turn). |
| Add projects by browsing | The `+` button (or the empty state) opens the ProjectSwitcher: Recent / Browse / Create / Clone. Browse walks the real filesystem via `GET /api/browse`; opening a folder registers it via `POST /api/hub/projects`. |
| Git panel | Collapsible, resizable **left** sidebar running the full GitPanel (changes, history, branches, stashes, remotes — every `POST /api/git` action) bound to the ACTIVE project. Diff/patch/file viewing opens in a lightweight modal. |
| Preview panel | Collapsible, resizable **right** sidebar with an iframe of a local/dev URL and element-pick → attach-to-chat. |
| Terminal panel | Collapsible, resizable **bottom** multi-tab PTY panel (Ghostty + zigpty WS). |
| Multi-session chat | Each chat is a `catcode-core` process bound to a session `.jsonl` file. The bridge keeps many sessions alive concurrently across projects. New / switch / rename / delete from the in-chat sidebar. |
| Live feed | `GET /api/stream?session=…&workspace=…` hydrates from a server snapshot then streams raw core events (SSE). Closing the browser never kills a mid-turn session; reconnecting reattaches. |
| Cross-device | Account layout (`GET/PUT /api/hub/layout` → `~/.config/catalyst-code/hub-layout.json`) stores open project tabs + the last-viewed session file per project. Any signed-in device reopens the same chats and reattaches the live cores. |
| Cross-session status | Background sessions broadcast `session_status` (streaming / needs attention) so the sidebar and notification center surface blocked or finished work in other projects. |
| Account menu | Settings + **Sign out**. Signing out never tears down live cores. |

## Mobile responsiveness

The shell is built off `useIsMobile()` (below Tailwind's `lg` breakpoint):

- **Git panel** becomes a left-hand overlay drawer with a backdrop; drawer state
  is transient on mobile so a small screen never boots with chat covered.
- Tab close buttons are always visible on touch; project names truncate earlier.
- Verified headless at 390×844 for the previous terminal hub; chat hub reuses
  the same chrome breakpoints.

## Persistence guarantee (leave / sign out / close the page / other device)

Chat sessions are owned by the SERVER's `HarnessBridge` pool — not by the
browser connection:

- **Close the tab / navigate away**: SSE subscribers detach; live cores keep
  running (idle GC only after 2h with no viewers and no mid-turn work).
- **Sign out**: clears the auth session only. Sign back in (any device) restores
  tabs + last-viewed session paths from the account store and reopens the SSE
  stream on those still-running cores.
- **Another device**: same layout + same session files → same live feed. No
  "restart the chat" step.
- **The ONLY paths that end a core**: idle GC, explicit session delete, or a
  web-server restart (on-disk JSONL still resumes on next open).

## Architecture

```
web/src/app/page.tsx                auth-gated primary entry
web/src/app/hub/page.tsx            /hub alias (same HubShell)
web/src/components/hub/
  hub-shell.tsx                     project tabs + git(left) + chat + terminal(bottom) + preview(right)
  git-sidebar.tsx                   GitPanel + minimal IdeContext shim + viewer modal
  preview-panel.tsx                 iframe preview + element pick
  hub-state.ts                      sanitize + fetch/push account layout (v3 panel chrome)
web/src/components/chat.tsx         chat shell (sidebar, header, messages, composer)
web/src/lib/use-agent.ts            EventSource + command POST + optimistic UI
web/src/lib/reducer.ts              core event → AgentState
web/src/server/core-bridge.ts       pool of LiveSessions (one core per session file)
web/src/server/live-session.ts      CoreProcess + per-session state + SSE fanout
web/src/server/hub-layout-store.ts  account-scoped layout file (hub-layout.json)
web/src/app/api/stream/route.ts     SSE of raw core events for one session
web/src/app/api/command/route.ts    POST core / bridge commands
web/src/app/api/hub/layout/route.ts GET/PUT account layout
web/src/app/api/hub/projects/route.ts  add/remove/list registry entries
```

Shared pieces kept from the terminal hub: better-auth session gating,
`/api/browse`, `/api/git`, `/api/file`, `ProjectSwitcher`, `GitPanel`, and the
optional zigpty terminal WS (unused by the chat shell, still present for
compat).

### Workspace allowlist

`POST /api/hub/projects {action:"add", path}` validates the path is an
existing directory and registers it in the shared projects store
(`~/.config/catalyst-code/projects.json`). That registration is the capability
grant for workspace routes (`workspace.ts`).

### Session files

On-disk sessions live under `~/.config/catalyst-code/sessions/<project-hash>/`
(same layout as the TUI). The bridge spawns `catcode-core` with `--session`
pointing at those files so web and TUI share history.

## Tests

- `web/src/lib/reducer.test.ts` — agent state machine + SDK event coverage.
- `web/src/lib/hub-layout-store.test.ts` — v3 panel layout sanitize + round-trip.
- `web/src/lib/hub-layout.test.ts` — pure split-tree helpers (legacy, still pure).
- `cd web && npm run typecheck && bun test`.
