# Glossary

| Term | Definition |
|------|-----------|
| **Agent** | A configurable AI agent with a system prompt, tool set, and optional lifecycle defined as a markdown file with YAML frontmatter (`.catalyst-code/agents/<name>.md`). |
| **Approval gate** | The human-in-the-loop safety mechanism that prompts before executing destructive tool calls (bash, write_file, edit, …). Three modes: never, destructive (default), always. |
| **Core** | The Rust binary (`catcode-core`) that manages conversation state, streams model responses, executes tools, and persists sessions. |
| **Deferred tools** | Tools not included in the default schema; loaded on demand via `load_tools` (e.g. `fetch`, `web_search`, git mutators, `bulk_*`, `diagnostics`, `spawn`). Read-only `git_status`/`diff`/`log`/`show` are core. |
| **Goal mode** | Plan-then-deploy subagent orchestration: `/goal` triggers a planning turn that submits a structured plan, then deploys worker subagents under concurrency and model allowlists. |
| **Hook** | A plugin-declared script that fires at a specific lifecycle point (before/after a tool, session start/stop, etc.), receiving JSON on stdin and returning `{allow, reason?, modify?}` on stdout. |
| **Intercom** | The peer-to-peer coordination channel that lets subagents communicate with the orchestrator and with each other. |
| **Learning** | The self-learning subsystem: persistent memory store (markdown with YAML frontmatter), skills (advisory prompt fragments), and auto-reflection at turn end. |
| **Plugin** | An extension loaded from `.catalyst-code/plugins/<name>/plugin.json` that can register hooks, custom tools, OAuth providers, memory providers, and slash commands. |
| **Plugin scope** | Where a plugin is installed: `global` (`~/.catalyst-code/plugins/`, every workspace) or `workspace` (this repo's `.catalyst-code/plugins/`, project only). |
| **Provider** | A configured AI model endpoint (e.g. Umans, OpenCode Go, OpenRouter, DeepSeek, or a custom OpenAI/Anthropic-compatible endpoint). Multiple providers can be logged in simultaneously. |
| **Sandbox** | Optional Microsandbox microVM that isolates agent-controlled process execution (bash, git, diagnostics, plugin scripts). Runs a separate Linux kernel + filesystem root on Linux (KVM), Apple Silicon macOS, and Windows (WHP). The host environment and credentials are not inherited. See [Sandbox Guide](../guides/sandbox.md). |
| **SDK** | The TypeScript package (`@catalyst-code/coding-agent`) that wraps the core binary's JSONL protocol into a pi-coding-agent-compatible API. |
| **Session** | An append-only JSONL file recording every message in a conversation. Auto-resumed on restart. Schema-versioned for forward compatibility. |
| **Skill** | An advisory prompt fragment stored at `.catalyst-code/skills/<name>/SKILL.md` with YAML frontmatter. Applied via `/skill:<name>` to guide the model on recurring task patterns. |
| **Subagent** | A nested agentic loop that shares the workspace and tools but runs with a focused system prompt. Built-in agents: scout, researcher, planner, worker, reviewer, context-builder, oracle, delegate. |
| **Tool** | A callable function the model invokes via function-calling. Built-in tools include `read_file`, `edit`, `bash`, `grep`, `glob`, `fetch`, `web_search`, `subagent`, and 30+ more. |
| **TUI** | The Go terminal UI (`catcode` binary) built with Bubble Tea. Spawns the core, renders streaming output and tool calls, and handles all slash commands and modals. |
| **Workspace confinement** | Path resolution that restricts all file operations to the workspace root �� absolute paths, `..` traversal, and symlink escapes are rejected. |
