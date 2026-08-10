# Configuration Reference

Catalyst Code configuration is layered from multiple sources with a clear
precedence. Most settings can be set via CLI flag, environment variable, or
JSON config file.

---

## Precedence

Config is merged in this order (later sources override earlier ones):

1. **Defaults** (hardcoded in `Config::default()`)
2. **`managed-settings.d/*.json`** — auto-generated, lowest priority overrides
3. **`managed-settings.json`** — auto-generated overrides
4. **`~/.config/catalyst-code/settings.json`** — user-global config
5. **`<workspace>/settings.json`** — project-scoped config
6. **`<workspace>/settings.local.json`** — project-local overrides (gitignored)
7. **Environment variables** — `CATALYST_CODE_*`, `UMANS_*`
8. **CLI flags** — highest precedence

The config system is defined in `core/src/config.rs` (/core/src/config.rs).
There is no TOML or YAML — only JSON.

### Config File Names

The loader scans these files (in precedence order):

```text
<workspace>/settings.json
<workspace>/settings.local.json
~/.config/catalyst-code/settings.json
managed-settings.json
managed-settings.d/*.json
```

Arrays are concatenated + deduplicated; objects are deep-merged; a `null` value
deletes a key from the merged result.

Config file paths can also be overridden via `--config <FILE>`.

---

## CLI Flags and Environment Variables

All flags and their equivalent environment variables:

| Flag | Env Var | Default | Description |
|------|---------|---------|-------------|
| `--workspace <DIR>` | `CATALYST_CODE_WORKSPACE` | Current directory | Workspace root (constrains all file/bash ops) |
| `--base-url <URL>` | `UMANS_BASE_URL` | `https://api.code.umans.ai/v1` | OpenAI-compatible base URL |
| `--approval <MODE>` | `CATALYST_CODE_APPROVAL` | `destructive` | `never` / `destructive` / `always` |
| `--bash-timeout <SECS>` | `CATALYST_CODE_BASH_TIMEOUT` | `30` | Per-command bash timeout in seconds |
| `--max-bash-timeout <SECS>` | `CATALYST_CODE_MAX_BASH_TIMEOUT` | `600` | Ceiling for the bash tool's per-call `timeout` override |
| `--fetch-timeout <SECS>` | `CATALYST_CODE_FETCH_TIMEOUT` | `20` | Wall-clock timeout for the `fetch` tool |
| `--diag-timeout <SECS>` | `CATALYST_CODE_DIAG_TIMEOUT` | `120` | Diagnostics tool timeout (cargo check / tsc / go build) |
| `--sandbox <MODE>` | `CATALYST_CODE_SANDBOX` | `none` | `none` / `microsandbox` (runs agent workloads in a Microsandbox microVM) |
| `--no-network` | `CATALYST_CODE_NO_NETWORK=1` | `false` | Block guest network egress (Microsandbox network policy) |
| `--trust-project-plugins` | `CATALYST_CODE_TRUST_PROJECT_PLUGINS=1` | `false` | Load project-scoped plugins (`.catalyst-code/plugins`). Off by default for safety. **Cannot** be set from config files — only env/CLI. |
| `--idle-timeout <SECS>` | `CATALYST_CODE_IDLE_TIMEOUT` | `120` | SSE idle timeout in seconds |
| `--max-session-tokens <N>` | `CATALYST_CODE_MAX_SESSION_TOKENS` | `0` (unlimited) | Hard session token budget |
| `--debug-log <FILE>` | `CATALYST_CODE_DEBUG_LOG` | None (off) | Structured JSONL debug log (rotates at 64 MiB) |
| `--debug` | `CATALYST_CODE_DEBUG=1` | off | Full-verbosity mode: HTTP bodies, tool args/outputs, protocol command/event mirror. Defaults a debug log path when unset. |
| `--session <FILE>` | `CATALYST_CODE_SESSION` | None (off) | Append-only JSONL session file (resume on restart) |
| `--model <ID>` | — | None | Default model ID |
| `--provider <NAME>` | `UMANS_ACTIVE_PROVIDER` | None | Active model provider (see `providers` in config) |

### Env-Only Settings (no CLI flag)

| Env Var | Default | Description |
|---------|---------|-------------|
| `CATALYST_CODE_FETCH_MAX_BYTES` | `262144` (256 KiB) | Max response body for the `fetch` tool |
| `CATALYST_CODE_FETCH_ALLOWLIST` | `""` (allow any host) | Comma-separated host glob patterns for `fetch` tool allowlist |
| `CATALYST_CODE_AUTO_REFLECT` | `true` | Auto-inject reflection continuation after non-trivial turns |
| `CATALYST_CODE_AUTO_REFLECT_MIN_TOOL_CALLS` | `1` | Minimum tool-call count for auto-reflect to fire |
| `CATALYST_CODE_AUTO_COMPACT` | `true` | Automatically compact context near the limit |
| `CATALYST_CODE_COMPACT_INSTRUCTIONS` | None | Optional guidance woven into the summarize prompt |
| `UMANS_PROVIDERS` | `[]` | JSON array of provider configs (see Provider Configuration below) |

---

## Config File Format

Settings files are JSON with this structure:

```json
{
  "providers": [
    {
      "name": "my-endpoint",
      "kind": "openai",
      "base_url": "https://api.example.com/v1",
      "api_key": "sk-...",
      "api_key_env": "MY_API_KEY",
      "extra_headers": {
        "HTTP-Referer": "my-app"
      }
    }
  ],
  "settings": {
    "approval": "destructive",
    "bash_timeout": 60,
    "sandbox": "none",
    "no_network": true,
    "sandbox_image": "ghcr.io/catalystctl/catcode-sandbox:0.1",
    "sandbox_cpus": 2,
    "sandbox_memory_mb": 2048,
    "sandbox_network_mode": "restricted"
  }
}
```

The `providers` array defines named endpoints. The `settings` object holds
all other config keys. Keys named exactly as the CLI flag (e.g. `"approval"`,
`"bash_timeout"`).

---

## Approval Modes

Defined by the `Approval` (/core/src/config.rs) enum:

| Value | Meaning |
|-------|---------|
| `never` (or `off` / `none` / `auto`) | Auto-approve everything — no prompts. Path confinement disabled; model is fully trusted. |
| `destructive` (default) | Ask only for `Destructive`-classified tools (bash, write_file, edit, …). |
| `always` (or `all` / `y`) | Ask for every tool call. |

---

## Sandbox Modes

Defined by the `Sandbox` (/core/src/config.rs) enum:

| Value | Effect |
|-------|--------|
| `none` (default) | No sandboxing; denylist tripwire only. Commands run on the host. |
| `microsandbox` (or `msb` / `on` / `true` / `enabled`) | Run agent workloads (bash, git, diagnostics, plugin scripts, subagent commands) inside a Microsandbox microVM. Linux via KVM, Apple Silicon macOS, Windows via WHP. |

### Legacy value migration

Old `firejail` / `fj` / `seatbelt` / `macos` / `sandbox-exec` values are still
accepted for backward compatibility and migrate to `microsandbox` (never to
`none`) with a deprecation notice. The user's intention to enable sandboxing is
preserved; if the environment cannot run Microsandbox, CatCode fails closed.

### Sandbox configuration

See the [Sandbox Guide](../guides/sandbox.md) for platform setup, network
policy, environment-variable policy, and troubleshooting. Key fields:

| Field | Default | Description |
|-------|---------|-------------|
| `sandbox_image` | `ghcr.io/catalystctl/catcode-sandbox:0.1` | OCI image the guest boots from |
| `sandbox_cpus` | `2` | vCPUs |
| `sandbox_memory_mb` | `2048` | Guest RAM (MiB) |
| `sandbox_disk_mb` | `8192` | Writable guest disk (MiB) |
| `sandbox_idle_timeout_secs` | `900` | Idle teardown timeout |
| `sandbox_network_mode` | `restricted` | `none` / `restricted` / `allowlist` |
| `sandbox_network_allowlist` | `[]` | Hosts permitted under `allowlist` |
| `sandbox_allow_private_networks` | `false` | Permit RFC 1918 / link-local |
| `sandbox_env_allowlist` | `[]` | Extra env vars forwarded to the guest |

All values are validated (positive/capped CPUs, safe min/max memory & disk,
non-empty image, valid env var names).

---

## Advisor and WATCHDOG files

The advisor is an optional second-model reviewer. It is fail-open: provider
errors, missing credentials, malformed advice, and malformed watchdog files do
not stop the executor. Advisor output is bounded, secret-redacted, XML-escaped,
deduplicated, and injected only as advisory context. Advisors never execute
tools, approve actions, or mutate files.

```json
{
  "advisor": {
    "enabled": true,
    "model": "claude-sonnet-4-5",
    "subagents": true,
    "subagentModel": "claude-haiku-4-5"
  }
}
```

`model` defaults to the executor model. Subagents use `subagentModel`, then
`model`, then their executor model. `/advisor` configures enablement and these
model choices through the TUI.

### Evidence pack and structured notes

On natural completion (no tool calls), each enabled watchdog receives a bounded
**evidence pack**, not a raw chat dump:

- user request (latest real user message)
- rolling work state (goal, todos, recent files)
- unified diffs from successful mutating tools this turn (redacted, capped)
- recent assistant/user claims (tool result bodies omitted for secret safety)

Accepted notes use a structured schema when the reviewer cooperates:

```text
SEVERITY: concern
FINDING: rename drops partial failure
WHERE: core/src/fsutil.rs
ACTION: handle io::Error after rename
EVIDENCE: diff removed the Err arm
CHECK: cargo test fsutil --lib
```

Legacy free-form bodies after `SEVERITY:` still parse. Each accepted note is
emitted as `advisor_note` (with optional `finding` / `where` / `action` /
`check` fields) and injected as:

```xml
<advisory advisor="Architecture" severity="concern" scope="main"
  where="core/src/fsutil.rs" check="cargo test fsutil --lib">
rename drops partial failure
ACTION: handle io::Error after rename
</advisory>
```

### Soft-continue (main scope only)

When at least one **concern** or **blocker** is accepted on the main agent, the
harness injects a single `[advisor-continue]` user message and re-enters the
turn loop **once** so the executor can act before `done`. Nits alone never
force a continue. Subagent reviews still inject `<advisory>` messages into the
child history but do not soft-continue (parent finalize stays terminal).

Concern/blocker notes are also pinned as a transient **open advisory** on later
model requests. A subsequent mutation of the note's `WHERE` path resolves the
pin; a later review can reopen it if the issue remains. Exact repeats are
suppressed, while a concern/blocker may supersede an earlier lower-severity
note for the same reviewer/location (history capped at 4096 entries).

### WATCHDOG.md

`WATCHDOG.md` contains reviewer-only guidance: project traps, architectural
boundaries, dangerous APIs, and review priorities. It is never injected into
the executor's ordinary prompt. The loader searches the user advisor directory
(`~/.catalyst-code/WATCHDOG.md`), project ancestors, and
`<project>/.catalyst-code/WATCHDOG.md`. A line containing `@relative/path` is
expanded from the watchdog file's directory; missing imports remain literal.

### WATCHDOG.yml

`WATCHDOG.yml` or `.yaml` defines named reviewers:

```yaml
instructions: Check public API changes and test coverage.
advisors:
  - name: Architecture
    enabled: true
    model: claude-sonnet-4-5
    tools: [read, grep, glob]
    triggers: [core/src, protocol]
    instructions: Watch coupling and unintended API growth.
  - name: Security
    enabled: true
    model: claude-sonnet-4-5
    instructions: Watch trust boundaries and secret handling.
```

Named reviewers are independent side calls. An explicit `model` wins over the
role model. Duplicate names are resolved by discovery order, with more-specific
project files loaded after broad user/ancestor files. `enabled: false` pauses a
reviewer. `triggers` limits a specialist to matching touched paths/diffs. The
harness may execute up to three requested `tools` from the strict
`read`/`read_file`/`grep`/`glob` allowlist and append bounded, redacted results
to the evidence pack; watchdog models still cannot call tools themselves.
Eligible named reviewers run in parallel. Setting `advisor.nudge` enables one
additional review checkpoint after the first successful mutation in a turn.

---


## Provider Configuration

### Config File Format

Each provider in the `providers` array:

```json
{
  "name": "my-provider",
  "kind": "openai",
  "base_url": "https://api.example.com/v1",
  "api_key": "sk-123...",
  "api_key_env": "MY_API_KEY_ENV_VAR",
  "extra_headers": {
    "Header-Name": "Header-Value"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Unique provider name |
| `kind` | string | yes | `"openai"` (default) or `"anthropic"` — determines wire protocol translation |
| `base_url` | string | yes | API endpoint base URL |
| `api_key` | string | no | Literal API key (stored in user-owned config file only — never in project-local files) |
| `api_key_env` | string | no | Name of an env var to read the key from at request time (e.g. `"OPENAI_API_KEY"`) |
| `extra_headers` | object | no | Additional HTTP headers appended to every request |

If both `api_key` and `api_key_env` are present, the runtime key (set via
`/login` or `set_key`) wins, then `api_key`, then `api_key_env`. If none are
set, the provider is unsigned.

### Provider Presets

Built-in provider presets for quick setup:

| ID | Label | Kind | Base URL | Key Env Var |
|----|-------|------|----------|-------------|
| `umans` | Umans (GLM-5.2) | OpenAI | `https://api.code.umans.ai/v1` | `UMANS_API_KEY` |
| `opencode-go` | OpenCode Go | OpenAI | `https://opencode.ai/zen/go/v1` | `OPENCODE_GO_API_KEY` |
| `openrouter` | OpenRouter | OpenAI | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `deepseek` | DeepSeek | OpenAI | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` |

OpenCode Go expands into **two** provider configs (OpenAI-kind + Anthropic-kind)
because it serves models over both wire protocols under one API key.

### Provider Resolution

At request time, a `ResolvedProvider` (/core/src/config.rs) is built by
applying:

1. Runtime keys (set via `/login` / `set_key` or OAuth) → highest priority
2. Config literal `api_key`
3. Config `api_key_env` → read from env var
4. Fallback: legacy `base_url` + runtime key named `"default"`

When no providers are configured, the harness uses the legacy single-endpoint
mode (`cfg.base_url` + the `"default"` runtime key).

### Provider Presets from Env

Providers can also be injected via the `UMANS_PROVIDERS` env var as a JSON
array (same schema as the config file `providers` array):

```bash
export UMANS_PROVIDERS='[{"name":"custom","kind":"openai","base_url":"https://my-api/v1","api_key_env":"MY_API_KEY"}]'
```

---

## Permission Rules

Per-tool, per-content matching rules with allow / deny / ask behavior. Format:

```
ToolName(ruleContent)
```

Parsed by `parse_permission_rule()` (/core/src/config.rs). Examples:

```
Bash(npm test)
Edit(//src/**)
ReadFile(.env)
```

Three rule lists in config:

| List | Behavior |
|------|----------|
| `allow_rules` | Matching calls bypass the approval gate (auto-allow). |
| `deny_rules` | Matching calls are rejected before execution. |
| `ask_rules` | Matching calls always prompt (even under Approval::Never). |

---

## Core Config Fields

All config fields with their types and defaults.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `base_url` | string | `https://api.code.umans.ai/v1` | OpenAI-compatible base URL |
| `workspace` | string | Current directory | Workspace root |
| `approval` | enum | `destructive` | `never` / `destructive` / `always` |
| `bash_timeout_secs` | number | `30` | Per-command bash timeout |
| `max_bash_timeout_secs` | number | `600` | Ceiling for bash `timeout` arg |
| `diag_timeout_secs` | number | `120` | Diagnostics timeout |
| `fetch_timeout_secs` | number | `20` | Fetch tool timeout |
| `fetch_max_bytes` | number | `262144` | Max fetch response body |
| `fetch_allowlist` | array | `[]` | Host glob patterns for fetch (empty = allow any) |
| `bash_deny` | array | `["rm -rf /", "rm -rf ~", "mkfs", "dd if=...", ":(){...}"]` | Bash denylist tripwire |
| `bash_deny_regex` | array | `[]` | Regex patterns blocking bash commands |
| `max_read_bytes` | number | `5242880` (5 MiB) | Max file size for `read_file` |
| `max_read_lines` | number | `10000` | Max lines for `read_file` |
| `context_compact_at` | number | `0.90` | Fraction of context window triggering compaction |
| `context_digest_at` | number | `0.70` | Fraction triggering stale-tool-result digest |
| `auto_compact` | bool | `true` | Automatically compact when approaching limit |
| `debug_log` | string/null | `null` (off) | Path to JSONL debug log |
| `debug_verbose` / `--debug` | bool | `false` | Full-verbosity debug mode (`catcode --debug`) |
| `audit_log` | bool | `false` | Append-only security audit sidecar |
| `session_file` | string/null | `null` | Path to JSONL session file |
| `default_model` | string/null | `null` | Default model ID |
| `sandbox` | enum | `none` | `none` / `microsandbox` (legacy `firejail`/`seatbelt` migrate to `microsandbox`) |
| `no_network` | bool | `false` | Block guest network egress (Microsandbox policy) |
| `idle_timeout_secs` | number | `120` | SSE idle timeout |
| `max_session_tokens` | number | `0` (unlimited) | Hard session token budget |
| `summarize_on_compact` | bool | `true` | Use model call to summarize dropped turns |
| `compact_instructions` | string/null | `null` | Guidance for the summarize prompt |
| `rolling_state` | bool | `true` | Inject transient tail work-state summary |
| `auto_reflect` | bool | `true` | Auto-inject reflection after non-trivial turns |
| `auto_reflect_min_tool_calls` | number | `1` | Minimum tool calls for auto-reflection |
| `allow_vision` | bool | `true` | Accept `image_url` content in send |
| `plugin_dir` | string | `.catalyst-code/plugins` | Directory scanned for plugins |
| `plugins_disabled` | array | `[]` | Plugin names explicitly disabled |
| `trust_project_plugins` | bool | `false` | Load project-scoped plugins (env/CLI only) |
| `providers` | array | `[]` | Named model provider configs |
| `active_provider` | string/null | `null` | Active provider name |
| `search_keys` | object | `{}` | Search-tool API keys (Exa / Tavily) |

### Subagent Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `subagents.max_depth` | number | `2` | Max nesting depth (0 blocks all subagents) |
| `subagents.intercom_bridge_mode` | enum | `always` | `off` / `fork-only` / `always` |
| `subagents.parallel_max_tasks` | number | `8` | Max tasks in a parallel run |
| `subagents.parallel_concurrency` | number | `4` | Default concurrency for parallel runs |
| `subagents.async_by_default` | bool | `false` | Top-level calls use background execution |
| `subagents.disable_builtins` | bool | `false` | Hide builtin agents from discovery |

### Routing Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `routing.enabled` | bool | `true` | Enable task-aware model routing |
| `routing.fast_roles` | array | `["scout", "researcher", "context-builder"]` | Agents preferring cheap/fast models |
| `routing.strong_roles` | array | `["worker", "reviewer", "oracle", "planner"]` | Agents preferring strong models |
| `routing.fast_markers` | array | `["haiku", "flash", "mini", "small", "fast", "lite", "nano"]` | Model ID substrings for "fast" |
| `routing.strong_markers` | array | `["opus", "sonnet", "pro", "max", "large", "ultra", "glm-5"]` | Model ID substrings for "strong" |

---

## Config File Locations

The harness searches for config files in this order (first found wins for each
layer):

1. `<workspace>/settings.json` — project config (checked into VCS)
2. `<workspace>/settings.local.json` — local overrides (gitignored)
3. `~/.config/catalyst-code/settings.json` — user-global config
4. `managed-settings.json` — auto-generated by the TUI
5. `managed-settings.d/*.json` — auto-generated fragments

The `--config <FILE>` flag points to an additional file that is loaded after
the defaults but before env/CLI.

---

## Security Notes

- **`trust_project_plugins`** is intentionally never read from any config file.
  It can only be set via `--trust-project-plugins` or the
  `CATALYST_CODE_TRUST_PROJECT_PLUGINS` env var. This prevents an untrusted
  repository from shipping a `settings.json` that enables its own plugin hooks.
- **`api_key`** values in config files should only be stored in user-owned
  files (not project-scoped settings). Project configs should use
  `api_key_env` to reference an environment variable instead.
- The **`bash_deny`** list is a tripwire, not a security boundary. Use
  `--sandbox microsandbox` for real isolation (a separate microVM kernel +
  filesystem root). See [Sandbox Guide](../guides/sandbox.md).
- **`debug_log`** is the structured JSONL path (metrics, tool hashes, errors).
  Off by default for bare `core`; the TUI always passes a path. Rotates at 64 MiB.
- **`--debug` / `debug_verbose`** additionally records full tool arguments and
  outputs, provider HTTP request/response bodies, inbound commands, and a
  mirror of protocol events. Secrets (api keys, tokens) are still redacted;
  individual payloads are capped at 64 KiB. Enable only when debugging
  (`catcode --debug` or `CATALYST_CODE_DEBUG=1`).
