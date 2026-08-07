---
name: plugin-authoring
description: Author and debug Catalyst Code plugins — hooks, custom tools, OAuth providers, memory backends, and system-prompt injection. Read before creating or changing a plugin.
---

## Plugin System

You can extend the harness with plugins. Plugins are self-contained directories
under `.catalyst-code/plugins/`. Each plugin hooks into tool execution and
session lifecycle events to inspect, approve, modify, or log operations.

### Creating a plugin

1. Create a directory: `.catalyst-code/plugins/<plugin-name>/`
2. Write a `plugin.json` manifest (see format below)
3. Write executable hook scripts (bash, python, or any language)
4. Make hook scripts executable (`chmod +x hooks/*.sh`)
5. The core loads new plugins on next restart. Manage plugins with slash
   commands `/plugin-install`, `/plugin-enable`, `/plugin-disable`,
   `/plugin-remove`, `/plugin-reload` (or the matching protocol commands).
   There is **no** built-in `plugin` tool. Use `/plugin-reload` to re-scan
   directories without restarting — enabled flags are preserved.

### Installing a plugin

Install from a **local directory** or a **GitHub Release**:

- Local: `/plugin-install /path/to/plugin-dir`
- Local (this repo only): `/plugin-install /path/to/plugin-dir workspace`
- GitHub (latest release): `/plugin-install https://github.com/owner/repo`
- GitHub (pinned release): `/plugin-install https://github.com/owner/repo@v1.2.0`
- Shorthand: `/plugin-install owner/repo@v1.2.0`
- Scope flags: append `global` (default) or `workspace`, or use `--global` / `--workspace`

**Install scope**

| Scope | Destination | Loads when |
|-------|-------------|------------|
| `global` (default) | `~/.catalyst-code/plugins/<name>/` | Every workspace, always |
| `workspace` | `<repo>/.catalyst-code/plugins/<name>/` | This repo only (user installs load without `--trust-project-plugins`) |

Prefer **global** for personal plugins (memory backends, OAuth, vision handoff).
Use **workspace** when the plugin should stay repo-local. Repo-shipped plugins
(without the user-install marker) still need `--trust-project-plugins` to load.

**Prefer GitHub Releases for distribution.** Tag a release so the install
downloads that release's source `.zip` (via the GitHub API `zipball_url`).
Release tags are the version pin — they enable reproducible installs and a
future auto-updater that re-fetches the latest (or newer) release zip. A
repo with no Releases cannot be installed via URL; publish one first.

Optional subdir for monorepos: `owner/repo@v1.2.0:path/to/plugin` (the
plugin.json lives under that path inside the release zip).

### Publishing / discovery (plugin listing)

To show up on a site that scrapes GitHub (e.g. code.catalystctl.com):

1. Publish **GitHub Releases** (versioned source zips).
2. Add the repository topic **`catcode-plugin`** (canonical). Optional alias:
   `catalyst-code-plugin`.
3. Keep `plugin.json` at the repo root (or document a `:subdir`).

Listing scrapers search:
`https://api.github.com/search/repositories?q=topic:catcode-plugin`
then read each repo's latest Release + `plugin.json`. An optional root
`catalog.json` (`schema: catcode-plugin/v1`) can carry extra listing metadata.

### plugin.json format

```
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "What this plugin does",
  "hooks": {
    "pre_write": {
      "script": "hooks/pre_write.sh",
      "timeout_ms": 5000,
      "pass_args": true
    },
    "post_bash": {
      "script": "hooks/post_bash.py",
      "timeout_ms": 30000,
      "pass_args": false
    }
  }
}
```

Fields:
- `name` (required): unique plugin identifier (directory name must match)
- `version` (required): semver string
- `description` (optional): human-readable summary
- `hooks` (optional): map of hook-point name to config
  - `script` (required): path to executable, relative to the plugin directory
  - `timeout_ms` (optional): override the default hook timeout (default: 5s for pre_*, 30s for post_*)
  - `pass_args` (optional): if true, the hook context JSON includes the tool's `args` object (default: false)

### Hook contract

Each hook script receives a single JSON object on stdin and MUST write a single
JSON object to stdout before exiting. Stderr is captured for error reporting.

**Context (stdin → script):**
```
{
  "hook": "pre_write",
  "tool": "write_file",
  "workspace": "/path/to/workspace",
  "args": { "path": "src/file.rs", "content": "..." },
  "session_id": "abc123.jsonl",
  "timestamp": 1719000000
}
```

**Response (script → stdout):**
```
{
  "allow": true,
  "reason": "File passes lint check",
  "modify": { "content": "reformatted code" }
}
```

- `allow` (required, bool): true to proceed, false to block (pre hooks) or skip result (post hooks)
- `reason` (optional, string): human-readable explanation. For pre hooks it
  is shown to the model — appended to the tool result as a note on `allow`, and
  used as the deny message on `allow:false`. Also logged. For post hooks it is
  appended to the tool result as a note.
- `notify` (optional, string): non-empty text is emitted as an `info` event
  framed `[plugin-name] …` for the TUI/UI.
- `status` (optional, string): emitted as a `plugin_status` event. An empty
  string clears the plugin's status text; omit the key for no change.
- `modify` (optional, object): for pre hooks, a JSON object whose keys are
  **merged over** the original tool args (shallow, per-key override). Return only
  the fields you want to change; everything else is preserved. Examples:
  pre_write `{ "content": "reformatted" }` overrides content but keeps `path`/
  `edits`; pre_bash `{ "command": "fixed command" }` overrides the command;
  pre_read `{ "path": "new/path" }` redirects the read. For **post hooks**,
  `modify` transforms the tool's RESULT: `{ "output": "...", "ok": false,
  "diff": "..." }` replaces the result text, flips success, or replaces/clears
  the diff — e.g. redact a secret, append context, or reformat. (The post
  context includes the current result under the `result` key so the hook can
  read it.)

  Note: pre-hook `modify` runs AFTER the approval gate + diff preview (which use
  the original args), so a rewritten `path`/`command` is NOT re-prompted. File
  tools still re-confine the path internally and `bash` re-checks its denylist,
  so the security boundaries hold — but a plugin that redirects a safe path to a
  sensitive one bypasses the user-facing prompt. Pre-hooks are trusted,
  user-installed code (project hooks gated by `--trust-project-plugins`).

Safety rules enforced by the core (per-hook **policy table**, not name prefix):
- Blocking pre hooks (`pre_bash`/`pre_write`/`pre_read`/`pre_tool`/`pre_input`):
  non-zero exit, timeout, or JSON parse failure → `allow: false` (blocks)
- Post hooks + advisory lifecycle hooks: failure → silently skipped
- `pre_context` / `pre_agent_start`: fail-open on timeout (keep prior messages/prompt)
- Disabled plugins are never invoked
- Default timeouts: 5s for blocking pre hooks, 30s otherwise
- Hook failures never crash the core
- Multi-plugin composition order is **alphabetical by plugin name** (stable)

### Available hook points

| Hook point         | Fires when                                      | Type |
|--------------------|-------------------------------------------------|------|
| pre_bash           | Before a bash command executes                  | pre (block) |
| pre_write          | Before a file write/edit                        | pre (block) |
| pre_read           | Before a file is read                           | pre (block) |
| post_bash          | After a bash command completes                  | post |
| post_write         | After a file write/edit completes               | post |
| post_read          | After a file is read                            | post |
| pre_tool           | Before ANY tool (main, bulk inners, subagent)   | pre (block) |
| post_tool          | After ANY tool (main + subagent; result modify) | post |
| pre_input          | Before user text becomes a Message              | pre (block) |
| pre_agent_start    | Dynamic system-prompt surgery (transient)       | advisory |
| pre_context        | Rewrite messages before each LLM call           | advisory |
| turn_start         | Turn begins (after pre_input)                   | lifecycle |
| turn_end           | Turn ends (before session_stop)                 | lifecycle |
| session_start      | When a turn starts (prompt received)            | lifecycle |
| session_stop       | When a turn ends (done/abort) — per-turn        | lifecycle |
| session_shutdown   | Process/session teardown (once, stdin EOF)      | lifecycle |
| pre_compact        | Before conversation compaction                  | lifecycle |
| pre_turn           | Vision handoff only (image-gated; advisory)     | advisory |

PI doc aliases: `pre_input`≈`input`, `pre_agent_start`≈`before_agent_start`,
`pre_context`≈`context`, `turn_*`≈`turn_*`. Prefer Catalyst-native names.

### pre_turn hook (vision handoff — not a general turn hook)

`pre_turn` fires **only when images are attached**, after the user message is
built and before the first model request. It is advisory and vision-specific:
it can remap the model for the turn but can never block it. For general turn
boundaries use `turn_start` / `turn_end`. For input/prompt/context surgery use
`pre_input` / `pre_agent_start` / `pre_context`.

Context `args` (set `pass_args: true` in the manifest):
```
{
  "model": "umans-glm-5.2",
  "has_images": true,
  "image_count": 2,
  "models": [ {"id":"...", "vision":true}, ... ]
}
```
Response: return `modify: { "model": "<new-model-id>" }` to swap the turn's
model. The core validates the id against discovered models and emits an `info`
event on handoff. Use this to route image-bearing turns to a vision-capable
model when the active one lacks vision (see the bundled `vision-handoff` plugin).

### Declaring tools (custom capabilities, no MCP)

A plugin can ALSO declare tools — first-class capabilities the model can call,
defined without MCP and without recompiling. A tool is a JSON Schema (sent to
the model like any built-in tool) plus a handler script the core spawns per
call. This is the no-MCP way to give the agent a new capability (a domain tool,
a CLI wrapper, an internal-API client, …) by dropping files in
`.catalyst-code/plugins/`. (Plugin tools are available to the main agent;
subagents use the built-in tool set.)

Add a `tools` array to `plugin.json` (a plugin may declare only tools, only
hooks, or both):

```
{
  "name": "my-tools",
  "version": "0.1.0",
  "description": "Custom domain tools",
  "tools": [
    {
      "name": "lookup_order",
      "description": "Look up an order by id from the internal API.",
      "parameters": {
        "type": "object",
        "properties": { "order_id": { "type": "string" } },
        "required": ["order_id"]
      },
      "script": "tools/lookup_order.sh",
      "kind": "readonly",
      "timeout_ms": 15000
    }
  ]
}
```

Fields:
- `name` (required): tool name. By default it must not collide with a built-in
  tool (bash, read_file, edit, subagent, …) — a colliding plugin tool is skipped
  and the built-in wins. Set `override: true` (below) to instead REPLACE the
  built-in's implementation with this plugin's handler.
- `description` (optional): shown to the model.
- `parameters` (optional): a JSON Schema for the tool's arguments. Defaults
  to an empty object.
- `script` (required): path to the executable handler, relative to the plugin
  directory (path-confined; `..` escapes are rejected).
- `kind` (optional): `"readonly"` (skips the approval gate) or `"destructive"`
  (prompts under Approval::Destructive — the default). Arbitrary external code
  runs on every call, so default to `destructive`.
- `override` (optional, bool): when `true` AND `name` matches a built-in tool,
  this plugin's handler REPLACES that built-in — the model still sees a tool of
  that name (this plugin's declared `description`/`parameters`), but calls route
  to the plugin script instead of the core handler. This is the no-recompile way
  to fully override a core tool (a sandboxed `bash`, a redacting `read_file`, a
  rate-limited `git_commit`, …). Default `false`: a name collision stays built-in.
- `timeout_ms` (optional): hard per-call timeout (default 30s).

Tool handler contract (one JSON object on stdin, one on stdout):

```
# stdin (→ handler)
{ "args": { "order_id": "12345" }, "workspace": "/abs/path", "session_id": "x.jsonl", "timestamp": 1719000000 }

# stdout (→ core)
{ "ok": true,  "output": "Order #12345: shipped" }
{ "ok": false, "output": "order not found" }   # ok omitted defaults to true
```

- `output` is the text shown to the model as the tool result. `ok:false` (or
  an `error` field) marks the call failed — the conversation continues; the
  model sees the error and can react.
- Optional `notify` / `status` fields work the same as on hook responses
  (UI info event / plugin status bar).
- A bare non-JSON stdout is accepted as `output` with `ok=true`, so a trivial
  `echo` handler works. Prefer the structured form.
- Non-zero exit, timeout, or spawn failure produce an error result (the model
  is told the tool failed); they never crash the core or the turn.

Safety (identical to hooks): tool scripts are path-confined to the plugin
directory, must be executable, and run with the same `trust_project_plugins`
gate — a repo's project-scoped tools load only after you opt in with
`--trust-project-plugins` (built-in + `~/.catalyst-code/plugins` tools load
always). Tool calls honor the approval gate and your allow/deny permission
rules like any tool.

`.catalyst-code/plugins/my-tools/tools/lookup_order.sh`:
```bash
#!/bin/bash
input=$(cat)
id=$(echo "$input" | jq -r '.args.order_id')
# …call your internal API…
jq -n --arg o "Order #$id: shipped" '{ "ok": true, "output": $o }'
```

Remember: `chmod +x` the handler.

### Slash commands (`commands` array)

Plugins can declare user-invoked slash commands (in addition to model-facing
tools). Add a `commands` array to `plugin.json`:

```
{
  "name": "my-cmds",
  "version": "0.1.0",
  "commands": [
    {
      "name": "greet",
      "description": "Say hello",
      "script": "scripts/greet.sh",
      "timeout_ms": 15000
    }
  ]
}
```

- `name` (required): command name without a leading `/` (a leading `/` is
  stripped). Empty names are skipped. Must not collide with reserved builtins
  (`help`, `login`, `plugin-list`, `memory`, `skill`, …).
- `description` (optional): shown in command listings.
- `script` (required): path-confined executable relative to the plugin dir.
- `timeout_ms` (optional): default 30s.

Stdin: `{ "command", "args", "workspace", "session_id", "timestamp", "plugin" }`
Stdout: `{ "ok", "output", "notify"?, "status"? }` (same side-effect fields as
hooks/tools). Reload plugins at runtime with `/plugin-reload` (protocol:
`reload_plugins`) — enabled/disabled flags are preserved.

### Prompt-backed commands (`prompt_file` + `mode: "agent_turn"`)

A command can also be **prompt-backed**: instead of running a script, it renders a
template file and submits the result as a normal agent turn (the same path as a user
`send`). This lets a command like `/deep-research` drive a full agent loop — with
tool use, subagents, and the active model — without a script, an interpreter, or a
recompile. Use it for any "run a big agentic workflow from one slash command" need
(`/review-pr`, `/document-repo`, `/investigate-bug`, `/migration-plan`, …).

A command is **either** script-backed (`script`) **or** prompt-backed
(`prompt_file`); exactly one must be set. `mode` is optional and inferred
(`"script"` / `"agent_turn"`).

```
{
  "name": "deep-research",
  "version": "1.0.0",
  "commands": [
    {
      "name": "deep-research",
      "description": "Conduct comprehensive, citation-backed research.",
      "prompt_file": "prompts/deep-research.md",
      "mode": "agent_turn"
    },
    {
      "name": "research",
      "description": "Alias for deep research.",
      "prompt_file": "prompts/deep-research.md",
      "mode": "agent_turn"
    }
  ]
}
```

- `prompt_file` (required for prompt-backed): path-confined to the plugin dir
  (no absolute paths, no `..`), must be a regular file, capped at 256 KiB. It need
  not be executable — it is read as text and rendered, not spawned.
- `mode` (optional): `"script"` or `"agent_turn"`. Inferred from which field is
  set; set it explicitly only to be self-documenting. A mismatch (e.g. `script`
  with `mode: "agent_turn"`) is rejected at load.
- At most one of `script` / `prompt_file`. Both set, or neither set, is an error.

**Template tokens** (substituted before the turn starts; whitespace-tolerant
`{{ key }}` works; unknown `{{...}}` is left verbatim):

| Token | Value |
|---|---|
| `{{args}}` | the raw text after the command name (the user's request) |
| `{{workspace}}` | absolute workspace path |
| `{{session_id}}` | session file name |
| `{{timestamp}}` | unix seconds at invocation |
| `{{plugin}}` | plugin name |
| `{{command}}` | command name (without `/`) |

**Execution:** the harness renders the template, emits a concise `info` event
showing the original command (not the full prompt), resolves the model
(last-used → configured default → first available) and effort (`"medium"`), and
calls the same `start_turn` path as a user `send`. This preserves cancellation,
session persistence, approval, tool use, model selection, compaction, and
telemetry. If `{{args}}` renders to an empty/whitespace prompt, the harness emits
a usage error instead of starting a turn — so prompt-backed commands should
document their usage in the template or rely on this fallback.

**Why no hidden state:** the conversation is both the model input and the
persisted transcript, with no clean display/actual split. The rendered prompt
becomes the user message (the visible-prompt fallback). Keep the template a
coherent directive (not raw internal scaffolding) so the transcript stays
readable; surface the original command via the `info` event the harness emits.

**Collecting input via a modal (`ask`):** the command palette does not pass
inline arguments — selecting a `/name` command from the palette invokes it with
`args: ""`. So a prompt-backed command that needs user input should not just
print a usage message when `{{args}}` is empty; instead, instruct the agent to
call the `ask` tool, which emits an `ask_request` event the TUI renders as an
input modal (text + select fields), then proceed with the user's answers. This is
how the bundled `deep-research` plugin collects the research request, depth,
format, and source scope when launched from the palette. Inline `/name <args>` is
still supported when the client passes it; design the prompt to use inline args
if present and fall back to the `ask` modal when empty.

**Capability note:** a prompt-backed command executes no subprocess, so it does
not require the `execute_subprocess` capability (only `register_commands`). It
carries no arbitrary-code-execution surface beyond a normal agent turn.

### Add, override, and remove core behavior

A plugin can do everything a direct core edit can — add, override, and remove
behavior — without recompiling. Five mechanisms cover the full surface:

| Operation | Mechanism | Notes |
|-----------|-----------|-------|
| **ADD a tool** | `tools` array | A new capability the model can call. |
| **OVERRIDE a tool** | a tool with `override: true` | Replaces a built-in's implementation (the model still sees that tool name; calls route to the plugin script). |
| **REMOVE a tool** | `disable_tools` | Drops the named tool from the model's toolset entirely (built-in or override). The strongest lever — wins over `override`. |
| **MODIFY tool input** | `pre_bash`/`pre_write`/`pre_read`/`pre_tool` `modify` | Override specific args before execution. |
| **MODIFY tool output** | `post_*`/`post_tool` `modify` | Replace the result text / flip success / change the diff after execution. |
| **MODIFY the model** | `pre_turn` `modify.model` | Remap the turn's model (advisory). |
| **ADD to the system prompt** | `system_prompt` field | Static text appended to the system prompt. |
| **REPLACE the memory store** | `memory_provider` block | Replaces standing-prompt injection, slash `/remember`/`/memory`/`/forget`, compaction extract, and (unless a tool `override`s `memory`) the built-in `memory` tool. Only one enabled provider should be active. |

#### `memory_provider` — replace the memory backend

A plugin can replace the built-in markdown memory store with an external
engine (vector DB, Engraphis, remote API, …). Declare a single script that
handles all memory actions, same pattern as `oauth`:

```json
{
  "name": "engraphis-memory",
  "version": "1.0.0",
  "memory_provider": {
    "script": "memory/provider.py",
    "timeout_ms": 30000
  }
}
```

Fields:
- `script` (required): path relative to the plugin directory
- `timeout_ms` (optional, default 30000): hard timeout per action

**Action contract** — stdin is one JSON object; stdout is one JSON object.

Base context always includes `action`, `workspace`, `session_id`, `timestamp`.
Action-specific fields live under `args`.

| action | args | success response |
|--------|------|------------------|
| `inject` | optional `query` | `{ "ok": true, "injection": "…" }` (empty string = no memories) |
| `save` | `name`, `content`, optional `type`, `description`, `scope` | `{ "ok": true, "output": "…", "id": "…" }` |
| `append` | same as save | same |
| `list` | optional `scope` | `{ "ok": true, "output": "…", "entries": […] }` |
| `forget` | `id`, optional `scope` | `{ "ok": true, "output": "…" }` |
| `compact_append` | `content`, optional `name`, `cap_bytes` | `{ "ok": true, "output": "…" }` |

On failure return `{ "ok": false, "output": "reason" }` (or `{ "error": "…" }`).
`inject` failures are soft — the core uses an empty injection and continues.
Write failures surface to the model / slash-command caller.

When a `memory_provider` is loaded, the core skips the markdown store for
injection, slash memory commands, and compaction extracts. The self-learning
auto-reflect loop still calls the `memory` tool — those writes go to the
provider (or to a plugin `memory` tool with `override: true` if declared).

#### `disable_tools` — remove a capability

```json
{
  "name": "no-bash",
  "version": "1.0.0",
  "disable_tools": ["bash", "git_commit"]
}
```

The listed tool names vanish from the model's toolset — it can never call them
(this is stronger than a per-call `pre_bash` deny). Composes across plugins (the
union is removed). Applied as a final filter, so it also removes a tool another
plugin `override`s.

#### `system_prompt` — inject context

```json
{
  "name": "domain-rules",
  "version": "1.0.0",
  "system_prompt": "All database access must go through the `db_query` tool. Never construct raw SQL in bash."
}
```

The text is appended to the system prompt (after the plugin docs), framed with
the plugin name + version — the same surface a core edit of the system prompt
touches. Empty by default, so the prompt + its prefix cache are untouched when no
plugin declares one. (Main agent only; subagents use the built-in tool set.)

#### `override: true` — replace a core tool

```json
{
  "name": "sandboxed-bash",
  "version": "1.0.0",
  "tools": [
    {
      "name": "bash",
      "override": true,
      "description": "Run a command in the project sandbox.",
      "parameters": {"type":"object","properties":{"command":{"type":"string"}},"required":["command"]},
      "script": "tools/bash.sh",
      "kind": "destructive"
    }
  ]
}
```

The model calls `bash` as usual, but the call routes to `tools/bash.sh` instead
of the core handler — and the plugin controls the description/schema. The tool's
`kind` (approval gate) is the plugin's. The specific `pre_bash`/`post_bash`
hooks still fire (keyed on the tool name), and `pre_tool`/`post_tool` fire too.

### Declaring an OAuth provider (subscription auth)

Built-in presets are API-key only. A plugin adds a subscription-OAuth provider
for any vendor with no recompile. The plugin supplies ONE script that handles
four actions (`login`, `complete`, `token`, `clear`); the harness owns the
loopback redirect server (web flow), can invoke `complete` immediately for an
automatic device-code flow, and provides the `/oauth-code` paste path for
manual flow.

Working example: `~/catcode-chatgpt-provider` (ChatGPT Plus/Pro → Codex).
Template: `docs/examples/plugins/grok-oauth`.

Add an `oauth` block to `plugin.json`:

```json
{
  "name": "grok-oauth",
  "version": "0.1.0",
  "oauth": {
    "provider_id": "grok",
    "label": "Grok (xAI)",
    "kind": "openai",
    "base_url": "https://api.x.ai/v1",
    "description": "Grok via xAI device-code OAuth.",
    "headers": [["X-Source", "catalyst-code"]],
    "token_path": "grok.json",
    "script": "oauth/oauth.sh",
    "login_timeout_ms": 180000,
    "token_timeout_ms": 30000
  }
}
```

Fields:
- `provider_id` (required): the provider identity. The harness creates the
  provider config with this `name` on a successful `/login`, and `/oauth-code`
  + `/logout` dispatch on it.
- `label` (optional): shown in the `/login` picker (defaults to provider_id).
- `kind` (optional, default `"openai"`): `"openai"` (OpenAI-compatible
  `/chat/completions`) or `"anthropic"` (`/v1/messages`). Decides the wire
  protocol + auth header.
- `base_url` (required): the endpoint, including `/v1`. Paths are appended
  directly.
- `description` (optional): shown in the picker.
- `headers` (optional): extra HTTP headers on every request, `[[key,val],…]`.
  These are persisted into the provider config.
- `token_path` (optional, default `<provider_id>.json`): the token-file name,
  relative to `~/.config/catalyst-code/oauth/`. The plugin owns the token's
  on-disk format; the harness only checks existence (for the "logged in"
  status) and passes the absolute path to the script.
- `detect_path` (optional): an external credential file used for cheap login
  detection. `$CODEX_HOME/auth.json` and `~/.codex/auth.json` are supported;
  the provider script remains responsible for importing its format.
- `script` (required unless every action has an explicit override): the script
  handling ALL actions, dispatched by the `action` field in stdin.
- `login_script` / `complete_script` / `token_script` (optional): per-action
  overrides. When absent, the action falls back to `script`.
- `login_timeout_ms` (optional, default 120000): timeout for `login` +
  `complete`.
- `token_timeout_ms` (optional, default 30000): timeout for `token` + `clear`.
- `redirect_path` (optional, default `"/callback"`): the path component the
  harness binds on its loopback redirect server for the web flow. **Must
  match the redirect URI registered with the provider's OAuth client** —
  Google's installed-app OAuth clients (Antigravity IDE, Gemini CLI) require
  `"/oauth2callback"`; using the default `"/callback"` makes Google reject
  the request as a non-compliant redirect URI (`redirect_uri_mismatch`).
  The harness prefixes a `/` if absent, so `"/oauth2callback"` and
  `"oauth2callback"` are equivalent. See
  [`docs/plugins/oauth.md`](../../../docs/plugins/oauth.md#redirect_path-matching-the-providers-registered-redirect-uri)
  for the full table of which providers need which path.
- `env_passthrough` (optional): non-secret env var names the harness forwards
  to your scripts (e.g. `["ACME_OAUTH_HOST"]` for a self-hosted auth server,
  or `["CATALYST_CODE_<NAME>_PROJECT"]` for a plugin-specific project
  override that survives env scrubbing). The harness otherwise scrubs the
  environment, so undeclared vars never reach the script. Names must match
  `[A-Za-z_][A-Za-z0-9_]*`; any name containing KEY/TOKEN/SECRET/PASSWORD/
  CREDENTIAL (case-insensitive) is rejected at load time — passthrough must
  never defeat env scrubbing. See
  [`docs/plugins/oauth.md`](../../../docs/plugins/oauth.md#env_passthrough-plugin-specific-config-knobs-that-survive-env-scrubbing)
  for the conventions and the rationale.

#### Script action contract

Every invocation receives ONE JSON object on stdin (with `action` + the base
context) and MUST write ONE JSON object to stdout. The base context always
includes `action`, `provider_id`, `token_path` (absolute), `workspace`, and
`timestamp`; each action adds its own fields.

**`login`** — build the authorize/verify URL. Input adds `headless` (bool) and,
for the web flow, `redirect_uri` (a `http://localhost:<port>/callback` the
harness already bound — embed it verbatim in your authorize URL). Output:
```json
{ "url": "https://auth.example.com/device?...", "code": "ABCD-EFGH",
  "message": "Open the URL and enter the code",
  "flow": "web" | "manual" | "poll" | "already_authenticated",
  "auto_complete": true,
  "state": "<csrf>", "pending": { "verifier": "...", "device_id": "..." } }
```
- `flow`: `"web"` = the harness waits for the loopback redirect (local
  machine); `"manual"` = the user pastes a code back via `/oauth-code`
  (SSH/headless, or a provider that requires manual completion); `"poll"`
  or `"auto"` = the harness invokes `complete` immediately and waits while
  the provider polls; `"already_authenticated"` = the provider found an
  existing credential store and needs no browser flow. `auto_complete: true`
  is an equivalent explicit marker. Honor `headless` when choosing between
  web and manual; automatic polling works in either environment.
- `state` (web flow): the CSRF state you put in the authorize URL, so the
  harness can verify the redirect.
- `pending` (both flows): an opaque JSON blob you need to carry to `complete`
  (e.g. a PKCE verifier, a device-auth id). The harness stashes it and passes
  it back verbatim.
- `code` (optional, manual/device flow): a user-code to display.

**`complete`** — exchange the code for a token and WRITE it to `token_path`.
Input adds `code` (the pasted/redirected code) and `redirect_uri` (web flow) or
`pending` (manual flow). Output:
```json
{ "ok": true }
{ "ok": false, "error": "expired code" }
```

**`token`** — resolve/refresh the access token. Read `token_path`; if expired,
refresh (make your own HTTP call) and write the updated token back. Output:
```json
{ "access_token": "<bearer>", "expires_at": 1719003600,
  "headers": [["chatgpt-account-id", "<uuid>"]] }
{ "access_token": null }
{ "ok": false, "error": "refresh failed" }
```
`expires_at` is unix seconds (optional; if 0/absent the harness caches for ~5
min). Optional `headers` are merged onto every request for that provider
(plugin wins on name conflicts) and cached with the token — use this for
per-user identity headers such as ChatGPT's `chatgpt-account-id` or
Google Code Assist's `x-code-assist-project` (Antigravity / Gemini CLI
bundles). This runs on the per-turn hot path, so it is cached until near
expiry.

**Header gotcha (Google Code Assist):** inject `x-code-assist-project`,
**not** `x-goog-user-project` and **not** `cloudaicompanion-project`. The
Code Assist chat gateway treats the three names as different routing
signals: only `x-code-assist-project` is authorized for Antigravity /
Gemini CLI OAuth tokens; the other two route to the consumer GenAI gate
and return `403 SERVICE_DISABLED`. Verified live and pinned by the
`wire_shape_contract` test module in
`core/src/providers/google_code_assist.rs`.

Concurrency: several harness processes (TUI, web service, a second TUI) can
invoke `token` at the same time, and providers commonly rotate refresh tokens.
Write `token_path` ATOMICALLY (temp file + rename) and serialize the refresh
(e.g. `flock` on a sidecar lock, then re-check freshness before refreshing) —
a truncated read or a lost refresh-token rotation surfaces to the user as an
unexplained "run /login" prompt.

**`clear`** — delete any credentials + extra state you manage. The harness
ALSO deletes `token_path`, so this is optional (use it for sidecar files).
Output: `{ "ok": true }`.

#### How it fits together

- `/login <provider_id>`: the harness runs `login`, emits the URL as an
  `oauth_prompt`, and either waits for the redirect (web), invokes `complete`
  immediately (automatic polling), or stashes `pending` for `/oauth-code`
  (manual). On success it creates the provider config (name = provider_id,
  your base_url/kind/headers, no api_key) and refreshes `/models`.
- At turn + discovery time: the harness runs `token` (cached), injects the
  access token as `Authorization: Bearer`, merges any returned `headers`, and
  routes the turn to your `base_url` over your declared `kind`.
- `/logout <provider_id>`: deletes `token_path` + runs `clear` + drops the
  provider config.

The plugin's token format is entirely its own — the harness never parses it.

### Example: a pre_write linter plugin

`.catalyst-code/plugins/lint-check/plugin.json`:
```
{
  "name": "lint-check",
  "version": "0.1.0",
  "description": "Run cargo fmt on Rust files before writing",
  "hooks": {
    "pre_write": {
      "script": "hooks/pre_write.sh",
      "timeout_ms": 10000,
      "pass_args": true
    }
  }
}
```

`.catalyst-code/plugins/lint-check/hooks/pre_write.sh`:
```bash
#!/bin/bash
input=$(cat)
path=$(echo "$input" | jq -r '.args.path // ""')
content=$(echo "$input" | jq -r '.args.content // ""')

if [[ "$path" == *.rs ]] && command -v rustfmt &>/dev/null; then
  formatted=$(echo "$content" | rustfmt --edition 2021 2>/dev/null)
  if [ $? -eq 0 ] && [ -n "$formatted" ]; then
    jq -n --arg c "$formatted" '{ "allow": true, "reason": "rustfmt applied", "modify": { "content": $c } }'
    exit 0
  fi
fi
echo '{"allow": true}'
```

Remember: `chmod +x .catalyst-code/plugins/lint-check/hooks/pre_write.sh`
