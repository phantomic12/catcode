---
name: parallel-codebase-review
description: Fan out many parallel reviewer subagents to comprehensively review a whole codebase, then synthesize findings
---

# Parallel Whole-Codebase Review

Use when asked to "review the whole codebase" / "audit everything" / find bugs across all components, and you want maximum parallelism via subagents.

## When to use
- Full-codebase review/audit request.
- You want N focused reviewers (e.g. one per module) rather than one giant review.

## When NOT to use
- A targeted review of one file/feature — just read it and review directly.
- Fewer than ~3 review units — single `reviewer` subagent suffices.

## Steps

1. **Map structure + sizes first.** `list_dir` each component, then `wc -l` the files sorted descending. Split the largest component (often Rust core) into multiple reviewers by file/group so no single reviewer drowns. Rule of thumb: ≤1.5k LOC per reviewer for depth.

2. **Soft default, not a hard reject.** The `subagent` tool's parallel `tasks` mode uses a soft default max of **8** (`parallel_max_tasks`). Larger batches are allowed — they queue under the concurrency semaphore (only `concurrency` run at once) and emit an info note. Set `concurrency` to the batch size when you want them all running together. Absolute safety max is 256 tasks. (Single-mode has no task-count cap but blocks one-at-a-time.)

3. **Each task = fresh context, so be self-contained.** Give every reviewer: the exact files to read (with approx LOC), the focus areas, the output contract (`file:line` evidence + severity critical/high/medium/low + suggested fix), and "be thorough and evidence-based." Don't assume it inherited anything from your conversation.

4. **Pick the model per the request.** Pass `model: "<id>"` on each task (per-task model override is supported: `{agent, task, model?}`). Verify the model resolves first with ONE cheap single-mode test (`task: "Reply with the single word OK"`) — if it fails, the model likely isn't in the discovered-models cache; the failure is instant (0.0s, no runs).

5. **Dispatch in one parallel call when practical**, or split only if you prefer smaller waves. Each parallel call returns concatenated `=== Parallel Task N (agent) ===` blocks. Prefer setting `concurrency` to the number of tasks so they run together rather than queue behind the default concurrency (4).

6. **Synthesize, don't dump.** Dedupe related findings across reviewers, group by severity (not by reviewer), and rank. Keep full detail in a written report file (e.g. `REVIEW.md`); give the user a tight exec summary + pointer.

7. **Verify surprising Criticals before reporting.** Reviewer models can drift on line numbers. For any Critical claim that is surprising/high-stakes, re-read the cited lines yourself before asserting it. This converts "the reviewer said" into "verified."

8. **Remember the soft default** (`parallel-subagent-cap` memory): default advisory max is 8, but explicit larger batches + higher `concurrency` are honored (absolute max 256).

## Example shape (this repo)
12 reviewers → one parallel call with `concurrency: 12`, all `model: "ck-grok-4.5"`:
- 6 Rust-core reviewers (main.rs / tools.rs / provider.rs / subagent+intercom / plugins+config / smaller modules)
- 3 Go-TUI reviewers (dispatch+lifecycle / modal+keybinds / rendering)
- 1 SDK, 1 web, 1 build/CI/Docker
Output → `REVIEW.md` with Critical/High/Medium/Low + a prioritized fix list; exec summary in chat.
