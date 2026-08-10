# Built-in Tool Reference

This document describes every built-in tool the Catalyst Code agent can call.
Tools are partitioned into two sets:

- **Core tools** — always available to the agent.
- **Deferred tools** — not sent in the initial schema; enabled via the
  `load_tools` tool on demand.

The tool loop runs in the **core** (Rust engine). Classification and approval
happen before execution; concurrency follows the wave model.

---

## Tool Classification

Every tool is classified as one of:

| Class | Gate | Tools |
|-------|------|-------|
| `ReadOnly` | Never gated (executes immediately) | `read_file`, `list_dir`, `grep`, `glob`, `bulk_read`, `todo_read`, `diagnostics`, `finish`, `contact_supervisor`, `intercom`, `git_status`, `git_diff`, `git_log`, `git_show`, `memory`, `knowledge`, `load_tools`, `ask`, `web_search`, `workspace_activity`, `goal_write_plan`, plus browser read-only tools |
| `Destructive` | Gated under `Approval::Destructive` (default) — prompts user before executing | Everything else (writes, edits, bash, subagent, …) |

Classification is determined by the `classify()` (/core/src/tools.rs) function.

### Browser Read-Only Tools

The following browser sub-tools are `ReadOnly`:

- `browser_list_sessions`
- `browser_snapshot`
- `browser_find`
- `browser_screenshot`

All other browser tools are `Destructive`.

### Tool Ownership

- `contact_supervisor` and `intercom` are only available **inside subagents**
  (not in the main agent loop).
- `goal_write_plan` is only available during `GoalPhase::Planning` (deferred,
  not loadable via `load_tools`).
- `ask` is only available in the main orchestrator loop (requires HITL reply).

---

## Approval Modes

The `--approval` / `CATALYST_CODE_APPROVAL` config controls which tools gate:

| Mode | Behavior |
|------|----------|
| `never` / `auto` | No prompts; all tools execute immediately. Path confinement is also disabled — the model is fully trusted. |
| `destructive` (default) | Prompts user only for `Destructive`-classified tools. |
| `always` / `all` | Prompts on every tool call. |

---

## Core Tools

Always available (defined by `is_core_tool()` (/core/src/tools.rs)).

| Tool | Description | Class |
|------|-------------|-------|
| `read_file` | Read a file (workspace-relative). Large files auto-window; pass `offset`/`limit` to page. Returns plain content or line-numbered. | ReadOnly |
| `edit` | Search/replace edits on a file. Each search must match exactly and be unique (or set `replace_all`). Empty `replace` deletes. `normalize_whitespace` tolerates indent drift. All edits apply atomically. Returns a unified diff. | Destructive |
| `write_file` | Write content to a file (creates parents, overwrites). Uses atomic write (temp + rename) to avoid cross-process corruption. Returns a unified diff of the change. | Destructive |
| `delete` | Delete a file or empty directory within the workspace. | Destructive |
| `rename` | Rename or move a file/directory within the workspace (creates parent dirs of the destination). | Destructive |
| `mkdir` | Create a directory (and parents) at a workspace-relative path. | Destructive |
| `list_dir` | List entries in a directory. Directories suffixed with `/`. | ReadOnly |
| `grep` | Search file contents with regex. Supports `glob`/`type` scoping, `case_insensitive`, `output_mode` (content / files_with_matches / count), pagination (`head_limit`/`offset`), and context lines (`context`). Uses `rg` when available, pure-Rust fallback. | ReadOnly |
| `glob` | Find files by glob pattern (e.g. `**/*.rs`). Returns relative paths, capped at 200. | ReadOnly |
| `bash` | Run a shell command (bash on Unix, PowerShell on Windows). Stdout+stderr truncated to 32 KiB. Default timeout 30s (configurable). Path-confined to workspace. Uses OS-selected shell for matching syntax. | Destructive |
| `todo_write` | Write the full task list (plan). Each todo has `{subject, status, content?}`. `status` is pending / in_progress / completed. Replaces the whole list. | Destructive |
| `todo_read` | Read the current task list. Returns the JSON plan (or `[]` if empty). | ReadOnly |
| `finish` | Signal that the task is complete; exits the agentic loop cleanly. | ReadOnly |
| `memory` | Persist/list/get/forget durable memories. Actions: `save`, `append`, `list`, `get`, `forget`, `consolidate`, `stats`, `deprecate`, `migrate`. Supports `scope: workspace` (default) or `global`. Saves require a name, content, and optional type/description/importance. | ReadOnly |
| `knowledge` | Read-only codebase intelligence queries (offline). Actions: `context`, `search`, `symbol`, `related`, `tests`, `episodes`, `preferences`, `rejected`, `coverage`, `explain`. | ReadOnly |
| `ask` | Ask the user structured questions and wait for answers. Supports `select` and `text` question types, with optional answers and custom input. | ReadOnly |
| `load_tools` | Enable deferred tools for this session. Pass `tools:[...]` or `tool:"name"`. Groups: `all`, `git`, `web`, `bulk`, `browser`. | ReadOnly |
| `subagent` | Delegate to a child agent. Modes: `single` (one agent), `parallel` (tasks array), `chain` (sequential steps), plus management actions. Supports worktree isolation, async background execution, and agent configuration. | Destructive |
| `patch` | Apply a unified diff patch to a file. Uses `@@` hunks with `+`/`-`/space prefixes. For larger refactors than `edit` handles well. | Destructive |
| `git_status` | Show working-tree status (`git status --short --branch`). Optional `path` scopes to a subdirectory. Prefer over bash `git status`. | ReadOnly |
| `git_diff` | Show unstaged/staged changes. Optional `path` / `staged`. Prefer over bash `git diff`. | ReadOnly |
| `git_log` | Recent commit history (`git log --oneline`). Prefer over bash `git log`. | ReadOnly |
| `git_show` | Show a commit/tree/blob (`git show <object>`). Prefer over bash `git show`. | ReadOnly |

---

## Deferred Tools

Enabled via `load_tools`. Each tool's schema is not sent to the model until
loaded. Defined by `deferred_tool_names()` (/core/src/tools.rs).

### General Deferred

| Tool | Description | Class |
|------|-------------|-------|
| `bulk` | Batch several independent tool calls in one round-trip (shared approval). Supported inner tools: `read_file`, `write_file`, `edit`, `list_dir`, `grep`, `glob`, `bash`, `fetch`, `web_search`, `delete`, `rename`, `mkdir`. | Destructive |
| `bulk_read` | Read many files in one call. Each file returned as a headed block. Per-file errors reported inline. | ReadOnly |
| `bulk_write` | Write many files in one call. Each entry `{path, content}`; parents created, existing files overwritten. | Destructive |
| `bulk_edit` | Apply search/replace edits to many files. Each entry `{path, edits}` (same shape as `edit`). Per-file atomic; failed search fails only that file. | Destructive |
| `diagnostics` | Run the project's type checker/compiler (cargo check, tsc --noEmit, go build, py_compile). Returns diagnostics. | ReadOnly |
| `fetch` | Fetch a URL over HTTP(S); HTML is lightly stripped to text. Bounded to `fetch_max_bytes` (default 256 KiB). Works under `--no-network` (runs on the host control plane, not the guest). Host allowlist may restrict domains. | ReadOnly |
| `web_search` | Web search. Prefers Exa / Tavily APIs (with round-robin load balancing + quota tracking). Falls back to public SearXNG, DDG, and Mojeek scrapes. Honors `--no-network` / `fetch_allowlist` (host-side, not the guest network policy). | ReadOnly |
| `workspace_activity` | List other active catalyst-code sessions in this workspace (separate processes). Returns goals, current work, recently touched files. Read-only awareness tool. | ReadOnly |

### Git Tools

Read-only `git_status` / `git_diff` / `git_log` / `git_show` are **core** (always offered). The group `load_tools` `git` enables the mutators below.

| Tool | Description | Class |
|------|-------------|-------|
| `git_add` | Stage files for commit (`git add -- <paths>`). Destructive (modifies the index). | Destructive |
| `git_commit` | Create a commit (`git commit -m <message>`). Pass `all:true` to stage modified tracked files first (does NOT add untracked). | Destructive |
| `git_push` | Push to a remote (`git push`). Optional `remote`/`refspec`/`set_upstream`/`tags`. | Destructive |
| `git_pull` | Pull from a remote (`git pull`). Optional `remote`/`refspec`/`rebase`. | Destructive |
| `git_branch` | List/create/delete/checkout branches (`action` + optional `name`/`all`). | Destructive |

### Execution Tools

| Tool | Description | Class |
|------|-------------|-------|
| `spawn` | Run a nested agentic turn with a fresh sub-conversation and its own tool loop. The sub-agent shares the workspace but cannot spawn further sub-agents. | Destructive |
| `test_env` | Spin up and drive ephemeral Linux containers / Windows VMs for platform-specific testing, with VNC screen access. Actions: `create`, `exec`, `screenshot`, `input`, `vnc_url`, `destroy`, `list`. | Destructive |

### Goal-Only Tool

| Tool | Description | Class |
|------|-------------|-------|
| `goal_write_plan` | Submit a structured multi-subagent plan (goal mode only). Each step becomes a subagent prompt. Supports dependency DAG, model overrides, and validation criteria. | ReadOnly |

### Browser Tools

Loaded as a group via `load_tools` with `group:"browser"`. Uses a native WRY
webview per session. All 18 tools:

| Tool | Description | Class | Required Params |
|------|-------------|-------|-----------------|
| `browser_create` | Create a native browser session. Default profile is ephemeral. Returns `session_id`, `tab_id`, and capability flags. | Destructive | — |
| `browser_close` | Close a browser session and release the webview. | Destructive | `session_id` |
| `browser_list_sessions` | List open browser sessions. | ReadOnly | — |
| `browser_navigate` | Navigate a tab to a URL. Prefer `wait_until: dom_stable`. | Destructive | `session_id`, `url` |
| `browser_back` | Go back in history for the tab. | Destructive | `session_id` |
| `browser_reload` | Reload the current page. | Destructive | `session_id` |
| `browser_snapshot` | Primary perception tool: DOM snapshot with element refs (e1, e2, …). Modes: `interactive`, `text`, `structure`, `full`. | ReadOnly | `session_id` |
| `browser_find` | Search the live DOM for elements; returns refs. Strategies: `text`, `role`, `css`, `label`, `placeholder`. | ReadOnly | `session_id`, `query` |
| `browser_click` | Click an element by snapshot ref. Requires `snapshot_id` + `ref` from latest snapshot. | Destructive | `session_id`, `snapshot_id`, `ref` |
| `browser_fill` | Replace the full value of an input/textarea (framework-compatible events). | Destructive | `session_id`, `snapshot_id`, `ref`, `text` |
| `browser_type` | Type incrementally into a focused field (use when keystroke behavior matters). | Destructive | `session_id`, `snapshot_id`, `ref`, `text` |
| `browser_press` | Press a key (Enter, Tab, Escape, Arrow*, etc.) on an element or the page. | Destructive | `session_id`, `key` |
| `browser_scroll` | Scroll the document or an element. Direction: up/down/left/right. Units: pixels/pages/percent. | Destructive | `session_id` |
| `browser_wait` | Wait for a condition (text, element, url, dom_stable, timeout, javascript). Prefer over polling snapshots. | Destructive | `session_id`, `condition` |
| `browser_evaluate` | Run JavaScript in the page. Escape hatch for operations semantic tools cannot express. | Destructive | `session_id`, `script` |
| `browser_screenshot` | Capture a viewport screenshot (PNG). Returns a workspace-relative path when possible. | ReadOnly | `session_id` |
| `browser_show` | Show the native browser window (CAPTCHA, OAuth, passkeys, human takeover). | Destructive | `session_id` |
| `browser_hide` | Hide the native browser window. | Destructive | `session_id` |

All browser tools accept optional `session_id` and `tab_id` (defaults to active tab).

---

## Parallel Wave Tools

Tools that can run concurrently in a top-level wave after gates have passed.
Read-only, no interactive flyouts, no session mutation. Sequential tools (writes,
bash, finish, subagent, ask) stay ordered to preserve HITL and side-effect
ordering.

```rust
is_parallel_wave_tool(name) => matches!(
    name,
    "read_file" | "list_dir" | "grep" | "glob"
        | "bulk_read" | "todo_read"
        | "git_status" | "git_diff" | "git_log" | "git_show"
        | "workspace_activity"
        | "fetch" | "web_search"
        | "diagnostics"
)
```

**Concurrency:** up to `BULK_CONCURRENCY` (4) parallel tasks when the model
emits a multi-tool batch. The orchestrator dispatches all wave-eligible tools
simultaneously, bounded by a semaphore.

---

## Bulk Tool Concurrency

The `bulk` tool and parallel wave execution share a concurrency cap of **4**
(defined as `BULK_CONCURRENCY` in `core/src/tools.rs`). This limits how many
sub-operations run simultaneously when batching reads, writes, or edits.

---

## Tool Execution

Most tools execute synchronously via `execute()` (/core/src/tools.rs). Tools
that are inherently async are dispatched through specialized handlers:

| Handler | Tools |
|---------|-------|
| `execute_bash` | `bash` |
| `execute_fetch` | `fetch` |
| `execute_web_search` | `web_search` |
| `execute_diagnostics` | `diagnostics` |
| `execute_browser` | All `browser_*` |
| `execute_subagent` | `spawn`, `subagent` |
| `execute_test_env` | `test_env` |
| `execute_bulk` | `bulk` |
| `handle_load_tools` | `load_tools` |
| `request_ask` | `ask` |
| `handle_goal_write_plan` | `goal_write_plan` |
| `execute_intercom` | `contact_supervisor`, `intercom` |

All file operations are confined to the workspace root (path confinement is
disabled only under `Approval::Never`). The `bash` tool runs with
`cwd=workspace`, a real timeout and kill, and a denylist tripwire.

---

## Tool Result

Every tool returns an `Outcome` (/core/src/tools.rs):

```json
{
  "ok": true,
  "output": "result text",
  "diff": "optional unified diff for edits/writes"
}
```

Tools that produce file diffs (`edit`, `write_file`, `patch`) include a `diff`
field rendered separately in the TUI so the model's result text stays compact.
