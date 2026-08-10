**CatCode** — open-source coding harness that actually compounds.

Most agents forget everything between sessions and dump one giant turn at a problem. CatCode is different:

🧠 **Self-learning built in** — reflects mid-session, saves durable memories/skills, and injects them next time. `/index` a new repo once; it keeps getting smarter on *your* codebase.

🐶 **Watchdog advisor** — optional second model reviews the executor's work (correctness/security/tests), posts structured findings, and can soft-continue the turn. Fail-open: never blocks the agent if review fails.

🎯 **Goal mode + Control Center** — not "hope the agent finishes." Plan → (self-review) → deploy parallel subagents → verify evidence → replan until certified. CEO mode runs the loop without mid-mission babysitting.

🤝 **Real multi-agent system** — scout / planner / worker / reviewer with fork, parallel, chain, peer intercom, and optional git worktree isolation.

🔒 **MicroVM sandbox** — not a denylist. Real Microsandbox isolation (Linux KVM / Apple Silicon / Windows WHP), fail-closed.

🖥️ **Hub that doesn't die when you close the tab** — multi-project, multi-session chat; leave, sign out, or switch devices and reattach live cores mid-turn. Git + preview + terminal around the chat.

🔌 **Extend without recompiling** — hooks, custom tools, OAuth providers, prompt-backed slash commands (e.g. `/deep-research`).

Same Rust core under both terminal and browser. Open source.

🔗 https://github.com/catalystctl/catcode
🌐 https://code.catalystctl.com

---

**Short version**

> **CatCode** — open-source coding harness that actually compounds.
>
> • Learns your repo across sessions (memory + skills + `/index` `/reflect`)
> • Watchdog second-model review on finished turns
> • Goal / Control Center: plan → parallel subagents → verify → replan until done
> • Real microVM sandbox, not pretend isolation
> • Multi-project Hub: close the browser, work keeps running — reattach anytime
>
> Terminal + browser. One Rust core. Your models, your machine.
> https://github.com/catalystctl/catcode
