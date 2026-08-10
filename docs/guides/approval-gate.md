# Safety and Approval System

Catalyst Code uses a three-tier safety model: **tool classification**,
**workspace confinement**, and **sandbox isolation**. The approval gate sits
between the model and side-effectful operations, ensuring a human can review
every destructive action before it executes.

---

## Table of Contents

- [Approval Modes](#approval-modes)
- [Tool Classification](#tool-classification)
- [Permission Rules](#permission-rules)
- [Restricted Path Protection](#restricted-path-protection)
- [Workspace Confinement](#workspace-confinement)
- [Sandbox Modes](#sandbox-modes)
- [No-Network Mode](#no-network-mode)
- [Bash Denylist](#bash-denylist)
- [The Approval Gate Flow](#the-approval-gate-flow)
- [Runtime Mode Switching](#runtime-mode-switching)

---

## Approval Modes

The approval mode controls which tool calls require a human prompt before
execution. Set via `--approval` / `CATALYST_CODE_APPROVAL` / config file
`"approval"` key.

Defined by the `Approval` (/core/src/config.rs) enum (line 8):

| Mode | CLI / Config Value | Effect |
|------|--------------------|--------|
| **Never** | `never`, `off`, `none`, `auto` | Auto-approve everything. No prompts. Path confinement and restricted-path protection are **also disabled** — the model is fully trusted. |
| **Destructive** (default) | `destructive` (or any unrecognized value) | Prompt only for tools classified as `Destructive` (bash, write_file, edit, delete, …). Read-only tools execute immediately. |
| **Always** | `always`, `all`, `y` | Prompt on **every** tool call ��� including reads. Useful for review-heavy workflows or untrusted models. |

### Warning: Approval::Never

Under `Approval::Never`, the approval gate is skipped (no prompts). On the
host, path confinement via `resolve_unconfined` (/core/src/workspace.rs) skips
confinement checks — absolute paths, `..` traversal, and symlink escapes are
**not** rejected there. Only use this mode when the model is fully trusted.

**Important:** `Approval::Never` is independent of the sandbox. When the
Microsandbox sandbox is enabled, file confinement and credential isolation are
**still enforced** — `Approval::Never` means "do not prompt," not "allow
arbitrary host filesystem access." See the [Sandbox Guide](sandbox.md).

---

## Tool Classification

Every tool is classified by `classify()` (/core/src/tools.rs) (line ~44) into
one of two `ToolKind` (/core/src/tools.rs) values:

| Class | Gate | Tools |
|-------|------|-------|
| `ReadOnly` | Never gated (executes immediately) | `read_file`, `list_dir`, `grep`, `glob`, `bulk_read`, `todo_read`, `diagnostics`, `finish`, `contact_supervisor`, `intercom`, `git_status`, `git_diff`, `git_log`, `git_show`, `memory`, `knowledge`, `load_tools`, `ask`, `web_search`, `workspace_activity`, `goal_write_plan`, plus browser read-only tools (`browser_list_sessions`, `browser_snapshot`, `browser_find`, `browser_screenshot`) |
| `Destructive` | Gated under `Approval::Destructive` — prompts user before executing | Everything else: `bash`, `edit`, `write_file`, `delete`, `rename`, `mkdir`, `bulk_write`, `bulk_edit`, `patch`, `git_add`, `git_commit`, `subagent`, `spawn`, `test_env`, `todo_write`, `bulk`, all browser navigation/interaction tools (`browser_navigate`, `browser_click`, `browser_fill`, `browser_type`, `browser_press`, `browser_scroll`, `browser_wait`, `browser_evaluate`, `browser_create`, `browser_close`, `browser_reload`, `browser_back`, `browser_show`, `browser_hide`) |

Source: `classify()` (/core/src/tools.rs), line 44.

### Note on `web_search` and `fetch`

Although `web_search` and `fetch` make network requests, they are classified
as `ReadOnly` because they do not mutate the local system. Network egress can
be blocked independently via `--no-network`.

---

## Permission Rules

Beyond the global approval mode, **per-tool, per-content** rules allow, deny,
or force-ask for specific tool calls. Rules are defined in config files under
three lists.

### Rule Format

```
ToolName(ruleContent)
```

Parsed by `parse_permission_rule()` (/core/src/config.rs), line 66.

| Component | Meaning | Example |
|-----------|---------|---------|
| `ToolName` | Exact tool name | `Bash`, `Edit`, `ReadFile`, `WriteFile` |
| `ruleContent` | Glob pattern or command substring | `npm test`, `//src/**`, `.env` |

### Rule Lists

| Config Key | Behavior |
|------------|----------|
| `allow_rules` | Matching calls **bypass** the approval gate and execute immediately (even under `Approval::Always`) |
| `deny_rules` | Matching calls are **rejected** before execution (tool returns an error) |
| `ask_rules` | Matching calls **always prompt** (even under `Approval::Never`) |

### Examples

```json
{
  "allow_rules": ["Bash(npm test)", "Bash(^cargo test)"],
  "deny_rules": ["Bash(rm -rf)", "Edit(/etc/"],
  "ask_rules": ["ReadFile(.env)"]
}
```

Rule content matching is done against the tool's primary path argument (for
file tools) or the command string (for bash). Glob patterns use the same
matching as the restricted-path system.

### How Rules Interact

The evaluation order for each tool call is:

1. **Deny rules** checked first — if any deny rule matches, the call is rejected.
2. **Allow rules** — if any allow rule matches, execution proceeds without
   prompting (regardless of approval mode).
3. **Ask rules** — if any ask rule matches, a prompt is forced.
4. **Fallback to approval mode** — if no rule matched, the global approval mode
   (`Never` / `Destructive` / `Always`) applies.

Source: Main loop approval gate logic in `main.rs` (/core/src/main.rs), around
line ~5390.

---

## Restricted Path Protection

Certain files are dangerous to read or write — VCS internals, shell/SSH configs,
secret files. The restricted-path system flags these paths and forces an
approval prompt (under `Destructive` / `Always`) before the tool executes.

### Dangerous Paths

Defined in `DANGEROUS_PATHS` (/core/src/workspace.rs), line 25:

| Pattern | Rationale |
|---------|-----------|
| `.git/**` | VCS internals — corrupting `.git` loses history |
| `**/.bashrc` | Shell config — could execute arbitrary code on next login |
| `**/.bash_profile` | Shell config |
| `**/.profile` | Shell config |
| `**/.zshrc` | Shell config |
| `**/.ssh/**` | SSH keys and authorized_hosts |
| `**/.gnupg/**` | GPG keys |
| `**/id_rsa` | SSH private key |
| `**/id_ed25519` | SSH private key |
| `**/.env` | Environment secrets (API keys, tokens) |
| `**/.env.local` | Local environment secrets |
| `**/.env.production` | Production environment secrets |

Restricted paths that exist only on case-insensitive filesystems (macOS,
Windows) are also caught: `.GIT/config`, `.SSH/`, `.ENV`, etc.

Source: Cases verified in `workspace.rs` tests (/core/src/workspace.rs), line
~249.

### Which Tools Are Checked

The `restricted_path_for_tool()` function (in `main.rs` (/core/src/main.rs),
line ~4309) checks the following tools for restricted paths:

- `read_file`, `write_file`, `edit`, `patch`
- `bulk_read`, `bulk_write`, `bulk_edit`
- `bulk` (checks each inner call)
- `delete`, `rename`, `mkdir`

Tools that are **not** checked by restricted-path protection:

- `bash` — because it doesn't read a single file by path (the denylist tripwire
  provides a weak guard instead)
- `grep`, `glob`, `list_dir` — search/list over directory trees, not single files

Source: `restricted_path_for_tool()` tests (/core/src/main.rs), line ~10050.

### Symlink Escapes

If a restricted directory (e.g., `.git`) is symlinked from outside the
workspace, reading or writing through the symlink alias is still flagged
because the canonical path is re-checked. Source: test at line ~10136 in
`main.rs`.

### Under Approval::Never

When `Approval::Never` is active, restricted-path protection is **completely
disabled** — no files are flagged and the model can read/write any path.
Source: `main.rs` (/core/src/main.rs), line ~5392.

---

## Workspace Confinement

Every file operation is confined to the workspace root (the current directory
or the `--workspace` flag value).

### Confinement Rules

The `resolve()` (/core/src/workspace.rs) function (line ~117) enforces:

1. **No absolute paths** — `/etc/passwd`, `C:\Windows`, etc. are rejected.
2. **No `..` traversal** — `../etc/passwd`, `a/../../b` are rejected.
3. **No symlink escapes** — the resolved canonical path must start with the
   canonical workspace root. Symlinks pointing outside the workspace are
   detected by incremental canonicalization of the path prefix.
4. **Non-existent paths** — for write/create operations, the existing prefix is
   canonicalized incrementally so a symlinked intermediate directory that points
   outside the workspace is resolved and rejected before the write.

Source: `resolve()` (/core/src/workspace.rs), line 117.

### Unconfined Mode

Under `Approval::Never`, `resolve_unconfined()` (/core/src/workspace.rs) (line
~230) skips all confinement checks. The model is fully trusted and can reach
any path the process has access to. Absolute paths are returned as-is.

---

## Sandbox Modes

Sandboxes provide **real isolation** for agent workloads (unlike the denylist
tripwire, which is easy to bypass). Defined by the `Sandbox`
(/core/src/config.rs) enum.

| Mode | CLI Value | Platform | Effect |
|------|-----------|----------|--------|
| `none` (default) | `none` | all | No sandboxing. Commands run on the host. Denylist tripwire only. |
| `microsandbox` | `microsandbox`, `msb`, `on`, `true`, `enabled` | Linux (KVM), Apple Silicon macOS, Windows (WHP) | Run bash, git, diagnostics, plugin scripts, and subagent commands inside a Microsandbox microVM (separate kernel + filesystem root). |

Set via `--sandbox` / `CATALYST_CODE_SANDBOX` / config `"sandbox"` key.
Dynamically changeable at runtime via `set_config` `"sandbox"`.

### Legacy value migration

Old `firejail` / `fj` / `seatbelt` / `macos` / `sandbox-exec` values migrate to
`microsandbox` (never to `none`) with a deprecation notice. The user's intention
to enable sandboxing is preserved; if the environment cannot run Microsandbox,
CatCode fails closed.

### What the microVM isolates

When `sandbox: microsandbox` is active, agent-controlled workloads boot inside a
lightweight Linux guest (driven by the official `microsandbox` Rust SDK, pinned).
No external `msb` CLI, Docker, Podman, Firejail, WSL, or daemon is required —
the SDK downloads its own runtime on first use. The guest:

- has its own kernel and filesystem root (drawn from the sandbox image);
- mounts only the workspace, writable, at `/workspace`;
- does **not** inherit the host environment (secrets denied by default);
- does **not** mount the host home, `~/.ssh`, or the Docker/SSH-agent sockets.

See the [Sandbox Guide](sandbox.md) for platform setup, network policy,
environment-variable policy, and troubleshooting.

---

## No-Network Mode

`--no-network` / `CATALYST_CODE_NO_NETWORK=1` / config `"no_network": true`

When enabled (or `sandbox_network_mode: none`), **guest network egress is
fully blocked** through the Microsandbox network policy — no `unshare`, no
host firewall reliance.

1. **Agent workloads** (bash, git, diagnostics, plugins) run with no network.
2. **`fetch` tool** and **`web_search` tool** still work on the host control
   plane (the core makes these requests directly, not through the guest). They
   are governed by `--no-network` intent and the `fetch_allowlist` allowlist.
3. The `fetch` host allowlist (`fetch_allowlist`) can further restrict which
   hosts the non-bash tools can reach.

Source: Config field at `config.rs` (/core/src/config.rs).

---

## Bash Denylist

The bash denylist is a **tripwire**, not a security boundary. It blocks the
most obviously catastrophic commands using simple substring matching. For real
isolation, use a sandbox.

### Default Denylist (String Patterns)

```rust
// From Config::default() at core/src/config.rs, line 814
bash_deny: vec![
    "rm -rf /",          // Wipes the filesystem root
    "rm -rf ~",          // Wipes the home directory
    "mkfs",              // Formats filesystems
    "dd if=",            // Low-level disk writes (partial)
    ":(){ ... }",        // Fork bombs (partial)
]
```

The denylist checks the **normalized command string** (whitespace-collapsed).
Leading whitespace variants like "  rm -rf /" still match because the tool
normalizes whitespace before checking.

Source: `execute_bash()` (/core/src/tools.rs), line ~2265.

### Custom Denylist

Extend or replace the denylist via config:

```json
{
  "bash_deny": ["rm -rf /", "dd if=", "reboot", "shutdown"],
  "bash_deny_regex": ["^sudo .*rm", "curl\\s+http://evil"]
}
```

The `bash_deny_regex` field accepts regex patterns compiled at startup and
matched against the whitespace-normalized command. Both denylists are tripped
before any sandbox or execution.

### What the Denylist Can't Block

The denylist is trivially bypassed by any determined model — it only catches
exact substrings. Examples of evasions:

```bash
rm -rf /var/empty/../   # traverses to /
rm -rf "$HOME"           # variable expansion
/bin/rm -rf /            # full path
```

Use a sandbox (`microsandbox`) when you need real containment.

---

## The Approval Gate Flow

When a tool call is received, the following sequence runs (simplified from the
orchestrator loop in `main.rs` (/core/src/main.rs)):

```
Tool call received
  ↓
[1] Check deny rules → if match → reject with error
  ↓
[2] Check restricted path → if match → force prompt (under Destructive/Always)
  ↓
[3] Check allow rules → if match → execute immediately (skip gate)
  ↓
[4] Check approval mode:
      Never → execute immediately (no path confinement either)
      Destructive → gate only Destructive-classified tools
      Always → gate every tool call
  ��
[5] Send `approval_request` event to the UI:
    {"type": "approval_request", "request_id": "...",
     "tool": "...", "args": "..."}
  ↓
[6] Wait for user decision:
      "yes" → execute
      "no" → return denied result
      "always" → remember "always allow" for this session
      "allow_session" → auto-allow for rest of session
      "allow_pattern" → auto-allow matching this path pattern
      "aborted" → abort the entire turn
```

### Approval Request Event

The `approval_request` event carries:

| Field | Description |
|-------|-------------|
| `request_id` | Unique ID for this approval prompt |
| `tool` | Tool name (e.g., `"bash"`, `"write_file"`) |
| `args` | JSON string of the tool's arguments |
| `diff` | For `edit`/`write_file`/`patch`: a unified diff preview of the change |

The UI renders the prompt and the optional diff, then sends an `approve` command
back with the user's decision.

### Approve Command

```json
{
  "type": "approve",
  "request_id": "req-123",
  "decision": "yes",
  "pattern": "//src/**"
}
```

| Decision | Behavior |
|----------|----------|
| `"yes"` | Execute this one call |
| `"no"` | Deny this call |
| `"always"` | Allow all tools for this session (like Approval::Never) |
| `"allow_session"` | Allow **this tool** for the rest of the session |
| `"allow_pattern"` | Allow calls matching `pattern` (a path/command glob) for the session |

The `pattern` field is used only with `allow_pattern`; it defaults to the
tool's path argument if omitted.

Source: `Command::Approve` (/core/src/protocol.rs), line ~195.

---

## Runtime Mode Switching

The approval mode can be changed at runtime without restarting:

```json
{"type": "set_approval", "mode": "always"}
```

Valid `mode` values: `"never"`, `"destructive"`, `"always"`.

Emits an `approval_changed` event with the new mode. Source: main.rs line ~2598.

Config knobs like `sandbox`, `no_network`, `bash_timeout_secs`, and
`auto_compact` can also be changed at runtime via `set_config`:

```json
{"type": "set_config", "key": "sandbox", "value": "microsandbox"}
```

Emits a `config_changed` event. Source: main.rs line ~2658.

---

## Summary: Safety Layers

| Layer | What It Blocks | Bypassable By Model? | Under Approval::Never? |
|-------|---------------|----------------------|------------------------|
| Tool classification | All destructive tools (default) | No (gate is mandatory) | Disabled |
| Permission rules | Specific tool+content combos | No (enforced before gate) | Allow/deny rules still apply; ask rules still apply |
| Restricted-path protection | .git, .ssh, .env, etc. | No (flagged before gate) | Disabled on host; still enforced inside the sandbox |
| Workspace confinement | Absolute paths, `..`, symlink escapes | No (enforced in `resolve()`) | Disabled on host; **still enforced when the sandbox is on** |
| Bash denylist | Catastrophic command substrings | **Yes** (trivial to bypass) | Still active |
| Sandbox (microsandbox) | Filesystem + network isolation (microVM) | No (OS-enforced) | Still active |
| No-network | Guest network egress (Microsandbox policy) | No (OS-enforced) | Still active |
