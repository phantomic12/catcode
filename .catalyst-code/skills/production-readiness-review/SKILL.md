---
name: production-readiness-review
description: Audit a repo for production readiness before going public — fan out code reviewers + a secrets/PII scan + a build/test gate, then synthesize a go/no-go verdict
---

# Production-Readiness Review (Pre-Public Launch)

Use when the user is about to make a repo **public** (or cut a release) and asks
"is this ready?" / "production readiness" / "can we go public?". Distinct from a
general code-quality review (`parallel-codebase-review`) — this adds the three
things that specifically matter for *going public*: a **secrets/PII audit**, a
**build + test + lint gate**, and an explicit **go/no-go verdict**.

## When to use
- "Is the repo ready to go public / open-source?"
- Pre-release gate before flipping a repo from private to public.
- "Production readiness review" of a whole codebase.

## When NOT to use
- Targeted review of one file/feature → read it directly.
- General bug hunt with no public-launch angle → use `parallel-codebase-review`.
- A single component → one `reviewer` suffices, no fan-out needed.

## Workflow

### 1. Fan out 3–4 parallel subagents (respect the user's subagent cap)
Each is a fresh context — be self-contained: name the exact files, the focus
areas, the output contract (`file:line` evidence + severity + concrete fix), and
any "ignore this in-progress feature" caveat the user gave.

| Reviewer | Scope |
|----------|-------|
| `reviewer` (code) | one per major language component (Rust core, Go TUI, …). Adversarial: panics on adversarial input, resource leaks, error swallowing, secret-in-code, security (path confinement, command injection, sandbox). |
| `reviewer` (secrets/PII) | **repo-wide** — hardcoded secrets/tokens/private keys, personal paths/emails/IPs, committed binaries/artifacts, local-only files tracked, LICENSE/README/CI sanity, author fields in manifests. This is the highest-stakes pass. |
| `worker` (build gate) | actually RUN the lint/build/test suite. Report per-step PASS/FAIL + warnings, and split known/in-progress issues from real regressions. |

Tell each reviewer what is **expected-incomplete** (e.g. "presence feature is
mid-implementation — don't flag its incompleteness") so it doesn't waste budget
on known work, and what's a **known pre-existing issue** vs a new regression.

### 2. Re-verify the secrets/PII audit YOURSELF — don't trust a truncated summary
A subagent's detailed body sometimes gets truncated to a one-line verdict in the
orchestrator's parallel-task result. For the highest-stakes claim ("secrets are
clean"), **re-run the scan yourself** with `rg --hidden` (rg skips dotfiles by
default — always pass `--hidden`, exclude `.git/`, `target/`, `node_modules`).
Write a small script (don't inline long `rg` chains) and check:

- Real-looking secrets: `sk-[A-Za-z0-9]{16,}`, `AKIA[0-9A-Z]{16}`,
  `gh[pousr]_…`, `xoxb-`, `Bearer <real-token>`, `BEGIN … PRIVATE KEY`.
  Filter out obvious placeholders (`example`, `your_key`, `<key>`, `sk-xxxx`).
- Personal paths: `/home/<user>`, `/Users/<user>`, `/root`, `C:\Users\…`
  (a container service user like `harness` is fine; a real username is PII).
- Emails; private keys; internal hostnames (`.local`, `.internal`, cloud-metadata
  `169.254.169.254` — note: that IP appearing in code is often the SSRF *risk
  being documented*, not a leak).
- Tracked local-only files: `git ls-files | rg '^scripts/|^tmp/|context\.md|plan\.md|\.env|\.log|\.pem|\.key'`.
- Committed large binaries: `git ls-files -z | while read f; do git cat-file -s "HEAD:$f"; done` — flag anything >1MB.
- `LICENSE` + `README` presence; author/owner fields in `Cargo.toml`/`go.mod`/`package.json` (PII leak); CI `secrets.` usage + `pull_request_target` + internal URLs.

A legitimate MIT/Apache LICENSE attributed to the user's own handle is fine (it's
their copyright, not a leak). The auto-provided `${{ secrets.GITHUB_TOKEN }}` is
standard, not a custom secret.

### 3. Verify surprising Critical/High findings before reporting them
Re-read the cited `file:line` yourself — converts "the reviewer said" into
"verified." Especially for security claims (e.g. "writes keys world-readable"):
confirm the code actually does what's claimed and that the doc comment (if any)
contradicts it. Line numbers drift; the code is the truth.

### 4. Synthesize — don't dump
Merge the reports, **dedupe across reviewers**, and rank by severity:
- **P0 blocker** — must fix or do NOT go public (a leaked secret, a crash on
  normal input, broken build).
- **P1 should-fix** — small surgical fixes to land before public; not
  architectural (a permission gap, a soft wedge, a real data race, fmt drift).
- **P2 nice-to-have** — hardening for a fast follow-up; doesn't block launch.

Lead with a one-line **go/no-go verdict**, then the P1 table (where / issue /
fix), then P2s, then a **"verified clean"** section so the user knows what WAS
checked (security boundaries, no-panic-on-bad-input, resource bounds, secrets
absent). End by offering to implement the P1 fixes.

## Gotchas
- **Stale "known issue" memories.** Before treating a compile/test failure as a
  known pre-existing issue, actually run the gate — the tree may have moved on
  and the issue resolved. Update the stale memory if so (don't leave a wrong
  "tests are broken" note that scares off the next session).
- **Subagent body truncation.** If a parallel task returns only a one-line
  summary instead of its detailed body, that detail is lost — re-verify the
  critical claims yourself rather than reporting the bare verdict.
- **CI blind spots.** The build gate should run the SAME checks CI runs; if CI
  omits something (e.g. a Go project whose CI runs `go vet`/`go build`/`go test`
  but NOT `gofmt --check`), call that out as a finding — formatting drift slips
  through silently. `cargo fmt --all` / `gofmt -w .` are one-command fixes.
- **Parallel batch size.** Default advisory max is 8 (`parallel_max_tasks`); larger batches are allowed and queue under concurrency. Set `concurrency` higher when you want more than the default 4 running at once. Absolute safety max is 256. See `parallel-subagent-cap` memory.

## Applying the findings (fix-all)

When the user says "fix all issues" / "fix everything found" after a review,
this is the apply-and-verify counterpart. The risk is volume + correctness
across two codebases — structure it so each file tree has ONE writer and the
trickiest fix is yours.

1. **Delegate a cohesive same-tree cluster to ONE worker.** Group all fixes
   that share files (e.g. every `tui/main.go` lifecycle fix: atomic process
   ptr, signal-handler quit, startup watchdog, double-`Wait`) into a single
   worker so it keeps coherence across its own edits. Give it EXACT fixes
   (file:line + the change + the rationale) — workers execute well-specified
   edits reliably; vague ones they botch. Tell it what's expected-incomplete
   ("presence feature is mid-impl — don't touch it") and that it's the SOLE
   writer of its tree (so it may run the formatter itself, no race).
2. **Keep the security-sensitive / judgment-heavy fix for yourself.** SSRF
   range-blocking, sandbox-bypass fixes, lifecycle redesigns — read the file,
   design carefully, write tests. Don't hand these to a worker.
3. **Verify with the EXACT CI gates, not just "it compiles":**
   - core: `cargo fmt --all -- --check` · `cargo clippy --all-targets` ·
     `cargo test --locked`
   - tui: `gofmt -l .` (empty) · `go vet ./...` · `go build ./...` ·
     `go test -race ./...` (the `-race` matters — it catches the data-race
     fixes a plain `go test` won't).
4. **Isolate failures to your own edits.** A test failing in a file you didn't
   touch is almost certainly the user's concurrent work (or a pre-existing
   issue) — confirm with `git status --short` which modified files are yours
   vs. theirs before "fixing" it. (See `concurrent-user-edits-isolate-errors`.)
5. **Spot-check worker output — build-passing ≠ fixes-correct.** A worker
   that claims "all 10 fixes done, tests green" may have no-op'd a tricky one.
   `grep` for the specific fix markers (e.g. `atomic.Pointer`, `s.busy = false`
   in the reset handler, `go fillMentionCache`, `modelIdx = -1`) and read the
   one or two trickiest regions yourself. (The `grep` TOOL is flaky — use
   `bash grep -n` instead.)
6. **Run the formatters yourself at the end** (`cargo fmt --all`, `gofmt -w .`)
   and confirm `--check`/`-l` is clean — this is both a fix (P1 fmt drift) and
   the CI gate.
