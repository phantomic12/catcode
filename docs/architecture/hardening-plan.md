# Architecture hardening plan

Status: active hardening record, verified against the repository on 2026-07-18.

## Current architecture

The core is a single JSONL subprocess. `main.rs` owns command dispatch, session
replacement, model turns, approval waits, tool waves, goal orchestration, and
most cancellation. Protocol output is called from many modules through
`protocol::emit`. Provider and tool behavior remain concentrated in
`provider.rs` and `tools.rs`.

Measured baseline:

- `main.rs`: 10,669 lines before this work
- `provider.rs`: 7,850 lines
- `tools.rs`: 5,921 lines
- `plugins.rs`: 5,157 lines
- `subagent.rs`: 3,666 lines
- `goal.rs`: 3,338 lines

There was no subprocess-level protocol integration harness under `core/tests`.
The TUI, SDK, and web reducer already ignored unknown events safely, and the
session loader already had a version header plus a future-version rejection
test. Those are useful compatibility foundations, not substitutes for run
ownership.

## Verified lifecycle map

Before the coordinator was introduced, `send` stored a bare
`CancellationToken` in `State.current` and a detached join handle in
`State.handle`. `steer` cancelled that token and relied on the old turn's drain
to launch its queued successor. `abort`, `reset`, `clear`, `new_session`, and
`load_session` shared a helper that only cancelled the token and cleared the
one-deep queue. It did not await cleanup or invalidate event output.

Approvals, asks, and sudo prompts each had separate pending maps and notifiers.
Goal deploy had a separate cancellation token. Subagents stored optional
cancellation tokens in a run map. Intercom owned independent mailboxes and
pending asks. Tool and provider tasks spawned additional Tokio tasks, while
bash, plugin hooks, OAuth helpers, browser support, checkpointing, and worktree
support spawned subprocesses through separate paths.

Consequently, session replacement could swap conversation and persistence state
while old work was still unwinding, and there was no identity gate preventing a
late event from becoming visible in the replacement session.

## Implementation sequence

1. Add characterization tests for legacy init and terminal event behavior.
2. Make `RuntimeCoordinator` authoritative for session/run identity and
   cancellation; ensure stale finishers cannot clear newer runs.
3. Route protocol output through `EventSink`, attach v2 metadata and sequence,
   redact secrets, and reject inactive run scopes.
4. Move all session-bound waiters, child tasks, subprocesses, subagents, goal
   workers, browser resources, and intercom queues into coordinator-owned
   registries with bounded cleanup.
5. Add real-subprocess tests for streaming, approval, tool execution, abort,
   session replacement, and late-result rejection.
6. Split protocol commands/events/version/schema, then reduce `main.rs` into
   startup and transport wiring around command and agent modules.
7. Introduce provider adapters and fixture-normalized stream events.
8. Split tool schema, classification, policy, approval, scheduling, execution,
   and result status; prohibit unregistered spawning.
9. Harden approval scopes, subagent ancestry, goal transitions, session crash
   recovery, and plugin capability/resource limits.
10. Add runtime diagnostics, structured tracing, memory evaluation fixtures,
    cross-language protocol conformance, architecture gates, and final docs.

Each phase must keep legacy JSONL commands/session files usable and must add
coverage for the behavior it changes. Cosmetic file movement is deferred until
ownership is established.

## Implemented safeguards

- Explicit process-local `session_id` and per-turn `run_id` generation.
- Coordinator cancellation and stale-finisher invariants.
- Bounded turn-task cleanup shared by abort/reset/clear/new/load/shutdown.
- Session replacement invalidation before state replacement.
- Cancellation of goal tokens, tracked subagent token trees, approval/ask/sudo
  waits, and intercom mailboxes on lifecycle boundaries.
- Central event sink with protocol v2 metadata, per-run ordering, redaction,
  and stale-run rejection.
- Backward-compatible init capability negotiation in Rust, Go, and TypeScript.
- Coordinator-owned resource leases for foreground runs, goal work, approvals,
  asks, and sudo waits, exposed through the read-only `runtime_status` command.
- Approval decisions bound to session, run, request, tool-call, tool name, and
  argument digest; expired, replayed, mismatched, and stale decisions are denied.
- Append-only session run records, automatic v1-to-v2 header migration, truncated
  tail recovery, and safe `interrupted` terminal records after a crash. Optional
  activity kind, parent-run, and tool-call identities distinguish interrupted
  foreground, tool, subagent, and goal work without automatically resuming it.
- A real core-subprocess harness exercising negotiation (including unknown
  capabilities), abort and session replacement during streams and bash,
  stale-stream rejection, approval grant/denial/reset invalidation, ordered
  readonly and write waves, provider retry/fatal errors, subsequent-turn
  health, resource cleanup, direct subagent and goal-worker cancellation,
  compaction, plugin timeout, cross-client fixtures, and crash recovery against
  loopback providers. Its 21 integration tests cover all 23 required scenario
  categories; related cases are intentionally combined.
- Stable tool-result statuses and a central built-in metadata catalog for risk,
  access, parallel safety, approval, timeout/cancellation, capabilities, and
  redaction.
- Narrow OpenAI, Anthropic, Codex Responses, and Google Code Assist adapters
  with request builders, normalized stream events, redacted error taxonomy,
  and a shared bounded incremental SSE decoder. Offline fixtures cover text,
  reasoning, usage, multiple tools, arbitrary fragmentation, truncated streams,
  rate/auth/server/context errors, discovery shapes, and vision requests.
- Plugin protocol version 1, explicit capability validation for new manifests,
  inferred minimum capabilities for legacy manifests, per-invocation timeouts,
  cancellation-safe child handling, and 1 MiB input/output limits enforced by
  concurrent bounded pipe readers rather than post-capture size checks.
- Bounded intercom mailboxes/messages/pending asks and cancellation on endpoint
  removal.
- Protocol command fixtures consumed by Rust and Go, a JSON Schema/catalog
  consistency checker, exhaustive versioned fixtures for all 59 commands and
  89 events, and CI gates for Rust, Go, TypeScript, protocol drift,
  architecture-regression baselines, and an optimized core artifact smoke test.
- Deterministic synthetic-repository memory evaluation fixtures for path,
  symbol, architecture, preference, scope, stale/contradictory facts, failure
  patterns, irrelevant suppression, and retrieval-token budgets; score
  components and optional session/run/creation provenance remain inspectable.
- Command transport dispatch, the agent turn loop, compaction policy, and goal
  orchestration extracted from `main.rs`; provider discovery/usage and protocol
  adapters extracted from `provider.rs`; tool schemas, metadata, policy,
  approval decisions, scheduling, git, and memory built-ins extracted from
  `tools.rs`.
- Structured per-call tool execution contexts carry workspace,
  session/run/tool identity, cancellation, approval, central event access,
  restricted secret access, configuration, parent identity, and runtime
  resource registration. Late sequential or parallel results are discarded
  before state mutation.
- Readonly waves use owned concurrent futures rather than detached Tokio tasks;
  cancellation therefore drops every in-flight executor and kills owned
  subprocesses. A real two-process diagnostics regression verifies cleanup.
- `cancel_goal` uses unified lifecycle cleanup, and speculative goal scouts are
  session-scoped resources. Cancelled subagents retain an explicit `cancelled`
  state rather than being mislabeled as failed.
- Goal transitions are validated against an explicit state table and invalid
  jumps return a structured `invalid_goal_transition` error without mutation.
  Goal workers have a bounded total runtime derived from the provider idle
  timeout, cancel their child token on expiry, and receive cleanup grace time.
- Subagent events and status retain `parent_run_id`; foreground children point
  to the owning run and goal workers/scouts point to the goal ID. Subagent and
  goal deploy lifecycle records are also written to the session journal.
- Structured logs attach active session/run identity, hash rather than copy tool
  arguments, recursively redact credential fields, and record turn, tool,
  approval-wait, and plugin-hook durations and terminal statuses.
- An explicit bounded Tokio worker-stack size fixes the debug/instrumented-build
  stack overflow found by the subprocess suite.

## Required subprocess scenario matrix

`core/tests/protocol_harness.rs` launches the compiled core over JSONL and uses
loopback providers and real temporary workspaces. The required scenarios map to
tests as follows (combined tests deliberately prove more than one condition):

| Required scenario | Harness test |
| --- | --- |
| Basic text response; context compaction | `basic_text_turns_can_be_compacted_with_ordered_events` |
| Tool call and result; destructive approval | `approval_executes_destructive_tool_with_bound_identity` |
| Approval denied | `approval_denial_has_stable_status_and_does_not_execute_tool` |
| Abort during streaming | `abort_during_stream_cancels_old_run_and_allows_next_turn` |
| Abort during bash | `abort_during_bash_cancels_subprocess_resource_promptly` |
| New session during streaming | `new_session_during_stream_rejects_old_deltas_and_remains_usable` |
| New session during tool execution; late old tool result | `new_session_during_bash_terminates_old_process_and_new_session_works` |
| Reset during approval | `reset_during_approval_invalidates_request_and_prevents_execution` |
| Multiple readonly tools in parallel | `multiple_readonly_tools_complete_as_one_ordered_wave` |
| Sequential writes | `multiple_writes_execute_sequentially_in_model_order` |
| Subagent cancelled with parent | `aborting_parent_turn_cancels_direct_subagent` |
| Goal workers cancelled | `cancelling_goal_cancels_planner_and_speculative_subagent` |
| Provider retry then success | `provider_retry_then_success_preserves_event_order` |
| Provider fatal error | `provider_authentication_error_is_fatal_and_redacted` |
| Session crash recovery | `startup_reports_and_terminalizes_interrupted_session_run` |
| Plugin timeout | `plugin_tool_timeout_is_bounded_and_reported_with_stable_status` |
| Unknown client capability | `unknown_client_capability_is_tolerated_during_negotiation` |
| TUI, web, and SDK fixtures | `checked_in_client_and_event_fixtures_match_real_core_negotiation` |

Additional wave tests prove member-failure isolation and cancellation of two
owned subprocesses without late results. Provider adapter unit fixtures cover
malformed/truncated chunks, disconnects, arbitrary chunk fragmentation, retry
classification, tool-call normalization, usage, reasoning, and vision request
shapes.

## Remaining decomposition debt

The three original megamodules are materially smaller, delegate to named
boundaries, and remain below their no-growth warning ceilings, but they are
still large enough to benefit from incremental extraction. The command
dispatcher is also 2.6k lines and warrants per-command modules. The
process/task-spawn baseline was
lowered and remapped after extraction. It prevents new spawn sites outside
reviewed ownership boundaries; the readonly wave's formerly detached scheduler
spawn has been removed. Remaining spawn sites are explicit executor/bootstrap
implementations and are covered by outer runtime leases plus cancellation/drop
cleanup when they belong to a run.


## Agent OS roadmap implementation status

Issue #8 is the source of truth for the staged Agent OS program. The current
repository has landed the P0a correctness slice (cancelled subagents do not
promote worktrees, writes require an explicit content value, and streamed tool
indices are bounded), plus browser scheme lockdown, bounded sandbox output,
plugin install-root validation, UTF-8-safe indexing, and memory append metadata
preservation. Remaining P0/P1/P2/P3 work must retain these invariants and land
with targeted regression coverage; no safety claim should depend on an
unimplemented roadmap item.

### Design contracts for the next slices

**Async jobs and hub.** A job is identified by `session_id`, `run_id`, and
`parent_run_id`; its durable record lives below the session directory and moves
through `queued`, `running`, `done`, `failed`, or `cancelled`. Completion writes
an artifact before emitting a parent-delivery event. Cancellation is terminal
and never promotes a worktree. Hub messaging builds on `IntercomBus`; wait and
cancel operate on the durable job id and return the terminal record.

**Session tree and compaction.** Additive entry fields `id` and `parentId` keep
existing JSONL readable. A leaf pointer selects the active branch; `/tree`
enumerates ancestry and siblings, while `/branch` creates a child from a
selected entry without rewriting prior entries. Compaction is an append-only
entry containing the retained summary and artifact references; display history
remains non-destructive. Shake may replace oversized tool output with an
artifact reference only after the original is durably stored.

**Deferred IDE tools.** New IDE tools belong in `tooling/` and are exposed only
through `load_tools`; diagnostics/refs are read-only and parallel-safe, while
rename/edit operations are sequential and use the existing approval policy.
`ast_edit` uses an embedded ast-grep 0.44 engine with bundled Rust, Go,
JavaScript, TypeScript/TSX, Python, JSON, YAML, and Markdown grammars. It has
bounded single-file input, dry-run diffs, atomic apply, workspace confinement,
and no external `ast-grep` executable dependency.

## Verification status for issue #8

Verified on 2026-08-08 and rechecked locally:
- Core: `cargo test --locked` — 1,036 unit tests and 25 protocol harness tests passed; 3 tests ignored.
- Core formatting: `cargo fmt --all -- --check` passed.
- Core lint: `cargo clippy --all-targets` completed without errors.
- TUI: `gofmt`, `go vet ./...`, `go test ./...`, and `go build ./...` passed.
- Web: protocol and architecture consistency checks, `bun run typecheck`, `bun run lint`, and `bun test` passed (180 tests).
- SDK: `bun run typecheck` and `bun test` passed.

Implemented issue slices: P0 trust/correctness hardening; durable job status/wait/cancel and artifact delivery; bounded intercom coordination; project-scoped named process supervision with readiness, logs, stop, restart, and stale recovery; append-only session entries, active-leaf navigation, ancestry/sibling tree output, branch summaries, compaction checkpoints, and artifact shake retention; replay-bounded eval lifecycle; deferred IDE tools including embedded ast-grep structural rewrites; MCP client with trusted user-only transports; multi-root `skill://` resolution; provider/extensibility scaffolding; and TUI/web job-tree and plugin-trust surfaces.

The session tree contract is deliberately non-destructive: `session_branch` appends a durable message-less branch entry under the selected node, makes that entry the active leaf, and the next normal session append becomes its child. Marker entries are excluded from reconstructed model history, so branching never injects synthetic content. The web hub exposes authenticated session links that reopen the same authorized workspace/session on another signed-in device; the URL is not a bearer credential. Its process panel exposes observed process state and requests logs or a stop through the existing approved process-tool path. No issue #8 acceptance gap remains in the current local implementation; the remaining architecture warnings concern decomposition debt, not missing program-level behavior.
