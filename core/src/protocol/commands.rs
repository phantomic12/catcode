use super::ClientInfo;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

fn default_vision_enabled() -> bool {
    true
}

/// Commands read from stdin.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Command {
    #[serde(rename = "init")]
    Init {
        /// Absent for legacy clients. A newer client version is tolerated and
        /// answered with the core's supported version/capabilities.
        #[serde(default)]
        protocol_version: Option<u32>,
        #[serde(default)]
        client: Option<ClientInfo>,
    },
    #[serde(rename = "set_key")]
    SetKey {
        api_key: String,
        /// Optional provider name this key applies to. When omitted, the key
        /// applies to the currently active provider (backward-compatible with
        /// the pre-provider single-endpoint flow).
        #[serde(default)]
        provider: Option<String>,
    },
    /// Set or clear a search-tool API key (Exa / Tavily) for `web_search`.
    /// `provider` is "exa" | "tavily"; an empty `api_key` clears it. Persisted
    /// to config.json `search_keys` so it survives restarts; read by
    /// `search_tool` ahead of the `EXA_API_KEY` / `TAVILY_API_KEY` env vars.
    #[serde(rename = "set_search_key")]
    SetSearchKey { provider: String, api_key: String },
    /// Switch the active model provider at runtime. Re-resolves base URL / key /
    /// wire protocol, re-discovers models, and emits `provider_changed` + a
    /// fresh `models` event. Unknown names are ignored (stays on current).
    #[serde(rename = "set_provider")]
    SetProvider { name: String },
    /// List the built-in first-party provider presets (Umans, OpenCode Go,
    /// OpenRouter). Emits a `provider_presets` event so the TUI/web can render a
    /// one-click "login" picker. Each entry carries whether a key or OAuth
    /// token is already stored from a prior explicit `/login`.
    #[serde(rename = "list_provider_presets")]
    ListProviderPresets,
    /// Log in to a first-party provider (`umans` | `opencode-go` | `openrouter`):
    /// create the provider config, set its API key, persist, and re-aggregate
    /// models so the provider's models appear in `/models` alongside any others
    /// already logged in. Requires an explicit `api_key` paste (env vars are not
    /// scanned). For plugin subscription OAuth use `login_oauth` instead.
    /// Multiple providers can be logged in at once; each turn routes to the
    /// selected model's provider.
    #[serde(rename = "login")]
    Login {
        preset: String,
        #[serde(default)]
        api_key: Option<String>,
    },
    /// Log out of a provider: drop its runtime key, remove it from the
    /// configured providers, persist the change, and re-aggregate models so its
    /// models disappear from `/models`. No-op (error event) when not logged in.
    #[serde(rename = "logout")]
    Logout { provider: String },
    /// Perform interactive OAuth for a **plugin-declared** `provider_id`
    /// (built-in vendor OAuth was removed from core). Emits `oauth_prompt`
    /// events; on success creates the provider config and refreshes models.
    #[serde(rename = "login_oauth")]
    LoginOauth { preset: String },
    /// Complete a pending plugin OAuth login by submitting the authorization /
    /// user code (or redirect URL) from a prior `oauth_prompt`.
    #[serde(rename = "oauth_code")]
    OauthCode { code: String },
    /// Add (or update) a custom provider with full config parity: same fields
    /// `parse_provider` reads from config.json (`name`, `kind`, `base_url`,
    /// `api_key` | `api_key_env`, `headers`, `context_window`). Inserted or
    /// replaced in the providers list, made the active/fallback provider,
    /// persisted to config.json, and models are re-discovered so the new
    /// provider's models appear immediately. `headers` accepts the same shapes
    /// as config.json: an object {"K":"V"} or an array of ["K","V"] pairs.
    #[serde(rename = "add_custom_provider")]
    AddCustomProvider {
        name: String,
        base_url: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        headers: Option<serde_json::Value>,
        #[serde(default)]
        context_window: Option<u32>,
        /// Per-model capability overrides (same shape as config.json
        /// `models_override`): an object keyed by model id, or an array of
        /// `{id, context_window?, max_tokens?, reasoning?, thinking_levels?}`.
        /// Applied to matching discovered models after discovery + enrichment.
        #[serde(default)]
        models_override: Option<serde_json::Value>,
    },
    /// Discover models from an endpoint WITHOUT persisting a provider. Takes a
    /// base URL + wire kind + optional API key/headers (enough to call the
    /// endpoint's model-list path), runs the normal discovery + models.dev
    /// enrichment, and emits a `provider_models_preview` event with the model
    /// list. Used by the add-custom-provider flow so the user can see (and
    /// refine caps for) the models an endpoint exposes before committing.
    #[serde(rename = "discover_provider_models")]
    DiscoverProviderModels {
        base_url: String,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        headers: Option<serde_json::Value>,
    },
    #[serde(rename = "send")]
    Send {
        prompt: String,
        model: String,
        /// Optional provider the selected model belongs to. When the same model
        /// id is served by several logged-in providers, this is the user's
        /// explicit pick and MUST win routing over the active-provider
        /// tie-break (selecting a model in `/models` uses its owning provider,
        /// no `/login` + provider switch needed). Omitted by older clients.
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        reasoning_effort: Option<String>,
        /// Optional images: each is a data URL (data:image/png;base64,...) or an
        /// absolute file path to attach. Built into a multimodal user message.
        #[serde(default)]
        images: Option<Vec<String>>,
    },
    /// Steer an in-flight turn: interrupt the running turn and redirect it with
    /// `prompt`. If no turn is running, behaves like `send`. Carries model +
    /// reasoning_effort so the steered turn uses the same leader as the run it
    /// interrupted (the TUI always sends a model from its discovered list).
    #[serde(rename = "steer")]
    Steer {
        prompt: String,
        model: String,
        /// Optional owning provider for `model` (same semantics as `send`).
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "abort")]
    Abort,
    /// Drop a queued follow-up/steer prompt WITHOUT aborting the running
    /// turn. Lets the TUI's Esc cancel just the queued message and leave the
    /// in-flight chat running (vs Abort which cancels both).
    #[serde(rename = "clear_queue")]
    ClearQueue,
    #[serde(rename = "reset")]
    Reset,
    /// Clear the in-memory conversation but keep the session file (vs Reset which wipes both).
    #[serde(rename = "clear")]
    Clear,
    /// Drop the last turn (user prompt + its assistant reply + tool calls/results).
    /// Also restores the latest auto filesystem checkpoint when one exists.
    #[serde(rename = "undo")]
    Undo,
    /// Create a hybrid filesystem checkpoint (git stash ref or file snapshot).
    #[serde(rename = "create_checkpoint")]
    CreateCheckpoint {
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        paths: Option<Vec<String>>,
    },
    /// List known checkpoints for this session/workspace.
    #[serde(rename = "list_checkpoints")]
    ListCheckpoints,
    /// Restore a checkpoint by id (filesystem only; conversation unchanged).
    #[serde(rename = "restore_checkpoint")]
    RestoreCheckpoint { id: String },
    /// Force a context compaction now (regardless of the threshold). Optional
    /// `instructions` override `compact_instructions` for this call only (e.g.
    /// `/compact Focus on code samples and API usage`); empty/absent falls back
    /// to the configured default. Always works regardless of `auto_compact`.
    #[serde(rename = "compact")]
    Compact {
        #[serde(default)]
        instructions: Option<String>,
    },
    /// List available session files (returns a `sessions` event).
    #[serde(rename = "list_sessions")]
    ListSessions,
    /// Load a specific session file (replaces the current conversation).
    #[serde(rename = "load_session")]
    LoadSession { path: String },
    /// Set a human-readable title for a saved session.
    #[serde(rename = "rename_session")]
    RenameSession { path: String, title: String },
    /// Delete a non-active saved session and its metadata.
    #[serde(rename = "delete_session")]
    DeleteSession { path: String },
    /// Pin/unpin a session in the picker.
    #[serde(rename = "pin_session")]
    PinSession { path: String, pinned: bool },
    /// Start a fresh session file in the same project directory. The current
    /// file is left intact so sessions accumulate per project. An optional
    /// `path` (a filename) overrides the auto-generated name.
    #[serde(rename = "new_session")]
    NewSession {
        #[serde(default)]
        path: Option<String>,
    },
    /// Request a stats summary (returns a `stats` event).
    #[serde(rename = "stats")]
    Stats,
    /// Inspect lifecycle ownership without mutating it. Returns a
    /// `runtime_status` event containing the active identity, stale-result
    /// count, registered resources, and pending interactive waiter counts.
    #[serde(rename = "runtime_status")]
    RuntimeStatus,
    /// Request a token-usage breakdown of the current context (returns a
    /// `context_breakdown` event): total/context-window/pct, per-role buckets,
    /// and the top token consumers (biggest messages). Read-only.
    #[serde(rename = "context")]
    Context,
    /// Request provider plan/rate-limit usage for the currently selected model
    /// (returns a `usage` event). Each provider implements its own stats
    /// (Umans concurrency/requests, Claude 5h/weekly, Codex 5h/weekly, …).
    /// Optional `model` overrides the last-used model for routing.
    #[serde(rename = "usage")]
    Usage {
        #[serde(default)]
        model: Option<String>,
    },
    /// Approve a pending tool call. decision: "yes" | "no" | "always" |
    /// "allow_session" | "allow_pattern". Optional `pattern` supplies the
    /// path/command glob for `allow_pattern` (defaults to the tool's path arg).
    #[serde(rename = "approve")]
    Approve {
        request_id: String,
        decision: String,
        #[serde(default)]
        pattern: Option<String>,
    },
    /// Change the approval mode at runtime: "never" | "destructive" | "always".
    #[serde(rename = "set_approval")]
    SetApproval { mode: String },
    /// Change a runtime config knob at runtime. Recognized keys:
    ///   bash_timeout_secs (u64).
    /// Values are coerced from the JSON type (string or number).
    #[serde(rename = "set_config")]
    SetConfig { key: String, value: Value },
    /// Get the current sandbox status (mode, readiness, platform checks,
    /// setup actions). Emits a `sandbox_status` event.
    #[serde(rename = "get_sandbox_status")]
    GetSandboxStatus,
    /// Download/verify Microsandbox runtime assets + pull the configured OCI
    /// image (user-space only; never runs sudo or enables OS features). Emits
    /// `sandbox_prepare_progress` then `sandbox_ready` / `sandbox_error`.
    #[serde(rename = "prepare_sandbox")]
    PrepareSandbox,
    /// Stop and recreate the active sandbox (e.g. after it went unhealthy).
    /// Emits a `sandbox_status` event.
    #[serde(rename = "reset_sandbox")]
    ResetSandbox,
    /// Plugin lifecycle commands. `install_plugin.path` accepts a local
    /// directory **or** a GitHub Release source (`owner/repo[@tag]`, full
    /// github.com URL). Release installs download the source `.zip`.
    /// `scope` is `"global"` (default — `~/.catalyst-code/plugins`, every
    /// workspace) or `"workspace"` (this repo's `.catalyst-code/plugins` only).
    /// Workspace user-installs load without `--trust-project-plugins`.
    #[serde(rename = "install_plugin")]
    InstallPlugin {
        path: String,
        #[serde(default)]
        scope: Option<String>,
    },
    #[serde(rename = "remove_plugin")]
    RemovePlugin { name: String },
    #[serde(rename = "enable_plugin")]
    EnablePlugin { name: String },
    #[serde(rename = "disable_plugin")]
    DisablePlugin { name: String },
    #[serde(rename = "list_plugins")]
    ListPlugins,
    /// Re-scan plugin directories, preserving enabled/disabled flags.
    #[serde(rename = "reload_plugins")]
    ReloadPlugins,
    /// Re-emit a `plugin_trust_prompt` event for this project's untrusted
    /// project-scoped plugins (the `/plugin-trust` command). Includes plugins
    /// with a recorded decision so the user can change their mind.
    #[serde(rename = "plugin_trust_prompt")]
    PluginTrustPrompt,
    /// Record the user's trust decisions for this project's plugins
    /// (name → "trust" | "deny"), persist them, re-scan so newly-trusted
    /// plugins load, and emit `plugin_trust_applied`. Only the caller-supplied
    /// names are (re)decided; names the user did not touch keep their prior
    /// state when they had one.
    #[serde(rename = "plugin_trust_decisions")]
    PluginTrustDecisions {
        /// plugin name → "trust" | "deny".
        decisions: HashMap<String, String>,
    },
    /// Run a plugin-declared slash command by name.
    #[serde(rename = "plugin_command")]
    PluginCommand {
        name: String,
        #[serde(default)]
        args: String,
    },
    /// List slash commands declared by enabled plugins.
    #[serde(rename = "list_plugin_commands")]
    ListPluginCommands,
    /// Re-discover available subagents (builtin + user + project) and emit an
    /// `agents` event. Used by the web/TUI agent pickers.
    #[serde(rename = "list_agents")]
    ListAgents,
    /// Ask core to re-inject memories into the system prompt (called after saving a memory).
    #[serde(rename = "refresh_memory")]
    RefreshMemory,
    /// Save a memory note (persisted across sessions). Core generates a name,
    /// saves it, and refreshes the system-prompt injection. Emits a
    /// `memory_saved` event with the new id. `scope` is "workspace" (default)
    /// or "global" (cross-codebase: user identity, tech-stack preferences, etc).
    #[serde(rename = "save_memory")]
    SaveMemory {
        text: String,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        scope: Option<String>,
    },
    /// List saved memories (both global and workspace scopes). Emits a
    /// `memory_list` event.
    #[serde(rename = "list_memory")]
    ListMemory,
    /// Forget (delete) a memory by its id (the slug or the memory name).
    /// Searches both scopes when `scope` is omitted. Emits a `memory_saved`
    /// event describing the outcome.
    #[serde(rename = "forget_memory")]
    ForgetMemory {
        id: String,
        #[serde(default)]
        scope: Option<String>,
    },
    /// Reply to a subagent's contact_supervisor need_decision ask.
    /// The TUI surfaces an `intercom_message` event and the user (acting as
    /// the orchestrator) replies with this command; the awaiting subagent
    /// wakes and continues.
    #[serde(rename = "intercom_reply")]
    IntercomReply { request_id: String, reply: String },
    /// List live subagent jobs for process-tree inspection.
    #[serde(rename = "job_list")]
    JobList,
    /// Query a live subagent job or its durable artifact after completion.
    #[serde(rename = "job_status")]
    JobStatus { run_id: String },
    /// Cancel a live subagent job. Cancellation is terminal and prevents
    /// worktree promotion; terminal artifacts remain available for inspection.
    #[serde(rename = "job_cancel")]
    JobCancel { run_id: String },
    #[serde(rename = "job_wait")]
    JobWait {
        run_id: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "session_tree")]
    SessionTree,
    #[serde(rename = "session_branch")]
    SessionBranch { entry_id: String },
    /// Reply to a pending `ask_request` (the `ask` tool). `answers` is either a
    /// JSON object mapping each question id → its answer string, or JSON null
    /// to indicate the user skipped the questions. The awaiting tool call
    /// resumes and the formatted answers are returned to the model.
    #[serde(rename = "ask_reply")]
    AskReply { request_id: String, answers: Value },
    /// Reply to a pending `sudo_request` (a bash command that invokes `sudo`).
    /// `approved: false` declines the request (the command is NOT run and the
    /// agent is told). `approved: true` with a `password` runs the command with
    /// `sudo -S` and feeds the password on stdin (so sudo never touches /dev/tty
    /// and garbles the TUI). The password is used once and not stored.
    #[serde(rename = "sudo_reply")]
    SudoReply {
        request_id: String,
        #[serde(default)]
        approved: bool,
        #[serde(default)]
        password: Option<String>,
    },
    /// Get the current vision-handoff configuration (enabled flag, curated
    /// vision-capable models + preferred target). Emits a `vision_config` event.
    #[serde(rename = "get_vision_config")]
    GetVisionConfig,
    /// Set the vision-handoff configuration and persist it to
    /// .catalyst-code/vision.json. `enabled` defaults true when omitted
    /// (recommended ON). `vision_model` is the preferred handoff target; an
    /// empty string / null means "cheapest same-provider". Emits a
    /// `vision_config` event with the new state.
    #[serde(rename = "set_vision_config")]
    SetVisionConfig {
        /// When omitted, leave the previous `enabled` value unchanged on merge
        /// paths; the command handler treats absent as `Some(true)` only when
        /// constructing a full replace — see main.rs (defaults to true).
        #[serde(default = "default_vision_enabled")]
        enabled: bool,
        #[serde(default)]
        vision_models: Vec<String>,
        #[serde(default)]
        vision_model: Option<String>,
    },
    /// List discoverable skills (project then user scope). Emits a `skills`
    /// event with each skill's name, description, and location — used by the
    /// TUI/web to populate the `/skill:<name>` autocomplete.
    #[serde(rename = "list_skills")]
    ListSkills,
    /// List installed marketplace skills and disclaimer state.
    #[serde(rename = "list_marketplace_skills")]
    ListMarketplaceSkills,
    /// Persist first-time consent in the user-owned config directory.
    #[serde(rename = "accept_skill_disclaimer")]
    AcceptSkillDisclaimer,
    /// Search skills.sh. Requires prior disclaimer acceptance.
    #[serde(rename = "search_marketplace_skills")]
    SearchMarketplaceSkills { query: String },
    /// Install a skills.sh snapshot into project or global scope.
    #[serde(rename = "install_marketplace_skill")]
    InstallMarketplaceSkill {
        source: String,
        name: String,
        scope: String,
    },
    /// Update one managed skill from its recorded skills.sh source.
    #[serde(rename = "update_marketplace_skill")]
    UpdateMarketplaceSkill { name: String, scope: String },
    /// Remove one managed skill from the selected scope.
    #[serde(rename = "remove_marketplace_skill")]
    RemoveMarketplaceSkill { name: String, scope: String },
    /// On-demand model-cache refresh: force a LIVE discovery for every
    /// logged-in provider (bypassing the 8h disk-cache TTL, rewriting the
    /// cache), re-aggregate, and re-emit `models` + `provider_presets`,
    /// finishing with a `models_refreshed` event (count + per-provider ids).
    /// Runs off the command loop — live /models + models.dev enrichment can
    /// take many seconds; a dead endpoint falls back to the stale cache so
    /// the list never shrinks. Triggered by TUI `/refresh` and the web
    /// model-picker refresh button.
    #[serde(rename = "refresh_models")]
    RefreshModels,
    /// Invoke a skill by name: the core reads the matching SKILL.md (resolving
    /// project > user scope, bypassing the read_file path restriction so global
    /// skills under ~/.catalyst-code/skills work too), builds a prompt that
    /// instructs the model to apply it, and runs a normal assistant turn.
    /// `task` is an optional follow-up appended to the skill instructions.
    #[serde(rename = "apply_skill")]
    ApplySkill {
        name: String,
        #[serde(default)]
        task: Option<String>,
        model: String,
        #[serde(default)]
        reasoning_effort: Option<String>,
    },
    /// Start goal mode: plan then (optionally) deploy subagents under the
    /// given concurrency and model/provider allowlists. Emits `goal_state`
    /// and kicks a planning turn that must call `goal_write_plan`.
    #[serde(rename = "start_goal")]
    StartGoal {
        goal: String,
        #[serde(default)]
        concurrency: Option<u32>,
        #[serde(default)]
        max_tasks: Option<u32>,
        #[serde(default)]
        allowed_models: Option<Vec<String>>,
        #[serde(default)]
        allowed_providers: Option<Vec<String>>,
        /// Default true: deploy immediately after a valid plan.
        /// When false, stop at plan_ready until `approve_goal_plan`.
        #[serde(default)]
        auto_deploy: Option<bool>,
        /// Enable autonomous Control Center CEO loop (self-review + verify/replan).
        /// Default false preserves classic single-pass `/goal`.
        #[serde(default)]
        ceo_mode: Option<bool>,
        /// Cap on verify→replan cycles when `ceo_mode` (default 3).
        #[serde(default)]
        max_iterations: Option<u32>,
        /// Cap on pre-deploy plan self-review revisions when `ceo_mode` (default 2).
        #[serde(default)]
        max_plan_revisions: Option<u32>,
        /// Advanced: pin models for planner / worker / reviewer agents.
        #[serde(default)]
        planner_model: Option<String>,
        #[serde(default)]
        worker_model: Option<String>,
        #[serde(default)]
        reviewer_model: Option<String>,
        /// Advanced: max concurrent runs per model id (capped by `concurrency`).
        #[serde(default)]
        model_concurrency: Option<std::collections::HashMap<String, u32>>,
        model: String,
        #[serde(default)]
        reasoning_effort: Option<String>,
    },
    /// Cancel the active goal (interrupt planning/deploy runs).
    #[serde(rename = "cancel_goal")]
    CancelGoal,
    /// Re-emit the current goal_state (+ goal_plan if present).
    #[serde(rename = "goal_status")]
    GoalStatus,
    /// Approve a plan that is waiting at plan_ready (auto_deploy=false).
    #[serde(rename = "approve_goal_plan")]
    ApproveGoalPlan,
    /// Re-enter planning with user feedback (from plan_ready / failed).
    #[serde(rename = "revise_goal")]
    ReviseGoal {
        feedback: String,
        model: String,
        #[serde(default)]
        reasoning_effort: Option<String>,
    },
    /// User-initiated bash from the composer (`!cmd` / `!!cmd`), PI-compatible.
    /// Runs in the workspace (same sandbox/denylist as the agent `bash` tool),
    /// emits a `bash_execution` event for the UI, and — unless
    /// `exclude_from_context` — appends a user message with the output so the
    /// next model turn sees it. Does **not** start an assistant turn.
    #[serde(rename = "user_bash")]
    UserBash {
        command: String,
        /// `true` for `!!cmd` — run and show output, but do not add to LLM context.
        #[serde(default)]
        exclude_from_context: bool,
    },
}
