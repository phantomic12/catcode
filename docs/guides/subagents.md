# Subagent Guide

Subagents are **nested agentic loops** — focused child agents that share the
workspace, tools, and API key with the parent session but run with a tailored
system prompt and optional tool allowlist. They enable structured delegation:
scout for recon, plan before coding, review after changes, and more.

The subagent system is a port of [pi-subagents](https://github.com/nicobailon/pi-subagents),
adapted to the single-process Rust core.

---

## How Subagents Work

A subagent is launched via the `subagent` tool inside the parent agent's turn.
The core creates a new agentic loop (`run_agent` in `subagent.rs`) that:

1. Constructs a system prompt from the agent's definition (frontmatter + body)
2. Optionally **forks** the parent's conversation context (read-only reference)
   or starts **fresh** (empty conversation)
3. Applies the agent's **tool allowlist** (only listed tools are available)
4. Runs a full turn loop: model streaming → tool calls → results → repeat
5. Emits subagent progress events so the UI can render status
6. Returns the final output to the parent agent

Subagents share the **same core process** — no subprocess, no serialization
overhead. Coordination between subagents and the parent uses in-process
mailboxes (`intercom.rs`).

```
Parent agent loop
  │
  ├─► subagent({ agent:"scout", task:"Map the API surface", context:"fork" })
  │     └─► Scout runs, returns context.md
  │
  ├─► subagent({ agent:"planner", task:"Plan the changes", context:"fork" })
  │     └─► Planner returns plan.md
  │
  └─► subagent({ agent:"worker", task:"Implement step 1" })
        └─��� Worker writes code
```

**Source:** `core/src/subagent.rs`, header comment lines 1–18.

---

## Built-in Agents

Eight agents are built into the core. Each has a dedicated system prompt, tool
allowlist, and default configuration. Agents can be overridden by placing a
file with the same name in `.catalyst-code/agents/<name>.md`.

### Scout

| Field | Value |
|-------|-------|
| **Purpose** | Fast codebase recon that returns compressed context for handoff |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `write_file`, `memory`, `intercom` |
| **Thinking** | `low` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |
| **Default output** | `context.md` |
| **Default reads** | None |

The scout moves fast using targeted search and selective reading. Its output
format is `context.md` with sections for Files Retrieved, Key Code,
Architecture, and Start Here. Use it when entering an unfamiliar area of the
codebase.

**Source:** `SCOUT_PROMPT` in `core/src/subagent.rs`

### Researcher

| Field | Value |
|-------|-------|
| **Purpose** | Web/docs research with sources and a concise research brief |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `write_file`, `memory`, `intercom`, `fetch`, `web_search` |
| **Thinking** | `low` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |

The researcher gathers external evidence: official docs, specs, benchmarks, and
recent changes. Returns source links, confidence levels, gaps, and decision
implications. Use it when the task requires knowledge outside the repository.

**Source:** `RESEARCHER_PROMPT` in `core/src/subagent.rs`

### Planner

| Field | Value |
|-------|-------|
| **Purpose** | A concrete implementation plan from existing context; reads and plans, does not edit |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `intercom` |
| **Thinking** | `high` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |
| **Default context** | Fork |

The planner produces a concrete, actionable implementation plan from supplied
context. It reads but never edits files. The plan includes goals, affected
files, step-by-step changes, risks, validation steps, and open questions.
Inherited forked context is treated as reference-only — the planner does not
continue prior conversations.

**Source:** `PLANNER_PROMPT` in `core/src/subagent.rs`

### Worker

| Field | Value |
|-------|-------|
| **Purpose** | Implementation agent for normal tasks and approved oracle handoffs |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `edit`, `write_file`, `contact_supervisor` |
| **Thinking** | `high` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |
| **Default context** | Fork |
| **Default reads** | `context.md`, `plan.md` |
| **Default progress** | Yes |

The worker is the single writer thread. It executes the assigned task with
narrow, coherent edits. It reads supplied context/plan first, does not add
speculative scaffolding, and uses `contact_supervisor` for blocking decisions.
The response shape includes: `Implemented X`, `Changed files: Y`,
`Validation: Z`, `Open risks/questions: R`, `Recommended next step: N`.

**Source:** `WORKER_PROMPT` in `core/src/subagent.rs`

### Reviewer

| Field | Value |
|-------|-------|
| **Purpose** | Code review and small fixes against the task/plan, tests, edge cases, simplicity |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `edit`, `write_file`, `intercom` |
| **Thinking** | `high` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |
| **Default reads** | `context.md`, `plan.md` |

The reviewer inspects implementation vs intent, correctness/edge-cases, test
coverage, unintended side effects/regressions, and simplicity/readability.
Returns evidence-backed findings with file/line references. Makes small fixes
only when asked.

**Source:** `REVIEWER_PROMPT` in `core/src/subagent.rs`

### Context-Builder

| Field | Value |
|-------|-------|
| **Purpose** | Stronger setup pass before planning: gathers context and writes handoff material |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `write_file`, `memory`, `intercom` |
| **Thinking** | `low` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |

The context-builder does a deeper reconnaissance pass than the scout: reads
every relevant file, follows imports/callers/tests/docs/config, and writes
handoff material (e.g. `context.md`) plus a compact meta-prompt.

**Source:** `CONTEXT_BUILDER_PROMPT` in `core/src/subagent.rs`

### Oracle

| Field | Value |
|-------|-------|
| **Purpose** | High-context decision-consistency oracle; challenges assumptions, prevents drift |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `intercom` |
| **Thinking** | `high` |
| **System prompt mode** | Replace |
| **Inherit project context** | Yes |
| **Default context** | Fork |

The oracle prevents the main agent from making hidden, conflicting, or
inconsistent decisions. It treats inherited forked context as the authoritative
contract. It does not edit files. Output shape includes: Inherited decisions,
Diagnosis, Drift/contradiction check, Recommendation, Risks, Need from main
agent, Suggested execution prompt.

**Source:** `ORACLE_PROMPT` in `core/src/subagent.rs`

### Delegate

| Field | Value |
|-------|-------|
| **Purpose** | Lightweight general delegate that behaves close to the parent session |
| **Tools** | `read_file`, `grep`, `glob`, `list_dir`, `bash`, `edit`, `write_file`, `contact_supervisor` |
| **Thinking** | None (inherits parent's) |
| **System prompt mode** | Append |
| **Inherit project context** | Yes |

The delegate is the most flexible agent — it appends to the parent's system
prompt rather than replacing it. Use it for general sub-tasks that don't need a
specialized role. Unlike the worker, the delegate does not have `context.md` /
`plan.md` default reads and does not report progress by default.

**Source:** `DELEGATE_PROMPT` in `core/src/subagent.rs`

### Summary Table

| Agent | Edits Files | Uses Intercom | Default Context | Default Reads | Thinking |
|-------|:-----------:|:-------------:|-----------------|---------------|----------|
| scout | Write only | ✓ | Fresh | — | low |
| researcher | Write only | ✓ | Fresh | — | low |
| planner | No | ✓ | Fork | — | high |
| worker | ✓ | contact_supervisor | Fork | context.md, plan.md | high |
| reviewer | ✓ | ✓ | Fresh | context.md, plan.md | high |
| context-builder | Write only | ✓ | Fresh | — | low |
| oracle | No | ✓ | Fork | — | high |
| delegate | ✓ | contact_supervisor | Fresh | — | none |

---

## Custom Agent Definitions

Agents are defined as **markdown files with YAML frontmatter** in
`.catalyst-code/agents/<name>.md`. Discovery precedence (project wins on name):

1. **Builtin** (embedded in core binary) — lowest priority
2. **User** (`~/.catalyst-code/agents/`) — overrides builtins
3. **Project** (`<workspace>/.catalyst-code/agents/`) — highest priority

### File Format

```markdown
---
name: my-agent
description: Custom code reviewer specialized in SQL
tools: read_file, grep, glob, list_dir, edit, write_file
model: claude-sonnet-4
thinking: medium
systemPromptMode: replace
inheritProjectContext: true
defaultContext: fork
maxSubagentDepth: 3
completionGuard: false
disabled: false
---

You are a custom agent specialized in SQL review.
Analyze query performance, indexing, and normalization.
Use contact_supervisor if the task is ambiguous.
```

### Frontmatter Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | — | Agent name (slug; must match filename) |
| `description` | string | ��� | One-line description shown in `/subagents-list` |
| `tools` | comma-separated | — | Tool allowlist. Supports pi-name mapping (`read`→`read_file`, `find`→`glob`, `ls`→`list_dir`, `write`→`write_file`) |
| `model` | string | parent's | Model override for this agent |
| `fallbackModels` | comma-separated | — | Fallback model chain if primary fails |
| `thinking` | string | — | Reasoning effort or thinking level (`low`, `medium`, `high`) |
| `systemPromptMode` | `replace` or `append` | `replace` | Replace parent system prompt or append to it |
| `inheritProjectContext` | bool | `false` | Inherit project context from parent |
| `inheritSkills` | bool | `false` | Inherit skills from parent |
| `defaultContext` | `fork` or `fresh` | `fresh` | Default context kind for this agent |
| `maxSubagentDepth` | int | unlimited | Maximum nesting depth for this agent's subagents |
| `completionGuard` | bool | `false` | Enforce structured output completion |
| `output` | string | — | File path for output (scout→`context.md`) |
| `defaultReads` | comma-separated | — | Files to read before starting (worker→`context.md,plan.md`) |
| `defaultProgress` | bool | `false` | Emit progress events by default |
| `disabled` | bool | `false` | Skip this agent during discovery |

**Source:** `core/src/subagent.rs` — `parse_frontmatter()` (lines 24–69),
`discover_agents()` (line 316), `AgentConfig` struct (lines 113–130).

---

## Execution Modes

The `subagent` tool supports three execution modes plus management actions.

### Single Mode

Run one agent with one task:

```ts
subagent({
  agent: "worker",
  task: "Add input validation to the login form"
})
```

Options:
- `context`: `"fresh"` (default) or `"fork"` — start empty or branch from parent
- `model`: override the model for this run
- `async`: boolean — run in background (non-blocking for the parent)

### Parallel Mode

Run multiple agents concurrently:

```ts
subagent({
  tasks: [
    { agent: "scout", task: "Map the API routes" },
    { agent: "scout", task: "Review the database schema" }
  ],
  concurrency: 2,
  worktree: true,       // isolate in git worktrees
  context: "fresh"
})
```

Options:
- `concurrency`: max parallel tasks running at once. Defaults to config
  (`subagents.parallel.concurrency`, default 4) when omitted; **explicit
  higher values are honored** (clamped only by task count and absolute max 256).
- Task count soft default: `subagents.parallel.maxTasks` (default 8) is
  advisory — larger batches queue under concurrency instead of failing.
- `worktree`: boolean — isolate each task in a git worktree
  (`.catalyst-code/worktrees/<run_id>/`). Requires a git repo.
- `context`: applied to all tasks

### Chain Mode

Run sequential steps where each step can reference the previous output:

```ts
subagent({
  chain: [
    { agent: "scout", task: "Map the codebase", as: "scout_out" },
    { agent: "planner", task: "Plan from {previous}", as: "plan" },
    { agent: "worker", task: "Implement {outputs.plan}", async: true },
    { agent: "reviewer", task: "Review the result" }
  ]
})
```

Chain template substitutions:
- `{previous}` — full output of the prior step
- `{outputs.<name>}` — output of a step tagged with `as`
- `{task}` — the step's task string
- `{chain_dir}` — temp directory for ephemeral chain state

Options:
- `async`: run the entire chain in background
- `context`: applied to all steps
- Steps can contain `parallel: [...]` groups inside a chain step

### Management Mode

The `subagent` tool also provides runtime management actions (see below).

**Source:** `core/src/subagent.rs` — `run_agent()` (line 1261), `run_parallel()` (line 2620),
`run_chain()` (line 2863). Tool definition: `core/src/tools.rs` lines 216–230.

---

## Management Commands

Subagent management actions let you inspect and control running subagents.

### List Agents

```ts
subagent({ action: "list" })
```

Returns all discovered agents with name, source, and description. Shows
builtin, user, and project agents.

### Get Agent

```ts
subagent({ action: "get", agent: "worker" })
```

Returns the full agent configuration for a named agent.

### Create / Update / Delete

```ts
subagent({ action: "create", agent: "my-agent", config: { ... } })
subagent({ action: "update", agent: "my-agent", config: { ... } })
subagent({ action: "delete", agent: "my-agent" })
```

Management of custom agent definitions. Builtins cannot be deleted (override
with `disabled: true`).

### Status

```ts
subagent({ action: "status" })                        // all runs
subagent({ action: "status", id: "run_abc123" })      // specific run
```

Shows run state, elapsed time, and intercom target for each running or recent
subagent.

### Interrupt

```ts
subagent({ action: "interrupt", id: "run_abc123" })
```

Cancels a running subagent. The cancellation is cooperative — the subagent
stops at the next safe point.

### Resume

```ts
subagent({ action: "resume", id: "run_abc123", message: "Continue with the new API endpoint" })
```

Delivers a follow-up message to a completed or interrupted subagent (via its
intercom target). If the run is no longer live, suggests starting a new run.

### Peek

```ts
subagent({ action: "peek", id: "run_abc123" })
```

Inspect a running subagent's conversation state — shows recent messages,
current tool call, and status.

### Steer

```ts
subagent({ action: "steer", id: "run_abc123", message: "Focus on error handling first" })
```

Inject a message into a running subagent's conversation. The subagent will
process the steering message as a user input in its next turn.

### Doctor

```ts
subagent({ action: "doctor" })
```

Runs setup diagnostics: shows agent count, max subagent depth, intercom bridge
mode, and environment checks.

**Source:** `core/src/subagent.rs` ��� `handle_action()` (line 3085), `peek_action()` (line 3122),
`steer_action()` (line 3167), `status_action()` (line 3347), `interrupt_action()` (line 3363),
`resume_action()` (line 3379), `doctor_action()` (line 3458). Tool definition: `core/src/tools.rs`
lines 240–243 (action enum).

---

## Slash Commands

The TUI provides these slash commands for working with subagents:

| Command | Equivalent | Description |
|---------|------------|-------------|
| `/run <agent> "<task>"` | `subagent({agent, task})` | Run a single agent |
| `/parallel <a1> "<t1>" \| <a2> "<t2>"` | `subagent({tasks: [...]})` | Parallel execution |
| `/chain <a1> "<t1>" -> <a2> "<t2>"` | `subagent({chain: [...]})` | Chain execution |
| `/subagents` or `/subagents-list` | `subagent({action:"list"})` | List available agents |
| `/subagents-status` | `subagent({action:"status"})` | Show running subagents |
| `/subagents-doctor` | `subagent({action:"doctor"})` | Run diagnostics |
| `/subagents-models` | `subagent({action:"models"})` | Show model mapping for builtin agents |

The `/run`, `/parallel`, and `/chain` commands accept a bare agent name (opens
a modal for the task string) or the full form with quoted task. Bare commands
with no remainder open a value-edit modal.

**Source:** `tui/handlers.go` — `runSubagentCommand()` (line 1414), slash dispatch
(lines 2686–2699).

---

## Intercom Bus

The intercom system enables communication between subagents and the orchestrator
(parent session). It is implemented as in-process mailboxes (`intercom.rs`).

### contact_supervisor

Available to subagents that have `contact_supervisor` in their tool allowlist
(worker, delegate, scout, context-builder).

```ts
contact_supervisor({
  reason: "need_decision",
  message: "Should I refactor the auth module or leave it as-is?"
})
```

- `reason: "need_decision"` — blocking: the subagent pauses and waits for the
  orchestrator's reply. Surfaces as an `intercom_message` event in the TUI/web.
- `reason: "progress_update"` — non-blocking: reports progress and returns
  immediately.

The orchestrator replies via `intercom_reply` in the TUI or through the parent
agent's response. The subagent blocks up to 300 seconds (5 minutes) for a
`need_decision` reply, then resumes with a timeout error.

### intercom

Available when the intercom bridge mode is not "off" and the agent's resolved
tools include `intercom`.

```ts
intercom({
  action: "send",       // fire-and-forget
  to: "scout-1",        // peer target name
  message: "Found the API router in src/routes.rs"
})

intercom({
  action: "ask",        // blocking request-reply
  to: "reviewer-2",
  message: "Is there a test for the auth flow?"
})
```

Actions:

| Action | Behavior |
|--------|----------|
| `send` | Fire-and-forget to a peer's mailbox |
| `ask` | Blocking request — waits for a reply (up to 300s) |
| `receive` / `poll` | Read your own mailbox |
| `reply` | Answer a pending ask (quote the `id`) |
| `targets` | List known peer targets |

**Source:** `core/src/intercom.rs` (631 lines). `contact_supervisor` schema:
`core/src/tools.rs` lines 256–267. `intercom` schema: `core/src/tools.rs`
lines 269–281.

---

## Context Types

When launching a subagent, you can choose how its conversation context is
initialized:

### Fresh (`context: "fresh"`)

The subagent starts with an **empty conversation**. No parent messages are
visible. The agent receives only its system prompt and task. This is the
default for most agents (scout, researcher, reviewer, context-builder, delegate).

### Fork (`context: "fork"`)

The subagent receives the parent's conversation as **read-only context**. The
forked context is injected into the system prompt area so the agent can refer
to prior decisions, but it cannot continue the parent's conversation. The
forked context is treated as reference-only — the agent does not respond to
prior messages.

Default for: planner, worker, oracle.

The parameter is set per-call:

```ts
subagent({ agent: "worker", task: "...", context: "fork" })
```

Or in the agent's frontmatter default:

```yaml
defaultContext: fork
```

The resolved context is `Fresh` if neither the parameter nor the agent default
specifies `Fork`.

**Source:** `core/src/subagent.rs` — `parse_context()` (line 1103), `ContextKind` enum
(line 18).

---

## Worktree Isolation

When running parallel subagents that write files, **git worktree isolation**
prevents writers from clobbering each other. Set `worktree: true` on a parallel
or goal deployment with `concurrency > 1` for mutating agents.

Each task gets a linked worktree under `.catalyst-code/worktrees/<run_id>/`.
Changes are promoted to the main working tree on success (via
`promote_worktree`). Non-git workspaces cannot use worktree isolation — the
system returns a clear error pointing to checkpoint mode instead.

Worktree operations are serialized (process-wide mutex) to prevent races on
`.git` locks.

**Source:** `core/src/worktree.rs` (536 lines).

---

## Escalation Flow

Subagents escalate to the parent orchestrator when they need a decision or want
to report progress:

1. **Subagent calls `contact_supervisor({ reason: "need_decision", message })`**
2. The core emits an `intercom_message` event with the `ask_id`
3. The TUI/web surfaces the question to the user
4. The user (or parent agent) replies via `intercom_reply` with decision text
5. The reply is delivered to the subagent's intercom mailbox
6. The subagent resumes with the reply as input

If the subagent cannot wait (e.g., the task requires an immediate answer), the
300-second timeout fires and the subagent continues with an explicit timeout
error in its conversation.

**Source:** `core/src/intercom.rs` — `PendingAsk`, `INTERCOM_ASK_TIMEOUT`,
`execute_contact_supervisor`.

---

## Depth Limiting

Subagents have a configurable maximum nesting depth to prevent runaway
delegation. The effective depth is the minimum of:

- The global `max_subagent_depth` in config
- The individual agent's `maxSubagentDepth` frontmatter field
- A hard default of 5 (if neither is set)

When the depth limit is reached, the subagent tool returns an error instead of
entering a new nested loop.

**Source:** `core/src/subagent.rs` — `resolve_max_depth()`, `child_max_depth()`.

---

## Agent Discovery

Agents are discovered at runtime from three sources, in ascending priority:

1. **Builtin** — hardcoded in `builtin_agents()` within `subagent.rs`
2. **User** — `~/.catalyst-code/agents/<name>.md`
3. **Project** — `<workspace>/.catalyst-code/agents/<name>.md`

On name collision, the higher-priority source wins. Builtins can be disabled
fully via the `disable_builtins` config flag.

Each agent file is parsed by `parse_frontmatter()` which extracts flat
YAML-like key-value pairs (no nested YAML — `key: value` only). The body text
after the closing `---` becomes the agent's system prompt.

**Source:** `core/src/subagent.rs` — `discover_agents()` (line 316),
`parse_frontmatter()` (line 24).

---

## Practical Examples

### Code Review Workflow

```text
/run scout "Map the authentication module"
/parallel planner "Plan auth refactoring" | worker "Implement the plan"
/run reviewer "Review the auth refactoring"
```

### Research → Plan → Implement → Review

```ts
// Programmatic (via subagent tool)
subagent({
  chain: [
    { agent: "researcher", task: "Research best practices for OAuth token rotation" },
    { agent: "planner", task: "Plan the implementation", as: "plan" },
    { agent: "worker", task: "Implement per {outputs.plan}" },
    { agent: "reviewer", task: "Review the changes" }
  ]
})
```

### Parallel Exploration

```ts
subagent({
  tasks: [
    { agent: "scout", task: "Map all API routes" },
    { agent: "scout", task: "Map the database schema" },
    { agent: "scout", task: "Map the React component tree" }
  ],
  concurrency: 3,
  context: "fresh"
})
```

### Escalation Mid-Task

A worker subagent needs a decision:

```
Worker: contact_supervisor({
  reason: "need_decision",
  message: "The migration has two approaches: 1) in-place ALTER TABLE (fast, locks table)
            2) new table + backfill (safe, slow). Which should I use?"
})

User (or parent): "Use strategy 2 — this is a production database."
```

**Sources:** All examples are derived from the subagent tool definition in
`core/src/tools.rs` and the system prompts in `core/src/subagent.rs`.

---

## See Also

- [Architecture Overview](../architecture/index.md) — core subsystems
- [Wire Protocol Reference](../architecture/protocol.md) — subagent events
- [Configuration Reference](../configuration/index.md) — subagent config keys
- [Approval Gate Guide](./approval-gate.md) — security model
- [Provider Login Guide](./providers-login.md) — multi-provider auth
